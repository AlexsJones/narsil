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
//!
//! For a demonstration (the screenshots in the documentation are made this
//! way), the scripts can be kept out of what is typed: with
//! `NARSIL_MOCK_SCRIPT` naming a file, a request with no script of its own is
//! answered with the one the file has for it. The file holds blocks apart by
//! a line of `====`; a block's first line is `? words found in the request`,
//! and the rest is its script, in steps apart by `;;`. A step that begins
//! `!after <seconds>` is taken after that long, as a model takes its time.

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

/// The script a demonstration file has for the latest request that it has one for.
fn demo_script(req: &ChatRequest<'_>) -> Option<(usize, Vec<String>)> {
    let file = std::fs::read_to_string(std::env::var_os("NARSIL_MOCK_SCRIPT")?).ok()?;
    let scripts: Vec<(&str, &str)> =
        file.split("\n====").filter_map(|block| block.trim().strip_prefix('?')?.split_once('\n')).map(|(key, script)| (key.trim(), script)).collect();
    req.messages.iter().enumerate().rev().filter(|(_, m)| m.role == Role::User).find_map(|(at, m)| {
        let asked = m.text();
        let (_, script) = scripts.iter().find(|(key, _)| !key.is_empty() && asked.contains(key))?;
        Some((at, script.split(";;").map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect()))
    })
}

impl Client {
    pub async fn chat(&self, req: &ChatRequest<'_>, sink: EventSink<'_>) -> Result<ChatResponse, ProviderError> {
        let prompt_chars: usize = req.system.len()
            + req.messages.iter().map(|m| serde_json::to_string(&m.blocks).map(|s| s.len()).unwrap_or(0)).sum::<usize>();
        let last_user = req.messages.iter().rev().find(|m| m.role == Role::User).map(|m| m.text()).unwrap_or_default();

        let demo = std::env::var_os("NARSIL_MOCK_SCRIPT").is_some();
        let script = find_script(req).or_else(|| demo_script(req));
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
            let mut step = steps.get(done).map(String::as_str);
            // `!after 20 <step>`: the step, once that many seconds have passed.
            if let Some((secs, rest)) = step.and_then(|s| s.strip_prefix("!after ")).and_then(|s| s.trim_start().split_once(char::is_whitespace)) {
                tokio::time::sleep(Duration::from_secs_f64(secs.parse().unwrap_or(0.0))).await;
                step = Some(rest.trim());
            }
            match step {
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
        // Counted as a backend with a prompt cache would: the first request writes the
        // instructions to the cache, and each later one reads everything up to its last message.
        // In a demonstration the tools are counted as a backend counts them, so that the sizes shown
        // are the sizes a real one would give.
        let tool_chars: usize = if demo { req.tools.iter().map(|t| t.name.len() + t.description.len() + t.schema.to_string().len()).sum() } else { 0 };
        let whole = estimate_tokens(prompt_chars + tool_chars);
        let before_last: usize = req.system.len()
            + req.messages.iter().rev().skip(1).map(|m| serde_json::to_string(&m.blocks).map(|s| s.len()).unwrap_or(0)).sum::<usize>();
        let (cache_read, cache_write) = match req.messages.len() {
            0 | 1 => (0, estimate_tokens(req.system.len()).min(whole)),
            _ => (estimate_tokens(before_last).min(whole), 0),
        };
        // And what a tool is called with is output, as it is for a real model.
        let called: usize = if demo {
            blocks.iter().map(|b| if let Block::ToolUse { name, input, .. } = b { name.len() + input.to_string().len() } else { 0 }).sum()
        } else {
            0
        };
        Ok(ChatResponse {
            blocks,
            stop,
            usage: Usage {
                input: whole - cache_read - cache_write,
                output: estimate_tokens(say.len() + called) + 1,
                cache_read,
                cache_write,
                estimated: false,
            },
        })
    }
}

fn tool(name: &str, input: Value) -> Block {
    Block::ToolUse { id: format!("mock_{}", new_id()), name: name.to_string(), input, extra: None }
}
