//! How `narsil run` shows a run on a terminal: the steps the agent takes, set
//! apart from what it says, each on a line that fits the window; a question
//! put to the user that can be read at a glance; and the answer flowed to the
//! width of the window at word boundaries, with code and emphasis shown as
//! such. When the output is not a terminal (a pipe, a script) none of this
//! applies: the answer is written as it comes and the steps stay one plain
//! line each, as scripts expect.

use crate::util::first_line;

pub const RESET: &str = "\x1b[0m";
pub const BOLD: &str = "\x1b[1m";
pub const UNBOLD: &str = "\x1b[22m";
pub const DIM: &str = "\x1b[2m";
pub const RED: &str = "\x1b[31m";
pub const YELLOW: &str = "\x1b[33m";
pub const CYAN: &str = "\x1b[36m";
pub const PLAIN: &str = "\x1b[39m";

/// The step an agent is taking, on one line that fits: `▸ bash  ls -la | wc -l…`.
pub fn step(who: &str, name: &str, summary: &str, width: usize, color: bool) -> String {
    let room = width.saturating_sub(who.chars().count() + name.chars().count() + 6).max(12);
    let summary = first_line(summary, room);
    if color { format!("  {DIM}{who}{RESET}{CYAN}▸{RESET} {BOLD}{name}{RESET}  {DIM}{summary}{RESET}") } else { format!("  {who}▸ {name}  {summary}") }
}

/// What a step came to, under it: the first line of what it returned, and how much more there was.
pub fn outcome(who: &str, output: &str, failed: bool, width: usize, color: bool) -> String {
    let lines = output.lines().count();
    let more = if lines > 1 { format!(" (+{} lines)", lines - 1) } else { String::new() };
    let pad = " ".repeat(who.chars().count());
    let room = width.saturating_sub(pad.len() + more.len() + 8).max(12);
    // The first line that says something: not the bare `[stderr]` that only labels what follows.
    let telling = output.lines().map(str::trim).find(|l| !l.is_empty() && !matches!(*l, "[stderr]" | "[stdout]")).unwrap_or("");
    let said = first_line(telling, room);
    let said = if said.is_empty() { "(no output)".to_string() } else { said };
    match (failed, color) {
        (true, true) => format!("  {pad}  {RED}✗ {said}{RESET}{DIM}{more}{RESET}"),
        (true, false) => format!("  {pad}  ✗ {said}{more}"),
        (false, true) => format!("  {pad}  {DIM}⎿ {said}{more}{RESET}"),
        (false, false) => format!("  {pad}  ⎿ {said}{more}"),
    }
}

/// The question put to the user before a step that needs their leave: what
/// wants to run, set out in full and wrapped, and how to answer. The last
/// line is the prompt itself, to be written without a newline.
pub fn approval(who: &str, tool: &str, summary: &str, width: usize, color: bool) -> Vec<String> {
    let room = width.saturating_sub(8).max(20);
    let mut out = Vec::new();
    out.push(if color { format!("  {YELLOW}?{RESET} {who}{BOLD}{tool}{RESET} wants to run") } else { format!("  ? {who}{tool} wants to run") });
    let mut shown = 0;
    'all: for line in summary.lines() {
        for piece in textwrap::wrap(line, textwrap::Options::new(room).break_words(true)) {
            if shown == 14 {
                out.push(format!("      … {} more lines", summary.lines().count().saturating_sub(shown).max(1)));
                break 'all;
            }
            out.push(format!("      {piece}"));
            shown += 1;
        }
    }
    out.push(if color {
        format!("    allow?  {BOLD}y{RESET} yes  {DIM}·{RESET}  {BOLD}a{RESET} always  {DIM}·{RESET}  {BOLD}n{RESET} no {DIM}(enter){RESET}  › ")
    } else {
        "    allow?  y yes  ·  a always  ·  n no (enter)  › ".to_string()
    });
    out
}

/// The line that closes a run: which session it was, and how to go back to it.
pub fn footer(session: &str, color: bool) -> String {
    if color { format!("{DIM}session {session}  ·  narsil resume {session}{RESET}") } else { format!("session {session}  ·  narsil resume {session}") }
}

/// The answer as it arrives, flowed to a width. Text comes in pieces of any
/// size and goes out a word at a time: broken at spaces, not in the middle of
/// words; a list item's later lines set in under its text; code in fences
/// left exactly as it is. With `styled`, `code` and **emphasis** are shown in
/// colour and bold in place of their marks.
pub struct Flow {
    width: usize,
    styled: bool,
    /// Where on the line the next thing goes.
    col: usize,
    /// How far in a line's later lines start: under a list item's text.
    hang: usize,
    /// Nothing but spaces has come on this line yet, and how many of those.
    fresh: bool,
    lead: usize,
    /// Spaces met since the last word, to put before the next one if it fits.
    gap: usize,
    word: String,
    fence: bool,
    code: bool,
    bold: bool,
}

