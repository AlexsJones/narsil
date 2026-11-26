//! A real narsil for a test to drive: its own daemon, its own folders, and
//! the scripted `mock` model in place of a real one. Nothing here touches the
//! narsil installed for everyday use.
//!
//! A test tells the model what to do by putting a script in the prompt (see
//! `src/provider/mock.rs`), then looks at what narsil left behind: the notes
//! on disk, the transcript, the journal of tool calls.

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use rusqlite::{Connection, OpenFlags};

static NEXT: AtomicUsize = AtomicUsize::new(0);

pub struct Narsil {
    base: PathBuf,
    runtime: PathBuf,
}

/// One session, as the journal has it.
pub struct Session<'a> {
    narsil: &'a Narsil,
    pub id: String,
    /// What `narsil run` printed.
    pub said: String,
}

/// One tool call from the journal.
#[derive(Debug, Clone)]
pub struct Call {
    pub name: String,
    pub input: String,
    pub result: String,
    pub failed: bool,
}

impl Narsil {
    /// A fresh instance with the scripted model.
    pub fn new() -> Narsil {
        Narsil::with(|_, _| {})
    }

    /// A fresh instance with an Obsidian vault holding `notes` (path in the
    /// vault, text), and the parts matching `closed` shut to narsil.
    pub fn with_vault(notes: &[(&str, &str)], closed: &[&str]) -> Narsil {
        Narsil::with(|base, config| Narsil::add_vault(base, config, notes, closed))
    }

    /// Put a vault with these notes in an instance being set up, with the
    /// parts matching `closed` shut to narsil.
    pub fn add_vault(base: &Path, config: &mut Config, notes: &[(&str, &str)], closed: &[&str]) {
        let vault = base.join("vault");
        std::fs::create_dir_all(vault.join(".obsidian")).unwrap();
        for (rel, text) in notes {
            let path = vault.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }
        config.set("obsidian", "vault", &format!("{:?}", vault.to_string_lossy()));
        let closed: Vec<String> = closed.iter().map(|c| format!("{c:?}")).collect();
        config.set("obsidian", "exclude", &format!("[{}]", closed.join(", ")));
    }

    /// The vault of an instance made by `with_vault`.
    pub fn vault(&self) -> PathBuf {
        std::fs::canonicalize(self.base.join("vault")).unwrap()
    }

    /// A fresh instance, with `config.toml` adjusted before the daemon first
    /// starts. The closure is given the instance's own folder to put things in.
    pub fn with(adjust: impl FnOnce(&Path, &mut Config)) -> Narsil {
        Narsil::set_up(&["--provider", "mock"], adjust)
    }

    /// A fresh instance that talks to a real model, named by the environment:
    /// NARSIL_LIVE_PROVIDER (a setup preset such as `anthropic` or `deepseek`),
    /// NARSIL_LIVE_MODEL, and NARSIL_LIVE_KEY_ENV (the name of the variable
    /// that holds the key). These tests cost money and are run on request only.
    pub fn live(adjust: impl FnOnce(&Path, &mut Config)) -> Narsil {
        let var = |name: &str| {
            std::env::var(name).ok().filter(|v| !v.trim().is_empty()).unwrap_or_else(|| {
                panic!(
                    "{name} is not set. The live tests call a real model:\n  NARSIL_LIVE_PROVIDER=anthropic NARSIL_LIVE_MODEL=claude-sonnet-5-5 \\\n  \
                     NARSIL_LIVE_KEY_ENV=MY_KEY_VARIABLE cargo test --test live -- --ignored --nocapture --test-threads 1"
                )
            })
        };
        let (provider, model, key) = (var("NARSIL_LIVE_PROVIDER"), var("NARSIL_LIVE_MODEL"), var("NARSIL_LIVE_KEY_ENV"));
        assert!(std::env::var(&key).is_ok_and(|v| !v.is_empty()), "the variable {key} (named by NARSIL_LIVE_KEY_ENV) holds no key");
        Narsil::set_up(&["--provider", &provider, "--model", &model, "--api-key-env", &key, "--no-test"], adjust)
    }

