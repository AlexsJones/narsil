//! Tools the model can call. Each is a name, a description, a JSON schema and
//! an executor. The set offered to a session is deterministic, so the request
//! prefix stays stable from call to call.

pub mod agents;
pub mod fs;
pub mod local;
pub mod misc;
pub mod shell;
pub mod vault;
pub mod work;

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

/// Refuse a path in a part of the Obsidian vault the user has closed to narsil.
pub fn guard(cfg: &Config, path: &std::path::Path) -> Result<()> {
    if crate::obsidian::Vault::open(cfg).is_some_and(|v| v.closes(path)) {
        anyhow::bail!(crate::obsidian::closed_message(&path.display().to_string()));
    }
    Ok(())
}

/// The vault, when a walk from `base` could reach into it: what is closed in
/// it is then left out of the walk.
pub fn vault_near(cfg: &Config, base: &std::path::Path) -> Option<crate::obsidian::Vault> {
    crate::obsidian::Vault::open(cfg).filter(|v| v.holds(base) || v.is_under(base))
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
            "change_directory",
            "Move this session to another directory. Do this first when the task is about a directory \
             other than the one you are in (\"create it in ~/Code/foo\", \"fix the tests in ../api\"): \
             afterwards commands start there, relative paths resolve there, and the project's memory, \
             instructions and work plan are that directory's. Work laid out before moving stays filed \
             under the old directory, so move before you plan.",
            json!({
                "path": { "type": "string" },
                "create": { "type": "boolean", "description": "Create the directory if it does not exist." }
            }),
            &["path"],
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
             when you learn something that would otherwise have to be rediscovered. Global and project \
             entries are Markdown notes the user can open and edit, and they form a graph: write \
             [[Title of another entry]] in the body wherever one idea bears on another. Prefer several \
             small linked entries to one long one, and link to an entry that does not exist yet if it \
             should. Save as you go, not at the end. In particular, the moment an approach fails for a \
             reason that will still hold next time (the data does not exist, a tool is not installed, a \
             service needs a key), save it with kind `dead_end`: what you were after, what you tried, why \
             it failed. Next time that is found before anything is tried. Kind `lesson` is for an \
             approach that worked and is worth repeating.",
            json!({
                "title": { "type": "string", "description": "Short and specific: the name other entries will link to." },
                "body": { "type": "string", "description": "The fact itself, and for corrections the reason behind it. Markdown, with [[links]] to related entries." },
                "scope": { "type": "string", "enum": ["global", "project", "task"] },
                "kind": { "type": "string", "enum": ["user", "feedback", "project", "reference", "note", "lesson", "dead_end"] }
            }),
            &["title", "body", "scope"],
        ),
        spec(
            "memory_search",
            "Search memory (global, this project, and this task's shared notes) by keywords. Use it when \
             the question may depend on something learned earlier, and to read notes other agents on \
             this task have written. Results show what links to each entry.",
            json!({
                "query": { "type": "string" },
                "limit": { "type": "integer", "description": "Default 8." }
            }),
            &["query"],
        ),
        spec(
            "memory_read",
            "Read one memory entry by title, with the entries it links to and the entries that link to \
             it. This is how to walk the graph: start from a search hit or an entry in your context and \
             follow its links to related ideas.",
            json!({ "title": { "type": "string", "description": "The entry's title, or a [[link]] as written." } }),
            &["title"],
        ),
        spec(
            "memory_forget",
            "Delete a memory entry by title, when it is wrong or no longer true. Its note is removed.",
            json!({ "title": { "type": "string" } }),
            &["title"],
        ),
    ];
    v.push(spec(
        "work_plan",
        "Lay out work as items with dependencies. Use it when work has more than a couple of steps, \
         before starting, and again whenever the plan changes. Each item is an outcome (what will be \
         true when it is done), small enough to finish and check in one go. `blocked_by` names the \
         items that must be finished first; items with nothing in their way can be done in parallel. \
         Something only the user can settle is an item with `question: true`: it waits for them \
         while the rest proceeds, and blocks only what truly depends on the answer. Items are known \
         by title, so calling this again with the same titles updates them. The user sees the graph \
         in a side panel and as notes they can edit.",
        json!({
            "directory": { "type": "string", "description": "The directory this work is in or will be made in. \".\" when that is the current working directory. If the task names another directory, or the work creates a new project directory, give that path: the session moves there (the directory is made if it does not exist), so commands, this plan and the project's memory all sit with the work." },
            "root": { "type": "string", "description": "The outcome all the items add up to, in a few words (it becomes a note's name). Give it for anything substantial." },
            "items": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "title": { "type": "string", "description": "The outcome, in a few words. Other items refer to it by this." },
                        "description": { "type": "string", "description": "What it involves and anything needed to do it. May link to memory entries with [[Title]]." },
                        "done_when": { "type": "array", "items": { "type": "string" }, "description": "How to tell it is done: checks that can be run or seen." },
                        "blocked_by": { "type": "array", "items": { "type": "string" }, "description": "Titles of items that must be finished first." },
                        "parent": { "type": "string", "description": "Title of the item this is a part of, if not the root." },
                        "question": { "type": "boolean", "description": "True if only the user can settle it." }
                    },
                    "required": ["title"]
                }
            }
        }),
        &["directory", "items"],
    ));
    v.push(spec(
        "work_next",
        "See where the work stands: what is ready to do now, what is in progress and by whom, what \
         waits on the user, and what is blocked by what. Call it when you start, after context was \
         compacted, and whenever you are unsure what comes next; the answer is always current, \
         including changes the user made by hand. With `claim`, take a ready item: you get the item, \
         why it exists, and the results of the items it rests on, which is what you need to begin. \
         Take one item at a time and finish it before taking another.",
        json!({
            "claim": { "type": "string", "description": "Title of a ready item to take." },
            "show": { "type": "string", "description": "Title of any item, finished ones included, to read in full: what it was, its result and evidence." },
            "project": { "type": "string", "description": "With `show`: the directory of another project, to read work finished there." }
        }),
        &[],
    ));
    v.push(spec(
        "work_done",
        "Close an item you finished. `result` says what was done and where it is, written for someone \
         who will build on it without seeing how you did it. `evidence` is how you know: the command \
         you ran and what it showed, the file, the commit. Check before you close; an item is not done \
         because the code was written. Work you came across that is not part of this item goes in \
         `follow_ups` so it is not lost. Answer a question item the same way, with the answer as the result.",
        json!({
            "item": { "type": "string", "description": "The item's title." },
            "result": { "type": "string" },
            "evidence": { "type": "string" },
            "follow_ups": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "title": { "type": "string" },
                        "description": { "type": "string" },
                        "blocked_by": { "type": "array", "items": { "type": "string" } },
                        "question": { "type": "boolean" }
                    },
                    "required": ["title"]
                }
            },
            "learned": {
                "type": "array",
                "description": "What you would want to know before doing something like this again. One entry per finding: an approach that worked, or with dead_end true something you tried that failed for a reason that will still hold next time (no such data exists, a tool is missing, a service needs a key). Pass [] only if there is truly nothing.",
                "items": {
                    "type": "object",
                    "properties": {
                        "title": { "type": "string", "description": "Specific enough to be found by someone starting a similar task, e.g. `No public data about Eldermoor`." },
                        "body": { "type": "string", "description": "What you were after, what you did or tried, and why it worked or failed." },
                        "dead_end": { "type": "boolean" },
                        "applies": { "type": "string", "enum": ["anywhere", "this project"], "description": "Default anywhere." }
                    },
                    "required": ["title", "body"]
                }
            }
        }),
        &["item", "result", "learned"],
    ));
    v.push(spec(
        "work_update",
        "Adjust an item without closing it: add a `note` to its log (a finding, a decision, where you \
         got to), set `status` to `waiting` when it needs the user (say what you need in the note), to \
         `dropped` when it will not be done (say why), or to `open` to release or reopen it, or add \
         items it is `blocked_by`.",
        json!({
            "item": { "type": "string", "description": "The item's title." },
            "status": { "type": "string", "enum": ["open", "waiting", "dropped"] },
            "note": { "type": "string" },
            "blocked_by": { "type": "array", "items": { "type": "string" } }
        }),
        &["item"],
    ));
    if !cfg.obsidian.vault.trim().is_empty() {
        v.push(spec(
            "vault_search",
            "Search the user's Obsidian vault (their own notes) by words. Returns the best-matching notes \
             with the lines that matched. This is the first place to look for anything about the user \
             (who they are, their work, the people and projects in their life, their plans and decisions) \
             and for anything they may have written down: search here before the web and before asking them.",
            json!({
                "query": { "type": "string" },
                "limit": { "type": "integer", "description": "Default 10." }
            }),
            &["query"],
        ));
        v.push(spec(
            "vault_read",
            "Read a note from the user's Obsidian vault, with the notes it links to and the notes that \
             link to it. Name it by path relative to the vault, by note name alone, or as a [[link]]; \
             `Note#Heading` returns just that section.",
            json!({ "note": { "type": "string" } }),
            &["note"],
        ));
        v.push(spec(
            "vault_write",
            "Write a note in the user's Obsidian vault. `create` makes a new note and fails if one is \
             there; `append` adds to the end of a note (creating it if needed), which is right for logs \
             and running lists; `replace` rewrites an existing note, so read it first. Use [[links]] to \
             related notes. Do this when the user asks for something to be written down or kept.",
            json!({
                "note": { "type": "string", "description": "Path relative to the vault, e.g. `Projects/House/Survey notes`." },
                "content": { "type": "string", "description": "Markdown." },
                "mode": { "type": "string", "enum": ["create", "append", "replace"], "description": "Default create." }
            }),
            &["note", "content"],
        ));
    }
    if local::program(cfg).is_some() {
        v.push(spec(
            "local_models",
            "Ask llmfit, which knows this machine's hardware and a catalogue of open models, what can be run \
             here and how well: whether a model fits in memory, how many tokens a second to expect, how much \
             context it has, whether it can call tools. Use it for any question about local or open-source \
             models on this machine (which is best for some purpose, whether one will run, what one needs) \
             instead of answering from general knowledge, which does not know this machine. `recommend` with \
             purpose `agentic` gives the models able to drive an agent such as this one: they call tools, and \
             they are sized for the context an agent needs. `hardware` describes the machine; `plan` says how a \
             named model would run here, at a quantisation of your choosing, and what it needs; `info` looks a model up.",
            json!({
                "action": { "type": "string", "enum": ["recommend", "hardware", "plan", "info"] },
                "purpose": { "type": "string", "enum": ["agentic", "coding", "reasoning", "chat", "general", "multimodal", "embedding"], "description": "With recommend: what the model is for. Default agentic." },
                "limit": { "type": "integer", "description": "With recommend: how many to return. Default 5." },
                "context": { "type": "integer", "description": "The context, in tokens, to size memory for. Default 32768 for agentic work." },
                "model": { "type": "string", "description": "With plan and info: the model's name, as recommend gives it." },
                "quant": { "type": "string", "description": "With plan: the quantisation to plan for, such as Q4_K_M. Default: the one llmfit picks." }
            }),
            &["action"],
        ));
    }
    if session.depth < cfg.agents.max_depth {
        v.push(spec(
            "spawn_agents",
            "Run several subagents in parallel and wait for all of them. Each gets its own context and the \
             same tools, memory and working directory as you, and returns one final report. Use this \
             for independent pieces of work: several ready work items, research across separate areas, \
             changes to unrelated files. Each task description is all a subagent knows, so make it \
             self-contained: the goal, the relevant paths, and what to report back. Do not use it for \
             work that is quick to do yourself or whose parts depend on each other. Give a subagent a \
             ready work item with `work`: it takes the item, is handed what the item rests on, and \
             closes it with its result, and the user sees it under that item as it works.",
            json!({
                "agents": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "name": { "type": "string", "description": "Short label the user will see, e.g. `api-survey`." },
                            "task": { "type": "string", "description": "Complete, self-contained instructions." },
                            "work": {
                                "type": "string",
                                "description": "Title of the ready work item this agent takes. Omit if its task is not one."
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
            "goal_define",
            "Say where this goal ends, before working on it. `done_when` is what will be true when it is \
             done, as checks you can make (a command that passes, a page that exists, a number reached). If \
             the objective names no end to reach, set `open_ended` and leave `done_when` empty: do not make \
             an end up. The user is shown which it is, and an open-ended goal is paused after a while so \
             that going on is their choice. Call it again if what done means changes.",
            json!({
                "open_ended": { "type": "boolean", "description": "True when nothing in the objective says when to stop." },
                "done_when": { "type": "array", "items": { "type": "string" }, "description": "The checks that will hold when it is done. Empty when open-ended." }
            }),
            &["open_ended", "done_when"],
        ));
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
            json!({
                "summary": { "type": "string" },
                "lessons": {
                "type": "array",
                "description": "What you would want to know before doing something like this again. One entry per finding: an approach that worked, or with dead_end true something you tried that failed for a reason that will still hold next time (no such data exists, a tool is missing, a service needs a key). Pass [] only if there is truly nothing.",
                "items": {
                    "type": "object",
                    "properties": {
                        "title": { "type": "string", "description": "Specific enough to be found by someone starting a similar task, e.g. `No public data about Eldermoor`." },
                        "body": { "type": "string", "description": "What you were after, what you did or tried, and why it worked or failed." },
                        "dead_end": { "type": "boolean" },
                        "applies": { "type": "string", "enum": ["anywhere", "this project"], "description": "Default anywhere." }
                    },
                    "required": ["title", "body"]
                }
            }
            }),
            &["summary", "lessons"],
        ));
        v.push(spec(
            "goal_blocked",
            "Stop the goal because it cannot proceed without the user. State exactly what you need from \
             them. Do not use this for obstacles you can work around.",
            json!({
                "reason": { "type": "string" },
                "dead_ends": {
                    "type": "array",
                    "description": "What you tried that failed for a reason that will still hold next time (the data does not exist, a source is gone, a key is missing), so that nobody tries it again. Empty if there was none.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "title": { "type": "string", "description": "Specific enough to be found by someone starting a similar task, e.g. `No public data about Eldermoor`." },
                            "body": { "type": "string", "description": "What you were after, what you tried, and why it failed." }
                        },
                        "required": ["title", "body"]
                    }
                }
            }),
            &["reason", "dead_ends"],
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
        "write_file" | "edit_file" | "vault_write" => true,
        _ => false,
    }
}

