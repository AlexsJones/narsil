//! Anthropic Messages API over raw HTTP + SSE.
//!
//! The transcript narsil replays is append-only and the system prompt is frozen
//! per context epoch, which is what both prompt caching and signed reasoning
//! blocks require. If the API still rejects replayed reasoning (for example
//! after a model switch), the caller retries once with those blocks stripped.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Result, bail};
use futures_util::StreamExt;
use serde_json::{Value, json};

use super::sse::{SseDecoder, SseEvent};
use super::{
    ChatRequest, ChatResponse, EventSink, ProviderError, StopReason, StreamEvent, classify_http, error_message,
    http_client, network_error, parse_tool_input, retry_after,
};
use crate::config::ProviderConfig;
use crate::types::{Block, Message, Role, Usage};

const API_VERSION: &str = "2026-10-21";
const FALLBACK_BETA: &str = "server-side-fallback-2026-07-01";
const DEFAULT_BASE: &str = "https://api.anthropic.com";

pub struct Client {
    http: reqwest::Client,
    base_url: String,
    api_key: String,
    /// Talking to api.anthropic.com itself rather than a proxy or gateway;
    /// newer request fields are only sent there.
    first_party: bool,
    refusal_fallback: AtomicBool,
    headers: BTreeMap<String, String>,
}

/// Models from before adaptive thinking and the effort parameter.
fn is_legacy(model: &str) -> bool {
    let m = model.to_lowercase();
    ["claude-3", "claude-2", "-4-5", "-4-1", "-4-0", "opus-4-2025", "sonnet-4-2025"].iter().any(|p| m.contains(p))
}

fn supports_fallbacks(model: &str) -> bool {
    matches!(
        model,
        "claude-fable-5-2" | "claude-fable-5-1" | "claude-fable-5" | "claude-opus-5-5" | "claude-opus-5" | "claude-sonnet-5-5"
    )
}

impl Client {
    pub fn new(cfg: &ProviderConfig, api_key: String) -> Result<Client> {
        let base = if cfg.base_url.is_empty() { DEFAULT_BASE } else { cfg.base_url.as_str() };
        let base_url = base.trim_end_matches('/').trim_end_matches("/v1").to_string();
        Ok(Client {
            http: http_client(cfg.timeout_secs)?,
            first_party: base_url.contains("api.anthropic.com"),
            base_url,
            api_key,
            refusal_fallback: AtomicBool::new(cfg.refusal_fallback),
            headers: cfg.headers.clone(),
        })
    }

    fn request(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        let mut rb = self
            .http
            .request(method, format!("{}{}", self.base_url, path))
            .header("anthropic-version", API_VERSION);
        if !self.api_key.is_empty() {
            rb = rb.header("x-api-key", &self.api_key);
        }
        for (k, v) in &self.headers {
            rb = rb.header(k, v);
        }
        rb
    }

    pub async fn list_models(&self) -> Result<Vec<String>> {
        let resp = self.request(reqwest::Method::GET, "/v1/models?limit=1000").send().await?;
        let status = resp.status();
        let body = resp.text().await?;
        if !status.is_success() {
            bail!("HTTP {}: {}", status.as_u16(), error_message(&body));
        }
        let v: Value = serde_json::from_str(&body)?;
        Ok(v["data"]
            .as_array()
            .map(|a| a.iter().filter_map(|m| m["id"].as_str().map(str::to_string)).collect())
            .unwrap_or_default())
    }