    fn set_up(backend: &[&str], adjust: impl FnOnce(&Path, &mut Config)) -> Narsil {
        let tag = format!("{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::SeqCst));
        let base = std::env::temp_dir().join(format!("narsil-test-{tag}"));
        // The socket's path has to be short, which a temporary directory's may not be.
        let runtime = match std::env::var_os("XDG_RUNTIME_DIR").filter(|v| !v.is_empty()) {
            Some(dir) => PathBuf::from(dir).join(format!("narsil-test-{tag}")),
            None => PathBuf::from(format!("/tmp/narsil-test-{tag}")),
        };
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("home")).unwrap();
        std::fs::create_dir_all(base.join("dirs")).unwrap();
        std::fs::create_dir_all(base.join("user")).unwrap();
        let narsil = Narsil { base, runtime };
        let mut args = vec!["setup", "--yes"];
        args.extend(backend);
        narsil.ok(&narsil.base.clone(), &args);
        let path = narsil.home().join("config/config.toml");
        let mut config = Config { text: std::fs::read_to_string(&path).unwrap() };
        // Notes changed by hand are noticed within a second, not twenty.
        config.set("memory", "rescan_secs", "1");
        adjust(&narsil.base, &mut config);
        std::fs::write(&path, config.text).unwrap();
        narsil
    }

    pub fn home(&self) -> PathBuf {
        self.base.join("home")
    }

    /// The home directory narsil and the commands it runs see as `~`: the
    /// instance's own, so that no test can reach the real one.
    pub fn user_home(&self) -> PathBuf {
        std::fs::canonicalize(self.base.join("user")).unwrap()
    }

    /// A directory to work in, made if it is not there.
    pub fn dir(&self, name: &str) -> PathBuf {
        let dir = self.base.join("dirs").join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::canonicalize(&dir).unwrap()
    }

    /// Where the memory notes are kept (unless the test moved them into a vault).
    pub fn memory(&self) -> PathBuf {
        self.home().join("data/memory")
    }

    /// Where the work notes are kept (unless the test moved them into a vault).
    pub fn work(&self) -> PathBuf {
        self.home().join("data/work")
    }

