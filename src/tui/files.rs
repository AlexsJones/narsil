//! The file explorer: what is in the directory a session works in, and what
//! narsil has done there. It is for the person watching: which files the
//! agent has read, which it made or changed, which it is on right now, and
//! what it has touched outside this directory altogether.
//!
//! Two things are known. What the file tools did is in the journal, call by
//! call. What a shell command did is not, so anything changed on disk since
//! the session began is marked too, more quietly.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde_json::Value;

/// What has happened to a file, the most telling first.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Mark {
    /// Nothing that narsil did.
    None,
    /// Read.
    Read,
    /// Changed on disk since the session began, by something other than a file tool: a command, most likely.
    Disk,
    /// Changed by a file tool.
    Changed,
    /// Made by a file tool.
    Created,
    /// Being read or written this moment.
    Now,
}

impl Mark {
    pub fn glyph(self) -> &'static str {
        match self {
            Mark::None => " ",
            Mark::Read => "○",
            Mark::Disk | Mark::Changed => "~",
            Mark::Created => "+",
            Mark::Now => "▶",
        }
    }
}

/// One call of a file tool, from the journal.
#[derive(Clone, Debug)]
pub struct Call {
    pub name: String,
    pub input: Value,
    /// pending, running or done.
    pub status: String,
    pub is_error: bool,
    /// How the result begins.
    pub result: String,
    /// The message that asked for it.
    pub message_id: i64,
    pub session: String,
    /// The directory of the session that made it, as it is now.
    pub cwd: String,
    /// The file as it resolved when the call ran. Calls from before this was
    /// recorded have none, and are placed by where the session is now.
    pub resolved: Option<String>,
}

/// What the file tools have done to one file.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Touch {
    pub mark: Mark,
    /// Where in the conversation on screen it was last touched, if it was touched there.
    pub anchor: Option<i64>,
}

/// What each file has had done to it, from the calls of a session and its
/// subagents. `shown` is the session whose conversation is on screen: only
/// its calls can be jumped to.
pub fn touches(calls: &[Call], shown: &str) -> HashMap<PathBuf, Touch> {
    let mut out: HashMap<PathBuf, Touch> = HashMap::new();
    for call in calls {
        let Some(path) = call.input.get("path").and_then(Value::as_str) else { continue };
        let path = match &call.resolved {
            Some(resolved) => PathBuf::from(resolved),
            None => {
                // The session may have moved since: a guess that names no file would put a phantom in the tree.
                let guess = crate::obsidian::lexical(&crate::tools::resolve(&call.cwd, path));
                if !Path::new(path).is_absolute() && !guess.exists() {
                    continue;
                }
                guess
            }
        };
        let done = call.status == "done";
        let mark = match (call.name.as_str(), done) {
            // Still pending or running: it is on this file now.
            ("read_file" | "write_file" | "edit_file", false) => Mark::Now,
            (_, true) if call.is_error => continue,
            ("read_file", true) => Mark::Read,
            ("write_file", true) if call.result.starts_with("Created") => Mark::Created,
            ("write_file" | "edit_file", true) => Mark::Changed,
            _ => continue,
        };
        let entry = out.entry(path).or_insert(Touch { mark: Mark::None, anchor: None });
        // A file made and then edited is still a file narsil made; one being worked on now says so.
        entry.mark = match (entry.mark, mark) {
            (_, Mark::Now) => Mark::Now,
            (Mark::Now, later) => later.max(Mark::Read),
            (before, later) => before.max(later),
        };
        if call.session == shown {
            entry.anchor = Some(call.message_id);
        }
    }
    out
}

/// One line of the explorer.
#[derive(Clone, Debug, PartialEq)]
pub struct Row {
    pub path: PathBuf,
    /// What to show: a name in the tree, a whole path for a file elsewhere.
    pub name: String,
    pub depth: usize,
    pub dir: bool,
    /// For a directory: whether its contents are shown.
    pub open: bool,
    /// For a file, what happened to it; for a directory, the most telling thing that happened inside it.
    pub mark: Mark,
    pub anchor: Option<i64>,
    /// Outside the directory being shown.
    pub elsewhere: bool,
}

