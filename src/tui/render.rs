//! Turning transcript items into styled, pre-wrapped terminal lines.

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use serde_json::Value;

use unicode_width::UnicodeWidthStr;

use crate::store::AgentInfo;
use crate::tools::summarize_call;
use crate::util::{first_line, fmt_duration, now};

/// The colour and the mark of anything that touches the user's Obsidian vault.
pub const VAULT: Color = Color::Indexed(141);
pub const VAULT_MARK: &str = "◈";

static VAULT_ROOT: std::sync::RwLock<String> = std::sync::RwLock::new(String::new());

/// Tell the renderer where the vault is (empty: there is none), so that file
/// tools and commands that reach into it are marked like the vault tools.
pub fn set_vault(root: &str) {
    *VAULT_ROOT.write().unwrap_or_else(|e| e.into_inner()) = root.trim_end_matches('/').to_string();
}

/// Whether a tool call reads or writes the user's Obsidian vault.
fn touches_vault(name: &str, input: &Value, cwd: &str) -> bool {
    if name.starts_with("vault_") {
        return true;
    }
    let root = VAULT_ROOT.read().unwrap_or_else(|e| e.into_inner());
    if root.is_empty() {
        return false;
    }
    let text = |key: &str| input.get(key).and_then(Value::as_str);
    let inside = |path: &str| {
        let path = crate::util::expand_tilde(path);
        let full = if path.is_absolute() { path } else { std::path::Path::new(cwd).join(path) };
        // `../Vault/x` from a folder beside the vault is in the vault.
        crate::obsidian::lexical(&full).starts_with(root.as_str())
    };
    match name {
        "bash" => text("command").is_some_and(|c| c.contains(root.as_str())) || inside(text("cwd").unwrap_or(".")),
        "read_file" | "write_file" | "edit_file" | "list_dir" | "glob" | "grep" => inside(text("path").unwrap_or(".")),
        _ => false,
    }
}

pub const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

pub enum Item {
    User { text: String },
    Assistant { text: String },
    Thinking { text: String },
    Tool {
        id: String,
        name: String,
        input: Value,
        output: Option<String>,
        is_error: bool,
        running: bool,
        /// For `spawn_agents`: the subagents this call created, as they stand now.
        agents: Vec<AgentInfo>,
    },
    Note { text: String },
    Error { text: String },
    Divider { text: String },
}

pub struct Entry {
    pub item: Item,
    /// The transcript message this item came from (0 for local notices).
    pub msg_id: i64,
    /// Rendered lines, valid for one (width, expanded) combination.
    cache: Option<(usize, bool, Vec<Line<'static>>)>,
}

impl Entry {
    #[cfg(test)]
    pub fn new(item: Item) -> Self {
        Entry { item, msg_id: 0, cache: None }
    }

    pub fn at(item: Item, msg_id: i64) -> Self {
        Entry { item, msg_id, cache: None }
    }

    pub fn touch(&mut self) {
        self.cache = None;
    }

    /// `cwd` is the session's working directory; paths under it are shown relative.
    pub fn lines(&mut self, width: usize, expanded: bool, spin: usize, cwd: &str) -> &[Line<'static>] {
        let animated = match &self.item {
            Item::Tool { running, agents, .. } => *running || agents.iter().any(|a| a.state == "running"),
            _ => false,
        };
        let stale = !matches!(&self.cache, Some((w, e, _)) if *w == width && *e == expanded);
        if stale || animated {
            self.cache = Some((width, expanded, render(&self.item, width, expanded, spin, cwd)));
        }
        &self.cache.as_ref().expect("just rendered").2
    }
}

fn dim() -> Style {
    Style::new().fg(Color::DarkGray)
}

pub fn wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(8);
    let mut out = Vec::new();
    for line in text.split('\n') {
        let line = line.replace('\t', "    ");
        if line.trim().is_empty() {
            out.push(String::new());
            continue;
        }
        for piece in textwrap::wrap(&line, textwrap::Options::new(width).break_words(true)) {
            out.push(piece.into_owned());
        }
    }
    out
}

