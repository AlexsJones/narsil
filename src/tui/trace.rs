//! The /trace window: what a session did, as steps to move through, with the
//! whole of the step under the cursor beside them.
//!
//! A step is one request to the model and what came of it: nearly always a
//! tool call, at the end an answer. The list gives a step one line (what was
//! done, how long the model took to ask for it, how long it took to do), so
//! that a long run can be read down. Requests and subagents are headings that
//! fold. Everything that will not fit on a line (what the tool was called
//! with, what came back, what the model said, the tokens) is in the detail
//! window for the step the cursor is on.
//!
//! Nothing here draws: these are the rows and the lines, built from the
//! recorded spans and the transcript, for `tui/mod.rs` to put on screen.

use std::collections::{HashMap, HashSet};

use serde_json::Value;

use crate::store::{SpanRow, Store};
use crate::telemetry::{Notes, fmt_ms};
use crate::types::Block;
use crate::util::{first_line, fmt_tokens};

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Kind {
    /// A run of the session: what was asked, and the steps taken for it.
    Run,
    /// A subagent's run, under the step that started it.
    Agent,
    /// A request to the model, with the first tool call it led to.
    Step,
    /// A further tool call of the same step.
    Tool,
    /// A wait: for approval, or to try a request again.
    Wait,
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum State {
    Done,
    Failed,
    Running,
}

#[derive(Debug, Clone)]
pub struct Row {
    /// The span the row stands for: the run, the model call of a step, the tool call, the wait.
    pub id: i64,
    pub depth: usize,
    pub kind: Kind,
    pub state: State,
    /// A step's place among the steps of its run (0: not a step).
    pub number: usize,
    pub title: String,
    /// How long the model took to answer, and how long what it asked for took. For a run, the
    /// whole of it is in `tool_ms`.
    pub model_ms: Option<i64>,
    pub tool_ms: Option<i64>,
    pub foldable: bool,
    pub folded: bool,
    /// The tool call a step led to.
    pub tool: Option<i64>,
    pub session: String,
    /// The stored reply of the model, to go to in the conversation (0: none).
    pub message: i64,
}

fn took(s: &SpanRow, now: i64) -> i64 {
    s.dur_ms.unwrap_or(now - s.start_ms).max(0)
}

/// The rows of the list. `folded` holds the runs that are shut; with
/// `failed_only`, only what failed is given, under the runs it failed in.
pub fn rows(spans: &[SpanRow], notes: &Notes, folded: &HashSet<i64>, failed_only: bool, now: i64) -> Vec<Row> {
    let known: HashSet<i64> = spans.iter().map(|s| s.id).collect();
    // A tool call hangs under the model call that asked for it, not beside it.
    let asked_by: HashMap<(&str, i64), i64> =
        spans.iter().filter(|s| s.kind == "model_call" && s.num("message_id") > 0).map(|s| ((s.session.as_str(), s.num("message_id")), s.id)).collect();
    let mut children: HashMap<Option<i64>, Vec<&SpanRow>> = HashMap::new();
    for s in spans {
        let asked =
            matches!(s.kind.as_str(), "tool_call" | "approval_wait").then(|| asked_by.get(&(s.session.as_str(), s.num("asked_in"))).copied()).flatten();
        children.entry(asked.or(s.parent.filter(|p| known.contains(p)))).or_default().push(s);
    }
    let build = Build { children, notes, folded, shut: !failed_only, now };
    let mut out = Vec::new();
    for top in build.children.get(&None).cloned().unwrap_or_default() {
        build.any(top, 0, &mut out);
    }
    if failed_only {
        // What failed, and the runs it failed in.
        let keep: Vec<bool> = (0..out.len())
            .map(|i| match out[i].kind {
                Kind::Run | Kind::Agent => out[i + 1..].iter().take_while(|r| r.depth > out[i].depth).any(|r| r.state == State::Failed) || out[i].state == State::Failed,
                _ => out[i].state == State::Failed,
            })
            .collect();
        let mut kept = keep.iter();
        out.retain(|_| *kept.next().unwrap_or(&false));
    }
    out
}

struct Build<'a> {
    children: HashMap<Option<i64>, Vec<&'a SpanRow>>,
    notes: &'a Notes,
    folded: &'a HashSet<i64>,
    /// Whether a folded run keeps its rows to itself.
    shut: bool,
    now: i64,
}