/// What the person has opened and shut by hand, over what opens by itself.
#[derive(Default)]
pub struct Folds {
    pub opened: HashSet<PathBuf>,
    pub shut: HashSet<PathBuf>,
}

/// A directory's entries: folders first, then files, each by name. Honours
/// `.gitignore`, so build output and dependencies do not bury the source,
/// and never shows `.git`.
fn entries(dir: &Path) -> Vec<(PathBuf, bool)> {
    let mut found: Vec<(PathBuf, bool)> = ignore::WalkBuilder::new(dir)
        .max_depth(Some(1))
        .hidden(false)
        .filter_entry(|e| e.file_name() != ".git")
        .build()
        .flatten()
        .filter(|e| e.depth() == 1)
        .map(|e| {
            let is_dir = std::fs::metadata(e.path()).is_ok_and(|m| m.is_dir());
            (e.into_path(), is_dir)
        })
        .take(400)
        .collect();
    found.sort_by_key(|(path, is_dir)| (!is_dir, path.file_name().map(|n| n.to_string_lossy().to_lowercase()).unwrap_or_default()));
    found
}

/// Files under `root` changed on disk since `since`, as far as a quick look
/// goes. This is how what a shell command wrote shows up at all.
pub fn changed_on_disk(root: &Path, since: SystemTime) -> HashSet<PathBuf> {
    let mut out = HashSet::new();
    let walk = ignore::WalkBuilder::new(root).hidden(false).filter_entry(|e| e.file_name() != ".git").build();
    for entry in walk.flatten().take(20_000) {
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        if entry.metadata().ok().and_then(|m| m.modified().ok()).is_some_and(|at| at >= since) {
            out.insert(entry.into_path());
            // Past this it is a build or an install, and a list of it helps nobody.
            if out.len() >= 300 {
                break;
            }
        }
    }
    out
}

/// The explorer's lines: the tree under `root`, opened wherever narsil has
/// been, then what it has touched elsewhere.
pub fn rows(root: &Path, touched: &HashMap<PathBuf, Touch>, on_disk: &HashSet<PathBuf>, folds: &Folds) -> Vec<Row> {
    // What happened to each file here, and through it to each folder above it.
    let mut marks: HashMap<PathBuf, Mark> = HashMap::new();
    for (path, touch) in touched.iter().filter(|(p, _)| p.starts_with(root)) {
        marks.insert(path.clone(), touch.mark);
    }
    // Many files changed on disk at once is a build or an install: marked, but not all opened up.
    let quiet = on_disk.len() > 40;
    for path in on_disk {
        marks.entry(path.clone()).or_insert(Mark::Disk);
    }
    let mut inside: HashMap<PathBuf, Mark> = HashMap::new();
    let mut opens: HashSet<PathBuf> = HashSet::new();
    for (path, mark) in &marks {
        let by_tool = touched.contains_key(path);
        let mut at = path.parent();
        while let Some(dir) = at.filter(|d| d.starts_with(root) && *d != root) {
            let seen = inside.entry(dir.to_path_buf()).or_insert(Mark::None);
            *seen = (*seen).max(*mark);
            if by_tool || !quiet {
                opens.insert(dir.to_path_buf());
            }
            at = dir.parent();
        }
    }

    let mut out = Vec::new();
    let mut stack: Vec<(PathBuf, usize)> = vec![(root.to_path_buf(), 0)];
    // Depth-first, each folder's entries in order: a stack of what is left to show.
    let mut pending: Vec<Vec<(PathBuf, bool, bool)>> = vec![with_touched(root, entries(root), &marks)];
    while let Some(level) = pending.last_mut() {
        if level.is_empty() {
            pending.pop();
            stack.pop();
            continue;
        }
        let (path, is_dir, listed) = level.remove(0);
        let depth = stack.len() - 1;
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        if is_dir {
            let open = !folds.shut.contains(&path) && (folds.opened.contains(&path) || opens.contains(&path));
            out.push(Row {
                name: format!("{name}/"),
                depth,
                dir: true,
                open,
                mark: inside.get(&path).copied().unwrap_or(Mark::None),
                anchor: None,
                elsewhere: false,
                path: path.clone(),
            });
            if open && out.len() < 3000 {
                // A folder git ignores (build output, dependencies) is here only because narsil
                // went into it: show what it touched there, not the thousands of files beside it.
                let children = with_touched(&path, if listed { entries(&path) } else { Vec::new() }, &marks);
                stack.push((path, depth + 1));
                pending.push(children);
            }
        } else {
            out.push(Row {
                name,
                depth,
                dir: false,
                open: false,
                mark: marks.get(&path).copied().unwrap_or(Mark::None),
                anchor: touched.get(&path).and_then(|t| t.anchor),
                elsewhere: false,
                path,
            });
        }
    }

    // What the file tools touched outside this directory, the latest first.
    let mut elsewhere: Vec<(&PathBuf, &Touch)> = touched.iter().filter(|(p, _)| !p.starts_with(root)).collect();
    elsewhere.sort_by_key(|(path, touch)| (std::cmp::Reverse(touch.anchor.unwrap_or(0)), (*path).clone()));
    for (path, touch) in elsewhere {
        out.push(Row {
            name: short(path),
            depth: 0,
            dir: false,
            open: false,
            mark: touch.mark,
            anchor: touch.anchor,
            elsewhere: true,
            path: path.clone(),
        });
    }
    out
}