/// Inline `code` and **bold**. State carries across the wrapped pieces of one source line.
fn inline(piece: &str, base: Style, bold: &mut bool, code: &mut bool) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    let mut buf = String::new();
    let style = |bold: bool, code: bool| {
        if code {
            base.fg(Color::Cyan)
        } else if bold {
            base.add_modifier(Modifier::BOLD)
        } else {
            base
        }
    };
    let chars: Vec<char> = piece.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        let toggle_code = c == '`';
        let toggle_bold = !*code && c == '*' && chars.get(i + 1) == Some(&'*');
        if toggle_code || toggle_bold {
            if !buf.is_empty() {
                spans.push(Span::styled(std::mem::take(&mut buf), style(*bold, *code)));
            }
            if toggle_code {
                *code = !*code;
                i += 1;
            } else {
                *bold = !*bold;
                i += 2;
            }
            continue;
        }
        buf.push(c);
        i += 1;
    }
    if !buf.is_empty() {
        spans.push(Span::styled(buf, style(*bold, *code)));
    }
    spans
}

/// The cells of a markdown table row, or None if the line is not one.
fn table_cells(line: &str) -> Option<Vec<String>> {
    let t = line.trim();
    if !t.starts_with('|') || t.len() < 3 {
        return None;
    }
    let inner = t.strip_prefix('|').unwrap_or(t);
    let inner = inner.strip_suffix('|').unwrap_or(inner);
    // Cell text is shown plain: emphasis and code markers would throw the columns off.
    Some(inner.split('|').map(|c| c.trim().replace("**", "").replace('`', "")).collect())
}

fn is_rule_row(cells: &[String]) -> bool {
    cells.iter().all(|c| c.contains('-') && c.chars().all(|ch| matches!(ch, '-' | ':' | ' ')))
}

/// A markdown table as aligned columns. Columns shrink, widest first, until
/// the table fits; cells that no longer fit on one line wrap inside their column.
fn table(rows: &[Vec<String>], width: usize) -> Vec<Line<'static>> {
    let has_header = rows.len() > 1 && is_rule_row(&rows[1]);
    let body: Vec<&Vec<String>> = rows.iter().filter(|r| !is_rule_row(r)).collect();
    let cols = body.iter().map(|r| r.len()).max().unwrap_or(0);
    if cols == 0 {
        return Vec::new();
    }
    let mut widths = vec![1usize; cols];
    for row in &body {
        for (i, cell) in row.iter().enumerate() {
            widths[i] = widths[i].max(cell.width());
        }
    }
    let room = width.saturating_sub(3 * (cols - 1)).max(cols * 3);
    while widths.iter().sum::<usize>() > room {
        let widest = (0..cols).max_by_key(|i| widths[*i]).unwrap_or(0);
        if widths[widest] <= 3 {
            break;
        }
        widths[widest] -= 1;
    }
    let fit = |text: &str, w: usize| -> Vec<String> {
        textwrap::wrap(text, textwrap::Options::new(w.max(1)).break_words(true)).into_iter().map(|p| p.into_owned()).collect()
    };
    let mut lines = Vec::new();
    for (n, row) in body.iter().enumerate() {
        let style = if n == 0 && has_header { Style::new().add_modifier(Modifier::BOLD) } else { Style::new() };
        let cells: Vec<Vec<String>> =
            (0..cols).map(|i| fit(row.get(i).map(String::as_str).unwrap_or(""), widths[i])).collect();
        let height = cells.iter().map(Vec::len).max().unwrap_or(1).max(1);
        for h in 0..height {
            let mut spans = Vec::new();
            for (i, cell) in cells.iter().enumerate() {
                if i > 0 {
                    spans.push(Span::styled(" │ ", dim()));
                }
                let piece = cell.get(h).map(String::as_str).unwrap_or("");
                let pad = if i + 1 < cols { widths[i].saturating_sub(piece.width()) } else { 0 };
                spans.push(Span::styled(format!("{piece}{}", " ".repeat(pad)), style));
            }
            lines.push(Line::from(spans));
        }
        if n == 0 && has_header {
            let rule = widths.iter().map(|w| "─".repeat(*w)).collect::<Vec<_>>().join("─┼─");
            lines.push(Line::styled(rule, dim()));
        }
    }
    lines
}

