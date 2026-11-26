//! Tools the model can call. Each is a name, a description, a JSON schema and
//! an executor. The set offered to a session is deterministic, so the request
//! prefix stays stable from call to call.

pub mod agents;
pub mod fs;
pub mod misc;
pub mod shell;

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Result, anyhow};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use crate::config::Config;
use crate::engine::Engine;
use crate::store::{Session, ToolCall};
use crate::types::{INVALID_JSON_KEY, ToolSpec};
use crate::util::{expand_tilde, first_line, truncate_middle};

pub struct ToolCtx<'a> {
    pub engine: &'a Arc<Engine>,
    pub session: &'a Session,
    pub cancel: &'a CancellationToken,
    pub call: &'a ToolCall,
    pub cfg: &'a Config,
}

pub struct ToolOutput {
    pub content: String,
    pub is_error: bool,
}

impl ToolOutput {
    pub fn ok(s: impl Into<String>) -> Self {
        ToolOutput { content: s.into(), is_error: false }
    }
    pub fn err(s: impl Into<String>) -> Self {
        ToolOutput { content: s.into(), is_error: true }
    }
}

pub fn str_arg<'a>(input: &'a Value, key: &str) -> Result<&'a str> {
    input
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("missing required string parameter `{key}`"))
}

pub fn opt_str<'a>(input: &'a Value, key: &str) -> Option<&'a str> {
    input.get(key).and_then(Value::as_str).filter(|s| !s.is_empty())
}

pub fn opt_u64(input: &Value, key: &str) -> Option<u64> {
    let v = input.get(key)?;
    v.as_u64().or_else(|| v.as_str().and_then(|s| s.trim().parse().ok()))
}

pub fn opt_bool(input: &Value, key: &str) -> bool {
    match input.get(key) {
        Some(Value::Bool(b)) => *b,
        Some(Value::String(s)) => s == "true",
        _ => false,
    }
}

/// Resolve a model-supplied path against the session's working directory.
pub fn resolve(cwd: &str, path: &str) -> PathBuf {
    let p = expand_tilde(path);
    if p.is_absolute() { p } else { PathBuf::from(cwd).join(p) }
}

fn spec(name: &str, description: &str, properties: Value, required: &[&str]) -> ToolSpec {
    ToolSpec {
        name: name.to_string(),
        description: description.to_string(),
        schema: json!({ "type": "object", "properties": properties, "required": required }),
    }
}