    fn build_body(&self, req: &ChatRequest<'_>, fallbacks: bool) -> Value {
        let modern = !is_legacy(req.model);
        let max_tokens = match req.max_tokens {
            0 if modern => 64_000,
            0 => 8192,
            n => n,
        };
        let mut messages = render_messages(req.messages, req.strip_thinking);
        let mut body = json!({ "model": req.model, "max_tokens": max_tokens, "stream": true });
        if !req.system.is_empty() {
            // Explicit breakpoint: the frozen system prompt is the stable prefix.
            body["system"] = json!([{ "type": "text", "text": req.system, "cache_control": { "type": "ephemeral" } }]);
        }
        if !req.tools.is_empty() {
            let tools: Vec<Value> = req
                .tools
                .iter()
                .map(|t| {
                    let mut v = json!({ "name": t.name, "description": t.description, "input_schema": t.schema });
                    if self.first_party {
                        // Stream large tool inputs as they are generated; the
                        // input is validated strictly on our side instead.
                        v["eager_input_streaming"] = json!(true);
                    }
                    v
                })
                .collect();
            body["tools"] = json!(tools);
        }
        if self.first_party {
            // Automatic caching follows the growing conversation tail.
            body["cache_control"] = json!({ "type": "ephemeral" });
        } else if let Some(last) = messages.last_mut().and_then(|m| m["content"].as_array_mut()).and_then(|c| c.last_mut()) {
            last["cache_control"] = json!({ "type": "ephemeral" });
        }
        body["messages"] = json!(messages);
        if modern {
            if req.thinking != "off" {
                body["thinking"] = json!({ "type": "adaptive", "display": "summarized" });
            }
            if !req.effort.is_empty() {
                let effort = if req.effort == "xhigh" && req.model.contains("-4-6") { "high" } else { req.effort };
                body["output_config"] = json!({ "effort": effort });
            }
        }
        if fallbacks {
            body["fallbacks"] = json!("default");
        }
        body
    }

    pub async fn chat(&self, req: &ChatRequest<'_>, sink: EventSink<'_>) -> Result<ChatResponse, ProviderError> {
        let fallbacks =
            self.first_party && self.refusal_fallback.load(Ordering::Relaxed) && supports_fallbacks(req.model);
        let body = self.build_body(req, fallbacks);
        let mut rb = self.request(reqwest::Method::POST, "/v1/messages");
        if fallbacks {
            rb = rb.header("anthropic-beta", FALLBACK_BETA);
        }
        let resp = rb.json(&body).send().await.map_err(network_error)?;
        let status = resp.status().as_u16();
        if !(200..300).contains(&status) {
            let wait = retry_after(resp.headers());
            let text = resp.text().await.unwrap_or_default();
            let msg = error_message(&text);
            if status == 400 {
                if fallbacks && (msg.contains("anthropic-beta") || msg.contains("fallbacks")) {
                    // This account cannot use server-side fallbacks; stop asking.
                    self.refusal_fallback.store(false, Ordering::Relaxed);
                    return Err(ProviderError::Retryable {
                        message: "refusal fallbacks are not available on this account; continuing without them".into(),
                        retry_after: Some(std::time::Duration::ZERO),
                    });
                }
                if msg.contains("signature") && msg.contains("thinking") {
                    return Err(ProviderError::ThinkingMismatch(msg));
                }
            }
            return Err(classify_http(status, &text, wait));
        }

        let mut stream = resp.bytes_stream();
        let mut decoder = SseDecoder::default();
        let mut state = StreamState::default();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(network_error)?;
            for ev in decoder.push(&chunk) {
                state.handle(&ev, sink)?;
            }
            if state.done {
                break;
            }
        }
        state.finish()
    }
}