/// For a list item ("- x", "* x", "12. x", possibly indented): the column
/// where its text starts. 0 for any other line.
fn list_indent(line: &str) -> usize {
    let lead = line.len() - line.trim_start().len();
    let rest = &line[lead..];
    let marker = if rest.starts_with("- ") || rest.starts_with("* ") || rest.starts_with("+ ") {
        2
    } else {
        let digits = rest.chars().take_while(char::is_ascii_digit).count();
        let after = &rest[digits..];
        if (1..=3).contains(&digits) && (after.starts_with(". ") || after.starts_with(") ")) { digits + 2 } else { 0 }
    };
    if marker == 0 { 0 } else { lead + marker }
}

/// A light markdown rendering: headings, fenced code, tables, lists, inline code and bold.
pub fn markdown(text: &str, width: usize) -> Vec<Line<'static>> {
    let base = Style::new();
    let mut lines = Vec::new();
    let mut fenced = false;
    let mut rows: Vec<Vec<String>> = Vec::new();
    for raw in text.trim_end().split('\n') {
        let trimmed = raw.trim_start();
        match table_cells(raw).filter(|_| !fenced) {
            Some(cells) => {
                rows.push(cells);
                continue;
            }
            None if !rows.is_empty() => lines.extend(table(&std::mem::take(&mut rows), width)),
            None => {}
        }
        if trimmed.starts_with("```") {
            fenced = !fenced;
            let label = trimmed.trim_start_matches('`').trim();
            if fenced && !label.is_empty() {
                lines.push(Line::styled(format!("  {label}"), dim()));
            }
            continue;
        }
        if fenced {
            for piece in wrap(raw, width.saturating_sub(2)) {
                lines.push(Line::styled(format!("  {piece}"), Style::new().fg(Color::Cyan)));
            }
            continue;
        }
        if trimmed.starts_with('#') {
            let title = trimmed.trim_start_matches('#').trim();
            for piece in wrap(title, width) {
                lines.push(Line::styled(piece, Style::new().add_modifier(Modifier::BOLD).fg(Color::White)));
            }
            continue;
        }
        // List items wrap under their own text, not under the marker.
        let hang = Some(list_indent(raw)).filter(|h| *h <= width / 2).unwrap_or(0);
        let (marker, body) = raw.split_at(hang);
        let (mut bold, mut code) = (false, false);
        for (i, piece) in wrap(body, width.saturating_sub(hang)).into_iter().enumerate() {
            let mut spans = inline(&piece, base, &mut bold, &mut code);
            if hang > 0 {
                spans.insert(0, Span::raw(if i == 0 { marker.replace('\t', " ") } else { " ".repeat(hang) }));
            }
            lines.push(Line::from(spans));
        }
    }
    if !rows.is_empty() {
        lines.extend(table(&rows, width));
    }
    lines
}

fn prefixed(text: &str, width: usize, first: &str, rest: &str, style: Style) -> Vec<Line<'static>> {
    let inner = width.saturating_sub(first.chars().count()).max(8);
    wrap(text.trim_end(), inner)
        .into_iter()
        .enumerate()
        .map(|(i, piece)| Line::styled(format!("{}{piece}", if i == 0 { first } else { rest }), style))
        .collect()
}

