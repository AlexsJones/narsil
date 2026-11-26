//! OpenAI-style Chat Completions over HTTP + SSE. This one protocol reaches
//! OpenAI, Gemini (its OpenAI endpoint), OpenRouter, DeepSeek, Mistral, Groq,
//! xAI, and local servers: llama.cpp, Ollama, LM Studio, vLLM.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Result, bail};
use futures_util::StreamExt;
use serde_json::{Value, json};

use super::sse::SseDecoder;
use super::{
    ChatRequest, ChatResponse, EventSink, ProviderError, StopReason, StreamEvent, classify_http, error_message,
    http_client, looks_like_context_overflow, network_error, parse_tool_input, retry_after,
};
use crate::config::ProviderConfig;
use crate::types::{Block, Message, Role, Usage};
use crate::util::new_id;

pub struct Client {
    http: reqwest::Client,
    base_url: String,
    api_key: String,
    /// api.openai.com names the output cap `max_completion_tokens`.
    is_openai: bool,
    /// Ask for token usage in the stream; turned off if a server rejects it.
    stream_usage: AtomicBool,
    reasoning_effort: String,
    echo_reasoning: bool,
    headers: BTreeMap<String, String>,
}

impl Client {
    pub fn new(cfg: &ProviderConfig, api_key: String) -> Result<Client> {
        if cfg.base_url.is_empty() {
            bail!("an OpenAI-compatible provider needs a base_url (for example http://127.0.0.1:8080/v1)");
        }
        let base_url = cfg.base_url.trim_end_matches('/').to_string();
        Ok(Client {
            http: http_client(cfg.timeout_secs)?,
            is_openai: base_url.contains("api.openai.com"),
            base_url,
            api_key,
            stream_usage: AtomicBool::new(true),
            reasoning_effort: cfg.reasoning_effort.clone(),
            echo_reasoning: cfg.echo_reasoning,
            headers: cfg.headers.clone(),
        })
    }

    fn request(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        let mut rb = self.http.request(method, format!("{}{}", self.base_url, path));
        if !self.api_key.is_empty() {
            rb = rb.bearer_auth(&self.api_key);
        }
        for (k, v) in &self.headers {
            rb = rb.header(k, v);
        }
        rb
    }

    pub async fn list_models(&self) -> Result<Vec<String>> {
        let resp = self.request(reqwest::Method::GET, "/models").send().await?;
        let status = resp.status();
        let body = resp.text().await?;
        if !status.is_success() {
            bail!("HTTP {}: {}", status.as_u16(), error_message(&body));
        }
        let v: Value = serde_json::from_str(&body)?;
        let list = v["data"].as_array().or_else(|| v["models"].as_array()).or_else(|| v.as_array());
        Ok(list
            .map(|a| {
                a.iter()
                    .filter_map(|m| m["id"].as_str().or_else(|| m["name"].as_str()).map(str::to_string))
                    .collect()
            })
            .unwrap_or_default())
    }

    fn build_body(&self, req: &ChatRequest<'_>) -> Value {
        let mut messages = Vec::new();
        if !req.system.is_empty() {
            messages.push(json!({ "role": "system", "content": req.system }));
        }
        messages.extend(render_messages(req.messages, self.echo_reasoning));
        let mut body = json!({ "model": req.model, "stream": true, "messages": messages });
        if self.stream_usage.load(Ordering::Relaxed) {
            body["stream_options"] = json!({ "include_usage": true });
        }
        if !req.tools.is_empty() {
            let tools: Vec<Value> = req
                .tools
                .iter()
                .map(|t| {
                    json!({ "type": "function", "function": { "name": t.name, "description": t.description, "parameters": t.schema } })
                })
                .collect();
            body["tools"] = json!(tools);
        }
        if req.max_tokens > 0 {
            let key = if self.is_openai { "max_completion_tokens" } else { "max_tokens" };
            body[key] = json!(req.max_tokens);
        }
        if !self.reasoning_effort.is_empty() {
            body["reasoning_effort"] = json!(self.reasoning_effort);
        }
        body
    }

