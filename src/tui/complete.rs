//! Slash commands: the list of them, and completing one from what has been typed.

/// One slash command as it is offered.
pub struct Cmd {
    pub name: &'static str,
    /// What it takes, as a hint shown after the name. Empty if nothing.
    pub args: &'static str,
    pub does: &'static str,
}

const fn cmd(name: &'static str, args: &'static str, does: &'static str) -> Cmd {
    Cmd { name, args, does }
}

/// Every command, in the order they are offered when several fit what was
/// typed: the ones reached for most come first.
pub const COMMANDS: &[Cmd] = &[
    cmd("/goal", "<what to achieve>", "start a goal here"),
    cmd("/goals", "", "list goals"),
    cmd("/continue", "", "pick up where it stopped"),
    cmd("/compact", "", "summarize and start a fresh context"),
    cmd("/cd", "<directory>", "move this session to another directory"),
    cmd("/sessions", "", "pick a session to resume"),
    cmd("/stats", "", "time and tokens in this session"),
    cmd("/trace", "", "the tree of what was done"),
    cmd("/title", "<text>", "rename this session"),
    cmd("/model", "[filter, or provider/model-id]", "choose a model"),
    cmd("/memory", "[query]", "list or search memory"),
    cmd("/new", "", "start a fresh session"),
    cmd("/clear", "", "start a fresh session (the same as /new)"),
    cmd("/cleargoal", "", "clear the side panel: drop unfinished work"),
    cmd("/agents", "", "list subagents"),
    cmd("/auto", "", "run tools without asking"),
    cmd("/ask", "", "ask before running tools"),
    cmd("/back", "", "return from a subagent"),
    cmd("/pause", "", "pause this goal, or stop this subagent"),
    cmd("/provider", "[name]", "list providers, or one provider's models"),
    cmd("/panel", "", "show or hide the side panel"),
    cmd("/resume", "<session id>", "open a session by id"),
    cmd("/remember", "<text>", "save a global memory"),
    cmd("/reload", "", "re-read config.toml"),
    cmd("/forget", "<id>", "delete a memory"),
    cmd("/voice", "", "start or stop voice input"),
    cmd("/logs", "[errors]", "the log, filtered, in a window"),
    cmd("/keys", "", "the key bindings in force"),
    cmd("/help", "", "keys and commands"),
    cmd("/quit", "", "leave; work continues"),
];

/// True while the text is a command name still being typed: it starts with a
/// slash and has no space yet.
pub fn typing_command(text: &str) -> bool {
    text.starts_with('/') && !text.contains(char::is_whitespace)
}

/// The commands that begin with what was typed. One typed in full comes
/// first, so `/goal` offers `/goal` before `/goals`.
pub fn matches(typed: &str) -> Vec<&'static Cmd> {
    if !typing_command(typed) {
        return Vec::new();
    }
    let typed = typed.to_lowercase();
    let mut found: Vec<&'static Cmd> = COMMANDS.iter().filter(|c| c.name.starts_with(&typed)).collect();
    found.sort_by_key(|c| c.name != typed);
    found
}

/// What to show, dimmed, after the cursor: the rest of the first command that
/// fits, or, once a command and a space are typed, what the command takes.
pub fn ghost(text: &str) -> Option<String> {
    if let Some(first) = matches(text).first() {
        return (first.name.len() > text.len()).then(|| first.name[text.len()..].to_string());
    }
    let name = text.strip_suffix(' ')?;
    let known = COMMANDS.iter().find(|c| c.name == name)?;
    (!known.args.is_empty()).then(|| known.args.to_string())
}

/// What the input should hold after completing to `choice`: the name, and a
/// space after it when it takes something and nothing else could be meant.
pub fn completed(choice: &Cmd, only: bool) -> String {
    if only && !choice.args.is_empty() { format!("{} ", choice.name) } else { choice.name.to_string() }
}

/// A one-line list of what fits, for above the input: the first with what it
/// does, the rest by name.
pub fn hint(typed: &str, room: usize) -> Option<String> {
    let found = matches(typed);
    let first = found.first()?;
    let mut out = format!("tab: {} {}", first.name, first.does);
    for other in found.iter().skip(1) {
        let next = format!(" · {}", other.name);
        if out.chars().count() + next.chars().count() + 2 > room {
            out.push_str(" · …");
            break;
        }
        out.push_str(&next);
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(typed: &str) -> Vec<&'static str> {
        matches(typed).iter().map(|c| c.name).collect()
    }

    #[test]
    fn what_was_typed_picks_the_commands_that_fit() {
        assert_eq!(names("/go"), vec!["/goal", "/goals"]);
        assert_eq!(names("/goals"), vec!["/goals"]);
        assert_eq!(names("/goal"), vec!["/goal", "/goals"], "the one typed in full comes first");
        assert_eq!(names("/TR"), vec!["/trace"]);
        assert_eq!(names("/s"), vec!["/sessions", "/stats"]);
        assert_eq!(names("/").len(), COMMANDS.len());
        assert!(names("/zzz").is_empty());
        assert!(names("/goal fix the tests").is_empty(), "past the name there is nothing to complete");
        assert!(names("go").is_empty() && names("").is_empty());
    }

    #[test]
    fn the_ghost_is_the_rest_of_the_command_then_what_it_takes() {
        assert_eq!(ghost("/go").as_deref(), Some("al"));
        assert_eq!(ghost("/tr").as_deref(), Some("ace"));
        assert_eq!(ghost("/goal"), None, "nothing more to add to a full name");
        assert_eq!(ghost("/goal ").as_deref(), Some("<what to achieve>"));
        assert_eq!(ghost("/trace "), None, "it takes nothing");
        assert_eq!(ghost("/goal fix"), None);
        assert_eq!(ghost("/zz"), None);
        assert_eq!(ghost("hello"), None);
    }

    #[test]
    fn completing_adds_a_space_only_when_the_choice_is_settled() {
        let goal = matches("/go")[0];
        assert_eq!(completed(goal, false), "/goal", "/goals may still be meant");
        let title = matches("/ti")[0];
        assert_eq!(completed(title, true), "/title ");
        let trace = matches("/tr")[0];
        assert_eq!(completed(trace, true), "/trace");
        assert_eq!(hint("/go", 80).as_deref(), Some("tab: /goal start a goal here · /goals"));
        assert!(hint("/", 40).unwrap().ends_with(" · …"));
        assert_eq!(hint("/zz", 80), None);
    }

    #[test]
    fn names_are_unique_lowercase_and_slashed() {
        for (i, c) in COMMANDS.iter().enumerate() {
            assert!(c.name.starts_with('/') && c.name == c.name.to_lowercase() && !c.does.is_empty(), "{}", c.name);
            assert!(!COMMANDS[..i].iter().any(|o| o.name == c.name), "{} is listed twice", c.name);
        }
    }
}