/// The tools offered to a session, in a fixed order.
pub fn specs(session: &Session, cfg: &Config) -> Vec<ToolSpec> {
    let mut v = vec![
        spec(
            "bash",
            "Run a shell command on the user's machine and return its output. Use this for anything a \
             terminal can do: inspecting the system, running builds and tests, git, package managers. \
             Each call starts in the working directory with a fresh shell, so `cd` does not carry over; \
             chain with `&&` or pass `cwd`. There is no terminal attached: commands that prompt for input \
             fail, so pass non-interactive flags. For long-running work raise `timeout_secs`, or start it \
             in the background with nohup and poll its log.",
            json!({
                "command": { "type": "string", "description": "The command line to run." },
                "timeout_secs": { "type": "integer", "description": "Kill the command after this many seconds. Default 120." },
                "cwd": { "type": "string", "description": "Directory to run in. Default: the working directory." }
            }),
            &["command"],
        ),
        spec(
            "read_file",
            "Read a text file, returned with line numbers. Use this instead of `cat` so you can cite and \
             edit by line. Reads up to 2000 lines; use `offset` and `limit` for larger files.",
            json!({
                "path": { "type": "string" },
                "offset": { "type": "integer", "description": "First line to read, 1-based." },
                "limit": { "type": "integer", "description": "Maximum number of lines." }
            }),
            &["path"],
        ),
        spec(
            "write_file",
            "Create a file or replace its whole contents. Parent directories are created. To change part \
             of an existing file, use edit_file instead.",
            json!({
                "path": { "type": "string" },
                "content": { "type": "string", "description": "The complete new contents." }
            }),
            &["path", "content"],
        ),
        spec(
            "edit_file",
            "Replace an exact string in a file. `old_string` must match the file exactly, including \
             whitespace, and must be unique unless `replace_all` is true; include enough surrounding \
             lines to make it unique. Read the file first.",
            json!({
                "path": { "type": "string" },
                "old_string": { "type": "string" },
                "new_string": { "type": "string" },
                "replace_all": { "type": "boolean", "description": "Replace every occurrence. Default false." }
            }),
            &["path", "old_string", "new_string"],
        ),
        spec(
            "list_dir",
            "List a directory: entry names, with sizes for files and a trailing slash for directories.",
            json!({ "path": { "type": "string", "description": "Default: the working directory." } }),
            &[],
        ),
        spec(
            "glob",
            "Find files by name pattern, newest first. `*` matches within one directory level and `**` \
             across levels, e.g. `src/**/*.rs`. Respects .gitignore.",
            json!({
                "pattern": { "type": "string" },
                "path": { "type": "string", "description": "Directory to search. Default: the working directory." }
            }),
            &["pattern"],
        ),
        spec(
            "grep",
            "Search file contents with a regular expression. Returns matching lines as path:line: text. \
             Respects .gitignore and skips binary files. Use this to locate code before reading it.",
            json!({
                "pattern": { "type": "string", "description": "Regular expression (Rust regex syntax)." },
                "path": { "type": "string", "description": "File or directory. Default: the working directory." },
                "glob": { "type": "string", "description": "Only search files matching this, e.g. `*.rs`." },
                "ignore_case": { "type": "boolean" },
                "max_results": { "type": "integer", "description": "Default 100." }
            }),
            &["pattern"],
        ),
        spec(
            "web_fetch",
            "Fetch a URL and return its text (HTML is reduced to readable text). Use for documentation, \
             references and anything you need to read rather than guess.",
            json!({ "url": { "type": "string", "description": "An http(s) URL." } }),
            &["url"],
        ),
        spec(
            "memory_save",
            "Save something to memory. Scope `global`: lasting facts about the user and how they want to \
             work, valid everywhere. Scope `project`: lasting facts about this working directory that its \
             files do not already state. Scope `task`: working notes for the current task, shared with \
             every agent on it including parallel subagents. Saving with an existing title in the same \
             scope replaces that entry. Call this when the user states a preference or corrects you, and \
             when you learn something that would otherwise have to be rediscovered.",
            json!({
                "title": { "type": "string", "description": "Short and specific; used to find the entry later." },
                "body": { "type": "string", "description": "The fact itself, and for corrections the reason behind it." },
                "scope": { "type": "string", "enum": ["global", "project", "task"] },
                "kind": { "type": "string", "enum": ["user", "feedback", "project", "reference", "note"] }
            }),
            &["title", "body", "scope"],
        ),
        spec(
            "memory_search",
            "Search memory (global, this project, and this task's shared notes) by keywords. Use it when \
             the question may depend on something learned earlier, and to read notes other agents on \
             this task have written.",
            json!({
                "query": { "type": "string" },
                "limit": { "type": "integer", "description": "Default 8." }
            }),
            &["query"],
        ),
        spec(
            "memory_forget",
            "Delete a memory entry by id, when it is wrong or no longer true.",
            json!({ "id": { "type": "integer" } }),
            &["id"],
        ),
    ];
    v.push(spec(
        "objectives",
        "Set the list of objectives for the work in this session, with a status for each. The user sees \
         this list in a side panel and uses it to follow progress and to jump to the matching point in \
         the conversation. Call it when you begin work that has more than a couple of steps, to lay out \
         the objectives; and call it again whenever one is started, finished or dropped. Always send \
         the complete list, keeping the wording of existing entries unchanged. Keep entries short and \
         outcome-shaped (what will be true when it is done). Mark `active` only what is being worked \
         on right now: usually one entry, or several while subagents work on them in parallel.",
        json!({
            "items": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "text": { "type": "string", "description": "The objective, in a few words." },
                        "status": { "type": "string", "enum": ["pending", "active", "done", "dropped"] }
                    },
                    "required": ["text", "status"]
                }
            }
        }),
        &["items"],
    ));
    if session.depth < cfg.agents.max_depth {
        v.push(spec(
            "spawn_agents",
            "Run several subagents in parallel and wait for all of them. Each gets its own context and the \
             same tools, memory and working directory as you, and returns one final report. Use this \
             for independent pieces of work: research across separate areas, changes to unrelated \
             files, parallel investigations. Each task description is all a subagent knows, so make it \
             self-contained: the goal, the relevant paths, and what to report back. Do not use it for \
             work that is quick to do yourself or whose parts depend on each other. The user watches \
             each subagent live, by name: what it is doing and how it ended. When a subagent's work \
             serves one of your objectives, name that objective so the user sees the agent under it.",
            json!({
                "agents": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "name": { "type": "string", "description": "Short label the user will see, e.g. `api-survey`." },
                            "task": { "type": "string", "description": "Complete, self-contained instructions." },
                            "objective": {
                                "type": "string",
                                "description": "The entry of your objectives list this agent works on, copied exactly. Omit if it serves none."
                            }
                        },
                        "required": ["name", "task"]
                    }
                }
            }),
            &["agents"],
        ));
    }
    if session.kind == "goal" {
        v.push(spec(
            "goal_update",
            "Record a milestone or important finding in the goal's progress journal. These entries survive \
             context compaction and restarts, and are shown to you when you resume. Call it whenever \
             something is finished or decided that you would need to know after losing your context.",
            json!({ "note": { "type": "string" } }),
            &["note"],
        ));
        v.push(spec(
            "goal_complete",
            "Mark the goal as achieved. Call this only after verifying the result. The summary is what the \
             user reads first: say what was done, where it is, and how you verified it.",
            json!({ "summary": { "type": "string" } }),
            &["summary"],
        ));
        v.push(spec(
            "goal_blocked",
            "Stop the goal because it cannot proceed without the user. State exactly what you need from \
             them. Do not use this for obstacles you can work around.",
            json!({ "reason": { "type": "string" } }),
            &["reason"],
        ));
    }
    v
}