/// A folder's entries, with any file narsil touched there that the listing
/// leaves out (it is ignored by git, say) put back: what was touched is
/// always shown. Each entry: its path, whether it is a folder, and whether
/// it was in the listing.
fn with_touched(dir: &Path, listed: Vec<(PathBuf, bool)>, marks: &HashMap<PathBuf, Mark>) -> Vec<(PathBuf, bool, bool)> {
    let mut all: Vec<(PathBuf, bool, bool)> = listed.into_iter().map(|(path, is_dir)| (path, is_dir, true)).collect();
    for path in marks.keys().filter(|p| p.starts_with(dir)) {
        // The child of `dir` on the way to it: the file itself, or a folder above it.
        let Some(first) = path.strip_prefix(dir).ok().and_then(|rest| rest.components().next()) else { continue };
        let child = dir.join(first);
        let is_dir = child != *path;
        if !all.iter().any(|(p, _, _)| *p == child) {
            all.push((child, is_dir, false));
        }
    }
    all.sort_by_key(|(path, is_dir, _)| (!is_dir, path.file_name().map(|n| n.to_string_lossy().to_lowercase()).unwrap_or_default()));
    all
}

/// One thing a file tool did to a file.
#[derive(Clone, Debug)]
pub struct Op {
    pub name: String,
    pub input: Value,
    pub status: String,
    pub is_error: bool,
    pub result: String,
    /// When it started, in seconds since 1970 (0 if it has not).
    pub at: i64,
    /// The subagent that did it, if one did.
    pub agent: String,
}