    pub async fn chat(&self, req: &ChatRequest<'_>, sink: EventSink<'_>) -> Result<ChatResponse, ProviderError> {
        let body = self.build_body(req);
        let resp = self
            .request(reqwest::Method::POST, "/chat/completions")
            .json(&body)
            .send()
            .await
            .map_err(network_error)?;
        let status = resp.status().as_u16();
        let request_id = resp.headers().get("x-request-id").and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
        tracing::Span::current().record("request_id", request_id.as_str());
        tracing::debug!(status, request_id, model = req.model, "request sent to the chat completions API");
        if !(200..300).contains(&status) {
            let wait = retry_after(resp.headers());
            let text = resp.text().await.unwrap_or_default();
            if matches!(status, 400 | 422) && text.contains("stream_options") && self.stream_usage.swap(false, Ordering::Relaxed) {
                return Err(ProviderError::Retryable {
                    message: "this server does not accept stream_options; continuing without usage reporting".into(),
                    retry_after: Some(std::time::Duration::ZERO),
                });
            }
            return Err(classify_http(status, &text, wait));
        }

        let mut stream = resp.bytes_stream();
        let mut decoder = SseDecoder::default();
        let mut state = StreamState::default();
        'outer: while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(network_error)?;
            for ev in decoder.push(&chunk) {
                if ev.data.trim() == "[DONE]" {
                    state.done = true;
                    break 'outer;
                }
                state.handle(&ev.data, sink)?;
            }
        }
        state.finish(req)
    }
}

/// Render the neutral transcript as Chat Completions messages.
pub fn render_messages(msgs: &[Message], echo_reasoning: bool) -> Vec<Value> {
    let mut out = Vec::new();
    for m in msgs {
        match m.role {
            Role::User => {
                let mut text = String::new();
                for b in &m.blocks {
                    match b {
                        Block::ToolResult { tool_use_id, content, is_error } => {
                            let body = match (content.is_empty(), *is_error) {
                                (true, _) => "(no output)".to_string(),
                                (false, true) => format!("ERROR: {content}"),
                                (false, false) => content.clone(),
                            };
                            out.push(json!({ "role": "tool", "tool_call_id": tool_use_id, "content": body }));
                        }
                        Block::Text { text: t } if !t.trim().is_empty() => {
                            if !text.is_empty() {
                                text.push_str("\n\n");
                            }
                            text.push_str(t);
                        }
                        _ => {}
                    }
                }
                if !text.is_empty() {
                    out.push(json!({ "role": "user", "content": text }));
                }
            }
            Role::Assistant => {
                let mut text = String::new();
                let mut reasoning = String::new();
                let mut calls = Vec::new();
                for b in &m.blocks {
                    match b {
                        Block::Text { text: t } => text.push_str(t),
                        Block::Thinking { text: t, .. } => reasoning.push_str(t),
                        Block::ToolUse { id, name, input, extra } => {
                            let mut call = json!({
                                "id": id,
                                "type": "function",
                                "function": { "name": name, "arguments": input.to_string() },
                            });
                            if let Some(extra) = extra {
                                call["extra_content"] = extra.clone();
                            }
                            calls.push(call);
                        }
                        _ => {}
                    }
                }
                if text.is_empty() && calls.is_empty() {
                    continue;
                }
                let mut msg = json!({ "role": "assistant" });
                msg["content"] = if text.is_empty() { Value::Null } else { json!(text) };
                if !calls.is_empty() {
                    msg["tool_calls"] = json!(calls);
                }
                if echo_reasoning && !reasoning.is_empty() {
                    msg["reasoning_content"] = json!(reasoning);
                }
                out.push(msg);
            }
        }
    }
    out
}

#[derive(Default)]
struct PartialCall {
    id: String,
    name: String,
    args: String,
    extra: Option<Value>,
}

#[derive(Default)]
struct StreamState {
    text: String,
    reasoning: String,
    calls: Vec<PartialCall>,
    finish: Option<String>,
    usage: Usage,
    saw_usage: bool,
    done: bool,
}