/// Show paths under the working directory relative to it, and the directory
/// itself as ".", so long absolute paths do not crowd out what matters.
fn relative_to(text: &str, cwd: &str) -> String {
    let cwd = cwd.trim_end_matches('/');
    if cwd.len() < 2 || !text.contains(cwd) {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find(cwd) {
        out.push_str(&rest[..at]);
        let after = &rest[at + cwd.len()..];
        match after.chars().next() {
            // cwd/sub/file -> sub/file
            Some('/') if after[1..].chars().next().is_some_and(|c| !c.is_whitespace() && c != '/') => {
                rest = &after[1..];
            }
            // A longer name that merely starts the same way is a different path.
            Some(c) if c.is_alphanumeric() || matches!(c, '_' | '-' | '.') => {
                out.push_str(cwd);
                rest = after;
            }
            _ => {
                out.push('.');
                rest = after.strip_prefix('/').unwrap_or(after);
            }
        }
    }
    out.push_str(rest);
    out
}

fn render(item: &Item, width: usize, expanded: bool, spin: usize, cwd: &str) -> Vec<Line<'static>> {
    let relative = |s: &str| relative_to(s, cwd);
    match item {
        Item::User { text } => prefixed(text, width, "› ", "  ", Style::new().fg(Color::LightBlue).add_modifier(Modifier::BOLD)),
        Item::Assistant { text } => markdown(text, width),
        Item::Thinking { text } => {
            let body = if expanded { text.clone() } else { first_line(text, width.saturating_sub(6).max(20)) };
            prefixed(&body, width, "∴ ", "  ", dim().add_modifier(Modifier::ITALIC))
        }
        // What narsil found in the vault for a request is shown as the vault's.
        Item::Note { text } if text.starts_with("Obsidian:") => prefixed(text, width, "◈ ", "  ", Style::new().fg(VAULT)),
        Item::Note { text } => prefixed(text, width, "· ", "  ", dim()),
        Item::Error { text } => prefixed(text, width, "✗ ", "  ", Style::new().fg(Color::Red)),
        Item::Divider { text } => {
            let pad = width.saturating_sub(text.chars().count() + 2) / 2;
            vec![Line::styled(format!("{} {text} {}", "─".repeat(pad), "─".repeat(pad)), dim())]
        }
        Item::Tool { name, input, output, is_error, running, agents, .. } => {
            let (mark, mark_style) = if *running {
                (SPINNER[spin % SPINNER.len()], Style::new().fg(Color::Yellow))
            } else if *is_error {
                ("✗", Style::new().fg(Color::Red))
            } else if output.is_some() {
                ("●", Style::new().fg(Color::Green))
            } else {
                ("○", dim())
            };
            let roster = name == "spawn_agents" && !agents.is_empty();
            let mut lines = if roster {
                agent_block(agents, *running, *is_error, output.is_some(), width, spin)
            } else {
                // Anything that reaches into the user's Obsidian vault is marked as such.
                let vault = touches_vault(name, input, cwd);
                let used = 2 + name.chars().count() + 1 + if vault { 2 } else { 0 };
                let summary = first_line(&relative(&summarize_call(name, input)), width.saturating_sub(used).max(10));
                let mut spans = vec![Span::styled(format!("{mark} "), mark_style)];
                let mut named = Style::new().add_modifier(Modifier::BOLD);
                if vault {
                    spans.push(Span::styled(format!("{VAULT_MARK} "), Style::new().fg(VAULT)));
                    named = named.fg(VAULT);
                }
                spans.extend([Span::styled(name.clone(), named), Span::raw(" "), Span::raw(summary)]);
                vec![Line::from(spans)]
            };
            // The agent rows already say how each one ended; the full reports are one keypress away.
            let show_output = output.is_some() && (!roster || expanded);
            if let (Some(out), true) = (output, show_output) {
                let all = wrap(relative(out).trim_end(), width.saturating_sub(4));
                let limit = if expanded { 400 } else { 5 };
                let style = if *is_error { Style::new().fg(Color::Red) } else { dim() };
                for (i, piece) in all.iter().take(limit).enumerate() {
                    lines.push(Line::styled(format!("{}{piece}", if i == 0 { "  ⎿ " } else { "    " }), style));
                }
                if all.len() > limit {
                    lines.push(Line::styled(format!("    … {} more lines (ctrl+o to expand)", all.len() - limit), dim()));
                }
            } else if roster && output.is_some() {
                lines.push(Line::styled("    ctrl+o shows the full reports", dim()));
            }
            lines
        }
    }
}