impl Flow {
    pub fn new(width: usize, styled: bool) -> Flow {
        Flow { width: width.max(20), styled, col: 0, hang: 0, fresh: true, lead: 0, gap: 0, word: String::new(), fence: false, code: false, bold: false }
    }

    /// Whether the last thing written ended a line (or nothing has been written).
    pub fn at_line_start(&self) -> bool {
        self.col == 0 && self.word.is_empty()
    }

    /// Take more of the answer, and return what can be written now.
    pub fn push(&mut self, text: &str) -> String {
        let mut out = String::new();
        for c in text.chars() {
            match c {
                '\n' => {
                    self.flush_word(&mut out);
                    out.push('\n');
                    self.col = 0;
                    self.hang = 0;
                    self.fresh = true;
                    self.lead = 0;
                    self.gap = 0;
                }
                ' ' | '\t' if self.fence => self.word.push(c),
                ' ' | '\t' => {
                    self.flush_word(&mut out);
                    if self.fresh { self.lead += 1 } else { self.gap += 1 }
                }
                _ => self.word.push(c),
            }
        }
        out
    }

    /// The end of the answer: whatever was being held back.
    pub fn finish(&mut self) -> String {
        let mut out = String::new();
        self.flush_word(&mut out);
        if self.styled && (self.code || self.bold) {
            out.push_str(RESET);
            self.code = false;
            self.bold = false;
        }
        out
    }

    fn flush_word(&mut self, out: &mut String) {
        if self.word.is_empty() {
            return;
        }
        let word = std::mem::take(&mut self.word);
        // A fence opens or closes code that is to be left alone.
        if self.fresh && word.starts_with("```") {
            self.fence = !self.fence;
            self.fresh = false;
            out.push_str(&" ".repeat(self.lead));
            if self.styled { out.push_str(&format!("{DIM}{word}{RESET}")) } else { out.push_str(&word) }
            self.col = self.lead + word.chars().count();
            return;
        }
        if self.fence {
            out.push_str(&" ".repeat(self.lead + self.gap));
            out.push_str(&word);
            self.col += self.lead + self.gap + word.chars().count();
            self.fresh = false;
            self.lead = 0;
            self.gap = 0;
            return;
        }
        let (shown, len) = self.inline(&word);
        if self.fresh {
            // The first word of a line: keep its indent, and note where a list item's text starts.
            out.push_str(&" ".repeat(self.lead));
            self.col = self.lead;
            let bullet = matches!(word.as_str(), "-" | "*" | "•") || (word.ends_with('.') && word.len() <= 4 && word[..word.len() - 1].chars().all(|d| d.is_ascii_digit()) && word.len() > 1);
            self.hang = self.lead + if bullet { len + 1 } else { 0 };
            self.fresh = false;
        } else if self.col + self.gap + len > self.width {
            out.push('\n');
            out.push_str(&" ".repeat(self.hang));
            self.col = self.hang;
        } else {
            out.push_str(&" ".repeat(self.gap));
            self.col += self.gap;
        }
        out.push_str(&shown);
        self.col += len;
        self.gap = 0;
    }

