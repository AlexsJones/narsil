//! What narsil reports of token use: the meter in the status bar, the receipt
//! under each request, and the /usage report. All of it is added up from one
//! place, the record of answered model calls (`model_usage`), in which each
//! row is what the backend said that call used. Nothing here counts again.
//!
//! Input is kept in the three parts a bill is made of: read from a cache,
//! written to one, and neither. "in" is always the three together, which is
//! the size of what was sent.

use std::collections::HashMap;

use anyhow::Result;
use serde_json::{Value, json};

use crate::config::Config;
use crate::store::{ModelCall, Session, Store};
use crate::telemetry::fmt_ms;
use crate::types::Block;
use crate::util::{estimate_tokens, first_line, fmt_tokens};

/// The tokens of some model calls, added up.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Tokens {
    pub calls: i64,
    /// Input that was neither read from a cache nor written to one.
    pub fresh: i64,
    pub cache_read: i64,
    pub cache_write: i64,
    pub output: i64,
    /// Some of it is narsil's own estimate: a backend reported nothing.
    pub estimated: bool,
    /// Time spent waiting on the model, added up over the calls.
    pub model_ms: i64,
    /// When the first call was made, and when the last one ended (unix ms).
    pub began_ms: i64,
    pub ended_ms: i64,
}

impl Tokens {
    pub fn add(&mut self, c: &ModelCall) {
        self.calls += 1;
        self.fresh += c.input;
        self.cache_read += c.cache_read;
        self.cache_write += c.cache_write;
        self.output += c.output;
        self.estimated |= c.estimated;
        self.model_ms += c.dur_ms;
        if self.began_ms == 0 || c.started_ms < self.began_ms {
            self.began_ms = c.started_ms;
        }
        self.ended_ms = self.ended_ms.max(c.started_ms + c.dur_ms);
    }

    pub fn of<'a>(calls: impl IntoIterator<Item = &'a ModelCall>) -> Tokens {
        let mut t = Tokens::default();
        calls.into_iter().for_each(|c| t.add(c));
        t
    }

    /// Everything that was sent: read from a cache, written to one, or neither.
    pub fn input(&self) -> i64 {
        self.fresh + self.cache_read + self.cache_write
    }

    /// How much of the input was read from a cache, to the nearest percent.
    pub fn cached_pct(&self) -> i64 {
        match self.input() {
            0 => 0,
            input => (self.cache_read * 100 + input / 2) / input,
        }
    }

    /// From the first call being made to the last one ending: the model, and the tools between.
    pub fn took_ms(&self) -> i64 {
        (self.ended_ms - self.began_ms).max(0)
    }

    /// A count as it is shown: marked where it is an estimate.
    fn n(&self, tokens: i64) -> String {
        format!("{}{}", if self.estimated { "~" } else { "" }, fmt_tokens(tokens))
    }

    fn json(&self) -> Value {
        json!({
            "calls": self.calls,
            "input": self.input(),
            "input_new": self.fresh,
            "cache_read": self.cache_read,
            "cache_write": self.cache_write,
            "output": self.output,
            "cached_pct": self.cached_pct(),
            "estimated": self.estimated,
            "model_ms": self.model_ms,
            "took_ms": self.took_ms(),
        })
    }
}

/// How long something took, for a person: `3.2s`, `42s`, `1m 42s`, `1h 05m`.
fn took(ms: i64) -> String {
    let secs = ms.max(0) / 1000;
    match secs {
        0..=9 => format!("{:.1}s", ms.max(0) as f64 / 1000.0),
        10..=59 => format!("{secs}s"),
        60..=3599 => format!("{}m {:02}s", secs / 60, secs % 60),
        _ => format!("{}h {:02}m", secs / 3600, (secs % 3600) / 60),
    }
}

/// What a request used, in a line: `12 calls · 184k in (93% cached) · 3.1k out · 1m 42s`.
pub fn receipt_line(t: &Tokens) -> String {
    let mut out = format!("{} call{} · {} in", t.calls, if t.calls == 1 { "" } else { "s" }, t.n(t.input()));
    if t.cache_read > 0 {
        out.push_str(&format!(" ({}% cached)", t.cached_pct()));
    }
    out.push_str(&format!(" · {} out · {}", t.n(t.output), took(t.took_ms())));
    out
}