/// One line describing a call, for the transcript.
pub fn summarize_call(name: &str, input: &Value) -> String {
    let s = |k: &str| input.get(k).and_then(Value::as_str).unwrap_or("");
    match name {
        "bash" => first_line(s("command"), 200),
        "read_file" | "write_file" | "edit_file" | "list_dir" | "change_directory" => s("path").to_string(),
        "glob" => s("pattern").to_string(),
        "grep" => {
            let path = s("path");
            if path.is_empty() { s("pattern").to_string() } else { format!("{} in {}", s("pattern"), path) }
        }
        "web_fetch" => s("url").to_string(),
        "memory_save" => format!("[{}] {}", s("scope"), s("title")),
        "memory_search" => s("query").to_string(),
        "memory_read" | "memory_forget" => s("title").to_string(),
        "vault_search" => s("query").to_string(),
        "vault_read" => s("note").to_string(),
        "vault_write" => format!("{} ({})", s("note"), if s("mode").is_empty() { "create" } else { s("mode") }),
        "spawn_agents" => input
            .get("agents")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(|x| x.get("name").and_then(Value::as_str)).collect::<Vec<_>>().join(", "))
            .unwrap_or_default(),
        "work_plan" => {
            let n = input.get("items").and_then(Value::as_array).map(|a| a.len()).unwrap_or(0);
            let root = s("root");
            format!("{n} item{}{}", if n == 1 { "" } else { "s" }, if root.is_empty() { String::new() } else { format!(" under {root}") })
        }
        "work_next" => match (s("claim"), s("show")) {
            ("", "") => "what is ready?".to_string(),
            ("", show) => format!("show: {show}"),
            (claim, _) => format!("take: {claim}"),
        },
        "work_done" => format!("{}: {}", s("item"), first_line(s("result"), 120)),
        "work_update" => format!("{}{}", s("item"), if s("status").is_empty() { String::new() } else { format!(" -> {}", s("status")) }),
        "goal_define" => match input["open_ended"] == true {
            true => "open-ended".to_string(),
            false => format!("done when: {}", first_line(&input["done_when"].as_array().map(|a| a.iter().filter_map(|v| v.as_str()).collect::<Vec<_>>().join("; ")).unwrap_or_default(), 180)),
        },
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
        "vault_write" => {
            let content = s("content");
            format!(
                "{} in the Obsidian vault, mode {}\n({} lines)\n{}",
                s("note"),
                if s("mode").is_empty() { "create" } else { s("mode") },
                content.lines().count(),
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
        "change_directory" => misc::change_directory(ctx),
        "glob" => fs::glob(ctx).await,
        "grep" => fs::grep(ctx).await,
        "web_fetch" => misc::web_fetch(ctx).await,
        "local_models" => local::local_models(ctx).await,
        "memory_save" => misc::memory_save(ctx),
        "memory_search" => misc::memory_search(ctx),
        "memory_forget" => misc::memory_forget(ctx),
        "memory_read" => misc::memory_read(ctx),
        "vault_search" => vault::vault_search(ctx).await,
        "vault_read" => vault::vault_read(ctx).await,
        "vault_write" => vault::vault_write(ctx).await,
        "spawn_agents" => agents::spawn_agents(ctx).await,
        "work_plan" => work::work_plan(ctx),
        "work_next" => work::work_next(ctx),
        "work_done" => work::work_done(ctx),
        "work_update" => work::work_update(ctx),
        "goal_define" | "goal_update" | "goal_complete" | "goal_blocked" => misc::goal_tool(ctx),
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
