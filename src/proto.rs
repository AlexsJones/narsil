//! Wire protocol between the daemon and its clients (TUI, CLI): JSON lines
//! over a Unix socket. Clients send `Request`s; the daemon answers each with a
//! `Reply` and pushes `Envelope`s for the session a client is attached to.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::store::{AgentInfo, Goal, Objective};
use crate::types::StoredMessage;

#[derive(Serialize, Deserialize, Debug)]
pub struct Request {
    pub id: u64,
    /// Nested rather than flattened: several commands have an `id` of their own.
    pub cmd: Command,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Command {
    Ping,
    Status,
    Shutdown,
    Reload,
    SessionCreate { cwd: String, title: Option<String>, model: Option<String> },
    /// Session info, full message history, and anything awaiting approval.
    SessionGet { id: String },
    /// Receive live events for this session tree. One attachment per connection.
    Attach { id: String },
    Send { session: String, text: String },
    /// Stop the current run. For a goal's session this also pauses the goal.
    Interrupt { session: String },
    /// Continue a session from wherever its transcript left off.
    Resume { session: String },
    Approve { id: String, decision: String },
    SetMode { session: String, mode: String },
    SetModel { session: String, model: String },
    /// Every configured provider with the models it offers right now.
    Models,
    /// Change the model that new sessions (`default`) or subagents (`subagent`)
    /// use. Saved to config.toml. An empty subagent model means "the session's".
    SetConfigModel { role: String, model: String },
    Compact { session: String },
    GoalAdd { objective: String, cwd: String, model: Option<String> },
    GoalAction { id: String, action: String },
}

#[derive(Serialize, Deserialize, Debug)]
#[serde(rename_all = "snake_case")]
pub enum ServerMsg {
    Reply {
        id: u64,
        ok: bool,
        #[serde(default)]
        data: Value,
        #[serde(default)]
        error: String,
    },
    Push(Envelope),
}

/// An event plus where it came from. `root` is the session tree it belongs to
/// (empty for global events); `session` may be a subagent inside that tree.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Envelope {
    pub root: String,
    pub session: String,
    pub agent: Option<String>,
    pub event: Event,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    TextDelta { text: String },
    ThinkingDelta { text: String },
    /// The partial reply shown so far was discarded (retry or interrupt).
    StreamReset,
    /// A message was committed to the transcript.
    Message { msg: StoredMessage },
    ToolStart { id: String, name: String, input: Value },
    ToolEnd { id: String, is_error: bool, output: String },
    RunState { state: String },
    Usage { context_tokens: i64, input_tokens: i64, output_tokens: i64 },
    Approval { id: String, tool: String, summary: String },
    ApprovalDone { id: String },
    Notice { level: String, text: String },
    Goal { goal: Goal },
    /// The session's objectives were set or changed status.
    Objectives { items: Vec<Objective> },
    /// A subagent was created, started, or finished. `info` is its full current record.
    Agent { info: Box<AgentInfo> },
    SessionChanged,
}

/// A configured backend and what it offers, for choosing a model.
#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct ProviderModels {
    pub name: String,
    pub kind: String,
    pub base_url: String,
    pub models: Vec<String>,
    /// Why the list could not be fetched; empty when it could.
    pub error: String,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ApprovalInfo {
    pub id: String,
    pub session: String,
    pub agent: Option<String>,
    pub tool: String,
    pub summary: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_command_survives_the_wire() {
        // Commands carrying their own `id` must not collide with the request id.
        let cmds = vec![
            Command::Ping,
            Command::Attach { id: "abc".into() },
            Command::SessionGet { id: "abc".into() },
            Command::Approve { id: "appr".into(), decision: "allow".into() },
            Command::GoalAction { id: "g1".into(), action: "pause".into() },
            Command::Send { session: "s".into(), text: "hi\nthere".into() },
            Command::SessionCreate { cwd: "/w".into(), title: None, model: Some("p/m".into()) },
            Command::Models,
            Command::SetConfigModel { role: "subagent".into(), model: String::new() },
        ];
        for (n, cmd) in cmds.into_iter().enumerate() {
            let line = serde_json::to_string(&Request { id: n as u64 + 7, cmd: cmd.clone() }).unwrap();
            assert!(!line.contains('\n'), "one request per line");
            let back: Request = serde_json::from_str(&line).unwrap();
            assert_eq!(back.id, n as u64 + 7);
            assert_eq!(serde_json::to_string(&back.cmd).unwrap(), serde_json::to_string(&cmd).unwrap());
        }
    }
}
