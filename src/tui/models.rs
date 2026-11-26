//! Choosing a model: the state behind the `/model` and `/provider` pickers.
//!
//! One list serves three purposes, switched with tab: the model of the session
//! on screen, the default for new sessions, and the model subagents use.

use crate::config::split_model_ref;
use crate::proto::ProviderModels;

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Target {
    Session,
    Default,
    Subagent,
}

impl Target {
    pub fn label(self) -> &'static str {
        match self {
            Target::Session => "this session",
            Target::Default => "new sessions",
            Target::Subagent => "subagents",
        }
    }
}

/// Shown in the subagent list for "no model of their own".
const SAME: &str = "(the same model as the session)";

pub struct Picker {
    pub all: Vec<ProviderModels>,
    /// Show only this provider's models.
    pub only: Option<String>,
    pub filter: String,
    pub sel: usize,
    pub target: Target,
    /// What each target uses now, as `provider/model-id`.
    pub session: String,
    pub default: String,
    pub subagent: String,
}

impl Picker {
    pub fn new(all: Vec<ProviderModels>, session: String, default: String, subagent: String) -> Picker {
        let mut p =
            Picker { all, only: None, filter: String::new(), sel: 0, target: Target::Session, session, default, subagent };
        p.seek_current();
        p
    }

    /// The model the current target uses now.
    fn current(&self) -> &str {
        match self.target {
            Target::Session => &self.session,
            Target::Default => &self.default,
            Target::Subagent => &self.subagent,
        }
    }

    /// The selectable models, as `provider/model-id`, after the filter. The
    /// provider in use comes first. For subagents the first entry is the empty
    /// string: no model of their own.
    pub fn items(&self) -> Vec<String> {
        let terms: Vec<String> = self.filter.to_lowercase().split_whitespace().map(str::to_string).collect();
        let matches = |text: &str| {
            let text = text.to_lowercase();
            terms.iter().all(|t| text.contains(t))
        };
        let lead = split_model_ref(if self.current().is_empty() { &self.session } else { self.current() }).0;
        let mut providers: Vec<&ProviderModels> = self.all.iter().collect();
        providers.sort_by_key(|p| p.name != lead);
        let mut out = Vec::new();
        if self.target == Target::Subagent && self.only.is_none() && matches(SAME) {
            out.push(String::new());
        }
        for p in providers {
            if self.only.as_ref().is_some_and(|o| *o != p.name) {
                continue;
            }
            out.extend(p.models.iter().map(|m| format!("{}/{m}", p.name)).filter(|full| matches(full)));
        }
        out
    }

    pub fn selected(&self) -> Option<String> {
        self.items().get(self.sel).cloned()
    }

    /// Put the cursor on the model in use, if it is listed.
    pub fn seek_current(&mut self) {
        let current = self.current().to_string();
        self.sel = self.items().iter().position(|m| *m == current).unwrap_or(0);
    }

    pub fn move_by(&mut self, delta: i64) {
        let last = self.items().len().saturating_sub(1) as i64;
        self.sel = (self.sel as i64 + delta).clamp(0, last) as usize;
    }

    pub fn cycle_target(&mut self, forward: bool) {
        self.target = match (self.target, forward) {
            (Target::Session, true) | (Target::Subagent, false) => Target::Default,
            (Target::Default, true) | (Target::Session, false) => Target::Subagent,
            (Target::Subagent, true) | (Target::Default, false) => Target::Session,
        };
        self.seek_current();
    }

    pub fn type_char(&mut self, c: char) {
        self.filter.push(c);
        self.sel = 0;
    }

    pub fn erase(&mut self) {
        self.filter.pop();
        self.sel = 0;
    }

    pub fn title(&self) -> String {
        let mut t = format!("models for {}", self.target.label());
        if let Some(only) = &self.only {
            t.push_str(&format!(" · {only}"));
        }
        if !self.filter.is_empty() {
            t.push_str(&format!(" · filter: {}", self.filter));
        }
        t
    }

    /// The list as text, with the row to highlight. Rows after the models
    /// explain a provider that could not be listed; they cannot be selected.
    pub fn lines(&self) -> (Vec<String>, Option<usize>) {
        let items = self.items();
        let current = self.current();
        let mut lines: Vec<String> = items
            .iter()
            .map(|m| {
                let mut used = Vec::new();
                for (who, model) in [("session", &self.session), ("default", &self.default), ("subagents", &self.subagent)] {
                    if model == m && !(m.is_empty() && who != "subagents") {
                        used.push(who);
                    }
                }
                let mark = if m == current { "●" } else { " " };
                let name = if m.is_empty() { SAME } else { m.as_str() };
                let tags = if used.is_empty() { String::new() } else { format!("   ← {}", used.join(", ")) };
                format!("{mark} {name}{tags}")
            })
            .collect();
        let sel = if items.is_empty() { None } else { Some(self.sel.min(items.len() - 1)) };
        if items.is_empty() && !self.filter.is_empty() {
            lines.push(format!("  nothing matches \"{}\" (esc clears the filter)", self.filter));
        }
        for p in &self.all {
            let shown = self.filter.is_empty() && self.only.as_ref().is_none_or(|o| *o == p.name);
            if shown && !p.error.is_empty() {
                lines.push(format!("  {}: could not list models: {}", p.name, p.error));
            }
        }
        if self.all.is_empty() {
            lines.push("  no providers are configured yet; run `narsil setup`".into());
        }
        (lines, sel)
    }