impl<'a> Build<'a> {
    fn under(&self, id: i64) -> &[&'a SpanRow] {
        self.children.get(&Some(id)).map(Vec::as_slice).unwrap_or(&[])
    }

    fn state(&self, s: &SpanRow) -> State {
        match (s.dur_ms.is_none(), s.ok) {
            (true, _) => State::Running,
            (_, false) => State::Failed,
            _ => State::Done,
        }
    }

    fn row(&self, s: &SpanRow, depth: usize, kind: Kind, title: String) -> Row {
        Row {
            id: s.id,
            depth,
            kind,
            state: self.state(s),
            number: 0,
            title,
            model_ms: None,
            tool_ms: None,
            foldable: false,
            folded: false,
            tool: None,
            session: s.session.clone(),
            message: 0,
        }
    }

    fn any(&self, s: &'a SpanRow, depth: usize, out: &mut Vec<Row>) {
        match s.kind.as_str() {
            "session_run" => self.run(s, depth, None, out),
            // A goal's turn is the run it holds, under the turn's name.
            "goal_turn" => match self.under(s.id) {
                [only] if only.kind == "session_run" => self.run(only, depth, Some(format!("goal turn {}", s.num("turn"))), out),
                inside => {
                    out.push(self.row(s, depth, Kind::Other, format!("goal turn {}", s.num("turn"))));
                    inside.iter().for_each(|c| self.any(c, depth + 1, out));
                }
            },
            "model_call" => self.step(s, depth, 0, out),
            "tool_call" => self.tool(s, depth, out),
            _ => out.push(self.plain(s, depth)),
        }
    }

    fn plain(&self, s: &SpanRow, depth: usize) -> Row {
        let (kind, title) = match s.kind.as_str() {
            "approval_wait" => (Kind::Wait, format!("waited for your word on {}: {}", s.what, s.text("decision"))),
            "retry_wait" => (Kind::Wait, format!("waited to try again: {}", first_line(&s.text("reason"), 160))),
            "compaction" => (Kind::Other, format!("compacted {} messages to {} tokens", s.num("messages"), fmt_tokens(s.num("summary_tokens")))),
            _ => (Kind::Other, if s.what.is_empty() { s.kind.clone() } else { format!("{} {}", s.kind, s.what) }),
        };
        let mut row = self.row(s, depth, kind, title);
        row.tool_ms = Some(took(s, self.now));
        row
    }

    fn run(&self, s: &'a SpanRow, depth: usize, named: Option<String>, out: &mut Vec<Row>) {
        let inside = self.under(s.id);
        // What it was asked: the last thing said to it before its first reply.
        let first_reply = inside.iter().find(|c| c.kind == "model_call" && c.num("message_id") > 0).map(|c| c.num("message_id"));
        let asked = first_reply
            .and_then(|mid| self.notes.prompts.get(&s.session).and_then(|p| p.iter().rev().find(|(id, _)| *id < mid)))
            .map(|(_, text)| text.clone())
            .unwrap_or_default();
        let agent = !s.agent.is_empty();
        let mut title = match (named, agent) {
            (Some(name), _) => name,
            (None, true) => s.agent.clone(),
            (None, false) if asked.is_empty() => format!("session {}", s.session),
            (None, false) => asked.clone(),
        };
        match (s.text("outcome").as_str(), s.text("error")) {
            ("" | "completed", _) => {}
            (other, error) if error.is_empty() => title.push_str(&format!("  ({other})")),
            (other, error) => title.push_str(&format!("  ({other}: {})", first_line(&error, 120))),
        }
        let mut row = self.row(s, depth, if agent { Kind::Agent } else { Kind::Run }, title);
        row.tool_ms = Some(took(s, self.now));
        row.foldable = !inside.is_empty();
        row.folded = row.foldable && self.folded.contains(&s.id);
        let shut = row.folded && self.shut;
        out.push(row);
        if shut {
            return;
        }
        let mut number = 0;
        for c in inside {
            if c.kind == "model_call" {
                number += 1;
                self.step(c, depth + 1, number, out);
            } else {
                self.any(c, depth + 1, out);
            }
        }
    }

