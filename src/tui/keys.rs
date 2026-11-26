//! Key bindings: the actions that can be rebound in `[keys]`, how a binding is
//! written (`ctrl+t`, `alt+right`, `f1`), and the list shown in the keys window.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::config::KeysConfig;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Action {
    NewTab,
    CloseTab,
    NextTab,
    PrevTab,
    GotoTab,
    Keys,
    Interrupt,
    Panel,
    ExpandTools,
    Thinking,
    Voice,
    Quit,
    ScrollUp,
    ScrollDown,
}

/// One key with its modifiers.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Chord {
    mods: KeyModifiers,
    code: KeyCode,
}

/// Read a chord as written in the config: modifiers and a key joined by `+`.
pub fn parse_chord(text: &str) -> Result<Chord, String> {
    let lower = text.trim().to_lowercase();
    let mut parts: Vec<&str> = lower.split('+').map(str::trim).collect();
    // "ctrl++" means ctrl and the plus key.
    let key = if lower.ends_with("++") || lower == "+" { "+" } else { parts.pop().unwrap_or("") };
    let mut mods = KeyModifiers::NONE;
    for m in parts.iter().filter(|m| !m.is_empty()) {
        mods |= match *m {
            "ctrl" | "control" => KeyModifiers::CONTROL,
            "alt" | "meta" | "option" => KeyModifiers::ALT,
            "shift" => KeyModifiers::SHIFT,
            other => return Err(format!("{text:?}: {other:?} is not a modifier (use ctrl, alt or shift)")),
        };
    }
    let code = match key {
        "enter" | "return" => KeyCode::Enter,
        "esc" | "escape" => KeyCode::Esc,
        "tab" => KeyCode::Tab,
        "space" => KeyCode::Char(' '),
        "up" => KeyCode::Up,
        "down" => KeyCode::Down,
        "left" => KeyCode::Left,
        "right" => KeyCode::Right,
        "pageup" | "pgup" => KeyCode::PageUp,
        "pagedown" | "pgdn" | "pgdown" => KeyCode::PageDown,
        "home" => KeyCode::Home,
        "end" => KeyCode::End,
        "backspace" => KeyCode::Backspace,
        "delete" | "del" => KeyCode::Delete,
        "insert" | "ins" => KeyCode::Insert,
        f if f.len() >= 2 && f.starts_with('f') && f[1..].parse::<u8>().is_ok_and(|n| (1..=12).contains(&n)) => {
            KeyCode::F(f[1..].parse().unwrap_or(1))
        }
        one if one.chars().count() == 1 => KeyCode::Char(one.chars().next().unwrap_or(' ')),
        other => return Err(format!("{text:?}: {other:?} is not a key")),
    };
    // Terminals report shift+tab as its own key.
    if code == KeyCode::Tab && mods.contains(KeyModifiers::SHIFT) {
        return Ok(Chord { mods: mods - KeyModifiers::SHIFT, code: KeyCode::BackTab });
    }
    Ok(Chord { mods, code })
}

impl Chord {
    fn matches(&self, key: &KeyEvent) -> bool {
        let relevant = KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SHIFT;
        let mut pressed = key.modifiers & relevant;
        let code = match key.code {
            // A letter arrives with its case; shift is already in it.
            KeyCode::Char(c) => {
                pressed -= KeyModifiers::SHIFT;
                KeyCode::Char(c.to_ascii_lowercase())
            }
            KeyCode::BackTab => {
                pressed -= KeyModifiers::SHIFT;
                KeyCode::BackTab
            }
            other => other,
        };
        let mut wanted = self.mods;
        if matches!(self.code, KeyCode::Char(_)) {
            wanted -= KeyModifiers::SHIFT;
        }
        code == self.code && pressed == wanted
    }

