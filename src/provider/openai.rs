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
    /// What an llama.cpp server (and Ollama, which runs one) says of its own work:
    /// tokens taken from its cache, newly evaluated, and generated.
    timings: Option<(i64, i64, i64)>,
    done: bool,
}

/// What a `usage` object says was used. Backends agree on the outline, and
/// differ in where they put what was cached and whether reasoning is counted
/// with the output:
///
/// - `prompt_tokens` is the whole prompt everywhere, cached part included.
/// - Read from a cache: `prompt_tokens_details.cached_tokens` (OpenAI, and
///   most others), or `prompt_cache_hit_tokens` (DeepSeek's first name for it).
/// - Written to a cache, where that is charged apart:
///   `prompt_tokens_details.cache_write_tokens` (OpenRouter) or
///   `.created_cache_tokens` (vLLM).
/// - Output: `completion_tokens`. xAI, and Gemini's OpenAI endpoint, leave
///   reasoning out of it and put it in `total_tokens` only, so where the total
///   says more was produced than `completion_tokens` does, the total is right.
fn read_usage(u: &Value) -> Usage {
    let n = |path: &str| u.pointer(path).and_then(Value::as_i64);
    let prompt = n("/prompt_tokens").unwrap_or(0).max(0);
    let cache_read = n("/prompt_tokens_details/cached_tokens").or(n("/prompt_cache_hit_tokens")).unwrap_or(0).clamp(0, prompt);
    let cache_write =
        n("/prompt_tokens_details/cache_write_tokens").or(n("/prompt_tokens_details/created_cache_tokens")).unwrap_or(0).clamp(0, prompt - cache_read);
    let completion = n("/completion_tokens").unwrap_or(0).max(0);
    let output = n("/total_tokens").map(|total| total - prompt).filter(|produced| *produced > completion).unwrap_or(completion);
    Usage { input: prompt - cache_read - cache_write, output, cache_read, cache_write, estimated: false }
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
        // Usage comes on a chunk of its own, on the last chunk with content, or on every
        // chunk as a running count: the last one seen stands. Groq also sends it under `x_groq`.
        if let Some(u) = v.get("usage").filter(|u| u.is_object()).or_else(|| v.pointer("/x_groq/usage").filter(|u| u.is_object())) {
            self.usage = read_usage(u);
            self.saw_usage = true;
        }
        if let Some(t) = v.get("timings").filter(|t| t.is_object()) {
            let n = |key: &str| t[key].as_i64().unwrap_or(0).max(0);
            self.timings = Some((n("cache_n"), n("prompt_n"), n("predicted_n")));
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
        if let Some((cached, evaluated, generated)) = self.timings {
            if !self.saw_usage {
                // No usage asked for or given, but the server's own count is as good.
                self.usage = Usage { input: evaluated, output: generated, cache_read: cached, cache_write: 0, estimated: false };
                self.saw_usage = true;
            } else {
                // The server evaluated `evaluated` tokens and took `cached` from its cache: together, the
                // prompt. Builds that report only the evaluated part as `prompt_tokens` (Ollama 0.30.0),
                // or that do not say what was cached (llama.cpp before 2026), are put right from this.
                let prompt = (self.usage.input + self.usage.cache_read + self.usage.cache_write).max(cached + evaluated);
                if self.usage.cache_read == 0 {
                    self.usage.cache_read = cached.min(prompt);
                }
                self.usage.input = (prompt - self.usage.cache_read - self.usage.cache_write).max(0);
            }
        }
        // How large the request was, near enough, by its length.
        let chars: usize = req.system.len()
            + req.messages.iter().map(|m| serde_json::to_string(&m.blocks).map(|s| s.len()).unwrap_or(0)).sum::<usize>()
            + req.tools.iter().map(|t| t.name.len() + t.schema.to_string().len() + t.description.len()).sum::<usize>();
        let expected = crate::util::estimate_tokens(chars);
        let stated = self.usage.input + self.usage.cache_read + self.usage.cache_write;
        if self.saw_usage && stated * 5 < expected * 2 {
            // The server says the prompt was under two fifths of any size it could have been. That
            // is a server counting only what it newly evaluated and leaving out what it had cached
            // (Ollama has done so in several versions). What it left out is put back, as read from
            // its cache, and the count is marked as partly narsil's own: without this the context
            // would look smaller than it is, and would be compacted too late.
            self.usage.cache_read += expected - stated;
            self.usage.estimated = true;
        }
        if !self.saw_usage {
            // No usage from the server: estimate, so compaction still triggers, and say that it is one.
            let said: usize = blocks
                .iter()
                .map(|b| match b {
                    Block::Text { text } | Block::Thinking { text, .. } => text.len(),
                    Block::ToolUse { name, input, .. } => name.len() + input.to_string().len(),
                    _ => 0,
                })
                .sum();
            self.usage = Usage { input: crate::util::estimate_tokens(chars), output: crate::util::estimate_tokens(said), estimated: true, ..Default::default() };
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

    /// What narsil takes a stream's usage to be: (input that was neither read from a cache
    /// nor written to one, read from cache, written to cache, output, estimated).
    fn used(chunks: &[&str]) -> (i64, i64, i64, i64, bool) {
        let sink = |_e: StreamEvent| {};
        let mut st = StreamState::default();
        for c in chunks {
            st.handle(c, &sink).unwrap();
        }
        let msgs = [];
        let u = st.finish(&req(&msgs)).unwrap().usage;
        (u.input, u.cache_read, u.cache_write, u.output, u.estimated)
    }

    const SAID: &str = r#"{"choices":[{"index":0,"delta":{"content":"ok"}}]}"#;
    const DONE: &str = r#"{"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#;

    // Each backend's usage as its documentation (or, for the open servers, its source) gives it.
    // The whole prompt is always input + read + written; what differs is where the parts are found.
    #[test]
    fn usage_is_read_as_each_backend_reports_it() {
        // OpenAI: a last chunk with no choices; cached tokens inside prompt_tokens, reasoning inside completion_tokens.
        let openai = r#"{"choices":[],"usage":{"prompt_tokens":2006,"completion_tokens":300,"total_tokens":2306,"prompt_tokens_details":{"cached_tokens":1920,"audio_tokens":0},"completion_tokens_details":{"reasoning_tokens":128}}}"#;
        assert_eq!(used(&[SAID, DONE, openai]), (86, 1920, 0, 300, false));

        // DeepSeek: on the last chunk, which has a choice; cache hits under its own name and under OpenAI's.
        let deepseek = r#"{"choices":[{"index":0,"delta":{"content":""},"logprobs":null,"finish_reason":"stop"}],"usage":{"prompt_tokens":4100,"completion_tokens":11,"total_tokens":4111,"prompt_tokens_details":{"cached_tokens":3968},"prompt_cache_hit_tokens":3968,"prompt_cache_miss_tokens":132}}"#;
        assert_eq!(used(&[SAID, deepseek]), (132, 3968, 0, 11, false));
        // And as it was before it took up OpenAI's name.
        let deepseek_then = r#"{"choices":[{"index":0,"delta":{"content":""},"finish_reason":"stop"}],"usage":{"prompt_tokens":1000,"completion_tokens":5,"total_tokens":1005,"prompt_cache_hit_tokens":896,"prompt_cache_miss_tokens":104}}"#;
        assert_eq!(used(&[SAID, deepseek_then]), (104, 896, 0, 5, false));

        // xAI: reasoning is not in completion_tokens; the total has it (32 + 9 + 94 = 135).
        let xai = r#"{"choices":[],"usage":{"prompt_tokens":32,"completion_tokens":9,"total_tokens":135,"prompt_tokens_details":{"text_tokens":32,"cached_tokens":6},"completion_tokens_details":{"reasoning_tokens":94}}}"#;
        assert_eq!(used(&[SAID, DONE, xai]), (26, 6, 0, 103, false));
        // Gemini through its OpenAI endpoint: thinking is in the total only.
        let gemini = r#"{"choices":[],"usage":{"completion_tokens":102,"prompt_tokens":758,"total_tokens":1725}}"#;
        assert_eq!(used(&[SAID, DONE, gemini]), (758, 0, 0, 967, false));

        // OpenRouter: a usage chunk with a choice that repeats the finish reason; cache writes named apart.
        let openrouter = r#"{"id":"gen-1","choices":[{"index":0,"delta":{"content":"","role":"assistant"},"finish_reason":"stop","native_finish_reason":"stop"}],"usage":{"prompt_tokens":194,"completion_tokens":2,"total_tokens":196,"cost":0.00095,"prompt_tokens_details":{"cached_tokens":0,"cache_write_tokens":100},"completion_tokens_details":{"reasoning_tokens":0}}}"#;
        assert_eq!(used(&[SAID, DONE, openrouter]), (94, 0, 100, 2, false));

        // Mistral: null on every chunk but the last, which carries content, the finish reason and the usage.
        let mistral_first = r#"{"choices":[{"index":0,"delta":{"content":"1"},"finish_reason":null}],"usage":null}"#;
        let mistral_last = r#"{"choices":[{"index":0,"delta":{"content":"00"},"finish_reason":"stop"}],"usage":{"prompt_tokens":19,"completion_tokens":7,"total_tokens":26}}"#;
        assert_eq!(used(&[mistral_first, mistral_last]), (19, 0, 0, 7, false));

        // Groq: timings beside the counts, and the same object under `x_groq` on the last chunk.
        let groq = r#"{"choices":[],"usage":{"queue_time":0.037,"prompt_tokens":4641,"prompt_time":0.0006,"completion_tokens":556,"completion_time":0.46,"total_tokens":5197,"total_time":0.46,"prompt_tokens_details":{"cached_tokens":4608}}}"#;
        assert_eq!(used(&[SAID, DONE, groq]), (33, 4608, 0, 556, false));
        let groq_x = r#"{"choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"x_groq":{"id":"req_1","usage":{"prompt_tokens":18,"completion_tokens":556,"total_tokens":574}}}"#;
        assert_eq!(used(&[SAID, groq_x]), (18, 0, 0, 556, false));

        // vLLM: what its prefix cache took in is named too; a running count on every chunk ends on the true one.
        let vllm_running = r#"{"choices":[{"index":0,"delta":{"content":"o"}}],"usage":{"prompt_tokens":900,"completion_tokens":1,"total_tokens":901}}"#;
        let vllm = r#"{"choices":[],"usage":{"prompt_tokens":900,"completion_tokens":40,"total_tokens":940,"prompt_tokens_details":{"cached_tokens":512,"created_cache_tokens":300},"completion_tokens_details":{"reasoning_tokens":12}}}"#;
        assert_eq!(used(&[vllm_running, DONE, vllm]), (88, 512, 300, 40, false));

        // LM Studio: the three counts and no more.
        let lmstudio = r#"{"choices":[],"usage":{"prompt_tokens":1203,"completion_tokens":57,"total_tokens":1260}}"#;
        assert_eq!(used(&[SAID, DONE, lmstudio]), (1203, 0, 0, 57, false));
    }

    #[test]
    fn a_llama_cpp_servers_own_count_fills_in_what_its_usage_leaves_out() {
        // A current llama-server: the whole prompt, what was cached, and its timings on the last chunk. They agree.
        let now = r#"{"choices":[],"usage":{"completion_tokens":24,"prompt_tokens":5120,"total_tokens":5144,"prompt_tokens_details":{"cached_tokens":5000}},"timings":{"cache_n":5000,"prompt_n":120,"predicted_n":24}}"#;
        assert_eq!(used(&[SAID, DONE, now]), (120, 5000, 0, 24, false));

        // A build from before it said what was cached: the timings say.
        let before = r#"{"choices":[],"usage":{"completion_tokens":24,"prompt_tokens":5120,"total_tokens":5144},"timings":{"cache_n":5000,"prompt_n":120,"predicted_n":24}}"#;
        assert_eq!(used(&[SAID, DONE, before]), (120, 5000, 0, 24, false));

        // Ollama 0.30.0 gave only the newly evaluated part as prompt_tokens. The prompt was 5120 all the same.
        let ollama_030 = r#"{"choices":[],"usage":{"completion_tokens":24,"prompt_tokens":120,"total_tokens":144},"timings":{"cache_n":5000,"prompt_n":120,"predicted_n":24}}"#;
        assert_eq!(used(&[SAID, DONE, ollama_030]), (120, 5000, 0, 24, false));

        // The timings on the finish chunk and the usage after it, as a stream has them.
        let timed = r#"{"choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"timings":{"cache_n":0,"prompt_n":812,"predicted_n":9}}"#;
        let counted = r#"{"choices":[],"usage":{"completion_tokens":9,"prompt_tokens":812,"total_tokens":821}}"#;
        assert_eq!(used(&[SAID, timed, counted]), (812, 0, 0, 9, false));

        // No usage at all (a server that would not take `stream_options`), but timings: a count, not an estimate.
        assert_eq!(used(&[SAID, timed]), (812, 0, 0, 9, false));
        // Neither: narsil's own estimate, and marked as one.
        assert!(used(&[SAID, DONE]).4);
    }

    #[test]
    fn a_prompt_reported_far_smaller_than_it_was_is_put_right_and_marked() {
        // A request of about 5,000 tokens by its length.
        let msgs = [Message::user_text("word ".repeat(4000))];
        let finish = |usage: &str| {
            let sink = |_e: StreamEvent| {};
            let mut st = StreamState::default();
            for chunk in [SAID, DONE, usage] {
                st.handle(chunk, &sink).unwrap();
            }
            let u = st.finish(&req(&msgs)).unwrap().usage;
            (u.input, u.cache_read, u.output, u.estimated)
        };
        // A server that counted only the 150 tokens it newly evaluated: the rest was in its cache.
        let (new, cached, output, estimated) = finish(r#"{"choices":[],"usage":{"prompt_tokens":150,"completion_tokens":21,"total_tokens":171}}"#);
        assert_eq!((new, output, estimated), (150, 21, true));
        assert!((4800..=5200).contains(&(new + cached)), "{new} + {cached}");
        // A count that is merely on the low side of narsil's guess is the server's to make: tokenizers differ.
        assert_eq!(finish(r#"{"choices":[],"usage":{"prompt_tokens":3100,"completion_tokens":21,"total_tokens":3121}}"#), (3100, 0, 21, false));
        // As is one on the high side.
        assert_eq!(finish(r#"{"choices":[],"usage":{"prompt_tokens":9000,"completion_tokens":21,"total_tokens":9021}}"#), (9000, 0, 21, false));
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
        assert!(res.usage.estimated);
    }
}