    /// A request to the model: one row with the first thing it asked for, then whatever else came of it.
    fn step(&self, s: &'a SpanRow, depth: usize, number: usize, out: &mut Vec<Row>) {
        let inside = self.under(s.id);
        let tools: Vec<&&SpanRow> = inside.iter().filter(|c| c.kind == "tool_call").collect();
        let (said, thinking) = self.notes.said.get(&s.num("message_id")).cloned().unwrap_or_default();
        let title = match tools.first() {
            Some(tool) => format!("{} {}", tool.what, tool.text("summary")),
            None if !s.ok => format!("the model did not answer: {}", first_line(&s.text("error"), 160)),
            None if !said.is_empty() && said != "(no reply)" => format!("\u{201c}{said}\u{201d}"),
            None if s.text("purpose") == "compaction" => "writes the handoff summary".to_string(),
            None if !thinking.is_empty() => format!("thinks: {thinking}"),
            None if s.dur_ms.is_none() => "waiting on the model".to_string(),
            None => "answers".to_string(),
        };
        let mut row = self.row(s, depth, Kind::Step, title.trim_end().to_string());
        row.number = number;
        row.model_ms = Some(took(s, self.now));
        row.message = s.num("message_id");
        if let Some(tool) = tools.first() {
            row.tool = Some(tool.id);
            row.tool_ms = Some(took(tool, self.now));
            if self.failed(tool) {
                row.state = State::Failed;
            } else if tool.dur_ms.is_none() {
                row.state = State::Running;
            }
        }
        out.push(row);
        for c in inside {
            match c.kind.as_str() {
                "tool_call" if Some(c.id) == tools.first().map(|t| t.id) => self.started(c, depth + 1, out),
                "tool_call" => self.tool(c, depth + 1, out),
                _ => out.push(self.plain(c, depth + 1)),
            }
        }
    }

    fn failed(&self, tool: &SpanRow) -> bool {
        !tool.ok || self.notes.results.get(&tool.text("call")).is_some_and(|(failed, _)| *failed)
    }

    /// A tool call that is a row of its own, and the subagents it started.
    fn tool(&self, s: &'a SpanRow, depth: usize, out: &mut Vec<Row>) {
        let mut row = self.row(s, depth, Kind::Tool, format!("{} {}", s.what, s.text("summary")).trim_end().to_string());
        row.tool = Some(s.id);
        row.tool_ms = Some(took(s, self.now));
        if self.failed(s) {
            row.state = State::Failed;
        }
        out.push(row);
        self.started(s, depth + 1, out);
    }

    /// The subagents a tool call started (and anything else recorded inside it).
    fn started(&self, tool: &'a SpanRow, depth: usize, out: &mut Vec<Row>) {
        self.under(tool.id).iter().for_each(|c| self.any(c, depth, out));
    }
}

/// How a line of the detail window is to be shown.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Tone {
    Head,
    Label,
    Text,
    Dim,
    Bad,
}

/// Lines of text broken to a width, each set in by `indent`.
fn laid(text: &str, width: usize, indent: &str, most: usize) -> Vec<String> {
    let room = width.saturating_sub(indent.chars().count()).max(16);
    let mut out: Vec<String> = Vec::new();
    let mut lines = text.lines();
    for line in lines.by_ref() {
        // A tab is as wide as the terminal makes it; here it is two spaces.
        let line = line.replace('\t', "  ");
        if line.is_empty() {
            out.push(String::new());
        } else {
            out.extend(super::render::wrap(&line, room).into_iter().map(|l| format!("{indent}{l}")));
        }
        if out.len() >= most {
            break;
        }
    }
    let more = lines.count();
    if out.len() > most {
        out.truncate(most);
    }
    if more > 0 {
        out.push(format!("{indent}\u{2026} {more} more line{}", if more == 1 { "" } else { "s" }));
    }
    out
}