/// A simple command (no pipes, redirects, chaining or substitution) that
/// starts with an allow-listed prefix can run without asking.
fn is_allowlisted(command: &str, cfg: &Config) -> bool {
    let cmd = command.trim();
    if cmd.chars().any(|c| ";&|<>$`\n(){}".contains(c)) {
        return false;
    }
    cfg.permissions.allow.iter().any(|a| {
        let a = a.trim();
        !a.is_empty() && (cmd == a || cmd.strip_prefix(a).is_some_and(|rest| rest.starts_with(' ')))
    })
}

/// Whether a call changes things and so needs the user's approval in `ask` mode.
pub fn needs_approval(name: &str, input: &Value, cfg: &Config) -> bool {
    match name {
        "bash" => !is_allowlisted(input.get("command").and_then(Value::as_str).unwrap_or(""), cfg),
        "write_file" | "edit_file" => true,
        _ => false,
    }
}

/// One line describing a call, for the transcript.
pub fn summarize_call(name: &str, input: &Value) -> String {
    let s = |k: &str| input.get(k).and_then(Value::as_str).unwrap_or("");
    match name {
        "bash" => first_line(s("command"), 200),
        "read_file" | "write_file" | "edit_file" | "list_dir" => s("path").to_string(),
        "glob" => s("pattern").to_string(),
        "grep" => {
            let path = s("path");
            if path.is_empty() { s("pattern").to_string() } else { format!("{} in {}", s("pattern"), path) }
        }
        "web_fetch" => s("url").to_string(),
        "memory_save" => format!("[{}] {}", s("scope"), s("title")),
        "memory_search" => s("query").to_string(),
        "spawn_agents" => input
            .get("agents")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(|x| x.get("name").and_then(Value::as_str)).collect::<Vec<_>>().join(", "))
            .unwrap_or_default(),
        "objectives" => {
            let items = input.get("items").and_then(Value::as_array).cloned().unwrap_or_default();
            let count = |st: &str| items.iter().filter(|i| i.get("status").and_then(Value::as_str) == Some(st)).count();
            let active = items
                .iter()
                .find(|i| i.get("status").and_then(Value::as_str) == Some("active"))
                .and_then(|i| i.get("text").and_then(Value::as_str))
                .map(|t| format!(", now: {}", first_line(t, 80)))
                .unwrap_or_default();
            format!("{} of {} done{active}", count("done"), items.len())
        }
        "goal_update" => first_line(s("note"), 200),
        "goal_complete" => first_line(s("summary"), 200),
        "goal_blocked" => first_line(s("reason"), 200),
        _ => first_line(&input.to_string(), 200),
    }
}