/// Render the neutral transcript as Messages API turns. Consecutive turns of
/// one role are merged (the API wants alternation), deterministically, so the
/// same transcript always renders to the same prefix.
pub fn render_messages(msgs: &[Message], strip_thinking: bool) -> Vec<Value> {
    let mut out: Vec<(Role, Vec<Value>)> = Vec::new();
    for m in msgs {
        let mut content = Vec::new();
        for b in &m.blocks {
            match b {
                Block::Text { text } => {
                    if !text.trim().is_empty() {
                        content.push(json!({ "type": "text", "text": text }));
                    }
                }
                Block::Thinking { text, signature: Some(sig) } => {
                    if !strip_thinking && m.role == Role::Assistant {
                        content.push(json!({ "type": "thinking", "thinking": text, "signature": sig }));
                    }
                }
                Block::Thinking { .. } => {} // unsigned reasoning from another backend
                Block::ToolUse { id, name, input, .. } => {
                    content.push(json!({ "type": "tool_use", "id": id, "name": name, "input": input }));
                }
                Block::ToolResult { tool_use_id, content: c, is_error } => {
                    let text = if c.is_empty() { "(no output)" } else { c.as_str() };
                    let mut v = json!({ "type": "tool_result", "tool_use_id": tool_use_id, "content": text });
                    if *is_error {
                        v["is_error"] = json!(true);
                    }
                    content.push(v);
                }
                Block::Raw { provider, json } => {
                    let is_reasoning = json["type"] == "redacted_thinking";
                    if provider == "anthropic" && !(strip_thinking && is_reasoning) {
                        content.push(json.clone());
                    }
                }
            }
        }
        if content.is_empty() {
            continue;
        }
        match out.last_mut() {
            Some((role, existing)) if *role == m.role => existing.extend(content),
            _ => out.push((m.role, content)),
        }
    }
    out.into_iter()
        .map(|(role, mut content)| {
            if role == Role::User {
                // Tool results must lead the turn that answers a tool call.
                content.sort_by_key(|v| v["type"] != "tool_result");
            }
            json!({ "role": role.as_str(), "content": content })
        })
        .collect()
}

enum Partial {
    Text(String),
    Thinking { text: String, signature: String },
    Tool { id: String, name: String, json: String },
    Raw(Value),
}

#[derive(Default)]
struct StreamState {
    blocks: Vec<Option<Partial>>,
    stop_reason: Option<String>,
    stop_detail: String,
    usage: Usage,
    done: bool,
}

impl StreamState {
    fn handle(&mut self, ev: &SseEvent, sink: EventSink<'_>) -> Result<(), ProviderError> {
        let Ok(v) = serde_json::from_str::<Value>(&ev.data) else {
            return Ok(());
        };
        match v["type"].as_str().unwrap_or(ev.event.as_str()) {
            "message_start" => self.read_usage(&v["message"]["usage"]),
            "content_block_start" => {
                let idx = v["index"].as_u64().unwrap_or(0) as usize;
                let cb = &v["content_block"];
                let s = |k: &str| cb[k].as_str().unwrap_or("").to_string();
                let partial = match cb["type"].as_str().unwrap_or("") {
                    "text" => Partial::Text(s("text")),
                    "thinking" => Partial::Thinking { text: s("thinking"), signature: s("signature") },
                    "tool_use" => {
                        let preset = cb["input"].as_object().filter(|o| !o.is_empty()).map(|_| cb["input"].to_string());
                        Partial::Tool { id: s("id"), name: s("name"), json: preset.unwrap_or_default() }
                    }
                    _ => Partial::Raw(cb.clone()),
                };
                if self.blocks.len() <= idx {
                    self.blocks.resize_with(idx + 1, || None);
                }
                self.blocks[idx] = Some(partial);
            }
            "content_block_delta" => {
                let idx = v["index"].as_u64().unwrap_or(0) as usize;
                let d = &v["delta"];
                let Some(Some(partial)) = self.blocks.get_mut(idx) else {
                    return Ok(());
                };
                match (d["type"].as_str().unwrap_or(""), partial) {
                    ("text_delta", Partial::Text(text)) => {
                        let piece = d["text"].as_str().unwrap_or("");
                        text.push_str(piece);
                        sink(StreamEvent::Text(piece.to_string()));
                    }
                    ("thinking_delta", Partial::Thinking { text, .. }) => {
                        let piece = d["thinking"].as_str().unwrap_or("");
                        text.push_str(piece);
                        sink(StreamEvent::Thinking(piece.to_string()));
                    }
                    ("signature_delta", Partial::Thinking { signature, .. }) => {
                        signature.push_str(d["signature"].as_str().unwrap_or(""));
                    }
                    ("input_json_delta", Partial::Tool { json, .. }) => {
                        json.push_str(d["partial_json"].as_str().unwrap_or(""));
                    }
                    _ => {}
                }
            }
            "message_delta" => {
                if let Some(r) = v["delta"]["stop_reason"].as_str() {
                    self.stop_reason = Some(r.to_string());
                }
                let details = &v["delta"]["stop_details"];
                if details.is_object() {
                    let cat = details["category"].as_str().unwrap_or("");
                    let why = details["explanation"].as_str().unwrap_or("");
                    self.stop_detail = [cat, why].iter().filter(|s| !s.is_empty()).cloned().collect::<Vec<_>>().join(": ");
                }
                self.read_usage(&v["usage"]);
            }
            "message_stop" => self.done = true,
            "error" => {
                let kind = v["error"]["type"].as_str().unwrap_or("error");
                let message = format!("{kind}: {}", v["error"]["message"].as_str().unwrap_or(""));
                return Err(match kind {
                    "overloaded_error" | "api_error" | "rate_limit_error" | "timeout_error" => {
                        ProviderError::Retryable { message, retry_after: None }
                    }
                    _ => ProviderError::Fatal(message),
                });
            }
            _ => {}
        }
        Ok(())
    }