/// Everything about the step (or run) a row stands for, as lines `width` wide.
pub fn detail(row: &Row, spans: &[SpanRow], store: &Store, now: i64, width: usize) -> Vec<(Tone, String)> {
    let find = |id: i64| spans.iter().find(|s| s.id == id);
    let Some(s) = find(row.id) else { return Vec::new() };
    let clock = |ms: i64| chrono::DateTime::from_timestamp_millis(ms).map(|t| t.with_timezone(&chrono::Local).format("%H:%M:%S").to_string()).unwrap_or_default();
    let mut out: Vec<(Tone, String)> = Vec::new();
    let mut head = |text: String| out.push((Tone::Head, text));
    let kept = |text: &str| crate::secrets::kept(text);

    match row.kind {
        Kind::Run | Kind::Agent => {
            let under: Vec<&SpanRow> = spans.iter().filter(|c| c.session == s.session && c.start_ms >= s.start_ms && c.start_ms <= s.start_ms + took(s, now)).collect();
            let calls: Vec<&&SpanRow> = under.iter().filter(|c| c.kind == "model_call").collect();
            let tools = under.iter().filter(|c| c.kind == "tool_call").count();
            head(if row.kind == Kind::Agent { format!("subagent {}", s.agent) } else { "request".to_string() });
            out.push((Tone::Dim, format!("{} \u{00b7} session {} \u{00b7} took {}", clock(s.start_ms), s.session, fmt_ms(took(s, now)))));
            out.push((
                Tone::Text,
                format!(
                    "{} request{} to the model, {} tool call{} \u{00b7} {} in, {} out",
                    calls.len(),
                    if calls.len() == 1 { "" } else { "s" },
                    tools,
                    if tools == 1 { "" } else { "s" },
                    fmt_tokens(calls.iter().map(|c| c.num("input_tokens")).sum()),
                    fmt_tokens(calls.iter().map(|c| c.num("output_tokens")).sum())
                ),
            ));
            match (s.text("outcome"), s.text("error")) {
                (outcome, error) if !error.is_empty() => out.push((Tone::Bad, format!("{outcome}: {error}"))),
                (outcome, _) if !outcome.is_empty() => out.push((Tone::Dim, format!("ended: {outcome}"))),
                _ => {}
            }
            // What it was asked, in full.
            let first_reply = calls.iter().map(|c| c.num("message_id")).filter(|id| *id > 0).min().unwrap_or(i64::MAX);
            let asked = store
                .messages_from(&s.session, 0)
                .unwrap_or_default()
                .into_iter()
                .filter(|m| m.id < first_reply && m.role == crate::types::Role::User && m.kind == crate::types::MsgKind::Message)
                .filter(|m| m.blocks.iter().any(|b| matches!(b, Block::Text { .. })))
                .next_back();
            if let Some(m) = asked {
                out.push((Tone::Text, String::new()));
                out.push((Tone::Label, "ASKED".to_string()));
                out.extend(laid(&kept(&crate::types::blocks_text(&m.blocks)), width, "  ", 40).into_iter().map(|l| (Tone::Text, l)));
            }
        }
        Kind::Step => {
            head(if row.number > 0 { format!("step {}", row.number) } else { "a request to the model".to_string() });
            out.push((Tone::Dim, format!("{} \u{00b7} session {}", clock(s.start_ms), s.session)));
            out.push((Tone::Text, String::new()));
            model_lines(s, store, now, width, &mut out);
            if let Some(tool) = row.tool.and_then(find) {
                out.push((Tone::Text, String::new()));
                tool_lines(tool, store, now, width, &mut out);
            }
        }
        Kind::Tool => {
            head(format!("{} (asked for in the same request as the step above)", s.what));
            out.push((Tone::Dim, format!("{} \u{00b7} session {}", clock(s.start_ms), s.session)));
            out.push((Tone::Text, String::new()));
            tool_lines(s, store, now, width, &mut out);
        }
        Kind::Wait | Kind::Other => {
            head(row.title.clone());
            out.push((Tone::Dim, format!("{} \u{00b7} session {} \u{00b7} {}", clock(s.start_ms), s.session, fmt_ms(took(s, now)))));
            if let Some(fields) = s.data.as_object() {
                for (key, value) in fields.iter().filter(|(_, v)| !v.is_null()) {
                    let value = value.as_str().map(str::to_string).unwrap_or_else(|| value.to_string());
                    out.extend(laid(&kept(&format!("{key}: {value}")), width, "  ", 6).into_iter().map(|l| (Tone::Text, l)));
                }
            }
        }
    }
    out
}