    /// The chord as a person would write it.
    pub fn label(&self) -> String {
        let mut out = String::new();
        for (m, name) in [(KeyModifiers::CONTROL, "ctrl"), (KeyModifiers::ALT, "alt"), (KeyModifiers::SHIFT, "shift")] {
            if self.mods.contains(m) {
                out.push_str(name);
                out.push('+');
            }
        }
        out.push_str(&match self.code {
            KeyCode::Enter => "enter".to_string(),
            KeyCode::Esc => "esc".to_string(),
            KeyCode::Tab => "tab".to_string(),
            KeyCode::BackTab => "shift+tab".to_string(),
            KeyCode::Char(' ') => "space".to_string(),
            KeyCode::Char(c) => c.to_string(),
            KeyCode::F(n) => format!("f{n}"),
            KeyCode::PageUp => "pageup".to_string(),
            KeyCode::PageDown => "pagedown".to_string(),
            other => format!("{other:?}").to_lowercase(),
        });
        out
    }
}

/// The bindings in force: what `[keys]` says, with the defaults for anything it leaves out or gets wrong.
pub struct KeyMap {
    binds: Vec<(Action, Vec<Chord>)>,
    /// Held with a digit to go straight to that tab, where the terminal lets it through.
    goto_tab_with: KeyModifiers,
    /// What in the config could not be used, to tell the user.
    pub problems: Vec<String>,
}

/// Every rebindable action: its name in `[keys]`, what it does, and the group it is listed under.
const ACTIONS: [(Action, &str, &str, &str); 14] = [
    (Action::NewTab, "new_tab", "open a new tab with a fresh session", "Tabs"),
    (Action::CloseTab, "close_tab", "close this tab (its work carries on)", "Tabs"),
    (Action::NextTab, "next_tab", "next tab", "Tabs"),
    (Action::PrevTab, "prev_tab", "previous tab", "Tabs"),
    (Action::GotoTab, "goto_tab", "then 1 … 9: go to that tab (or ←/→, t new, w close)", "Tabs"),
    (Action::Keys, "keys", "show this list", "Tabs"),
    (Action::Interrupt, "interrupt", "interrupt the agent; from a subagent, go back", "Session"),
    (Action::Panel, "panel", "complete a command, or focus the side panel", "Session"),
    (Action::ExpandTools, "expand_tools", "expand or fold tool output", "Session"),
    (Action::Thinking, "thinking", "show or hide the model's thinking", "Session"),
    (Action::Voice, "voice", "start or stop voice input", "Session"),
    (Action::ScrollUp, "scroll_up", "scroll the conversation up", "Session"),
    (Action::ScrollDown, "scroll_down", "scroll the conversation down", "Session"),
    (Action::Quit, "quit", "leave narsil (work carries on)", "Session"),
];

impl KeyMap {
    pub fn from_config(cfg: &KeysConfig) -> KeyMap {
        let defaults = KeysConfig::default();
        let mut problems = Vec::new();
        let mut binds = Vec::new();
        for (action, name, _, _) in ACTIONS {
            let read = |c: &KeysConfig| -> Result<Vec<Chord>, String> {
                c.get(name).split([' ', ',']).filter(|s| !s.trim().is_empty()).map(parse_chord).collect()
            };
            let chords = match read(cfg) {
                Ok(chords) => chords,
                Err(e) => {
                    problems.push(format!("[keys] {name}: {e}; using the default"));
                    read(&defaults).unwrap_or_default()
                }
            };
            binds.push((action, chords));
        }
        // Two actions on one key: the first listed wins, and the user should know.
        for (i, (_, chords)) in binds.iter().enumerate() {
            for chord in chords {
                if let Some((_, other, _, _)) = ACTIONS.iter().take(i).zip(&binds).find(|(_, (_, c))| c.contains(chord)).map(|(a, _)| a) {
                    problems.push(format!("[keys] {} is bound to both {other} and {}; it does {other}", chord.label(), ACTIONS[i].1));
                }
            }
        }
        let goto_tab_with = match cfg.goto_tab_with.trim().to_lowercase().as_str() {
            "ctrl" | "control" => KeyModifiers::CONTROL,
            "alt" | "meta" | "option" => KeyModifiers::ALT,
            "" | "none" | "off" => KeyModifiers::NONE,
            other => {
                problems.push(format!("[keys] goto_tab_with: {other:?} is not alt, ctrl or none; it is off"));
                KeyModifiers::NONE
            }
        };
        KeyMap { binds, goto_tab_with, problems }
    }

