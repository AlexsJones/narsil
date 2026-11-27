//! Provider-neutral conversation model. Every provider adapter maps to and from
//! these types, and the store persists them verbatim, so a transcript written
//! under one backend can be replayed under another.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Marker key the adapters put in a tool call's input when the model emitted
/// arguments that were not valid JSON. The tool runner answers such calls with
/// an error result instead of executing them.
pub const INVALID_JSON_KEY: &str = "__invalid_json__";

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    User,
    Assistant,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Role::User => "user",
            Role::Assistant => "assistant",
        }
    }
    pub fn parse(s: &str) -> Role {
        if s == "assistant" { Role::Assistant } else { Role::User }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Block {
    Text {
        text: String,
    },
    /// Model reasoning. `signature` is only present for providers that sign
    /// their reasoning (Anthropic); such blocks are replayed unchanged.
    Thinking {
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        signature: Option<String>,
    },
    ToolUse {
        id: String,
        name: String,
        input: Value,
        /// Provider-specific data that must be echoed with the call
        /// (for example Gemini thought signatures).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        extra: Option<Value>,
    },
    ToolResult {
        tool_use_id: String,
        content: String,
        #[serde(default)]
        is_error: bool,
    },
    /// A provider-specific block kept verbatim and replayed only to the
    /// provider that produced it.
    Raw {
        provider: String,
        json: Value,
    },
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Message {
    pub role: Role,
    pub blocks: Vec<Block>,
}

impl Message {
    pub fn user_text(text: impl Into<String>) -> Self {
        Message { role: Role::User, blocks: vec![Block::Text { text: text.into() }] }
    }

    /// Concatenated text blocks.
    pub fn text(&self) -> String {
        blocks_text(&self.blocks)
    }
}

pub fn blocks_text(blocks: &[Block]) -> String {
    let mut out = String::new();
    for b in blocks {
        if let Block::Text { text } = b {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(text);
        }
    }
    out
}

/// What kind of row a stored message is. Everything is sent to the model as a
/// normal user/assistant turn; the kind only affects display and bookkeeping.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum MsgKind {
    /// Ordinary turn: user input, model output, tool results.
    Message,
    /// Harness-authored user turn (goal continuation, truncation notice).
    Note,
    /// Start of a new context epoch: the summary that replaces earlier turns.
    Compaction,
}

impl MsgKind {
    pub fn as_str(self) -> &'static str {
        match self {
            MsgKind::Message => "message",
            MsgKind::Note => "note",
            MsgKind::Compaction => "compaction",
        }
    }
    pub fn parse(s: &str) -> MsgKind {
        match s {
            "note" => MsgKind::Note,
            "compaction" => MsgKind::Compaction,
            _ => MsgKind::Message,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct StoredMessage {
    pub id: i64,
    pub role: Role,
    pub kind: MsgKind,
    pub blocks: Vec<Block>,
    pub created_at: i64,
}

impl StoredMessage {
    pub fn to_message(&self) -> Message {
        Message { role: self.role, blocks: self.blocks.clone() }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct Usage {
    /// Input tokens that were neither read from a cache nor written to one.
    pub input: i64,
    pub output: i64,
    pub cache_read: i64,
    pub cache_write: i64,
    /// The backend reported no counts: these are narsil's own estimate.
    pub estimated: bool,
}

impl Usage {
    /// Tokens the next request will carry at minimum: this request's whole
    /// prompt plus what the model just produced.
    pub fn context_tokens(&self) -> i64 {
        self.input + self.cache_read + self.cache_write + self.output
    }
}

/// A tool as advertised to the model.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub schema: Value,
}