    /// One row per provider, for the `/provider` list.
    pub fn provider_lines(&self) -> Vec<String> {
        let in_use = split_model_ref(&self.session).0;
        let width = self.all.iter().map(|p| p.name.chars().count()).max().unwrap_or(4).clamp(4, 20);
        self.all
            .iter()
            .map(|p| {
                let status = if p.error.is_empty() {
                    format!("{} model{}", p.models.len(), if p.models.len() == 1 { "" } else { "s" })
                } else {
                    format!("unavailable: {}", p.error)
                };
                let mark = if p.name == in_use { "●" } else { " " };
                format!("{mark} {:<width$}  {:<9}  {status}   {}", p.name, p.kind, p.base_url)
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn picker() -> Picker {
        let provider = |name: &str, models: &[&str], error: &str| ProviderModels {
            name: name.into(),
            kind: "openai".into(),
            base_url: String::new(),
            models: models.iter().map(|m| m.to_string()).collect(),
            error: error.into(),
        };
        Picker::new(
            vec![
                provider("anthropic", &["claude-opus-5-5", "claude-haiku-5-5"], ""),
                provider("local", &["qwen3.8-27b"], ""),
                provider("openrouter", &[], "no API key"),
            ],
            "local/qwen3.8-27b".into(),
            "anthropic/claude-opus-5-5".into(),
            String::new(),
        )
    }

    #[test]
    fn the_list_starts_on_the_model_in_use_with_its_provider_first() {
        let p = picker();
        assert_eq!(p.items(), vec!["local/qwen3.8-27b", "anthropic/claude-opus-5-5", "anthropic/claude-haiku-5-5"]);
        assert_eq!(p.selected().as_deref(), Some("local/qwen3.8-27b"));
        let (lines, sel) = p.lines();
        assert_eq!(sel, Some(0));
        assert_eq!(lines[0], "● local/qwen3.8-27b   ← session");
        assert_eq!(lines[1], "  anthropic/claude-opus-5-5   ← default");
        assert_eq!(lines[3], "  openrouter: could not list models: no API key");
    }

    #[test]
    fn typing_filters_and_tab_switches_what_is_being_chosen() {
        let mut p = picker();
        for c in "HAIKU 5".chars() {
            p.type_char(c);
        }
        assert_eq!(p.items(), vec!["anthropic/claude-haiku-5-5"]);
        assert_eq!(p.title(), "models for this session · filter: HAIKU 5");
        p.type_char('x');
        assert!(p.selected().is_none());
        assert!(p.lines().0[0].contains("nothing matches"));
        for _ in 0..9 {
            p.erase();
        }
        p.cycle_target(true);
        assert_eq!(p.target, Target::Default);
        assert_eq!(p.selected().as_deref(), Some("anthropic/claude-opus-5-5"), "the cursor follows the target's model");
        p.cycle_target(true);
        assert_eq!(p.target, Target::Subagent);
        assert_eq!(p.selected().as_deref(), Some(""), "subagents can have no model of their own");
        assert_eq!(p.lines().0[0], format!("● {SAME}   ← subagents"));
        p.move_by(99);
        assert_eq!(p.sel, 3);
        p.cycle_target(true);
        assert_eq!(p.target, Target::Session);
        p.cycle_target(false);
        assert_eq!(p.target, Target::Subagent);
    }

    #[test]
    fn one_provider_can_be_shown_alone() {
        let mut p = picker();
        p.only = Some("anthropic".into());
        p.seek_current();
        assert_eq!(p.items(), vec!["anthropic/claude-opus-5-5", "anthropic/claude-haiku-5-5"]);
        assert_eq!(p.title(), "models for this session · anthropic");
        assert_eq!(p.lines().0.len(), 2, "another provider's error is not shown here");
        let rows = p.provider_lines();
        assert!(rows[0].starts_with("  anthropic") && rows[0].contains("2 models"));
        assert!(rows[1].starts_with("● local") && rows[1].contains("1 model "));
        assert!(rows[2].contains("unavailable: no API key"));
    }
}