impl StreamState {
    fn handle(&mut self, data: &str, sink: EventSink<'_>) -> Result<(), ProviderError> {
        let Ok(v) = serde_json::from_str::<Value>(data) else {
            return Ok(());
        };
        if v.get("error").is_some_and(|e| !e.is_null()) {
            let msg = error_message(data);
            return Err(if looks_like_context_overflow(&msg) {
                ProviderError::ContextOverflow(msg)
            } else {
                ProviderError::Retryable { message: format!("stream error: {msg}"), retry_after: None }
            });
        }
        if let Some(u) = v.get("usage").filter(|u| u.is_object()) {
            let cached = u.pointer("/prompt_tokens_details/cached_tokens").and_then(Value::as_i64).unwrap_or(0);
            let prompt = u["prompt_tokens"].as_i64().unwrap_or(0);
            self.usage.cache_read = cached.min(prompt);
            self.usage.input = prompt - self.usage.cache_read;
            self.usage.output = u["completion_tokens"].as_i64().unwrap_or(0);
            self.saw_usage = true;
        }
        let Some(choice) = v["choices"].get(0) else {
            return Ok(());
        };
        if let Some(f) = choice["finish_reason"].as_str() {
            self.finish = Some(f.to_string());
        }
        let d = &choice["delta"];
        if let Some(t) = d["content"].as_str().filter(|t| !t.is_empty()) {
            self.text.push_str(t);
            sink(StreamEvent::Text(t.to_string()));
        }
        let reasoning = d["reasoning_content"].as_str().or_else(|| d["reasoning"].as_str());
        if let Some(t) = reasoning.filter(|t| !t.is_empty()) {
            self.reasoning.push_str(t);
            sink(StreamEvent::Thinking(t.to_string()));
        }
        if let Some(calls) = d["tool_calls"].as_array() {
            for (pos, c) in calls.iter().enumerate() {
                let idx = c["index"].as_u64().map(|i| i as usize).unwrap_or(pos);
                if self.calls.len() <= idx {
                    self.calls.resize_with(idx + 1, PartialCall::default);
                }
                let call = &mut self.calls[idx];
                if let Some(id) = c["id"].as_str().filter(|s| !s.is_empty()) {
                    call.id = id.to_string();
                }
                if let Some(name) = c["function"]["name"].as_str() {
                    call.name.push_str(name);
                }
                match &c["function"]["arguments"] {
                    Value::String(s) => call.args.push_str(s),
                    Value::Object(_) => call.args = c["function"]["arguments"].to_string(),
                    _ => {}
                }
                if let Some(extra) = c.get("extra_content").filter(|e| !e.is_null()) {
                    call.extra = Some(extra.clone());
                }
            }
        }
        Ok(())
    }