/// The request to the model: which model, how long, the tokens, and what it said.
fn model_lines(s: &SpanRow, store: &Store, now: i64, width: usize, out: &mut Vec<(Tone, String)>) {
    out.push((Tone::Label, "THE MODEL".to_string()));
    let mut facts = vec![format!("{}/{}", s.text("provider"), s.what)];
    facts.push(if s.dur_ms.is_some() { format!("answered in {}", fmt_ms(took(s, now))) } else { format!("asked {} ago", fmt_ms(took(s, now))) });
    if s.num("ttft_ms") > 0 {
        facts.push(format!("first word after {}", fmt_ms(s.num("ttft_ms"))));
    }
    if s.num("attempt") > 1 {
        facts.push(format!("attempt {}", s.num("attempt")));
    }
    out.push((Tone::Text, format!("  {}", facts.join(" \u{00b7} "))));
    if s.num("input_tokens") + s.num("output_tokens") > 0 {
        let cached = match (s.num("cache_read"), s.num("cache_write")) {
            (0, 0) => String::new(),
            (read, 0) => format!(" ({} read from cache)", fmt_tokens(read)),
            (0, wrote) => format!(" ({} written to cache)", fmt_tokens(wrote)),
            (read, wrote) => format!(" ({} read from cache, {} written to it)", fmt_tokens(read), fmt_tokens(wrote)),
        };
        out.push((Tone::Text, format!("  {} in{cached} \u{00b7} {} out", fmt_tokens(s.num("input_tokens")), fmt_tokens(s.num("output_tokens")))));
    }
    if !s.ok || !s.text("error").is_empty() {
        out.extend(laid(&s.text("error"), width, "  ", 8).into_iter().map(|l| (Tone::Bad, l)));
    }
    // What it said, and what it thought, in its own words.
    let reply = store.message_blocks(&[s.num("message_id")]).unwrap_or_default().into_iter().next();
    let blocks: Vec<Block> = reply.and_then(|(_, blocks)| serde_json::from_str(&blocks).ok()).unwrap_or_default();
    for b in &blocks {
        let (label, text) = match b {
            Block::Thinking { text, .. } if !text.trim().is_empty() => ("  thought:", text),
            Block::Text { text } if !text.trim().is_empty() && text != "(no reply)" => ("  said:", text),
            _ => continue,
        };
        out.push((Tone::Dim, label.to_string()));
        out.extend(laid(&crate::secrets::kept(text.trim()), width, "    ", 24).into_iter().map(|l| (Tone::Text, l)));
    }
}

/// The tool call: how long, what it was called with, and what came back.
fn tool_lines(tool: &SpanRow, store: &Store, now: i64, width: usize, out: &mut Vec<(Tone, String)>) {
    let call = store.tool_call(&tool.text("call")).ok().flatten();
    let failed = !tool.ok || call.as_ref().is_some_and(|c| c.3);
    let how = match (tool.dur_ms.is_some(), failed) {
        (false, _) => format!("running for {}", fmt_ms(took(tool, now))),
        (true, true) => format!("failed after {}", fmt_ms(took(tool, now))),
        (true, false) => format!("took {}", fmt_ms(took(tool, now))),
    };
    out.push((Tone::Label, format!("THE TOOL  {} \u{00b7} {how}", tool.what)));
    let Some((_, input, result, _)) = call else {
        out.push((Tone::Text, format!("  {}", tool.text("summary"))));
        return;
    };
    match &input {
        Value::Object(fields) if !fields.is_empty() => {
            let key_w = fields.keys().map(|k| k.chars().count()).max().unwrap_or(0).min(14);
            for (key, value) in fields {
                let value = value.as_str().map(str::to_string).unwrap_or_else(|| serde_json::to_string_pretty(value).unwrap_or_default());
                let indent = " ".repeat(key_w + 4);
                let mut lines = laid(&crate::secrets::kept(&value), width, &indent, 14).into_iter();
                // The first line carries the name of what it is.
                let first = lines.next().unwrap_or_default();
                out.push((Tone::Text, format!("  {key:<key_w$}  {}", first.trim_start())));
                out.extend(lines.map(|l| (Tone::Text, l)));
            }
        }
        _ => {}
    }
    out.push((Tone::Text, String::new()));
    out.push((Tone::Label, "CAME BACK".to_string()));
    match result {
        Some(text) if !text.trim().is_empty() => {
            let tone = if failed { Tone::Bad } else { Tone::Text };
            out.extend(laid(&crate::secrets::kept(text.trim_end()), width, "  ", 60).into_iter().map(|l| (tone, l)));
        }
        Some(_) => out.push((Tone::Dim, "  (nothing)".to_string())),
        None => out.push((Tone::Dim, "  (not yet)".to_string())),
    }
}