    fn read_usage(&mut self, u: &Value) {
        let get = |k: &str| u[k].as_i64();
        if let Some(n) = get("input_tokens") {
            self.usage.input = n;
        }
        if let Some(n) = get("output_tokens") {
            self.usage.output = n;
        }
        if let Some(n) = get("cache_read_input_tokens") {
            self.usage.cache_read = n;
        }
        if let Some(n) = get("cache_creation_input_tokens") {
            self.usage.cache_write = n;
        }
    }

    fn finish(self) -> Result<ChatResponse, ProviderError> {
        if !self.done && self.stop_reason.is_none() {
            return Err(ProviderError::Retryable {
                message: "the response stream ended before the message was complete".into(),
                retry_after: None,
            });
        }
        let mut blocks: Vec<Block> = Vec::new();
        for p in self.blocks.into_iter().flatten() {
            match p {
                Partial::Text(text) => {
                    if !text.is_empty() {
                        blocks.push(Block::Text { text });
                    }
                }
                Partial::Thinking { text, signature } => {
                    if !text.is_empty() || !signature.is_empty() {
                        blocks.push(Block::Thinking { text, signature: Some(signature).filter(|s| !s.is_empty()) });
                    }
                }
                Partial::Tool { id, name, json } => {
                    blocks.push(Block::ToolUse { id, name, input: parse_tool_input(&json), extra: None });
                }
                Partial::Raw(json) => blocks.push(Block::Raw { provider: "anthropic".into(), json }),
            }
        }
        // After a server-side fallback, reasoning and tool calls from the model
        // that declined must not be echoed; the fallback marker stays in place.
        let is_fallback = |b: &Block| matches!(b, Block::Raw { json, .. } if json["type"] == "fallback");
        if let Some(boundary) = blocks.iter().rposition(is_fallback) {
            let mut i = 0;
            blocks.retain(|b| {
                let before = i < boundary;
                i += 1;
                let drop = match b {
                    Block::Thinking { .. } | Block::ToolUse { .. } => true,
                    Block::Raw { json, .. } => json["type"] == "redacted_thinking",
                    _ => false,
                };
                !(before && drop)
            });
        }
        let has_tool = blocks.iter().any(|b| matches!(b, Block::ToolUse { .. }));
        let stop = match self.stop_reason.as_deref() {
            Some("refusal") => StopReason::Refusal(self.stop_detail),
            Some("max_tokens") | Some("model_context_window_exceeded") => StopReason::MaxTokens,
            Some("tool_use") => StopReason::ToolUse,
            _ if has_tool => StopReason::ToolUse,
            _ => StopReason::EndTurn,
        };
        Ok(ChatResponse { blocks, stop, usage: self.usage })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(stream: &str) -> (Result<ChatResponse, ProviderError>, String) {
        let seen = std::sync::Mutex::new(String::new());
        let sink = |e: StreamEvent| {
            if let StreamEvent::Text(t) = e {
                seen.lock().unwrap().push_str(&t);
            }
        };
        let mut dec = SseDecoder::default();
        let mut st = StreamState::default();
        let mut err = None;
        for ev in dec.push(stream.as_bytes()) {
            if let Err(e) = st.handle(&ev, &sink) {
                err = Some(e);
                break;
            }
        }
        let res = match err {
            Some(e) => Err(e),
            None => st.finish(),
        };
        (res, seen.into_inner().unwrap())
    }

    const TOOL_TURN: &str = r#"event: message_start
data: {"type":"message_start","message":{"id":"msg_1","usage":{"input_tokens":12,"cache_read_input_tokens":900,"cache_creation_input_tokens":30,"output_tokens":1}}}

event: content_block_start
data: {"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":"","signature":""}}

event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"sig=="}}

