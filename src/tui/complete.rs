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
    cmd("/budget", "", "how long a goal with no finish line may work"),
    cmd("/usage", "", "tokens used: by request, by model, and what a call carries"),
    cmd("/trace", "", "the tree of what was done"),
    cmd("/title", "<text>", "rename this session"),
    cmd("/theme", "[name]", "choose a colour theme"),
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
    cmd("/files", "", "show or hide the file explorer"),
    cmd("/daemonrestart", "", "stop and start the daemon, and reconnect"),
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

/// What has been typed of a directory after `/cd `, if that is what is being typed.
pub fn cd_typed(text: &str) -> Option<&str> {
    text.strip_prefix("/cd ").filter(|rest| !rest.contains('\n'))
}

/// The directories that fit what has been typed after `/cd `, each as the
/// whole of what would stand there, ending in `/` so that the next tab goes
/// on inside it. `base` is where a relative path starts from. Hidden folders
/// are offered only once a dot has been typed.
pub fn directories(typed: &str, base: &std::path::Path) -> Vec<String> {
    // `Code/na` is the folder `Code/` and a name starting `na`.
    let (head, prefix) = match typed.rfind('/') {
        Some(at) => typed.split_at(at + 1),
        None => ("", typed),
    };
    let folder = match head {
        "" => base.to_path_buf(),
        _ => {
            let written = crate::util::expand_tilde(head);
            if written.is_absolute() { written } else { base.join(written) }
        }
    };
    let Ok(entries) = std::fs::read_dir(&folder) else { return Vec::new() };
    let mut names: Vec<String> = entries
        .flatten()
        // A link to a folder is a folder to go into.
        .filter(|e| std::fs::metadata(e.path()).is_ok_and(|m| m.is_dir()))
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|name| prefix.starts_with('.') || !name.starts_with('.'))
        .collect();
    // As typed; and only if nothing fits that way, without regard to capitals.
    let exact: Vec<String> = names.iter().filter(|n| n.starts_with(prefix)).cloned().collect();
    if exact.is_empty() {
        let lower = prefix.to_lowercase();
        names.retain(|n| n.to_lowercase().starts_with(&lower));
    } else {
        names = exact;
    }
    names.sort_by_key(|n| n.to_lowercase());
    names.truncate(200);
    names.into_iter().map(|name| format!("{head}{name}/")).collect()
}

/// What to show, dimmed, after a directory being typed: the rest of the first that fits.
pub fn cd_ghost(typed: &str, base: &std::path::Path) -> Option<String> {
    if typed.is_empty() {
        return None;
    }
    let first = directories(typed, base).into_iter().next()?;
    // Only when it continues what is there: a match found without regard to capitals does not.
    first.strip_prefix(typed).filter(|rest| !rest.is_empty()).map(str::to_string)
}

/// A one-line list of the directories that fit, for above the input.
pub fn cd_hint(typed: &str, base: &std::path::Path, room: usize) -> Option<String> {
    let found = directories(typed, base);
    let name = |full: &str| full.trim_end_matches('/').rsplit('/').next().unwrap_or(full).to_string() + "/";
    let mut out = format!("tab: {}", name(found.first()?));
    for other in found.iter().skip(1) {
        let next = format!(" · {}", name(other));
        if out.chars().count() + next.chars().count() + 2 > room {
            out.push_str(" · …");
            break;
        }
        out.push_str(&next);
    }
    Some(out)
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
    fn directories_complete_after_cd() {
        let base = std::env::temp_dir().join(format!("narsil-cd-test-{}", std::process::id()));
        for dir in ["Code/narsil/src", "Code/notes", "Documents", "Downloads", ".config/narsil", "code-old"] {
            std::fs::create_dir_all(base.join(dir)).unwrap();
        }
        std::fs::write(base.join("Dockerfile"), "x").unwrap();
        std::os::unix::fs::symlink(base.join("Code"), base.join("Linked")).unwrap();

        assert_eq!(cd_typed("/cd Co"), Some("Co"));
        assert_eq!(cd_typed("/cd "), Some(""));
        assert_eq!((cd_typed("/cd"), cd_typed("/cdx y"), cd_typed("cd x")), (None, None, None));

        // Folders only, as typed, in order; a file of a fitting name is not one, a link to a folder is.
        assert_eq!(directories("Do", &base), vec!["Documents/", "Downloads/"]);
        assert_eq!(directories("", &base), vec!["Code/", "code-old/", "Documents/", "Downloads/", "Linked/"], "hidden folders wait for a dot");
        assert_eq!(directories(".", &base), vec![".config/"]);
        // Inside a folder, and inside that.
        assert_eq!(directories("Code/", &base), vec!["Code/narsil/", "Code/notes/"]);
        assert_eq!(directories("Code/na", &base), vec!["Code/narsil/"]);
        assert_eq!(directories("Code/narsil/", &base), vec!["Code/narsil/src/"]);
        assert_eq!(directories("Linked/no", &base), vec!["Linked/notes/"]);
        // From the top, from above, and from home.
        assert_eq!(directories(&format!("{}/Doc", base.display()), &base), vec![format!("{}/Documents/", base.display())]);
        assert_eq!(directories("../Do", &base.join("Code")), vec!["../Documents/", "../Downloads/"]);
        assert!(directories("~/", &base).iter().all(|d| d.starts_with("~/") && d.ends_with('/')));
        // Capitals matter when something fits as typed, and not when nothing does.
        assert_eq!(directories("co", &base), vec!["code-old/"]);
        assert_eq!(directories("doc", &base), vec!["Documents/"]);
        assert!(directories("zzz", &base).is_empty() && directories("Nowhere/x", &base).is_empty());

        assert_eq!(cd_ghost("Doc", &base).as_deref(), Some("uments/"));
        assert_eq!(cd_ghost("Code/na", &base).as_deref(), Some("rsil/"));
        assert_eq!(cd_ghost("doc", &base), None, "what was found without regard to capitals does not continue what was typed");
        assert_eq!((cd_ghost("", &base), cd_ghost("zzz", &base), cd_ghost("Documents/", &base)), (None, None, None));
        assert_eq!(cd_hint("Do", &base, 80).as_deref(), Some("tab: Documents/ · Downloads/"));
        assert_eq!(cd_hint("Code/", &base, 80).as_deref(), Some("tab: narsil/ · notes/"), "by name, not by the whole path");
        assert!(cd_hint("", &base, 24).unwrap().ends_with(" · …"));
        assert_eq!(cd_hint("zzz", &base, 80), None);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn names_are_unique_lowercase_and_slashed() {
        for (i, c) in COMMANDS.iter().enumerate() {
            assert!(c.name.starts_with('/') && c.name == c.name.to_lowercase() && !c.does.is_empty(), "{}", c.name);
            assert!(!COMMANDS[..i].iter().any(|o| o.name == c.name), "{} is listed twice", c.name);
        }
    }
}