/// Where the time went: waiting on the model, in tools, waiting on the
/// person, waiting to retry. Subagents work at once, so these can add up to
/// more than the clock shows; a tool that only waits on subagents is left out.
pub fn spent(spans: &[SpanRow], now: i64) -> Vec<(&'static str, i64)> {
    let sum = |kind: &str| spans.iter().filter(|s| s.kind == kind && !(kind == "tool_call" && s.what == "spawn_agents")).map(|s| took(s, now)).sum::<i64>();
    vec![("the model", sum("model_call")), ("tools", sum("tool_call")), ("waiting on you", sum("approval_wait")), ("waiting to retry", sum("retry_wait"))]
}

/// The whole in a line, for the window's title: how long, how many steps, how many tokens.
pub fn headline(spans: &[SpanRow], now: i64) -> String {
    let calls: Vec<&SpanRow> = spans.iter().filter(|s| s.kind == "model_call").collect();
    let root = spans.first().map(|s| s.root.clone()).unwrap_or_default();
    let clock: i64 = spans.iter().filter(|s| s.kind == "session_run" && s.session == root).map(|s| took(s, now)).sum();
    let mut agents: Vec<&str> = spans.iter().filter(|s| s.session != root && !s.session.is_empty()).map(|s| s.session.as_str()).collect();
    agents.sort_unstable();
    agents.dedup();
    let mut parts = vec![format!("session {root}"), fmt_ms(clock), format!("{} step{}", calls.len(), if calls.len() == 1 { "" } else { "s" })];
    if !agents.is_empty() {
        parts.push(format!("{} subagent{}", agents.len(), if agents.len() == 1 { "" } else { "s" }));
    }
    parts.push(format!(
        "{} in, {} out",
        fmt_tokens(calls.iter().map(|c| c.num("input_tokens")).sum()),
        fmt_tokens(calls.iter().map(|c| c.num("output_tokens")).sum())
    ));
    parts.join(" \u{00b7} ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn span(id: i64, parent: Option<i64>, session: &str, kind: &str, what: &str, start: i64, dur: Option<i64>, data: Value) -> SpanRow {
        SpanRow {
            id,
            parent,
            root: "root".into(),
            session: session.into(),
            agent: if session == "root" { String::new() } else { "tests".into() },
            goal: String::new(),
            kind: kind.into(),
            what: what.into(),
            start_ms: start,
            dur_ms: dur,
            ok: data.get("failed").is_none(),
            data,
        }
    }

    /// A request that read a file, started a subagent which ran a failing command, and answered.
    fn session() -> (Vec<SpanRow>, Notes) {
        let spans = vec![
            span(1, None, "root", "session_run", "chat", 0, Some(9_000), json!({ "outcome": "completed" })),
            span(2, Some(1), "root", "model_call", "big", 0, Some(800), json!({ "message_id": 11, "provider": "p", "input_tokens": 5000, "cache_read": 4000, "output_tokens": 40 })),
            span(3, Some(1), "root", "tool_call", "read_file", 800, Some(2), json!({ "asked_in": 11, "call": "c1", "summary": "notes.txt" })),
            span(4, Some(1), "root", "model_call", "big", 900, Some(700), json!({ "message_id": 13, "provider": "p", "input_tokens": 5100, "output_tokens": 90 })),
            span(5, Some(1), "root", "tool_call", "spawn_agents", 1_600, Some(6_000), json!({ "asked_in": 13, "call": "c2", "summary": "tests" })),
            span(6, Some(5), "sub", "session_run", "subagent", 1_700, Some(5_800), json!({ "outcome": "completed" })),
            span(7, Some(6), "sub", "model_call", "small", 1_700, Some(300), json!({ "message_id": 21, "provider": "p", "input_tokens": 3000, "output_tokens": 20 })),
            span(8, Some(6), "sub", "approval_wait", "bash", 2_000, Some(4_000), json!({ "asked_in": 21, "decision": "allow" })),
            span(9, Some(6), "sub", "tool_call", "bash", 6_000, Some(400), json!({ "asked_in": 21, "call": "c3", "summary": "make test" })),
            span(10, Some(6), "sub", "model_call", "small", 6_500, Some(900), json!({ "message_id": 23, "provider": "p", "input_tokens": 3200, "output_tokens": 30 })),
            span(11, Some(1), "root", "model_call", "big", 7_700, Some(1_200), json!({ "message_id": 15, "provider": "p", "input_tokens": 5400, "output_tokens": 60 })),
        ];
        let mut notes = Notes::default();
        notes.prompts.insert("root".into(), vec![(10, "check the notes and run the tests".into())]);
        notes.said.insert(15, ("The notes are fine; one test fails.".into(), String::new()));
        notes.said.insert(23, ("make test failed in parser_test.".into(), String::new()));
        notes.results.insert("c3".into(), (true, "1 test failed".into()));
        (spans, notes)
    }

    #[test]
    fn a_run_reads_as_a_request_and_its_numbered_steps_with_subagents_beneath() {
        let (spans, notes) = session();
        let all = rows(&spans, &notes, &HashSet::new(), false, 10_000);
        let shown: Vec<String> = all.iter().map(|r| format!("{}{:?} {} {}", "  ".repeat(r.depth), r.kind, r.number, r.title)).collect();
        assert_eq!(
            shown,
            vec![
                "Run 0 check the notes and run the tests",
                "  Step 1 read_file notes.txt",
                "  Step 2 spawn_agents tests",
                "    Agent 0 tests",
                "      Step 1 bash make test",
                "        Wait 0 waited for your word on bash: allow",
                "      Step 2 \u{201c}make test failed in parser_test.\u{201d}",
                "  Step 3 \u{201c}The notes are fine; one test fails.\u{201d}",
            ],
            "{shown:#?}"
        );
        // A step has the model's time and the tool's apart; a run has its whole.
        assert_eq!((all[1].model_ms, all[1].tool_ms), (Some(800), Some(2)));
        assert_eq!((all[7].model_ms, all[7].tool_ms), (Some(1_200), None));
        assert_eq!((all[0].tool_ms, all[3].tool_ms), (Some(9_000), Some(5_800)));
        // The command that came back as failed marks its step; the reply it belongs to is there to go to.
        assert_eq!(all[4].state, State::Failed);
        assert_eq!((all[4].message, all[4].tool), (21, Some(9)));
        assert!(all[0].foldable && all[3].foldable && !all[1].foldable);

        // Shut, a subagent keeps its steps to itself; the request can be shut too.
        let shut = rows(&spans, &notes, &HashSet::from([6]), false, 10_000);
        assert_eq!(shut.iter().map(|r| r.id).collect::<Vec<_>>(), vec![1, 2, 4, 6, 11]);
        assert!(shut[3].folded);
        assert_eq!(rows(&spans, &notes, &HashSet::from([1]), false, 10_000).len(), 1);

        // Only what failed, under the runs it failed in, whatever is shut.
        let failed = rows(&spans, &notes, &HashSet::from([6]), true, 10_000);
        assert_eq!(failed.iter().map(|r| r.id).collect::<Vec<_>>(), vec![1, 6, 7]);
    }

    #[test]
    fn a_step_still_in_hand_is_running_and_times_are_counted_to_now() {
        let (mut spans, notes) = session();
        spans.truncate(4);
        spans[0].dur_ms = None;
        spans[3].dur_ms = None;
        let all = rows(&spans, &notes, &HashSet::new(), false, 2_500);
        assert_eq!(all.iter().map(|r| r.state).collect::<Vec<_>>(), vec![State::Running, State::Done, State::Running]);
        assert_eq!(all[2].title, "waiting on the model");
        assert_eq!((all[0].tool_ms, all[2].model_ms), (Some(2_500), Some(1_600)));
    }

    #[test]
    fn the_time_is_shared_out_and_the_whole_is_said_in_a_line() {
        let (spans, _) = session();
        // The tool that only waited on the subagent is not counted as work.
        assert_eq!(spent(&spans, 10_000), vec![("the model", 3_900), ("tools", 402), ("waiting on you", 4_000), ("waiting to retry", 0)]);
        assert_eq!(headline(&spans, 10_000), "session root \u{00b7} 9.00s \u{00b7} 5 steps \u{00b7} 1 subagent \u{00b7} 21.7k in, 240 out");
    }

    #[test]
    fn long_text_is_laid_out_to_the_width_and_cut_with_a_count() {
        let text = (1..=30).map(|i| format!("line {i} of the file")).collect::<Vec<_>>().join("\n");
        let lines = laid(&text, 40, "  ", 5);
        assert_eq!(lines.len(), 6);
        assert_eq!((lines[0].as_str(), lines[5].as_str()), ("  line 1 of the file", "  \u{2026} 25 more lines"));
        assert!(laid(&"word ".repeat(40), 30, "    ", 10).iter().all(|l| l.chars().count() <= 30 && l.starts_with("    ")));
    }
}