    /// A word with its marks of code and emphasis turned into colour and bold:
    /// what to write, and how many columns it takes.
    fn inline(&mut self, word: &str) -> (String, usize) {
        if !self.styled {
            return (word.to_string(), word.chars().count());
        }
        let mut out = String::new();
        let mut len = 0;
        let chars: Vec<char> = word.chars().collect();
        let mut i = 0;
        while i < chars.len() {
            match chars[i] {
                '`' => {
                    self.code = !self.code;
                    out.push_str(if self.code { CYAN } else { PLAIN });
                }
                '*' if !self.code && chars.get(i + 1) == Some(&'*') => {
                    self.bold = !self.bold;
                    out.push_str(if self.bold { BOLD } else { UNBOLD });
                    i += 1;
                }
                c => {
                    out.push(c);
                    len += 1;
                }
            }
            i += 1;
        }
        (out, len)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The answer as written when it arrives in pieces of the given size.
    fn flowed(text: &str, width: usize, styled: bool, piece: usize) -> String {
        let mut flow = Flow::new(width, styled);
        let chars: Vec<char> = text.chars().collect();
        let mut out = String::new();
        for chunk in chars.chunks(piece) {
            out.push_str(&flow.push(&chunk.iter().collect::<String>()));
        }
        out.push_str(&flow.finish());
        out
    }

    #[test]
    fn the_answer_breaks_at_words_however_it_arrives() {
        let text = "98 files in the home directory, counting only regular files and not the folders. There are 153 folders too.\n";
        let whole = flowed(text, 40, false, 1000);
        assert_eq!(
            whole,
            "98 files in the home directory, counting\nonly regular files and not the folders.\nThere are 153 folders too.\n"
        );
        assert!(whole.lines().all(|l| l.chars().count() <= 40));
        // A character at a time, or a few, it comes out the same.
        for piece in [1, 2, 3, 7, 13] {
            assert_eq!(flowed(text, 40, false, piece), whole, "in pieces of {piece}");
        }
        // Lines that already fit, blank lines and indents are left as they are.
        let kept = "First.\n\n  indented line\nLast.\n";
        assert_eq!(flowed(kept, 40, false, 3), kept);
        assert_eq!(flowed("no newline at the end", 40, false, 4), "no newline at the end");
    }

    #[test]
    fn a_list_item_runs_on_under_its_own_text() {
        let text = "- the first item is long enough to need a second line\n- short\n12. numbered ones line up under their text as well\n";
        assert_eq!(
            flowed(text, 30, false, 5),
            "- the first item is long\n  enough to need a second line\n- short\n12. numbered ones line up\n    under their text as well\n"
        );
    }

    #[test]
    fn code_is_shown_as_code_and_fenced_code_is_left_alone() {
        let text = "Run `cargo test` and **check** it.\n```sh\ncargo   test --all   # every   crate, however long this line turns out to be\n```\nDone.\n";
        // Plain: nothing is taken away.
        assert_eq!(flowed(text, 30, false, 4), "Run `cargo test` and **check**\nit.\n```sh\ncargo   test --all   # every   crate, however long this line turns out to be\n```\nDone.\n");
        // Styled: the marks give way to colour and bold, and do not count towards the width.
        let styled = flowed(text, 30, true, 4);
        assert!(styled.starts_with(&format!("Run {CYAN}cargo test{PLAIN} and {BOLD}check{UNBOLD} it.\n")), "{styled:?}");
        assert!(styled.contains("cargo   test --all   # every   crate, however long this line turns out to be\n"), "fenced code is neither wrapped nor respaced: {styled:?}");
        assert!(styled.ends_with("Done.\n"));
        assert_eq!(flowed(text, 30, true, 1), styled, "the same a character at a time");
        // A mark left open is closed at the end, so the terminal is not left in colour.
        assert!(flowed("an `unclosed span", 40, true, 5).ends_with(RESET));
    }

    #[test]
    fn steps_fit_the_window_and_say_how_they_ended() {
        let long = "ls -1A | wc -l; echo \"---files only (non-dir)---\"; find . -maxdepth 1 -type f | wc -l; echo \"---dirs only---\"";
        let line = step("", "bash", long, 60, false);
        assert!(line.starts_with("  ▸ bash  ls -1A | wc -l;") && line.ends_with('…') && line.chars().count() <= 60, "{line}");
        assert_eq!(step("[lexer] ", "read_file", "src/a.rs", 60, false), "  [lexer] ▸ read_file  src/a.rs");
        assert_eq!(outcome("", "253\n98\n---\n153\n2", false, 60, false), "    ⎿ 253 (+4 lines)");
        assert_eq!(outcome("", "", false, 60, false), "    ⎿ (no output)");
        assert_eq!(outcome("", "No such file\n[exit code 2]", true, 60, false), "    ✗ No such file (+1 lines)");
        assert_eq!(outcome("", "[stderr]\nls: cannot access 'x'\n[exit code 2]", true, 60, false), "    ✗ ls: cannot access 'x' (+2 lines)", "the label is not the news");
        assert!(step("", "bash", "ls", 60, true).contains(BOLD) && outcome("", "x", true, 60, true).contains(RED));

        let asked = approval("", "bash", long, 60, false);
        assert_eq!(asked[0], "  ? bash wants to run");
        assert!(asked.len() >= 4 && asked[1..asked.len() - 1].iter().all(|l| l.starts_with("      ") && l.chars().count() <= 60), "{asked:#?}");
        assert_eq!(asked[1..asked.len() - 1].iter().map(|l| l.trim()).collect::<Vec<_>>().join(" "), long, "all of the command is there to be read");
        assert!(asked.last().unwrap().starts_with("    allow?  y yes") && asked.last().unwrap().ends_with("› "));
        assert_eq!(footer("wtvcqma0", false), "session wtvcqma0  ·  narsil resume wtvcqma0");
    }
}
