//! The message input: a small multi-line editor with history.

use unicode_width::UnicodeWidthChar;

pub struct InputBox {
    pub text: String,
    /// Byte offset of the cursor in `text`.
    pub cursor: usize,
    history: Vec<String>,
    hist_pos: Option<usize>,
    draft: String,
}

impl InputBox {
    pub fn new(history: Vec<String>) -> Self {
        InputBox { text: String::new(), cursor: 0, history, hist_pos: None, draft: String::new() }
    }

    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    pub fn insert(&mut self, s: &str) {
        let s = s.replace("\r\n", "\n").replace('\r', "\n");
        self.text.insert_str(self.cursor, &s);
        self.cursor += s.len();
    }

    fn prev_boundary(&self) -> usize {
        self.text[..self.cursor].char_indices().next_back().map(|(i, _)| i).unwrap_or(0)
    }

    fn next_boundary(&self) -> usize {
        self.text[self.cursor..].chars().next().map(|c| self.cursor + c.len_utf8()).unwrap_or(self.text.len())
    }

    pub fn backspace(&mut self) {
        if self.cursor > 0 {
            let p = self.prev_boundary();
            self.text.replace_range(p..self.cursor, "");
            self.cursor = p;
        }
    }

    pub fn delete(&mut self) {
        if self.cursor < self.text.len() {
            let n = self.next_boundary();
            self.text.replace_range(self.cursor..n, "");
        }
    }

    pub fn left(&mut self) {
        self.cursor = self.prev_boundary();
    }

    pub fn right(&mut self) {
        self.cursor = self.next_boundary();
    }

    fn line_start(&self) -> usize {
        self.text[..self.cursor].rfind('\n').map(|i| i + 1).unwrap_or(0)
    }

    fn line_end(&self) -> usize {
        self.text[self.cursor..].find('\n').map(|i| self.cursor + i).unwrap_or(self.text.len())
    }

    pub fn home(&mut self) {
        self.cursor = self.line_start();
    }

    pub fn end(&mut self) {
        self.cursor = self.line_end();
    }

    pub fn word_left(&mut self) {
        let before = &self.text[..self.cursor];
        let trimmed = before.trim_end_matches(|c: char| !c.is_alphanumeric());
        self.cursor = trimmed.rfind(|c: char| !c.is_alphanumeric()).map(|i| i + trimmed[i..].chars().next().map_or(1, char::len_utf8)).unwrap_or(0);
    }

    pub fn word_right(&mut self) {
        let after = &self.text[self.cursor..];
        let skipped = after.len() - after.trim_start_matches(|c: char| !c.is_alphanumeric()).len();
        let rest = &after[skipped..];
        let word = rest.find(|c: char| !c.is_alphanumeric()).unwrap_or(rest.len());
        self.cursor += skipped + word;
    }

    pub fn delete_word(&mut self) {
        let end = self.cursor;
        self.word_left();
        self.text.replace_range(self.cursor..end, "");
    }

    pub fn kill_to_end(&mut self) {
        let e = self.line_end();
        self.text.replace_range(self.cursor..e, "");
    }

    pub fn kill_to_start(&mut self) {
        let s = self.line_start();
        self.text.replace_range(s..self.cursor, "");
        self.cursor = s;
    }

    pub fn clear(&mut self) {
        self.text.clear();
        self.cursor = 0;
        self.hist_pos = None;
    }

    pub fn set(&mut self, text: String) {
        self.cursor = text.len();
        self.text = text;
    }

    /// Take the text for sending and remember it in history.
    pub fn take(&mut self) -> String {
        let text = std::mem::take(&mut self.text);
        self.cursor = 0;
        self.hist_pos = None;
        if !text.trim().is_empty() && self.history.last() != Some(&text) {
            self.history.push(text.clone());
        }
        text
    }

    fn on_first_line(&self) -> bool {
        !self.text[..self.cursor].contains('\n')
    }

    fn on_last_line(&self) -> bool {
        !self.text[self.cursor..].contains('\n')
    }

