//! Model backends. Two wire protocols cover every major inference API:
//! Anthropic's Messages API, and OpenAI-style Chat Completions (which Gemini,
//! OpenRouter, DeepSeek, Mistral, Groq, xAI, llama.cpp, Ollama, LM Studio and
//! vLLM all speak). A scripted mock backend exists for tests.

pub mod anthropic;
pub mod mock;
pub mod openai;
pub mod sse;

use std::time::Duration;

use anyhow::{Result, bail};

use crate::config::{Config, Paths, api_key};
use crate::types::{Block, Message, ToolSpec, Usage};

pub struct ChatRequest<'a> {
    /// Model id without the provider prefix.
    pub model: &'a str,
    pub system: &'a str,
    pub messages: &'a [Message],
    pub tools: &'a [ToolSpec],
    /// 0: provider default.
    pub max_tokens: u32,
    pub effort: &'a str,
    pub thinking: &'a str,
    /// Drop signed reasoning blocks from the replayed history.
    pub strip_thinking: bool,
}

#[derive(Debug, Clone)]
pub enum StreamEvent {
    Text(String),
    Thinking(String),
}

pub type EventSink<'a> = &'a (dyn Fn(StreamEvent) + Send + Sync);

#[derive(Debug, Clone, PartialEq)]
pub enum StopReason {
    EndTurn,
    ToolUse,
    MaxTokens,
    /// The model or a safety classifier declined; the string says why if known.
    Refusal(String),
}

#[derive(Debug)]
pub struct ChatResponse {
    pub blocks: Vec<Block>,
    pub stop: StopReason,
    pub usage: Usage,
}

#[derive(Debug)]
pub enum ProviderError {
    /// Worth trying again: rate limit, overload, server error, network drop.
    Retryable { message: String, retry_after: Option<Duration> },
    /// The prompt does not fit the model's context window.
    ContextOverflow(String),
    /// Replayed reasoning blocks were rejected; retry without them.
    ThinkingMismatch(String),
    Fatal(String),
}

impl std::fmt::Display for ProviderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProviderError::Retryable { message, .. } => write!(f, "{message}"),
            ProviderError::ContextOverflow(m) => write!(f, "context window exceeded: {m}"),
            ProviderError::ThinkingMismatch(m) => write!(f, "reasoning blocks rejected: {m}"),
            ProviderError::Fatal(m) => write!(f, "{m}"),
        }
    }
}

pub fn network_error(e: reqwest::Error) -> ProviderError {
    ProviderError::Retryable { message: format!("network error: {e}"), retry_after: None }
}

/// Pull a human-readable message out of a JSON error body.
pub fn error_message(body: &str) -> String {
    serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|v| {
            v.pointer("/error/message")
                .or_else(|| v.pointer("/message"))
                .or_else(|| v.pointer("/error"))
                .and_then(|m| m.as_str().map(str::to_string))
        })
        .unwrap_or_else(|| crate::util::first_line(body, 400))
}

pub fn looks_like_context_overflow(msg: &str) -> bool {
    let m = msg.to_lowercase();
    m.contains("prompt is too long")
        || m.contains("context length")
        || m.contains("context window")
        || m.contains("context size")
        || m.contains("maximum context")
        || m.contains("too many tokens")
        || m.contains("exceeds the available context")
}

/// Map an HTTP failure to an error class shared by both protocols.
pub fn classify_http(status: u16, body: &str, retry_after: Option<Duration>) -> ProviderError {
    let msg = error_message(body);
    let full = format!("HTTP {status}: {msg}");
    match status {
        408 | 409 | 425 | 429 | 500 | 502 | 503 | 504 | 520..=529 => {
            ProviderError::Retryable { message: full, retry_after }
        }
        413 => ProviderError::ContextOverflow(full),
        400 | 422 if looks_like_context_overflow(&msg) => ProviderError::ContextOverflow(full),
        _ => ProviderError::Fatal(full),
    }
}

pub fn retry_after(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
    headers
        .get("retry-after")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<f64>().ok())
        .filter(|s| *s >= 0.0)
        .map(Duration::from_secs_f64)
}

pub enum Provider {
    Anthropic(anthropic::Client),
    OpenAi(openai::Client),
    Mock(mock::Client),
}

impl Provider {
    pub async fn chat(&self, req: &ChatRequest<'_>, sink: EventSink<'_>) -> Result<ChatResponse, ProviderError> {
        match self {
            Provider::Anthropic(c) => c.chat(req, sink).await,
            Provider::OpenAi(c) => c.chat(req, sink).await,
            Provider::Mock(c) => c.chat(req, sink).await,
        }
    }

    pub async fn list_models(&self) -> Result<Vec<String>> {
        match self {
            Provider::Anthropic(c) => c.list_models().await,
            Provider::OpenAi(c) => c.list_models().await,
            Provider::Mock(_) => Ok(vec!["mock".into()]),
        }
    }
}

/// Build the client for a configured provider.
pub fn build(cfg: &Config, paths: &Paths, name: &str) -> Result<Provider> {
    let Some(p) = cfg.providers.get(name) else {
        bail!("provider {name:?} is not configured; add [providers.{name}] to config.toml or run `narsil setup`");
    };
    let key = api_key(paths, name, p);
    match p.kind.as_str() {
        "anthropic" => Ok(Provider::Anthropic(anthropic::Client::new(p, key)?)),
        "openai" => Ok(Provider::OpenAi(openai::Client::new(p, key)?)),
        "mock" => Ok(Provider::Mock(mock::Client)),
        other => bail!("provider {name:?} has unknown kind {other:?} (expected \"anthropic\" or \"openai\")"),
    }
}

pub fn http_client(timeout_secs: u64) -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(20))
        .read_timeout(Duration::from_secs(timeout_secs.max(30)))
        .user_agent(concat!("narsil/", env!("CARGO_PKG_VERSION")))
        .build()?)
}

/// Parse accumulated tool arguments strictly. Models occasionally emit broken
/// JSON; that is reported back to the model instead of being run.
pub fn parse_tool_input(raw: &str) -> serde_json::Value {
    if raw.trim().is_empty() {
        return serde_json::json!({});
    }
    match serde_json::from_str::<serde_json::Value>(raw) {
        Ok(v) if v.is_object() => v,
        _ => serde_json::json!({ crate::types::INVALID_JSON_KEY: raw }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_http_failures() {
        assert!(matches!(classify_http(529, "{}", None), ProviderError::Retryable { .. }));
        assert!(matches!(classify_http(429, "{}", None), ProviderError::Retryable { .. }));
        assert!(matches!(
            classify_http(400, r#"{"error":{"message":"prompt is too long: 250000 tokens"}}"#, None),
            ProviderError::ContextOverflow(_)
        ));
        assert!(matches!(
            classify_http(401, r#"{"error":{"message":"invalid x-api-key"}}"#, None),
            ProviderError::Fatal(_)
        ));
    }

    #[test]
    fn broken_tool_arguments_are_flagged_not_guessed() {
        assert_eq!(parse_tool_input(""), serde_json::json!({}));
        assert_eq!(parse_tool_input(r#"{"a":1}"#), serde_json::json!({"a":1}));
        let bad = parse_tool_input(r#"{"command": "ls"#);
        assert!(bad.get(crate::types::INVALID_JSON_KEY).is_some());
    }
}
