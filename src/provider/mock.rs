//! A scripted stand-in for a model, used by tests and for trying the harness
//! without an API. It reads a tiny script from the conversation itself:
//!
//!   !bash echo hi ;; !tool memory_save {"title":"t","body":"b"} ;; !say all done
//!
//! Steps are separated by `;;` (or by `~~` in a script nested inside another,
//! such as a subagent's task). Each model call performs the next step:
//!   !bash <command>        call the bash tool
//!   !tool <name> <json>    call any tool
//!   !say <text>            reply with text and end the turn
//!   !tools                 reply with the names of the tools on offer
//! The script is the last user text containing a line that starts with `!`,
//! up to the first blank line. Without a script the mock echoes the user.

use std::time::Duration;

use serde_json::{Value, json};

use super::{ChatRequest, ChatResponse, EventSink, ProviderError, StopReason, StreamEvent};
use crate::types::{Block, Role, Usage};
use crate::util::{estimate_tokens, new_id};

#[derive(Default)]
pub struct Client;

const COMPACTION_MARK: &str = "about to be compacted";

fn find_script(req: &ChatRequest<'_>) -> Option<(usize, Vec<String>)> {
    for (i, m) in req.messages.iter().enumerate().rev() {
        if m.role != Role::User {
            continue;
        }
        for b in &m.blocks {
            let Block::Text { text } = b else { continue };
            if text.contains(COMPACTION_MARK) {
                continue;
            }
            let Some(start) = text.lines().position(|l| l.trim_start().starts_with('!')) else {
                continue;
            };
            let body: Vec<&str> = text.lines().skip(start).take_while(|l| !l.trim().is_empty()).collect();
            // `~~` separates steps in a script that is itself nested inside a `;;` script
            // (a subagent's task inside a spawn_agents step).
            let body = body.join("\n");
            let sep = if body.contains(";;") { ";;" } else { "~~" };
            let steps = body.split(sep).map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
            return Some((i, steps));
        }
    }
    None
}

impl Client {
    pub async fn chat(&self, req: &ChatRequest<'_>, sink: EventSink<'_>) -> Result<ChatResponse, ProviderError> {
        let prompt_chars: usize = req.system.len()
            + req.messages.iter().map(|m| serde_json::to_string(&m.blocks).map(|s| s.len()).unwrap_or(0)).sum::<usize>();
        let last_user = req.messages.iter().rev().find(|m| m.role == Role::User).map(|m| m.text()).unwrap_or_default();

        let script = find_script(req);
        let mut blocks = Vec::new();
        let mut stop = StopReason::EndTurn;
        let say;

        if last_user.contains(COMPACTION_MARK) {
            // Summaries carry the unfinished script forward, like a real model
            // would carry forward its unfinished plan.
            let remaining = script
                .map(|(at, steps)| {
                    let done = req.messages[at + 1..].iter().filter(|m| m.role == Role::Assistant).count();
                    steps.into_iter().skip(done).collect::<Vec<_>>().join(" ;; ")
                })
                .unwrap_or_default();
            say = format!("mock summary of {} earlier messages\n{remaining}", req.messages.len());
        } else if let Some((at, steps)) = script {
            let done = req.messages[at + 1..].iter().filter(|m| m.role == Role::Assistant).count();
            match steps.get(done).map(String::as_str) {
                Some("!tools") => say = req.tools.iter().map(|t| t.name.as_str()).collect::<Vec<_>>().join(" "),
                Some(step) if step.starts_with("!bash") => {
                    let command = step["!bash".len()..].trim().to_string();
                    say = String::new();
                    blocks.push(tool("bash", json!({ "command": command })));
                    stop = StopReason::ToolUse;
                }
                Some(step) if step.starts_with("!tool") => {
                    let rest = step["!tool".len()..].trim();
                    let (name, args) = rest.split_once(char::is_whitespace).unwrap_or((rest, "{}"));
                    let input = serde_json::from_str::<Value>(args.trim()).unwrap_or_else(|_| json!({}));
                    say = String::new();
                    blocks.push(tool(name, input));
                    stop = StopReason::ToolUse;
                }
                Some(step) => say = step.strip_prefix("!say").unwrap_or(step).trim().to_string(),
                None => say = "idle".to_string(),
            }
        } else {
            say = format!("echo: {last_user}");
        }

        if !say.is_empty() {
            for word in say.split_inclusive(' ') {
                sink(StreamEvent::Text(word.to_string()));
                tokio::time::sleep(Duration::from_millis(3)).await;
            }
            blocks.insert(0, Block::Text { text: say.clone() });
        }
        Ok(ChatResponse {
            blocks,
            stop,
            usage: Usage { input: estimate_tokens(prompt_chars), output: estimate_tokens(say.len()) + 1, ..Default::default() },
        })
    }
}

fn tool(name: &str, input: Value) -> Block {
    Block::ToolUse { id: format!("mock_{}", new_id()), name: name.to_string(), input, extra: None }
}