/// What was done to a file, latest first, as lines to read: when, what, and
/// for a change, what was taken out and what was put in.
pub fn history(ops: &[Op]) -> Vec<String> {
    const SHOWN: usize = 10;
    let mut out = Vec::new();
    let part = |out: &mut Vec<String>, sign: char, text: &str| {
        let lines: Vec<&str> = text.lines().collect();
        for line in lines.iter().take(SHOWN) {
            out.push(format!("  {sign} {line}"));
        }
        if lines.len() > SHOWN {
            out.push(format!("  {sign} … {} more lines", lines.len() - SHOWN));
        }
    };
    for op in ops.iter().take(40) {
        let when = chrono::DateTime::from_timestamp(op.at, 0)
            .filter(|_| op.at > 0)
            .map(|t| t.with_timezone(&chrono::Local).format("%H:%M:%S").to_string())
            .unwrap_or_else(|| "        ".to_string());
        let text = |key: &str| op.input.get(key).and_then(Value::as_str).unwrap_or("");
        let number = |key: &str| op.input.get(key).and_then(Value::as_i64);
        let what = match (op.name.as_str(), op.status.as_str(), op.is_error) {
            (_, "pending" | "running", _) => format!("{} now", match op.name.as_str() { "read_file" => "reading", "edit_file" => "editing", _ => "writing" }),
            (_, _, true) => format!("{} failed: {}", op.name.trim_end_matches("_file"), crate::util::first_line(&op.result, 90)),
            ("read_file", _, _) => match (number("offset"), number("limit")) {
                (Some(from), Some(n)) => format!("read lines {from} to {}", from + n - 1),
                (Some(from), None) => format!("read from line {from}"),
                (None, Some(n)) => format!("read the first {n} lines"),
                (None, None) => "read".to_string(),
            },
            ("write_file", _, _) if op.result.starts_with("Created") => format!("created, {} lines", text("content").lines().count()),
            ("write_file", _, _) => format!("rewritten, {} lines", text("content").lines().count()),
            _ if op.input.get("replace_all").and_then(Value::as_bool) == Some(true) => "edited, every occurrence".to_string(),
            _ => "edited".to_string(),
        };
        let by = if op.agent.is_empty() { String::new() } else { format!(" · subagent {}", op.agent) };
        if !out.is_empty() {
            out.push(String::new());
        }
        out.push(format!("{when}  {what}{by}"));
        if op.is_error {
            continue;
        }
        match op.name.as_str() {
            "edit_file" => {
                part(&mut out, '-', text("old_string"));
                part(&mut out, '+', text("new_string"));
            }
            "write_file" => part(&mut out, '+', text("content")),
            _ => {}
        }
    }
    out
}