    fn finish(mut self, req: &ChatRequest<'_>) -> Result<ChatResponse, ProviderError> {
        if !self.done && self.finish.is_none() {
            return Err(ProviderError::Retryable {
                message: "the response stream ended before the message was complete".into(),
                retry_after: None,
            });
        }
        let mut blocks = Vec::new();
        if !self.reasoning.is_empty() {
            blocks.push(Block::Thinking { text: self.reasoning, signature: None });
        }
        if !self.text.is_empty() {
            blocks.push(Block::Text { text: self.text.clone() });
        }
        let mut has_tool = false;
        for c in self.calls {
            if c.name.is_empty() {
                continue;
            }
            has_tool = true;
            let id = if c.id.is_empty() { format!("call_{}", new_id()) } else { c.id };
            blocks.push(Block::ToolUse { id, name: c.name, input: parse_tool_input(&c.args), extra: c.extra });
        }
        if !self.saw_usage {
            // No usage from the server: estimate, so compaction still triggers.
            let chars: usize = req.system.len()
                + req.messages.iter().map(|m| serde_json::to_string(&m.blocks).map(|s| s.len()).unwrap_or(0)).sum::<usize>()
                + req.tools.iter().map(|t| t.schema.to_string().len() + t.description.len()).sum::<usize>();
            self.usage.input = crate::util::estimate_tokens(chars);
            self.usage.output = crate::util::estimate_tokens(self.text.len());
        }
        let stop = match self.finish.as_deref() {
            Some("length") => StopReason::MaxTokens,
            Some("content_filter") => StopReason::Refusal("content_filter".into()),
            _ if has_tool => StopReason::ToolUse,
            _ => StopReason::EndTurn,
        };
        Ok(ChatResponse { blocks, stop, usage: self.usage })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req<'a>(msgs: &'a [Message]) -> ChatRequest<'a> {
        ChatRequest {
            model: "m",
            system: "sys",
            messages: msgs,
            tools: &[],
            max_tokens: 0,
            effort: "",
            thinking: "auto",
            strip_thinking: false,
        }
    }

    #[test]
    fn parses_streamed_text_reasoning_and_split_tool_calls() {
        let chunks = [
            r#"{"choices":[{"delta":{"role":"assistant","reasoning_content":"hmm "}}]}"#,
            r#"{"choices":[{"delta":{"content":"Let me "}}]}"#,
            r#"{"choices":[{"delta":{"content":"check."}}]}"#,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_a","type":"function","function":{"name":"bash","arguments":"{\"comm"}}]}}]}"#,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"and\":\"ls\"}"}},{"index":1,"id":"","function":{"name":"glob","arguments":"{\"pattern\":\"*.rs\"}"}}]}}]}"#,
            r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#,
            r#"{"choices":[],"usage":{"prompt_tokens":100,"completion_tokens":20,"prompt_tokens_details":{"cached_tokens":64}}}"#,
        ];
        let sink = |_e: StreamEvent| {};
        let mut st = StreamState::default();
        for c in chunks {
            st.handle(c, &sink).unwrap();
        }
        let msgs = [];
        let res = st.finish(&req(&msgs)).unwrap();
        assert_eq!(res.stop, StopReason::ToolUse);
        assert_eq!((res.usage.input, res.usage.cache_read, res.usage.output), (36, 64, 20));
        match &res.blocks[..] {
            [
                Block::Thinking { text: r, signature: None },
                Block::Text { text },
                Block::ToolUse { id: a, name: n1, input: i1, .. },
                Block::ToolUse { id: b, name: n2, input: i2, .. },
            ] => {
                assert_eq!(r, "hmm ");
                assert_eq!(text, "Let me check.");
                assert_eq!((a.as_str(), n1.as_str()), ("call_a", "bash"));
                assert_eq!(i1["command"], "ls");
                assert!(b.starts_with("call_"), "a missing id is filled in");
                assert_eq!((n2.as_str(), i2["pattern"].as_str()), ("glob", Some("*.rs")));
            }
            other => panic!("unexpected blocks: {other:?}"),
        }
    }

    #[test]
    fn renders_tool_results_as_tool_messages_before_user_text() {
        let msgs = vec![
            Message::user_text("go"),
            Message {
                role: Role::Assistant,
                blocks: vec![
                    Block::Thinking { text: "plan".into(), signature: None },
                    Block::ToolUse { id: "c1".into(), name: "bash".into(), input: json!({"command":"ls"}), extra: Some(json!({"google":{"thought_signature":"s"}})) },
                ],
            },
            Message {
                role: Role::User,
                blocks: vec![
                    Block::ToolResult { tool_use_id: "c1".into(), content: "boom".into(), is_error: true },
                    Block::Text { text: "and then stop".into() },
                ],
            },
        ];
        let out = render_messages(&msgs, false);
        assert_eq!(out.len(), 4);
        assert_eq!(out[1]["content"], Value::Null);
        assert_eq!(out[1]["tool_calls"][0]["function"]["arguments"], "{\"command\":\"ls\"}");
        assert_eq!(out[1]["tool_calls"][0]["extra_content"]["google"]["thought_signature"], "s");
        assert!(out[1].get("reasoning_content").is_none());
        assert_eq!(out[2]["role"], "tool");
        assert_eq!(out[2]["content"], "ERROR: boom");
        assert_eq!(out[3]["content"], "and then stop");
        assert_eq!(render_messages(&msgs, true)[1]["reasoning_content"], "plan");
    }

    #[test]
    fn missing_usage_is_estimated_and_truncation_is_reported() {
        let sink = |_e: StreamEvent| {};
        let mut st = StreamState::default();
        st.handle(r#"{"choices":[{"delta":{"content":"abcdefgh"},"finish_reason":"length"}]}"#, &sink).unwrap();
        let msgs = [Message::user_text("x".repeat(400))];
        let res = st.finish(&req(&msgs)).unwrap();
        assert_eq!(res.stop, StopReason::MaxTokens);
        assert!(res.usage.input >= 100);
        assert_eq!(res.usage.output, 2);
    }
}