event: content_block_stop
data: {"type":"content_block_stop","index":0}

event: content_block_start
data: {"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}

event: ping
data: {"type":"ping"}

event: content_block_delta
data: {"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"Counting "}}

event: content_block_delta
data: {"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"files."}}

event: content_block_start
data: {"type":"content_block_start","index":2,"content_block":{"type":"tool_use","id":"toolu_1","name":"bash","input":{}}}

event: content_block_delta
data: {"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":"{\"command\": \"ls | "}}

event: content_block_delta
data: {"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":"wc -l\"}"}}

event: message_delta
data: {"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":41}}

event: message_stop
data: {"type":"message_stop"}

"#;

    #[test]
    fn parses_a_streamed_tool_turn() {
        let (res, seen) = run(TOOL_TURN);
        let res = res.unwrap();
        assert_eq!(seen, "Counting files.");
        assert_eq!(res.stop, StopReason::ToolUse);
        assert_eq!(res.usage.context_tokens(), 12 + 900 + 30 + 41);
        match &res.blocks[..] {
            [
                Block::Thinking { text, signature: Some(sig) },
                Block::Text { text: t },
                Block::ToolUse { id, name, input, .. },
            ] => {
                assert!(text.is_empty(), "omitted reasoning still carries its signature");
                assert_eq!(sig, "sig==");
                assert_eq!(t, "Counting files.");
                assert_eq!((id.as_str(), name.as_str()), ("toolu_1", "bash"));
                assert_eq!(input["command"], "ls | wc -l");
            }
            other => panic!("unexpected blocks: {other:?}"),
        }
    }

    #[test]
    fn a_cut_stream_is_retryable_and_overload_is_too() {
        let cut = TOOL_TURN.split("event: message_delta").next().unwrap();
        assert!(matches!(run(cut).0, Err(ProviderError::Retryable { .. })));
        let overloaded = "event: error\ndata: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"Overloaded\"}}\n\n";
        assert!(matches!(run(overloaded).0, Err(ProviderError::Retryable { .. })));
    }

    #[test]
    fn refusal_is_reported_with_its_category() {
        let s = "data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":5}}}\n\n\
                 data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"refusal\",\"stop_details\":{\"type\":\"refusal\",\"category\":\"cyber\"}},\"usage\":{\"output_tokens\":0}}\n\n\
                 data: {\"type\":\"message_stop\"}\n\n";
        let res = run(s).0.unwrap();
        assert_eq!(res.stop, StopReason::Refusal("cyber".into()));
        assert!(res.blocks.is_empty());
    }

    #[test]
    fn fallback_boundary_drops_the_declined_models_reasoning() {
        let s = "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"thinking\",\"thinking\":\"\",\"signature\":\"a\"}}\n\n\
                 data: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"text\",\"text\":\"partial\"}}\n\n\
                 data: {\"type\":\"content_block_start\",\"index\":2,\"content_block\":{\"type\":\"fallback\",\"from\":{\"model\":\"x\"},\"to\":{\"model\":\"y\"}}}\n\n\
                 data: {\"type\":\"content_block_start\",\"index\":3,\"content_block\":{\"type\":\"thinking\",\"thinking\":\"\",\"signature\":\"b\"}}\n\n\
                 data: {\"type\":\"content_block_start\",\"index\":4,\"content_block\":{\"type\":\"text\",\"text\":\"answer\"}}\n\n\
                 data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":3}}\n\n\
                 data: {\"type\":\"message_stop\"}\n\n";
        let res = run(s).0.unwrap();
        let kinds: Vec<&str> = res
            .blocks
            .iter()
            .map(|b| match b {
                Block::Text { .. } => "text",
                Block::Thinking { .. } => "thinking",
                Block::Raw { .. } => "raw",
                _ => "other",
            })
            .collect();
        assert_eq!(kinds, ["text", "raw", "thinking", "text"]);
    }

    #[test]
    fn rendering_merges_turns_and_can_strip_reasoning() {
        let msgs = vec![
            Message::user_text("count files"),
            Message {
                role: Role::Assistant,
                blocks: vec![
                    Block::Thinking { text: String::new(), signature: Some("sig".into()) },
                    Block::Thinking { text: "local model musing".into(), signature: None },
                    Block::ToolUse { id: "t1".into(), name: "bash".into(), input: json!({"command":"ls"}), extra: None },
                ],
            },
            Message {
                role: Role::User,
                blocks: vec![Block::ToolResult { tool_use_id: "t1".into(), content: String::new(), is_error: true }],
            },
            Message::user_text("also show sizes"),
        ];
        let out = render_messages(&msgs, false);
        assert_eq!(out.len(), 3, "the two trailing user turns merge");
        assert_eq!(out[1]["content"].as_array().unwrap().len(), 2, "unsigned reasoning is not replayed");
        assert_eq!(out[1]["content"][0]["type"], "thinking");
        let last = out[2]["content"].as_array().unwrap();
        assert_eq!(last[0]["type"], "tool_result");
        assert_eq!(last[0]["content"], "(no output)");
        assert_eq!(last[0]["is_error"], true);
        assert_eq!(last[1]["text"], "also show sizes");

        let stripped = render_messages(&msgs, true);
        assert_eq!(stripped[1]["content"].as_array().unwrap().len(), 1);
        assert_eq!(stripped[1]["content"][0]["type"], "tool_use");
    }

    #[test]
    fn request_shape_depends_on_model_generation_and_endpoint() {
        let cfg = ProviderConfig { kind: "anthropic".into(), ..Default::default() };
        let c = Client::new(&cfg, "k".into()).unwrap();
        let msgs = [Message::user_text("hi")];
        let tools = [crate::types::ToolSpec { name: "bash".into(), description: "d".into(), schema: json!({"type":"object"}) }];
        let mut req = ChatRequest {
            model: "claude-opus-5-5",
            system: "sys",
            messages: &msgs,
            tools: &tools,
            max_tokens: 0,
            effort: "high",
            thinking: "auto",
            strip_thinking: false,
        };
        let b = c.build_body(&req, true);
        assert_eq!(b["thinking"]["type"], "adaptive");
        assert_eq!(b["output_config"]["effort"], "high");
        assert_eq!(b["max_tokens"], 64_000);
        assert_eq!(b["fallbacks"], "default");
        assert_eq!(b["tools"][0]["eager_input_streaming"], true);
        assert!(b.get("temperature").is_none(), "sampling parameters are rejected on this API version");

        req.model = "claude-haiku-4-5";
        let b = c.build_body(&req, false);
        assert!(b.get("thinking").is_none() && b.get("output_config").is_none() && b.get("fallbacks").is_none());

        // Through a gateway: no first-party-only fields, explicit cache marker instead.
        let proxied = ProviderConfig { kind: "anthropic".into(), base_url: "https://gw.example/v1/".into(), ..Default::default() };
        let c = Client::new(&proxied, String::new()).unwrap();
        assert_eq!(c.base_url, "https://gw.example");
        req.model = "claude-opus-5-5";
        let b = c.build_body(&req, false);
        assert!(b.get("cache_control").is_none());
        assert!(b["tools"][0].get("eager_input_streaming").is_none());
        assert_eq!(b["messages"][0]["content"][0]["cache_control"]["type"], "ephemeral");
    }
}