/// The glyph and colour for a subagent's state.
pub fn agent_mark(state: &str, spin: usize) -> (&'static str, Style) {
    match state {
        "running" => (SPINNER[spin % SPINNER.len()], Style::new().fg(Color::Yellow)),
        "done" => ("✓", Style::new().fg(Color::Green)),
        "failed" => ("✗", Style::new().fg(Color::Red)),
        "interrupted" => ("⏸", Style::new().fg(Color::Magenta)),
        _ => ("○", dim()),
    }
}

/// What a subagent is for, in a few words: its objective if it has one
/// (marked ◆), otherwise the start of its task.
pub fn agent_label(a: &AgentInfo) -> String {
    if a.objective.is_empty() { first_line(&a.task, 200) } else { format!("◆ {}", first_line(&a.objective, 200)) }
}

/// One line on where a subagent stands right now.
pub fn agent_status(a: &AgentInfo) -> String {
    match a.state.as_str() {
        "running" if a.activity.is_empty() => "thinking…".to_string(),
        "running" => a.activity.clone(),
        "queued" => "queued".to_string(),
        _ => a.summary.clone(),
    }
}

/// "3 agents · 2 running · 1 done"
pub fn agent_counts(agents: &[AgentInfo]) -> String {
    let mut parts = vec![format!("{} agent{}", agents.len(), if agents.len() == 1 { "" } else { "s" })];
    for state in ["running", "queued", "done", "failed", "interrupted"] {
        let n = agents.iter().filter(|a| a.state == state).count();
        if n > 0 {
            parts.push(format!("{n} {state}"));
        }
    }
    parts.join(" · ")
}

fn clip(text: &str, max: usize) -> String {
    if text.width() <= max {
        return text.to_string();
    }
    let mut out = String::new();
    for c in text.chars() {
        if out.width() + 2 > max {
            break;
        }
        out.push(c);
    }
    out.push('…');
    out
}