/// The running total for the status bar: `↑368k ↓12.2k`.
pub fn meter(t: &Tokens) -> String {
    format!("↑{} ↓{}", t.n(t.input()), t.n(t.output))
}

/// Everything a session and its subagents have used.
pub fn totals(store: &Store, root: &str) -> Tokens {
    Tokens::of(&store.usage_for_root(root).unwrap_or_default())
}

/// What each request used, with the subagents that worked on it: the id of
/// the message that asked, and the tokens, in the order they were asked.
pub fn by_request(calls: &[ModelCall]) -> Vec<(i64, Tokens)> {
    let mut out: Vec<(i64, Tokens)> = Vec::new();
    for c in calls {
        match out.iter_mut().find(|(id, _)| *id == c.request) {
            Some((_, t)) => t.add(c),
            None => out.push((c.request, Tokens::of([c]))),
        }
    }
    out
}

/// What the requests of a session tree used.
pub fn receipts(store: &Store, root: &str) -> Vec<(i64, Tokens)> {
    by_request(&store.usage_for_root(root).unwrap_or_default())
}

pub struct Request {
    pub id: i64,
    pub title: String,
    pub tokens: Tokens,
}

/// The /usage report for a session and its subagents.
pub struct Report {
    pub session: String,
    /// The session's own calls, and its subagents' (and how many subagents).
    pub own: Tokens,
    pub subagents: Tokens,
    pub subagent_count: usize,
    pub total: Tokens,
    /// By `provider/model`, in the order they were first used.
    pub by_model: Vec<(String, Tokens)>,
    pub requests: Vec<Request>,
    /// What a call carries now, and the parts it is made of.
    pub context: i64,
    pub parts: Vec<(&'static str, i64)>,
    /// The tools whose results take the most room in it.
    pub tool_results: Vec<(String, i64)>,
    /// Tokens in the session's running total from before each call was recorded.
    pub unrecorded: i64,
}

pub fn report(store: &Store, cfg: &Config, session: &Session) -> Result<Report> {
    let root = if session.id == session.root_id { session.clone() } else { store.require_session(&session.root_id)? };
    let calls = store.usage_for_root(&root.id)?;
    let own = Tokens::of(calls.iter().filter(|c| c.session == root.id));
    let theirs: Vec<&ModelCall> = calls.iter().filter(|c| c.session != root.id).collect();
    let mut subagent_ids: Vec<&str> = theirs.iter().map(|c| c.session.as_str()).collect();
    subagent_ids.sort_unstable();
    subagent_ids.dedup();

    let mut by_model: Vec<(String, Tokens)> = Vec::new();
    for c in &calls {
        let name = format!("{}/{}", c.provider, c.model);
        match by_model.iter_mut().find(|(n, _)| *n == name) {
            Some((_, t)) => t.add(c),
            None => by_model.push((name, Tokens::of([c]))),
        }
    }

    let grouped = by_request(&calls);
    let ids: Vec<i64> = grouped.iter().map(|(id, _)| *id).filter(|id| *id > 0).collect();
    let asked: HashMap<i64, String> = store
        .message_blocks(&ids)?
        .into_iter()
        .map(|(id, blocks)| {
            let blocks: Vec<Block> = serde_json::from_str(&blocks).unwrap_or_default();
            (id, first_line(crate::types::blocks_text(&blocks).trim(), 200))
        })
        .collect();
    let requests = grouped
        .into_iter()
        .map(|(id, tokens)| {
            let title = match asked.get(&id) {
                Some(text) if !text.is_empty() => text.clone(),
                _ if root.goal_id.is_some() => "the goal's turns".to_string(),
                _ => "(before anything was asked)".to_string(),
            };
            Request { id, title, tokens }
        })
        .collect();

    let (context, parts, tool_results) = carried(store, cfg, &root)?;
    let unrecorded = (root.input_tokens - own.input()).max(0) + (root.output_tokens - own.output).max(0);
    Ok(Report {
        session: root.id.clone(),
        own,
        subagents: Tokens::of(theirs),
        subagent_count: subagent_ids.len(),
        total: Tokens::of(&calls),
        by_model,
        requests,
        context,
        parts,
        tool_results,
        unrecorded,
    })
}

/// What a call to the model carries as the session stands: the size the
/// backend last reported, shared out among its parts in proportion to their
/// length in characters. The size is exact; the shares are an estimate.
fn carried(store: &Store, cfg: &Config, root: &Session) -> Result<(i64, Vec<(&'static str, i64)>, Vec<(String, i64)>)> {
    if root.system_prompt.is_empty() {
        return Ok((root.context_tokens, Vec::new(), Vec::new()));
    }
    let memory_at = crate::context::memory_starts(&root.system_prompt);
    let tools: usize = crate::tools::specs(root, cfg).iter().map(|t| t.name.len() + t.description.len() + t.schema.to_string().len()).sum();
    let mut called: HashMap<String, String> = HashMap::new();
    let mut conversation = 0usize;
    let mut results: Vec<(String, usize)> = Vec::new();
    for m in store.messages_from(&root.id, root.epoch_start)? {
        for b in &m.blocks {
            match b {
                Block::Text { text } | Block::Thinking { text, .. } => conversation += text.len(),
                Block::ToolUse { id, name, input, .. } => {
                    conversation += name.len() + input.to_string().len();
                    called.insert(id.clone(), name.clone());
                }
                Block::ToolResult { tool_use_id, content, .. } => {
                    let tool = called.get(tool_use_id).cloned().unwrap_or_else(|| "tool".to_string());
                    match results.iter_mut().find(|(name, _)| *name == tool) {
                        Some((_, chars)) => *chars += content.len(),
                        None => results.push((tool, content.len())),
                    }
                }
                // Something a backend sent that is kept to send back as it came.
                Block::Raw { .. } => conversation += serde_json::to_string(b).map(|s| s.len()).unwrap_or(0),
            }
        }
    }
    let chars = [
        ("instructions", memory_at),
        ("tools", tools),
        ("memory and work", root.system_prompt.len() - memory_at),
        ("conversation", conversation),
        ("tool results", results.iter().map(|(_, c)| c).sum()),
    ];
    let estimated: i64 = chars.iter().map(|(_, c)| estimate_tokens(*c)).sum();
    // The backend's own count of the whole, where there is one, shared out in proportion.
    let whole = if root.context_tokens > 0 { root.context_tokens } else { estimated };
    let share = |c: usize| if estimated > 0 { (estimate_tokens(c) as f64 * whole as f64 / estimated as f64).round() as i64 } else { 0 };
    let mut parts: Vec<(&'static str, i64)> = chars.iter().map(|(name, c)| (*name, share(*c))).collect();
    // Rounding must not make the parts disagree with the whole.
    let drift = whole - parts.iter().map(|p| p.1).sum::<i64>();
    if let Some(largest) = parts.iter_mut().max_by_key(|p| p.1) {
        largest.1 = (largest.1 + drift).max(0);
    }
    let mut tool_results: Vec<(String, i64)> = results.into_iter().map(|(name, c)| (name, share(c))).filter(|(_, t)| *t > 0).collect();
    tool_results.sort_by_key(|(_, t)| std::cmp::Reverse(*t));
    tool_results.truncate(4);
    Ok((whole, parts, tool_results))
}

/// A bar of `width` cells shared out among the parts, each with its own fill.
const FILLS: [char; 5] = ['█', '▓', '▒', '░', '▞'];

fn bar(parts: &[(&'static str, i64)], width: usize) -> String {
    let total: i64 = parts.iter().map(|p| p.1).sum();
    if total <= 0 || width == 0 {
        return String::new();
    }
    // Every part that is there gets a cell, however small it is.
    let mut cells: Vec<usize> =
        parts.iter().map(|p| if p.1 > 0 { ((p.1 as f64 / total as f64 * width as f64).round() as usize).max(1) } else { 0 }).collect();
    let used: usize = cells.iter().sum();
    if let Some(largest) = (0..cells.len()).max_by_key(|i| cells[*i]) {
        cells[largest] = (cells[largest] + width).saturating_sub(used).max(1);
    }
    cells.iter().enumerate().map(|(i, n)| FILLS[i % FILLS.len()].to_string().repeat(*n)).collect()
}

/// The report as lines of text, for a window `width` columns wide.
pub fn lines(r: &Report, width: usize) -> Vec<String> {
    let mut out = Vec::new();
    if r.total.calls == 0 {
        out.push("No requests to a model are recorded for this session yet.".to_string());
    } else {
        let name_w = width.saturating_sub(5 * 9).clamp(14, 36);
        let head = |title: &str, last: &str| format!("{title:<name_w$}{:>9}{:>9}{:>9}{:>9}{:>9}", "calls", "in", "cached", "out", last);
        let row = |name: &str, t: &Tokens, time: i64| {
            format!(
                "{:<name_w$}{:>9}{:>9}{:>9}{:>9}{:>9}",
                first_line(name, name_w.saturating_sub(2)),
                t.calls,
                t.n(t.input()),
                format!("{}%", t.cached_pct()),
                t.n(t.output),
                fmt_ms(time)
            )
        };
        out.push(head("", "model"));
        out.push(row("this session", &r.own, r.own.model_ms));
        if r.subagent_count > 0 {
            let name = format!("{} subagent{}", r.subagent_count, if r.subagent_count == 1 { "" } else { "s" });
            out.push(row(&name, &r.subagents, r.subagents.model_ms));
            out.push(row("total", &r.total, r.total.model_ms));
        }
        let t = &r.total;
        out.push(format!(
            "of the input: {} read from cache, {} written to it, {} neither",
            t.n(t.cache_read),
            t.n(t.cache_write),
            t.n(t.fresh)
        ));
        if t.estimated {
            out.push("~ a backend reported no counts for some calls: those are narsil's estimates".to_string());
        }
        if r.by_model.len() > 1 {
            out.push(String::new());
            out.push(head("by model", "model"));
            out.extend(r.by_model.iter().map(|(name, t)| row(name, t, t.model_ms)));
        }
    }
    if r.unrecorded > 0 {
        out.push(format!("{} more tokens were used here before narsil kept a record of each call", fmt_tokens(r.unrecorded)));
    }

    if !r.parts.is_empty() && r.context > 0 {
        out.push(String::new());
        out.push(format!("what a call carries now: {} (the parts are estimated shares of it)", fmt_tokens(r.context)));
        out.push(bar(&r.parts, width.clamp(20, 72)));
        // The key, as many to a line as fit.
        let mut line = String::new();
        for (i, (name, tokens)) in r.parts.iter().enumerate().filter(|(_, part)| part.1 > 0) {
            let key = format!("{} {name} {}", FILLS[i % FILLS.len()], fmt_tokens(*tokens));
            if !line.is_empty() && line.chars().count() + 3 + key.chars().count() > width {
                out.push(std::mem::take(&mut line));
            }
            if !line.is_empty() {
                line.push_str("   ");
            }
            line.push_str(&key);
        }
        out.push(line);
        if !r.tool_results.is_empty() {
            let largest: Vec<String> = r.tool_results.iter().map(|(name, t)| format!("{name} {}", fmt_tokens(*t))).collect();
            out.push(format!("largest tool results: {}", largest.join(" · ")));
        }
    }

    if !r.requests.is_empty() {
        let name_w = width.saturating_sub(5 * 9).clamp(14, 36);
        out.push(String::new());
        out.push(format!("{:<name_w$}{:>9}{:>9}{:>9}{:>9}{:>9}", "by request", "calls", "in", "cached", "out", "took"));
        for q in &r.requests {
            let t = &q.tokens;
            out.push(format!(
                "{:<name_w$}{:>9}{:>9}{:>9}{:>9}{:>9}",
                first_line(&q.title, name_w.saturating_sub(2)),
                t.calls,
                t.n(t.input()),
                format!("{}%", t.cached_pct()),
                t.n(t.output),
                fmt_ms(t.took_ms())
            ));
        }
    }
    out
}

/// The report for a program to read.
pub fn json(r: &Report) -> Value {
    json!({
        "session": r.session,
        "total": r.total.json(),
        "own": r.own.json(),
        "subagents": { "count": r.subagent_count, "tokens": r.subagents.json() },
        "by_model": r.by_model.iter().map(|(name, t)| json!({ "model": name, "tokens": t.json() })).collect::<Vec<_>>(),
        "requests": r.requests.iter().map(|q| json!({ "id": q.id, "asked": q.title, "tokens": q.tokens.json() })).collect::<Vec<_>>(),
        "context": {
            "tokens": r.context,
            "parts": r.parts.iter().map(|(name, t)| json!({ "part": name, "tokens": t })).collect::<Vec<_>>(),
            "tool_results": r.tool_results.iter().map(|(name, t)| json!({ "tool": name, "tokens": t })).collect::<Vec<_>>(),
        },
        "unrecorded": r.unrecorded,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(session: &str, request: i64, model: &str, fresh: i64, read: i64, write: i64, output: i64, started_ms: i64, dur_ms: i64) -> ModelCall {
        ModelCall {
            session: session.into(),
            root: "root".into(),
            request,
            purpose: "turn".into(),
            provider: "p".into(),
            model: model.into(),
            input: fresh,
            cache_read: read,
            cache_write: write,
            output,
            estimated: false,
            started_ms,
            dur_ms,
        }
    }

    #[test]
    fn calls_add_up_with_the_input_kept_in_its_three_parts() {
        let calls = vec![
            call("root", 5, "m", 1_000, 0, 9_000, 200, 1_000, 2_000),
            call("root", 5, "m", 300, 9_000, 700, 100, 4_000, 1_000),
            call("sub", 5, "m", 2_000, 0, 0, 400, 5_000, 3_000),
            call("root", 9, "m", 500, 10_000, 0, 50, 60_000, 500),
        ];
        let all = Tokens::of(&calls);
        assert_eq!((all.calls, all.fresh, all.cache_read, all.cache_write, all.output), (4, 3_800, 19_000, 9_700, 750));
        // "in" is everything that was sent, whichever way it was billed.
        assert_eq!(all.input(), 32_500);
        assert_eq!(all.cached_pct(), 58);
        assert_eq!(all.model_ms, 6_500);

        // By request, with the subagent's call counted in the request it worked on.
        let requests = by_request(&calls);
        assert_eq!(requests.iter().map(|(id, t)| (*id, t.calls, t.input(), t.output)).collect::<Vec<_>>(), vec![(5, 3, 22_000, 700), (9, 1, 10_500, 50)]);
        // From the first call being made to the last ending, tools and all.
        assert_eq!(requests[0].1.took_ms(), 7_000);
        // And the requests together are the whole: nothing is counted twice or lost.
        assert_eq!(requests.iter().map(|(_, t)| t.input()).sum::<i64>(), all.input());
        assert_eq!(requests.iter().map(|(_, t)| t.output).sum::<i64>(), all.output);
    }

    #[test]
    fn a_receipt_and_the_meter_say_it_in_a_line() {
        let t = Tokens { calls: 12, fresh: 13_000, cache_read: 171_000, cache_write: 0, output: 3_100, began_ms: 1_000, ended_ms: 103_000, ..Default::default() };
        assert_eq!(receipt_line(&t), "12 calls · 184.0k in (93% cached) · 3.1k out · 1m 42s");
        assert_eq!(meter(&t), "↑184.0k ↓3.1k");
        // One call, nothing cached: no share is claimed.
        let one = Tokens { calls: 1, fresh: 812, output: 40, began_ms: 0, ended_ms: 3_240, ..Default::default() };
        assert_eq!(receipt_line(&one), "1 call · 812 in · 40 out · 3.2s");
        // Counts that are narsil's own estimate say so.
        let guess = Tokens { estimated: true, ..one.clone() };
        assert_eq!(receipt_line(&guess), "1 call · ~812 in · ~40 out · 3.2s");
        assert_eq!(meter(&guess), "↑~812 ↓~40");
        assert_eq!([took(42_000), took(3_723_000)], ["42s", "1h 02m"]);
        assert_eq!(Tokens::default().cached_pct(), 0);
    }

    #[test]
    fn the_bar_is_as_wide_as_asked_and_shows_every_part() {
        let parts = [("instructions", 3_100), ("tools", 4_800), ("memory and work", 20), ("conversation", 9_600), ("tool results", 21_500)];
        for width in [20, 47, 72] {
            let drawn = bar(&parts, width);
            assert_eq!(drawn.chars().count(), width, "{drawn}");
            // The smallest part is there too.
            assert!(FILLS.iter().all(|fill| drawn.contains(*fill)), "{drawn}");
        }
        assert!(bar(&[("a", 0), ("b", 10)], 10).chars().all(|c| c == FILLS[1]));
        assert_eq!(bar(&[("a", 0)], 10), "");
    }

    #[test]
    fn the_report_reads_as_a_table_a_bar_and_the_requests() {
        let calls = vec![
            call("root", 5, "big", 13_000, 171_000, 0, 3_100, 1_000, 60_000),
            call("sub", 5, "small", 28_000, 68_000, 0, 4_200, 2_000, 30_000),
            call("root", 9, "big", 17_600, 70_400, 0, 4_900, 200_000, 40_000),
        ];
        let requests = by_request(&calls);
        let r = Report {
            session: "root".into(),
            own: Tokens::of(calls.iter().filter(|c| c.session == "root")),
            subagents: Tokens::of(calls.iter().filter(|c| c.session != "root")),
            subagent_count: 1,
            total: Tokens::of(&calls),
            by_model: vec![("p/big".into(), Tokens::of([&calls[0], &calls[2]])), ("p/small".into(), Tokens::of([&calls[1]]))],
            requests: vec![
                Request { id: 5, title: "identify gaps in the unit tests and write a plan for them".into(), tokens: requests[0].1.clone() },
                Request { id: 9, title: "now fix the flaky one".into(), tokens: requests[1].1.clone() },
            ],
            context: 40_200,
            parts: vec![("instructions", 3_100), ("tools", 4_800), ("memory and work", 1_200), ("conversation", 9_600), ("tool results", 21_500)],
            tool_results: vec![("read_file".into(), 14_200), ("bash".into(), 5_100)],
            unrecorded: 0,
        };
        let said = lines(&r, 80).join("\n");
        assert!(said.contains("this session                               2   272.0k      89%     8.0k    1m40s"), "{said}");
        assert!(said.contains("1 subagent                                 1    96.0k      71%     4.2k   30.00s"), "{said}");
        assert!(said.contains("total                                      3   368.0k      84%    12.2k    2m10s"), "{said}");
        assert!(said.contains("of the input: 309.4k read from cache, 0 written to it, 58.6k neither"), "{said}");
        assert!(said.contains("by model") && said.contains("p/small"), "{said}");
        assert!(said.contains("what a call carries now: 40.2k"), "{said}");
        assert!(said.contains("█ instructions 3.1k   ▓ tools 4.8k   ▒ memory and work 1.2k"), "{said}");
        assert!(said.contains("largest tool results: read_file 14.2k · bash 5.1k"), "{said}");
        // The first request: both the session's call and the subagent's, and the time from start to finish.
        assert!(said.contains("identify gaps in the unit tests …          2   280.0k      85%     7.3k    1m00s"), "{said}");
        assert!(lines(&r, 80).iter().all(|l| l.chars().count() <= 81), "{said}");

        // For a program: the same numbers.
        let v = json(&r);
        assert_eq!(v["total"]["input"], 368_000);
        assert_eq!(v["total"]["cache_read"], 309_400);
        assert_eq!(v["requests"][0]["tokens"]["calls"], 2);
        assert_eq!(v["context"]["parts"][4]["tokens"], 21_500);

        // Nothing recorded yet, and a session from before calls were recorded.
        let empty = Report { total: Tokens::default(), own: Tokens::default(), requests: Vec::new(), parts: Vec::new(), unrecorded: 5_300, ..r };
        let said = lines(&empty, 80).join("\n");
        assert!(said.contains("No requests to a model are recorded") && said.contains("5.3k more tokens were used here before"), "{said}");
    }
}