    fn command(&self, cwd: &Path, args: &[&str]) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_narsil"));
        c.args(args)
            .current_dir(cwd)
            .env("NARSIL_HOME", self.home())
            .env("HOME", self.base.join("user"))
            .env("NARSIL_RUNTIME_DIR", &self.runtime)
            .env_remove("NARSIL_LOG")
            .stdin(std::process::Stdio::null());
        c
    }

    /// Run a narsil command and return what it printed, whether or not it succeeded.
    pub fn try_run(&self, cwd: &Path, args: &[&str]) -> (bool, String) {
        let out = self.command(cwd, args).output().expect("running narsil");
        (out.status.success(), format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr)))
    }

    /// Run a narsil command that must succeed.
    pub fn ok(&self, cwd: &Path, args: &[&str]) -> String {
        let (fine, said) = self.try_run(cwd, args);
        assert!(fine, "narsil {} failed:\n{said}", args.join(" "));
        said
    }

    /// Ask something in a new session in `cwd` and wait for the turn to end.
    /// Tools run without asking.
    pub fn ask(&self, cwd: &Path, prompt: &str) -> Session<'_> {
        let said = self.ok(cwd, &["run", "--auto", prompt]);
        self.session_from(said)
    }

    /// Say something more in a session that already exists.
    pub fn ask_again(&self, session: &Session, cwd: &Path, prompt: &str) -> Session<'_> {
        let said = self.ok(cwd, &["run", "--auto", "--session", &session.id, prompt]);
        self.session_from(said)
    }

    fn session_from(&self, said: String) -> Session<'_> {
        let id = said
            .lines()
            .rev()
            .find_map(|l| l.trim().strip_prefix("(session ").and_then(|r| r.strip_suffix(')')))
            .unwrap_or_else(|| panic!("no session id in:\n{said}"))
            .to_string();
        Session { narsil: self, id, said }
    }

    /// Start a goal in `cwd`. Returns its id; it works in the background.
    pub fn goal(&self, cwd: &Path, objective: &str) -> String {
        let said = self.ok(cwd, &["goal", "add", objective]);
        said.split_whitespace().nth(1).unwrap_or_else(|| panic!("no goal id in:\n{said}")).to_string()
    }

    pub fn goal_status(&self, goal: &str) -> String {
        self.text("SELECT status FROM goals WHERE id = ?1", &[goal])
    }

    /// Wait until a goal's status is one of `wanted`, and return it.
    pub fn goal_reaches(&self, goal: &str, wanted: &[&str]) -> String {
        self.goal_reaches_within(goal, wanted, Duration::from_secs(30))
    }

    /// The same, for a goal that a real model is working on and may take minutes over.
    pub fn goal_reaches_within(&self, goal: &str, wanted: &[&str], limit: Duration) -> String {
        let mut last = String::new();
        self.wait_up_to(limit, &format!("goal {goal} to be {}", wanted.join(" or ")), || {
            last = self.goal_status(goal);
            wanted.contains(&last.as_str())
        });
        // The status changes inside the tool call that sets it; what the call returned, and
        // the model's closing words, are on record a moment later, when the session comes to rest.
        self.wait_up_to(limit, &format!("the session of goal {goal} to come to rest"), || {
            self.text("SELECT s.run_state FROM sessions s JOIN goals g ON g.session_id = s.id WHERE g.id = ?1", &[goal]) != "running"
        });
        last
    }

    /// The session a goal works in.
    pub fn goal_session(&self, goal: &str) -> Session<'_> {
        let id = self.text("SELECT session_id FROM goals WHERE id = ?1", &[goal]);
        Session { narsil: self, id, said: String::new() }
    }

    /// Wait for something to become true, or fail saying what was waited for.
    pub fn wait(&self, what: &str, ready: impl FnMut() -> bool) {
        self.wait_up_to(Duration::from_secs(30), what, ready)
    }

    pub fn wait_up_to(&self, limit: Duration, what: &str, mut ready: impl FnMut() -> bool) {
        let began = Instant::now();
        while !ready() {
            if began.elapsed() > limit {
                panic!("gave up waiting for {what}\n--- daemon log ---\n{}", self.log());
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    pub fn log(&self) -> String {
        let text = std::fs::read_to_string(self.home().join("state/daemon.log")).unwrap_or_default();
        let lines: Vec<&str> = text.lines().collect();
        lines[lines.len().saturating_sub(40)..].join("\n")
    }

    /// The journal, opened for reading only.
    pub fn db(&self) -> Connection {
        let db = Connection::open_with_flags(self.home().join("data/narsil.db"), OpenFlags::SQLITE_OPEN_READ_ONLY).expect("opening the journal");
        db.busy_timeout(Duration::from_secs(5)).unwrap();
        db
    }

    /// The first column of every row a query returns, as text.
    pub fn column(&self, sql: &str, args: &[&str]) -> Vec<String> {
        let db = self.db();
        let mut stmt = db.prepare(sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
        let rows = stmt.query_map(rusqlite::params_from_iter(args), |r| r.get::<_, rusqlite::types::Value>(0)).unwrap();
        rows.map(|v| match v.unwrap() {
            rusqlite::types::Value::Text(t) => t,
            rusqlite::types::Value::Integer(i) => i.to_string(),
            rusqlite::types::Value::Null => String::new(),
            other => format!("{other:?}"),
        })
        .collect()
    }

    /// The one value a query returns ("" if it returns nothing).
    pub fn text(&self, sql: &str, args: &[&str]) -> String {
        self.column(sql, args).into_iter().next().unwrap_or_default()
    }

    /// The status of a work item, by title.
    pub fn item(&self, title: &str) -> String {
        self.text("SELECT status FROM work_items WHERE title = ?1", &[title])
    }

    /// Every Markdown note under a folder, relative to it, sorted.
    pub fn notes_in(&self, dir: &Path) -> Vec<String> {
        fn walk(dir: &Path, base: &Path, out: &mut Vec<String>) {
            let Ok(entries) = std::fs::read_dir(dir) else { return };
            for e in entries.flatten() {
                let path = e.path();
                if path.is_dir() {
                    walk(&path, base, out);
                } else if path.extension().is_some_and(|x| x == "md") {
                    out.push(path.strip_prefix(base).unwrap().to_string_lossy().into_owned());
                }
            }
        }
        let mut out = Vec::new();
        walk(dir, dir, &mut out);
        out.sort();
        out
    }

    /// The note with this file name (without `.md`) anywhere under a folder.
    pub fn note(&self, dir: &Path, name: &str) -> Option<(PathBuf, String)> {
        let rel = self.notes_in(dir).into_iter().find(|p| Path::new(p).file_stem().is_some_and(|s| s == name))?;
        let path = dir.join(rel);
        let text = std::fs::read_to_string(&path).ok()?;
        Some((path, text))
    }

    pub fn stop(&self) {
        let _ = self.try_run(&self.base, &["stop"]);
    }

    pub fn start(&self) {
        self.ok(&self.base, &["start"]);
    }

    /// Kill the daemon outright, as a crash or a power cut would.
    pub fn kill(&self) {
        let pid = std::fs::read_to_string(self.runtime.join("narsil.pid")).expect("the daemon's pid file");
        let pid: i32 = pid.trim().parse().expect("a pid");
        unsafe { libc::kill(pid, libc::SIGKILL) };
        self.wait("the daemon to be gone", || unsafe { libc::kill(pid, 0) } != 0);
    }
}

impl Drop for Narsil {
    fn drop(&mut self) {
        self.stop();
        // Left in place when a test fails, to be looked at.
        if !std::thread::panicking() {
            let _ = std::fs::remove_dir_all(&self.base);
        }
        let _ = std::fs::remove_dir_all(&self.runtime);
    }
}

impl Session<'_> {
    /// The notes narsil itself put into the conversation, in order.
    pub fn notes(&self) -> Vec<String> {
        self.narsil
            .column("SELECT blocks FROM messages WHERE session_id = ?1 AND kind = 'note' ORDER BY id", &[&self.id])
            .iter()
            .map(|b| block_text(b))
            .collect()
    }

    /// The notes narsil put in before the model was first called in this session.
    pub fn told_before_starting(&self) -> Vec<String> {
        self.narsil
            .column(
                "SELECT blocks FROM messages WHERE session_id = ?1 AND kind = 'note' AND id < \
                 COALESCE((SELECT MIN(id) FROM messages WHERE session_id = ?1 AND role = 'assistant'), 1 << 62) ORDER BY id",
                &[&self.id],
            )
            .iter()
            .map(|b| block_text(b))
            .collect()
    }

    /// Every tool call the session made, in order.
    pub fn calls(&self) -> Vec<Call> {
        let db = self.narsil.db();
        let mut stmt = db
            .prepare("SELECT name, input, COALESCE(result, ''), is_error FROM tool_calls WHERE session_id = ?1 ORDER BY message_id, seq")
            .unwrap();
        let rows = stmt
            .query_map([&self.id], |r| Ok(Call { name: r.get(0)?, input: r.get(1)?, result: r.get(2)?, failed: r.get::<_, i64>(3)? != 0 }))
            .unwrap();
        rows.map(Result::unwrap).collect()
    }

    /// The calls to one tool.
    pub fn calls_to(&self, tool: &str) -> Vec<Call> {
        self.calls().into_iter().filter(|c| c.name == tool).collect()
    }

    /// The system prompt the session is running under.
    pub fn prompt(&self) -> String {
        self.narsil.text("SELECT system_prompt FROM sessions WHERE id = ?1", &[&self.id])
    }

    pub fn cwd(&self) -> String {
        self.narsil.text("SELECT cwd FROM sessions WHERE id = ?1", &[&self.id])
    }

    /// Everything the model was shown or said in this session, as one text:
    /// for checking that something never reached it.
    pub fn everything(&self) -> String {
        let mut all = self.prompt();
        for blocks in self.narsil.column("SELECT blocks FROM messages WHERE session_id = ?1 ORDER BY id", &[&self.id]) {
            all.push('\n');
            all.push_str(&blocks);
        }
        for call in self.calls() {
            all.push('\n');
            all.push_str(&call.result);
        }
        all
    }
}

/// The text of a stored message.
fn block_text(blocks: &str) -> String {
    let parsed: serde_json::Value = serde_json::from_str(blocks).unwrap_or_default();
    parsed
        .as_array()
        .map(|list| list.iter().filter_map(|b| b.get("text").and_then(|t| t.as_str())).collect::<Vec<_>>().join("\n"))
        .unwrap_or_default()
}

/// `config.toml` as text, for a test to change a setting in.
pub struct Config {
    text: String,
}

impl Config {
    /// Set `key` under `[section]` to a value written as TOML (`"\"text\""`, `"true"`, `"3"`).
    pub fn set(&mut self, section: &str, key: &str, value: &str) {
        let header = format!("[{section}]");
        let start = self.text.lines().position(|l| l.trim() == header).unwrap_or_else(|| panic!("no {header} in the config"));
        let mut lines: Vec<String> = self.text.lines().map(str::to_string).collect();
        let end = lines.iter().enumerate().skip(start + 1).find(|(_, l)| l.starts_with('[')).map(|(i, _)| i).unwrap_or(lines.len());
        let line = format!("{key} = {value}");
        match (start + 1..end).find(|&i| lines[i].split('=').next().is_some_and(|k| k.trim() == key)) {
            Some(i) => lines[i] = line,
            None => lines.insert(start + 1, line),
        }
        self.text = lines.join("\n") + "\n";
    }
}

/// A script for the mock model: the steps, joined the way it reads them.
pub fn script(steps: &[&str]) -> String {
    steps.join(" ;; ")
}