/// A path as a person would write it: `~/notes/todo.md`.
pub fn short(path: &Path) -> String {
    match path.strip_prefix(crate::util::home()) {
        Ok(rest) => format!("~/{}", rest.display()),
        Err(_) => path.display().to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn call(name: &str, path: &str, status: &str, result: &str, message_id: i64, session: &str) -> Call {
        Call {
            name: name.into(),
            input: json!({ "path": path }),
            status: status.into(),
            is_error: result.starts_with("error"),
            result: result.into(),
            message_id,
            session: session.into(),
            cwd: "/work/app".into(),
            // As recorded when the call ran.
            resolved: Some(crate::obsidian::lexical(&crate::tools::resolve("/work/app", path)).to_string_lossy().into_owned()),
        }
    }

    #[test]
    fn what_the_file_tools_did_is_read_from_the_journal() {
        let calls = [
            call("read_file", "src/main.rs", "done", "     1\tfn main", 10, "s1"),
            call("write_file", "src/new.rs", "done", "Created src/new.rs (3 lines)", 12, "s1"),
            call("edit_file", "src/new.rs", "done", "Edited src/new.rs (1 replacement)", 14, "s1"),
            call("write_file", "README.md", "done", "Overwrote README.md (9 lines)", 16, "s1"),
            call("edit_file", "src/main.rs", "running", "", 18, "s1"),
            call("read_file", "missing.rs", "done", "error: No such file", 20, "s1"),
            call("write_file", "/etc/app.conf", "done", "Created /etc/app.conf (1 lines)", 22, "s1"),
            call("read_file", "../other/lib.rs", "done", "     1\t// x", 24, "sub1"),
            call("bash", "ignored", "done", "ok", 26, "s1"),
        ];
        let seen = touches(&calls, "s1");
        let of = |p: &str| seen.get(Path::new(p)).copied();
        assert_eq!(of("/work/app/src/main.rs"), Some(Touch { mark: Mark::Now, anchor: Some(18) }), "being edited this moment");
        assert_eq!(of("/work/app/src/new.rs"), Some(Touch { mark: Mark::Created, anchor: Some(14) }), "made here, and still so after an edit");
        assert_eq!(of("/work/app/README.md").map(|t| t.mark), Some(Mark::Changed));
        assert_eq!(of("/etc/app.conf").map(|t| t.mark), Some(Mark::Created), "an absolute path is taken as it is");
        assert_eq!(of("/work/other/lib.rs"), Some(Touch { mark: Mark::Read, anchor: None }), "a subagent's call is seen, but is not in this conversation to go to");
        assert_eq!(of("/work/app/missing.rs"), None, "a call that failed did nothing to the file");
        assert_eq!(seen.len(), 5);

        // Once the edit is done, it is a changed file, not one being worked on.
        let mut later = calls.to_vec();
        later[4] = call("edit_file", "src/main.rs", "done", "Edited src/main.rs (1 replacement)", 18, "s1");
        assert_eq!(touches(&later, "s1").get(Path::new("/work/app/src/main.rs")).map(|t| t.mark), Some(Mark::Changed));

        // The session has since moved to /work/app/docs. What was recorded when the call ran still says where the file is;
        // a call from before that was recorded, whose path now names nothing, is left out and not put in the wrong place.
        let mut moved = call("read_file", "src/main.rs", "done", "     1\tfn main", 10, "s1");
        moved.cwd = "/work/app/docs".into();
        assert!(touches(std::slice::from_ref(&moved), "s1").contains_key(Path::new("/work/app/src/main.rs")));
        moved.resolved = None;
        assert!(touches(&[moved], "s1").is_empty());
    }

    #[test]
    fn what_was_done_to_a_file_reads_as_a_short_history() {
        let op = |name: &str, input: Value, result: &str, agent: &str| Op {
            name: name.into(),
            input,
            status: "done".into(),
            is_error: result.starts_with("error"),
            result: result.into(),
            at: 0,
            agent: agent.into(),
        };
        let long: String = (1..=14).map(|i| format!("line {i}\n")).collect();
        let mut running = op("edit_file", json!({ "path": "a.rs" }), "", "");
        running.status = "running".into();
        let lines = history(&[
            running,
            op("edit_file", json!({ "path": "a.rs", "old_string": "let x = 1;", "new_string": "let x = 2;\nlet y = 3;" }), "Edited a.rs (1 replacement)", "lexer"),
            op("edit_file", json!({ "path": "a.rs", "old_string": "nope", "new_string": "x" }), "error: old_string not found in a.rs", ""),
            op("read_file", json!({ "path": "a.rs", "offset": 40, "limit": 20 }), "    40\tfn x", ""),
            op("write_file", json!({ "path": "a.rs", "content": long }), "Created a.rs (14 lines, 98 bytes)", ""),
        ]);
        let text: Vec<&str> = lines.iter().map(|l| l.trim_start()).collect();
        assert_eq!(
            text,
            [
                "editing now",
                "",
                "edited · subagent lexer",
                "- let x = 1;",
                "+ let x = 2;",
                "+ let y = 3;",
                "",
                "edit failed: error: old_string not found in a.rs",
                "",
                "read lines 40 to 59",
                "",
                "created, 14 lines",
                "+ line 1",
                "+ line 2",
                "+ line 3",
                "+ line 4",
                "+ line 5",
                "+ line 6",
                "+ line 7",
                "+ line 8",
                "+ line 9",
                "+ line 10",
                "+ … 4 more lines",
            ]
        );
        assert!(lines[3].starts_with("  - ") && lines[4].starts_with("  + "), "what was taken out and put in is indented under its step");
        assert!(history(&[]).is_empty());
    }

    #[test]
    fn the_tree_opens_where_narsil_has_been_and_lists_what_is_elsewhere() {
        let root = std::env::temp_dir().join(format!("narsil-files-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for dir in ["src/tui", "src/tools", "docs", "target/debug", ".git/objects", ".github/workflows"] {
            std::fs::create_dir_all(root.join(dir)).unwrap();
        }
        for file in ["Cargo.toml", "README.md", "src/main.rs", "src/tui/mod.rs", "src/tui/keys.rs", "src/tools/fs.rs", "docs/guide.md", "target/debug/app", ".git/HEAD", ".github/workflows/ci.yml", ".gitignore"] {
            std::fs::write(root.join(file), "x").unwrap();
        }
        std::fs::write(root.join(".gitignore"), "/target\n").unwrap();
        let root = std::fs::canonicalize(&root).unwrap();
        let touch = |mark, anchor| Touch { mark, anchor };
        let mut touched = HashMap::new();
        touched.insert(root.join("src/tui/mod.rs"), touch(Mark::Now, Some(30)));
        touched.insert(root.join("src/tui/keys.rs"), touch(Mark::Read, Some(20)));
        touched.insert(root.join("README.md"), touch(Mark::Changed, Some(10)));
        touched.insert(root.join("target/debug/built.log"), touch(Mark::Created, Some(25)));
        touched.insert(PathBuf::from("/etc/app.conf"), touch(Mark::Created, Some(40)));
        touched.insert(PathBuf::from("/var/tmp/notes.txt"), touch(Mark::Read, Some(5)));
        let on_disk: HashSet<PathBuf> = [root.join("docs/guide.md")].into_iter().collect();

        let line = |r: &Row| format!("{}{}{} {}", "  ".repeat(r.depth), if r.dir { if r.open { "▾" } else { "▸" } } else { r.mark.glyph() }, if r.dir { format!(" {}", r.mark.glyph()) } else { String::new() }, r.name);
        let shown = |folds: &Folds| rows(&root, &touched, &on_disk, folds).iter().map(line).collect::<Vec<_>>();
        assert_eq!(
            shown(&Folds::default()),
            [
                "▸   .github/",
                "▾ ~ docs/",
                "  ~ guide.md",
                "▾ ▶ src/",
                "  ▸   tools/",
                "  ▾ ▶ tui/",
                "    ○ keys.rs",
                "    ▶ mod.rs",
                "    main.rs",
                "▾ + target/",
                "  ▾ + debug/",
                "    + built.log",
                "  .gitignore",
                "  Cargo.toml",
                "~ README.md",
                "+ /etc/app.conf",
                "○ /var/tmp/notes.txt",
            ],
            "folders first; open where something happened; .git never; what git ignores only where narsil went"
        );
        let all = rows(&root, &touched, &on_disk, &Folds::default());
        assert!(all.iter().filter(|r| r.elsewhere).all(|r| r.path.is_absolute() && r.depth == 0) && all.iter().filter(|r| r.elsewhere).count() == 2);
        assert_eq!(all.iter().find(|r| r.name == "mod.rs").and_then(|r| r.anchor), Some(30));

        // Shut by hand stays shut, with what happened inside still marked on it; opened by hand opens.
        let folds = Folds { opened: [root.join(".github")].into_iter().collect(), shut: [root.join("src")].into_iter().collect() };
        let folded = shown(&folds);
        assert!(folded.contains(&"▸ ▶ src/".to_string()) && !folded.iter().any(|l| l.contains("mod.rs")), "{folded:#?}");
        assert!(folded.contains(&"▾   .github/".to_string()) && folded.contains(&"  ▸   workflows/".to_string()), "{folded:#?}");

        // A flood of files changed on disk (a build) is marked on its folders, not all opened up.
        let flood: HashSet<PathBuf> = (0..60).map(|i| root.join(format!("src/tools/gen{i}.rs"))).collect();
        let quiet = rows(&root, &HashMap::new(), &flood, &Folds::default()).iter().map(line).collect::<Vec<_>>();
        assert!(quiet.contains(&"▸ ~ src/".to_string()), "{quiet:#?}");

        // And what a command wrote is found by looking at the disk.
        let found = changed_on_disk(&root, SystemTime::now() - std::time::Duration::from_secs(60));
        assert!(found.contains(&root.join("src/main.rs")) && !found.iter().any(|p| p.starts_with(root.join("target")) || p.starts_with(root.join(".git"))), "{found:?}");
        assert!(changed_on_disk(&root, SystemTime::now() + std::time::Duration::from_secs(60)).is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }
}