    pub fn action(&self, key: &KeyEvent) -> Option<Action> {
        self.binds.iter().find(|(_, chords)| chords.iter().any(|c| c.matches(key))).map(|(a, _)| *a)
    }

    /// The tab a key asks for (0 for the first), when it is the tab modifier with a digit.
    pub fn tab_number(&self, key: &KeyEvent) -> Option<usize> {
        if self.goto_tab_with.is_empty() || key.modifiers & (KeyModifiers::CONTROL | KeyModifiers::ALT) != self.goto_tab_with {
            return None;
        }
        match key.code {
            KeyCode::Char(c @ '1'..='9') => Some(c as usize - '1' as usize),
            _ => None,
        }
    }

    /// How an action is bound, for hints: `ctrl+t`, or `f1 / alt+k`.
    pub fn label(&self, action: Action) -> String {
        let chords = self.binds.iter().find(|(a, _)| *a == action).map(|(_, c)| c.as_slice()).unwrap_or(&[]);
        if chords.is_empty() { "unbound".to_string() } else { chords.iter().map(Chord::label).collect::<Vec<_>>().join(" / ") }
    }

    /// The whole list for the keys window: what can be rebound, as it is bound
    /// now, then the keys that are fixed.
    pub fn listing(&self) -> Vec<String> {
        let mut out = Vec::new();
        let mut group = "";
        for (action, name, does, section) in ACTIONS {
            if section != group {
                if !out.is_empty() {
                    out.push(String::new());
                }
                out.push(section.to_string());
                group = section;
                if section == "Tabs" && !self.goto_tab_with.is_empty() {
                    let m = if self.goto_tab_with == KeyModifiers::CONTROL { "ctrl" } else { "alt" };
                    out.push(format!("  {:<22} {:<52} {}", format!("{m}+1 … {m}+9"), "go to that tab", "goto_tab_with"));
                }
            }
            out.push(format!("  {:<22} {:<52} {name}", self.label(action), does));
        }
        out.push(String::new());
        out.push("Typing (fixed)".to_string());
        for (keys, does) in [
            ("enter", "send; while the agent works, queue for its next step"),
            ("alt+enter / ctrl+j", "new line"),
            ("up / down", "earlier and later messages you sent"),
            ("ctrl+left / ctrl+right", "back and forward a word"),
            ("ctrl+a / ctrl+e", "start and end of the line"),
            ("ctrl+w / ctrl+u / ctrl+k", "delete a word, to the start, to the end"),
            ("right, at the end", "take the suggested command"),
            ("ctrl+c", "clear the input; again to interrupt; again to quit"),
        ] {
            out.push(format!("  {keys:<26} {does}"));
        }
        out.push(String::new());
        out.push("In the side panel: up/down move, enter opens a subagent, esc returns to the latest messages.".to_string());
        out.push("When asked to approve: y allow, a always allow that tool, n deny.".to_string());
        out.push(String::new());
        out.push("The right-hand column is the name to use under [keys] in config.toml to rebind an action,".to_string());
        out.push("for example  new_tab = \"ctrl+n\".  Several keys for one action are separated by spaces.".to_string());
        for p in &self.problems {
            out.push(format!("! {p}"));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode, mods: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, mods)
    }