    /// Up: previous line of a multi-line draft, otherwise older history.
    pub fn up(&mut self) {
        if !self.on_first_line() {
            let col = self.text[self.line_start()..self.cursor].chars().count();
            let prev_end = self.line_start() - 1;
            let prev_start = self.text[..prev_end].rfind('\n').map(|i| i + 1).unwrap_or(0);
            let line = &self.text[prev_start..prev_end];
            self.cursor = prev_start + line.char_indices().nth(col).map(|(i, _)| i).unwrap_or(line.len());
            return;
        }
        if self.history.is_empty() {
            return;
        }
        let pos = match self.hist_pos {
            None => {
                self.draft = self.text.clone();
                self.history.len() - 1
            }
            Some(0) => 0,
            Some(p) => p - 1,
        };
        self.hist_pos = Some(pos);
        self.set(self.history[pos].clone());
    }

    /// Down: next line of a multi-line draft, otherwise newer history.
    pub fn down(&mut self) {
        if !self.on_last_line() {
            let col = self.text[self.line_start()..self.cursor].chars().count();
            let next_start = self.line_end() + 1;
            let next_end = self.text[next_start..].find('\n').map(|i| next_start + i).unwrap_or(self.text.len());
            let line = &self.text[next_start..next_end];
            self.cursor = next_start + line.char_indices().nth(col).map(|(i, _)| i).unwrap_or(line.len());
            return;
        }
        match self.hist_pos {
            None => {}
            Some(p) if p + 1 < self.history.len() => {
                self.hist_pos = Some(p + 1);
                self.set(self.history[p + 1].clone());
            }
            Some(_) => {
                self.hist_pos = None;
                let draft = std::mem::take(&mut self.draft);
                self.set(draft);
            }
        }
    }

    /// Wrap for display. Returns the visual lines and the cursor's (row, column).
    pub fn layout(&self, width: usize) -> (Vec<String>, (usize, usize)) {
        let width = width.max(4);
        let mut lines = vec![String::new()];
        let mut col = 0usize;
        let mut cursor = (0usize, 0usize);
        for (i, c) in self.text.char_indices() {
            if i == self.cursor {
                cursor = (lines.len() - 1, col);
            }
            if c == '\n' {
                lines.push(String::new());
                col = 0;
                continue;
            }
            let w = c.width().unwrap_or(0);
            if col + w > width {
                lines.push(String::new());
                col = 0;
                if i == self.cursor {
                    cursor = (lines.len() - 1, 0);
                }
            }
            lines.last_mut().expect("at least one line").push(c);
            col += w;
        }
        if self.cursor >= self.text.len() {
            cursor = (lines.len() - 1, col);
        }
        (lines, cursor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn editing_respects_multibyte_characters() {
        let mut b = InputBox::new(vec![]);
        b.insert("héllo wörld");
        b.left();
        b.left();
        b.backspace();
        assert_eq!(b.text, "héllo wöld");
        b.word_left();
        b.insert("big ");
        assert_eq!(b.text, "héllo big wöld");
        b.end();
        b.delete_word();
        assert_eq!(b.text, "héllo big ");
        b.home();
        b.delete();
        assert_eq!(b.text, "éllo big ");
    }

    #[test]
    fn history_walks_back_and_restores_the_draft() {
        let mut b = InputBox::new(vec!["first".into(), "second".into()]);
        b.insert("draft");
        b.up();
        assert_eq!(b.text, "second");
        b.up();
        assert_eq!(b.text, "first");
        b.up();
        assert_eq!(b.text, "first");
        b.down();
        b.down();
        assert_eq!(b.text, "draft");
        assert_eq!(b.take(), "draft");
        b.up();
        assert_eq!(b.text, "draft", "sent text joins the history");
    }

    #[test]
    fn layout_wraps_and_tracks_the_cursor() {
        let mut b = InputBox::new(vec![]);
        b.insert("abcdefgh\nxy");
        let (lines, cursor) = b.layout(5);
        assert_eq!(lines, vec!["abcde", "fgh", "xy"]);
        assert_eq!(cursor, (2, 2));
        b.up();
        let (_, cursor) = b.layout(5);
        assert_eq!(cursor, (0, 2), "up moves to the same column of the previous logical line");
    }
}