/// Fuller description shown when asking the user to approve a call.
pub fn describe_call(name: &str, input: &Value) -> String {
    let s = |k: &str| input.get(k).and_then(Value::as_str).unwrap_or("");
    match name {
        "bash" => s("command").to_string(),
        "write_file" => {
            let content = s("content");
            format!(
                "{}\n({} lines, {} bytes)\n{}",
                s("path"),
                content.lines().count(),
                content.len(),
                truncate_middle(content, 1200)
            )
        }
        "edit_file" => format!(
            "{}\n--- replace\n{}\n+++ with\n{}",
            s("path"),
            truncate_middle(s("old_string"), 800),
            truncate_middle(s("new_string"), 800)
        ),
        _ => summarize_call(name, input),
    }
}

pub async fn execute(ctx: &ToolCtx<'_>) -> ToolOutput {
    let input = &ctx.call.input;
    if let Some(raw) = input.get(INVALID_JSON_KEY) {
        // The arguments were not valid JSON. Hand them back so the model can retry.
        return ToolOutput::err(json!({ "INVALID_JSON": raw }).to_string());
    }
    let result = match ctx.call.name.as_str() {
        "bash" => shell::bash(ctx).await,
        "read_file" => fs::read_file(ctx).await,
        "write_file" => fs::write_file(ctx).await,
        "edit_file" => fs::edit_file(ctx).await,
        "list_dir" => fs::list_dir(ctx).await,
        "glob" => fs::glob(ctx).await,
        "grep" => fs::grep(ctx).await,
        "web_fetch" => misc::web_fetch(ctx).await,
        "memory_save" => misc::memory_save(ctx),
        "memory_search" => misc::memory_search(ctx),
        "memory_forget" => misc::memory_forget(ctx),
        "spawn_agents" => agents::spawn_agents(ctx).await,
        "objectives" => misc::objectives(ctx),
        "goal_update" | "goal_complete" | "goal_blocked" => misc::goal_tool(ctx),
        other => Ok(ToolOutput::err(format!("unknown tool: {other}"))),
    };
    let mut out = result.unwrap_or_else(|e| ToolOutput::err(format!("{e:#}")));
    out.content = truncate_middle(&out.content, ctx.cfg.context.tool_output_limit);
    if out.content.trim().is_empty() {
        out.content = "(no output)".to_string();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_simple_allowlisted_commands_skip_approval() {
        let cfg = Config::default();
        let bash = |c: &str| needs_approval("bash", &json!({ "command": c }), &cfg);
        assert!(!bash("ls -la"));
        assert!(!bash("git status"));
        assert!(!bash("  wc -l Cargo.toml "));
        assert!(bash("ls; rm -rf build"), "chaining defeats the allowlist");
        assert!(bash("ls | xargs rm"));
        assert!(bash("cat $(echo /etc/shadow)"));
        assert!(bash("lsblk"), "prefix must end at a word boundary");
        assert!(bash("git push"));
        assert!(bash("rm notes.txt"));
        assert!(needs_approval("write_file", &json!({}), &cfg));
        assert!(!needs_approval("read_file", &json!({}), &cfg));
    }

    #[test]
    fn tool_set_depends_on_session_kind_and_depth() {
        let cfg = Config::default();
        let mut s = Session {
            id: "s".into(),
            parent_id: None,
            root_id: "s".into(),
            goal_id: None,
            kind: "chat".into(),
            agent_name: None,
            depth: 0,
            title: String::new(),
            cwd: "/".into(),
            model: "m/x".into(),
            perm_mode: "ask".into(),
            run_state: "idle".into(),
            system_prompt: String::new(),
            epoch_start: 0,
            strip_thinking_before: 0,
            input_tokens: 0,
            output_tokens: 0,
            context_tokens: 0,
            created_at: 0,
            updated_at: 0,
        };
        let names = |s: &Session| specs(s, &cfg).into_iter().map(|t| t.name).collect::<Vec<_>>();
        assert!(names(&s).contains(&"spawn_agents".to_string()));
        assert!(!names(&s).contains(&"goal_complete".to_string()));
        s.kind = "goal".into();
        assert!(names(&s).contains(&"goal_complete".to_string()));
        s.kind = "subagent".into();
        s.depth = 1;
        assert!(!names(&s).contains(&"spawn_agents".to_string()), "subagents do not nest by default");
    }
}