    #[test]
    fn chords_are_read_as_written_and_shown_the_same_way() {
        for (text, label) in [("ctrl+t", "ctrl+t"), ("Alt+Right", "alt+right"), ("f1", "f1"), ("shift+tab", "shift+tab"), ("ESC", "esc"), ("ctrl+alt+k", "ctrl+alt+k"), ("pgup", "pageup")] {
            assert_eq!(parse_chord(text).unwrap().label(), label, "{text}");
        }
        assert!(parse_chord("hyper+t").unwrap_err().contains("not a modifier"));
        assert!(parse_chord("ctrl+banana").unwrap_err().contains("not a key"));
        assert!(parse_chord("f13").is_err());
    }

    #[test]
    fn the_default_bindings_do_what_the_keys_window_says() {
        let map = KeyMap::from_config(&KeysConfig::default());
        assert!(map.problems.is_empty(), "{:?}", map.problems);
        assert_eq!(map.action(&key(KeyCode::Char('t'), KeyModifiers::CONTROL)), Some(Action::NewTab));
        assert_eq!(map.action(&key(KeyCode::Char('T'), KeyModifiers::CONTROL | KeyModifiers::SHIFT)), Some(Action::NewTab));
        assert_eq!(map.action(&key(KeyCode::Right, KeyModifiers::ALT)), Some(Action::NextTab));
        assert_eq!(map.action(&key(KeyCode::Right, KeyModifiers::CONTROL)), None, "word movement is left alone");
        assert_eq!(map.action(&key(KeyCode::F(1), KeyModifiers::NONE)), Some(Action::Keys));
        assert_eq!(map.action(&key(KeyCode::Char('k'), KeyModifiers::ALT)), Some(Action::Keys));
        assert_eq!(map.action(&key(KeyCode::Tab, KeyModifiers::NONE)), Some(Action::Panel));
        assert_eq!(map.action(&key(KeyCode::Esc, KeyModifiers::NONE)), Some(Action::Interrupt));
        assert_eq!(map.action(&key(KeyCode::Char('t'), KeyModifiers::NONE)), None, "plain typing is never an action");
        assert_eq!(map.action(&key(KeyCode::Char('g'), KeyModifiers::CONTROL)), Some(Action::GotoTab));
        assert_eq!(map.tab_number(&key(KeyCode::Char('3'), KeyModifiers::ALT)), None, "alt+digit belongs to the terminal unless asked for");
        assert_eq!(map.tab_number(&key(KeyCode::Char('3'), KeyModifiers::NONE)), None);
        assert_eq!(map.label(Action::Keys), "f1 / alt+k");
        let listing = map.listing().join("\n");
        assert!(!listing.contains("alt+1"), "a key the terminal may keep is not listed unless it was asked for");
        for expected in ["ctrl+t", "new_tab", "ctrl+g", "then 1 … 9", "Typing (fixed)"] {
            assert!(listing.contains(expected), "{expected}");
        }
    }

    #[test]
    fn the_config_rebinds_and_mistakes_fall_back_with_a_warning() {
        let mut cfg = KeysConfig::default();
        cfg.new_tab = "ctrl+n".into();
        cfg.close_tab = "ctrl+banana".into();
        cfg.thinking = "ctrl+n".into();
        cfg.goto_tab_with = "ctrl".into();
        let map = KeyMap::from_config(&cfg);
        assert_eq!(map.action(&key(KeyCode::Char('n'), KeyModifiers::CONTROL)), Some(Action::NewTab));
        assert_eq!(map.action(&key(KeyCode::Char('t'), KeyModifiers::CONTROL)), None);
        assert_eq!(map.action(&key(KeyCode::Char('w'), KeyModifiers::ALT)), Some(Action::CloseTab), "the default stands in for a binding that cannot be read");
        assert_eq!(map.tab_number(&key(KeyCode::Char('2'), KeyModifiers::CONTROL)), Some(1));
        assert!(map.listing().join("\n").contains("ctrl+1 … ctrl+9"));
        assert_eq!(map.problems.len(), 2, "{:?}", map.problems);
        assert!(map.problems[0].contains("close_tab") && map.problems[1].contains("bound to both new_tab and thinking"), "{:?}", map.problems);
    }
}