/// A `spawn_agents` call drawn as a live roster: one row per subagent with its
/// state, what it is for, how long it has run and how many tool calls it has
/// made, and beneath it what it is doing now or how it ended.
fn agent_block(agents: &[AgentInfo], running: bool, is_error: bool, finished: bool, width: usize, spin: usize) -> Vec<Line<'static>> {
    let active = running || agents.iter().any(|a| a.state == "running");
    let (mark, mark_style) = if active {
        (SPINNER[spin % SPINNER.len()], Style::new().fg(Color::Yellow))
    } else if is_error {
        ("✗", Style::new().fg(Color::Red))
    } else if finished {
        ("●", Style::new().fg(Color::Green))
    } else {
        ("○", dim())
    };
    let mut lines = vec![Line::from(vec![
        Span::styled(format!("{mark} "), mark_style),
        Span::styled("spawn_agents", Style::new().add_modifier(Modifier::BOLD)),
        Span::raw("  "),
        Span::styled(clip(&agent_counts(agents), width.saturating_sub(16).max(8)), dim()),
    ])];
    let name_w = agents.iter().map(|a| a.name.width()).max().unwrap_or(0).clamp(4, 16);
    let clock = now();
    for a in agents {
        let (m, m_style) = agent_mark(&a.state, spin);
        let name = clip(&a.name, name_w);
        let meta = match a.state.as_str() {
            "queued" => "queued".to_string(),
            _ if a.started_at == 0 && a.worked == 0 => String::new(),
            _ => format!("{} · {} call{}", fmt_duration(a.elapsed(clock)), a.calls, if a.calls == 1 { "" } else { "s" }),
        };
        // "  m name  label   meta"
        let fixed = 2 + 2 + name_w + 2;
        let meta_w = if width >= fixed + 16 + meta.width() + 2 { meta.width() } else { 0 };
        let label_room = width.saturating_sub(fixed + if meta_w > 0 { meta_w + 2 } else { 0 }).max(8);
        let label = clip(&agent_label(a), label_room);
        let linked = !a.objective.is_empty();
        let mut spans = vec![
            Span::raw("  "),
            Span::styled(format!("{m} "), m_style),
            Span::styled(format!("{name}{}", " ".repeat(name_w - name.width().min(name_w) + 2)), Style::new().fg(Color::Magenta).add_modifier(Modifier::BOLD)),
        ];
        let pad = label_room.saturating_sub(label.width());
        spans.push(Span::styled(label, if linked { Style::new().fg(Color::LightBlue) } else { Style::new() }));
        if meta_w > 0 {
            spans.push(Span::raw(" ".repeat(pad + 2)));
            spans.push(Span::styled(meta, dim()));
        }
        lines.push(Line::from(spans));
        let status = agent_status(a);
        if a.state != "queued" && !status.is_empty() {
            let style = if a.state == "failed" { Style::new().fg(Color::Red) } else { dim() };
            let indent = " ".repeat(fixed);
            lines.push(Line::styled(format!("{indent}{}", clip(&status, width.saturating_sub(fixed).max(8))), style));
        }
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn text_of(lines: &[Line<'static>]) -> Vec<String> {
        lines.iter().map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect::<String>()).collect()
    }

    #[test]
    fn what_touches_the_obsidian_vault_is_marked() {
        let tool = |name: &str, input: Value| {
            let mut e = Entry::new(Item::Tool {
                id: "t".into(),
                name: name.into(),
                input,
                output: Some("ok".into()),
                is_error: false,
                running: false,
                agents: Vec::new(),
            });
            let lines = e.lines(80, false, 0, "/home/me/proj");
            (text_of(lines)[0].clone(), lines[0].spans.iter().any(|s| s.style.fg == Some(VAULT)))
        };
        // The vault tools are the vault's wherever it is.
        let (line, purple) = tool("vault_search", json!({ "query": "heat pump" }));
        assert_eq!(line, "● ◈ vault_search heat pump");
        assert!(purple);
        let (line, purple) = tool("read_file", json!({ "path": "/home/me/Vault/Me.md" }));
        assert_eq!(line, "● read_file /home/me/Vault/Me.md");
        assert!(!purple, "with no vault known, a file is just a file");

        set_vault("/home/me/Vault/");
        let (line, purple) = tool("read_file", json!({ "path": "/home/me/Vault/Me.md" }));
        assert_eq!(line, "● ◈ read_file /home/me/Vault/Me.md");
        assert!(purple);
        assert!(tool("grep", json!({ "pattern": "x", "path": "../Vault/People" })).0.starts_with("● ◈ grep"), "a relative path is taken from the working directory");
        assert!(tool("bash", json!({ "command": "ls /home/me/Vault" })).1);
        assert!(!tool("read_file", json!({ "path": "/home/me/Vault2/Me.md" })).1, "a folder that only starts the same is not the vault");
        assert!(!tool("read_file", json!({ "path": "src/main.rs" })).1);
        assert!(!tool("memory_save", json!({ "title": "t", "body": "b" })).1, "narsil's own memory is not the user's notes");
        set_vault("");

        let mut note = Entry::new(Item::Note { text: "Obsidian: notes of the user's that may bear on this: Me.md".into() });
        let lines = note.lines(80, false, 0, "");
        assert!(text_of(lines)[0].starts_with("◈ Obsidian:") && lines[0].style.fg == Some(VAULT));
        let mut plain = Entry::new(Item::Note { text: "Recalled from memory: x".into() });
        assert!(text_of(plain.lines(80, false, 0, ""))[0].starts_with("· "));
    }

    #[test]
    fn markdown_strips_markers_and_keeps_code_verbatim() {
        let md = "# Title\nSome **bold** and `code` text.\n```rust\nlet x = \"**not bold**\";\n```\ndone";
        let lines = text_of(&markdown(md, 60));
        assert_eq!(lines, vec!["Title", "Some bold and code text.", "  rust", "  let x = \"**not bold**\";", "done"]);
    }

    #[test]
    fn markdown_tables_become_aligned_columns() {
        let md = "Results:\n| File | Lines | Longest |\n|---|---:|---|\n| `east.txt` | 64 | 97 |\n| **north.txt** | 37 | 99 |\ndone";
        assert_eq!(
            text_of(&markdown(md, 60)),
            vec![
                "Results:",
                "File      │ Lines │ Longest",
                "──────────┼───────┼────────",
                "east.txt  │ 64    │ 97",
                "north.txt │ 37    │ 99",
                "done",
            ]
        );
        // Too wide for the pane: the long column wraps inside its own width.
        let wide = "| Option | Notes |\n|---|---|\n| A | a rather long explanation that cannot fit on one line |";
        let lines = text_of(&markdown(wide, 30));
        assert!(lines.iter().all(|l| l.width() <= 30), "{lines:#?}");
        assert!(lines.len() > 3 && lines[2].starts_with("A      │ "), "{lines:#?}");
        assert!(lines[3].starts_with("       │ "), "{lines:#?}");
        // A pipe inside a code block is not a table.
        assert_eq!(text_of(&markdown("```\n| not | a table |\n```", 40)), vec!["  | not | a table |"]);
    }

    #[test]
    fn long_tool_output_is_collapsed_until_expanded() {
        let out = (1..=20).map(|n| format!("line {n}")).collect::<Vec<_>>().join("\n");
        let mut e = Entry::new(Item::Tool {
            id: "t".into(),
            name: "bash".into(),
            input: serde_json::json!({ "command": "seq 20" }),
            output: Some(out),
            is_error: false,
            running: false,
            agents: Vec::new(),
        });
        let collapsed = text_of(e.lines(80, false, 0, ""));
        assert_eq!(collapsed.len(), 1 + 5 + 1);
        assert_eq!(collapsed[0], "● bash seq 20");
        assert!(collapsed[6].contains("15 more lines"));
        assert_eq!(e.lines(80, true, 0, "").len(), 21);
    }

    #[test]
    fn paths_under_the_working_directory_are_shown_relative() {
        let mut e = Entry::new(Item::Tool {
            id: "t".into(),
            name: "write_file".into(),
            input: serde_json::json!({ "path": "/home/me/proj/src/lib.rs", "content": "" }),
            output: Some("Created /home/me/proj/src/lib.rs (1 lines, 2 bytes)".into()),
            is_error: false,
            running: false,
            agents: Vec::new(),
        });
        let lines = text_of(e.lines(80, false, 0, "/home/me/proj"));
        assert_eq!(lines, vec!["● write_file src/lib.rs", "  ⎿ Created src/lib.rs (1 lines, 2 bytes)"]);
    }

    fn agent(name: &str, state: &str, objective: &str) -> AgentInfo {
        AgentInfo {
            id: format!("s-{name}"),
            parent: "p".into(),
            name: name.into(),
            task: format!("Do the {name} work across the whole tree and report back"),
            objective: objective.into(),
            spawn_call: "call".into(),
            state: state.into(),
            started_at: if state == "queued" { 0 } else { 100 },
            worked: if state == "done" || state == "failed" { 42 } else { 0 },
            calls: 6,
            ..Default::default()
        }
    }

    #[test]
    fn a_spawn_call_is_drawn_as_a_roster_of_its_agents() {
        let mut lexer = agent("lexer", "done", "Tokenizer passes the spec tests");
        lexer.summary = "Lexer done, 14 tests pass.".into();
        let mut docs = agent("docs", "running", "");
        docs.activity = "bash cargo doc".into();
        docs.started_at = now() - 5;
        let mut broken = agent("bench", "failed", "");
        broken.summary = "the model backend kept failing".into();
        let mut e = Entry::new(Item::Tool {
            id: "call".into(),
            name: "spawn_agents".into(),
            input: serde_json::json!({}),
            output: None,
            is_error: false,
            running: true,
            agents: vec![lexer, docs, broken, agent("later", "queued", "")],
        });
        let lines = text_of(e.lines(90, false, 0, ""));
        assert_eq!(lines[0], "⠋ spawn_agents  4 agents · 1 running · 1 queued · 1 done · 1 failed");
        // A linked agent is labelled with its objective, the others with their task.
        assert!(lines[1].starts_with("  ✓ lexer  ◆ Tokenizer passes the spec tests"), "{}", lines[1]);
        assert!(lines[1].ends_with("0:42 · 6 calls"), "{}", lines[1]);
        assert_eq!(lines[2].trim(), "Lexer done, 14 tests pass.");
        assert!(lines[3].starts_with("  ⠋ docs   Do the docs work"), "{}", lines[3]);
        assert_eq!(lines[4].trim(), "bash cargo doc");
        assert!(lines[5].starts_with("  ✗ bench  "), "{}", lines[5]);
        assert_eq!(lines[6].trim(), "the model backend kept failing");
        assert!(lines[7].starts_with("  ○ later  ") && lines[7].ends_with("queued"), "{}", lines[7]);
        assert_eq!(lines.len(), 8, "a queued agent has no second line");
        assert!(lines.iter().all(|l| l.width() <= 90), "{lines:#?}");
        // Narrow terminals drop the timing column rather than the label.
        let narrow = text_of(e.lines(34, false, 0, ""));
        assert!(narrow.iter().all(|l| l.width() <= 34), "{narrow:#?}");
        assert!(narrow[1].contains("◆ Tokenizer"));
    }

    #[test]
    fn a_finished_spawn_call_keeps_the_roster_and_folds_the_reports() {
        let mut a = agent("lexer", "done", "");
        a.summary = "All good.".into();
        let item = |output: &str, agents| Item::Tool {
            id: "call".into(),
            name: "spawn_agents".into(),
            input: serde_json::json!({ "agents": [{ "name": "lexer" }] }),
            output: Some(output.into()),
            is_error: false,
            running: false,
            agents,
        };
        let mut e = Entry::new(item("## lexer (session abc)\nAll good.\nDetails follow.", vec![a]));
        let folded = text_of(e.lines(80, false, 0, ""));
        assert_eq!(folded[0], "● spawn_agents  1 agent · 1 done");
        assert_eq!(folded.last().unwrap().trim(), "ctrl+o shows the full reports");
        assert!(!folded.iter().any(|l| l.contains("Details follow")));
        assert!(text_of(e.lines(80, true, 0, "")).iter().any(|l| l.contains("Details follow")));
        // Without agent records (a session from before they existed) it is an ordinary tool line.
        let mut plain = Entry::new(item("report", Vec::new()));
        assert_eq!(text_of(plain.lines(80, false, 0, ""))[0], "● spawn_agents lexer");
    }

    #[test]
    fn the_working_directory_itself_is_shown_as_a_dot() {
        let cwd = "/home/me/proj";
        assert_eq!(relative_to("cd /home/me/proj && ls -l src", cwd), "cd . && ls -l src");
        assert_eq!(relative_to("cat /home/me/proj/src/lib.rs", cwd), "cat src/lib.rs");
        assert_eq!(relative_to("ls /home/me/proj/", cwd), "ls .");
        assert_eq!(relative_to("ls /home/me/proj", cwd), "ls .");
        assert_eq!(relative_to("ls /home/me/proj2/x /home/me/proj-old", cwd), "ls /home/me/proj2/x /home/me/proj-old");
        assert_eq!(relative_to("In directory /home/me/proj, count files", cwd), "In directory ., count files");
        assert_eq!(relative_to("nothing here", cwd), "nothing here");
        assert_eq!(relative_to("/etc/hosts", "/"), "/etc/hosts");
    }

    #[test]
    fn list_items_wrap_under_their_own_text() {
        let lines = text_of(&markdown("1. alpha beta gamma delta epsilon\n   - nested item that is long enough to wrap\nplain text that wraps around", 22));
        assert_eq!(lines[0], "1. alpha beta gamma");
        assert_eq!(lines[1], "   delta epsilon");
        assert_eq!(lines[2], "   - nested item that");
        assert!(lines[3].starts_with("     is long"), "{lines:#?}");
        assert!(lines.last().unwrap().starts_with("around") || !lines.last().unwrap().starts_with(' '), "{lines:#?}");
        assert!(lines.iter().all(|l| l.width() <= 22), "{lines:#?}");
        assert_eq!(list_indent("2024. A year, not a list"), 0);
    }

    #[test]
    fn wrapping_never_exceeds_the_width() {
        let lines = wrap(&"x".repeat(205), 40);
        assert_eq!(lines.len(), 6);
        assert!(lines.iter().all(|l| l.chars().count() <= 40));
    }
}
