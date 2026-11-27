//! The unified store: one SQLite database that is the single source of truth
//! for everything an agent knows or was doing.
//!
//!   sessions / messages   append-only transcripts (episodic memory)
//!   tool_calls            write-ahead journal of tool execution
//!   pending_inputs        user messages waiting to be delivered to a running agent
//!   memories              search index over the memory notes, and per-task notes
//!   goals / goal_log      autonomous objectives and their progress journal
//!   work_items / _edges   index of the work notes: what is ready, who holds what
//!   work_events           who did what to each work item, and where in which transcript
//!   spans                 what narsil did and how long it took (telemetry)
//!   subagents             what each subagent was spawned for and how it ended
//!
//! Stop/resume, crash recovery and parallel subagents are all the same thing
//! from here: an agent's next action is derived from what is in these tables,
//! so any process that opens the database can pick the work up.
//!
//! Durable memory is the exception: its source of truth is a folder of linked
//! Markdown notes (see `memory.rs`). The `memories` rows for the global and
//! project scopes are an index of those notes that can be rebuilt from them.

use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use anyhow::{Result, anyhow, bail};
use rusqlite::types::Value as SqlValue;
use rusqlite::{Connection, OptionalExtension, Row, params, params_from_iter};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::types::{Block, MsgKind, Role, StoredMessage};
use crate::util::{new_id, now};

/// One answered request to a model, as the usage record has it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ModelCall {
    pub session: String,
    pub root: String,
    /// The message in the root session that asked for the work this was part of.
    pub request: i64,
    /// `turn` or `compaction`.
    pub purpose: String,
    pub provider: String,
    pub model: String,
    /// Input tokens neither read from a cache nor written to one.
    pub input: i64,
    pub cache_read: i64,
    pub cache_write: i64,
    pub output: i64,
    /// The backend reported nothing, and these are narsil's own estimate.
    pub estimated: bool,
    pub started_ms: i64,
    pub dur_ms: i64,
}

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS sessions (
    id            TEXT PRIMARY KEY,
    parent_id     TEXT,
    root_id       TEXT NOT NULL,
    goal_id       TEXT,
    kind          TEXT NOT NULL,              -- chat | goal | subagent
    agent_name    TEXT,
    depth         INTEGER NOT NULL DEFAULT 0,
    title         TEXT NOT NULL DEFAULT '',
    cwd           TEXT NOT NULL,
    model         TEXT NOT NULL,
    perm_mode     TEXT NOT NULL DEFAULT 'ask',
    run_state     TEXT NOT NULL DEFAULT 'idle', -- idle | running | interrupted
    system_prompt TEXT NOT NULL DEFAULT '',     -- frozen for the current context epoch
    epoch_start   INTEGER NOT NULL DEFAULT 0,   -- first message id of the current epoch
    strip_thinking_before INTEGER NOT NULL DEFAULT 0,
    input_tokens  INTEGER NOT NULL DEFAULT 0,
    output_tokens INTEGER NOT NULL DEFAULT 0,
    context_tokens INTEGER NOT NULL DEFAULT 0,
    created_at    INTEGER NOT NULL,
    updated_at    INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS sessions_parent ON sessions(parent_id);
CREATE INDEX IF NOT EXISTS sessions_updated ON sessions(updated_at);

CREATE TABLE IF NOT EXISTS messages (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id TEXT NOT NULL,
    role       TEXT NOT NULL,
    kind       TEXT NOT NULL DEFAULT 'message',
    blocks     TEXT NOT NULL,
    created_at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS messages_session ON messages(session_id, id);

CREATE TABLE IF NOT EXISTS tool_calls (
    session_id TEXT NOT NULL,
    message_id INTEGER NOT NULL,
    seq        INTEGER NOT NULL,
    id         TEXT NOT NULL,
    name       TEXT NOT NULL,
    input      TEXT NOT NULL,
    status     TEXT NOT NULL DEFAULT 'pending', -- pending | running | done
    state      TEXT,                            -- journal for resumable tools
    result     TEXT,
    is_error   INTEGER NOT NULL DEFAULT 0,
    started_at INTEGER,
    finished_at INTEGER,
    PRIMARY KEY (session_id, message_id, seq)
);

CREATE TABLE IF NOT EXISTS pending_inputs (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id TEXT NOT NULL,
    text       TEXT NOT NULL,
    created_at INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS memories (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    scope      TEXT NOT NULL,   -- global | project:<dir> | task:<root session id>
    kind       TEXT NOT NULL,   -- user | feedback | project | reference | note
    title      TEXT NOT NULL,
    body       TEXT NOT NULL,
    source_session TEXT,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    path       TEXT,                        -- the note, relative to the memory folder; NULL: not written yet
    mtime      INTEGER NOT NULL DEFAULT 0,  -- the note's modification time when it was indexed (ns)
    pending    INTEGER NOT NULL DEFAULT 0,  -- 1: waiting to be written to the folder; 2: waiting to be removed
    UNIQUE(scope, title)
);
-- [[links]] found in each entry, so the notes can be walked as a graph.
CREATE TABLE IF NOT EXISTS memory_links (
    from_id INTEGER NOT NULL,
    target  TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS memory_links_from ON memory_links(from_id);
CREATE TABLE IF NOT EXISTS meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
CREATE VIRTUAL TABLE IF NOT EXISTS memories_fts USING fts5(title, body, content='memories', content_rowid='id');
CREATE TRIGGER IF NOT EXISTS memories_ai AFTER INSERT ON memories BEGIN
    INSERT INTO memories_fts(rowid, title, body) VALUES (new.id, new.title, new.body);
END;
CREATE TRIGGER IF NOT EXISTS memories_ad AFTER DELETE ON memories BEGIN
    INSERT INTO memories_fts(memories_fts, rowid, title, body) VALUES ('delete', old.id, old.title, old.body);
END;
CREATE TRIGGER IF NOT EXISTS memories_au AFTER UPDATE ON memories BEGIN
    INSERT INTO memories_fts(memories_fts, rowid, title, body) VALUES ('delete', old.id, old.title, old.body);
    INSERT INTO memories_fts(rowid, title, body) VALUES (new.id, new.title, new.body);
END;

CREATE TABLE IF NOT EXISTS goals (
    id         TEXT PRIMARY KEY,
    objective  TEXT NOT NULL,
    cwd        TEXT NOT NULL,
    model      TEXT NOT NULL DEFAULT '',
    status     TEXT NOT NULL,   -- active | paused | blocked | completed | cancelled
    note       TEXT NOT NULL DEFAULT '',
    session_id TEXT,
    iterations INTEGER NOT NULL DEFAULT 0,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS goal_log (
    id      INTEGER PRIMARY KEY AUTOINCREMENT,
    goal_id TEXT NOT NULL,
    ts      INTEGER NOT NULL,
    kind    TEXT NOT NULL,      -- progress | status
    text    TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS objectives (
    session_id TEXT PRIMARY KEY,
    items      TEXT NOT NULL,
    updated_at INTEGER NOT NULL
);

-- One row per subagent session: what it was spawned for and how it ended.
CREATE TABLE IF NOT EXISTS subagents (
    session_id TEXT PRIMARY KEY,
    spawn_call TEXT NOT NULL,             -- the parent's tool call that created it
    task       TEXT NOT NULL,
    objective  TEXT NOT NULL DEFAULT '',  -- the parent's objective it serves, if any
    started_at INTEGER NOT NULL DEFAULT 0, -- start of the current or latest stretch of work
    ended_at   INTEGER NOT NULL DEFAULT 0,
    worked     INTEGER NOT NULL DEFAULT 0, -- seconds spent in stretches that have ended
    outcome    TEXT NOT NULL DEFAULT '',  -- '' | done | failed | interrupted
    summary    TEXT NOT NULL DEFAULT ''
);

-- The work graph: an index of the work notes (see `work.rs`), rebuilt from
-- them like the memory index, plus the record of who did what to each item.
CREATE TABLE IF NOT EXISTS work_items (
    id         TEXT PRIMARY KEY,
    project    TEXT NOT NULL,              -- the directory the work belongs to
    title      TEXT NOT NULL,
    status     TEXT NOT NULL,              -- open | in_progress | done | dropped | waiting
    kind       TEXT NOT NULL DEFAULT 'task', -- task | question
    parent     TEXT,
    discovered_from TEXT,
    goal       TEXT NOT NULL DEFAULT '',
    body       TEXT NOT NULL DEFAULT '',
    result     TEXT NOT NULL DEFAULT '',
    log        TEXT NOT NULL DEFAULT '',
    claimed_by TEXT NOT NULL DEFAULT '',   -- session id
    claimed_agent TEXT NOT NULL DEFAULT '',
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    path       TEXT,
    mtime      INTEGER NOT NULL DEFAULT 0,
    pending    INTEGER NOT NULL DEFAULT 0,
    UNIQUE(project, title)
);
CREATE TABLE IF NOT EXISTS work_edges (
    item       TEXT NOT NULL,
    blocked_by TEXT NOT NULL,
    PRIMARY KEY (item, blocked_by)
);
CREATE TABLE IF NOT EXISTS work_events (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    item       TEXT NOT NULL,
    ts         INTEGER NOT NULL,
    session    TEXT NOT NULL DEFAULT '',
    message_id INTEGER NOT NULL DEFAULT 0,  -- where in that session's transcript
    agent      TEXT NOT NULL DEFAULT '',
    what       TEXT NOT NULL,               -- created | claimed | note | done | dropped | waiting | reopened | edited
    text       TEXT NOT NULL DEFAULT ''
);
CREATE INDEX IF NOT EXISTS work_events_item ON work_events(item, id);

-- What narsil did and how long it took: one row per recorded span (a session
-- run, a model call, a tool call...). Written by the telemetry recorder as
-- spans open and close, so a span still open when the daemon died is visible.
-- Every request to a model that was answered, with the tokens the backend
-- says it used. What narsil reports of token use is added up from here.
CREATE TABLE IF NOT EXISTS model_usage (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id  TEXT NOT NULL,
    root_id     TEXT NOT NULL,
    request_id  INTEGER NOT NULL DEFAULT 0,   -- the message in the root session that asked for the work
    purpose     TEXT NOT NULL DEFAULT 'turn', -- turn | compaction
    provider    TEXT NOT NULL DEFAULT '',
    model       TEXT NOT NULL DEFAULT '',
    input       INTEGER NOT NULL DEFAULT 0,   -- input neither read from a cache nor written to one
    cache_read  INTEGER NOT NULL DEFAULT 0,
    cache_write INTEGER NOT NULL DEFAULT 0,
    output      INTEGER NOT NULL DEFAULT 0,
    estimated   INTEGER NOT NULL DEFAULT 0,   -- the backend reported nothing: narsil's own count
    started_ms  INTEGER NOT NULL,             -- unix time
    dur_ms      INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS model_usage_root ON model_usage(root_id, id);

CREATE TABLE IF NOT EXISTS spans (
    id       INTEGER PRIMARY KEY,
    parent   INTEGER,                    -- the recorded span this one ran inside
    root     TEXT NOT NULL DEFAULT '',   -- session tree
    session  TEXT NOT NULL DEFAULT '',
    agent    TEXT NOT NULL DEFAULT '',
    goal     TEXT NOT NULL DEFAULT '',
    kind     TEXT NOT NULL,              -- session_run | model_call | tool_call | ...
    what     TEXT NOT NULL DEFAULT '',   -- the model, the tool, ...
    start_ms INTEGER NOT NULL,           -- unix time
    dur_ms   INTEGER,                    -- NULL while it is running
    ok       INTEGER NOT NULL DEFAULT 1,
    data     TEXT NOT NULL DEFAULT '{}'  -- the span's other fields
);
CREATE INDEX IF NOT EXISTS spans_root ON spans(root, start_ms);
CREATE INDEX IF NOT EXISTS spans_start ON spans(start_ms);

CREATE TABLE IF NOT EXISTS input_history (
    id   INTEGER PRIMARY KEY AUTOINCREMENT,
    text TEXT NOT NULL,
    cwd  TEXT NOT NULL,
    ts   INTEGER NOT NULL
);
"#;

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Session {
    pub id: String,
    pub parent_id: Option<String>,
    pub root_id: String,
    pub goal_id: Option<String>,
    pub kind: String,
    pub agent_name: Option<String>,
    pub depth: i64,
    pub title: String,
    pub cwd: String,
    pub model: String,
    pub perm_mode: String,
    pub run_state: String,
    #[serde(skip)]
    pub system_prompt: String,
    pub epoch_start: i64,
    pub strip_thinking_before: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub context_tokens: i64,
    pub created_at: i64,
    pub updated_at: i64,
}

const SESSION_COLS: &str = "id, parent_id, root_id, goal_id, kind, agent_name, depth, title, cwd, model, \
    perm_mode, run_state, system_prompt, epoch_start, strip_thinking_before, input_tokens, output_tokens, \
    context_tokens, created_at, updated_at";

fn session_from(row: &Row) -> rusqlite::Result<Session> {
    Ok(Session {
        id: row.get(0)?,
        parent_id: row.get(1)?,
        root_id: row.get(2)?,
        goal_id: row.get(3)?,
        kind: row.get(4)?,
        agent_name: row.get(5)?,
        depth: row.get(6)?,
        title: row.get(7)?,
        cwd: row.get(8)?,
        model: row.get(9)?,
        perm_mode: row.get(10)?,
        run_state: row.get(11)?,
        system_prompt: row.get(12)?,
        epoch_start: row.get(13)?,
        strip_thinking_before: row.get(14)?,
        input_tokens: row.get(15)?,
        output_tokens: row.get(16)?,
        context_tokens: row.get(17)?,
        created_at: row.get(18)?,
        updated_at: row.get(19)?,
    })
}

#[derive(Default)]
pub struct NewSession {
    pub parent: Option<Session>,
    pub goal_id: Option<String>,
    pub kind: String,
    pub agent_name: Option<String>,
    pub title: String,
    pub cwd: String,
    pub model: String,
    pub perm_mode: String,
}

#[derive(Clone, Debug)]
pub struct ToolCall {
    pub id: String,
    pub message_id: i64,
    pub seq: i64,
    pub name: String,
    pub input: Value,
    pub status: String,
    pub state: Option<Value>,
    pub result: Option<String>,
    pub is_error: bool,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Memory {
    pub id: i64,
    pub scope: String,
    pub kind: String,
    pub title: String,
    pub body: String,
    pub created_at: i64,
    pub updated_at: i64,
    /// The entry's note, relative to the memory folder. None for task notes,
    /// and for an entry that has not been written to the folder yet.
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub mtime: i64,
    /// 0: in step with its note. 1: waiting to be written. 2: waiting to be removed.
    #[serde(default)]
    pub pending: i64,
}

/// One recorded span: something narsil did, when, and for how long.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct SpanRow {
    pub id: i64,
    pub parent: Option<i64>,
    pub root: String,
    pub session: String,
    pub agent: String,
    pub goal: String,
    pub kind: String,
    pub what: String,
    pub start_ms: i64,
    /// None while the span is still running.
    pub dur_ms: Option<i64>,
    pub ok: bool,
    pub data: Value,
}

impl SpanRow {
    /// A number recorded on the span, or 0.
    pub fn num(&self, key: &str) -> i64 {
        self.data.get(key).and_then(|v| v.as_i64().or_else(|| v.as_str().and_then(|s| s.parse().ok()))).unwrap_or(0)
    }

    pub fn text(&self, key: &str) -> String {
        match self.data.get(key) {
            Some(Value::String(s)) => s.clone(),
            Some(Value::Null) | None => String::new(),
            Some(v) => v.to_string(),
        }
    }
}

const SPAN_COLS: &str = "id, parent, root, session, agent, goal, kind, what, start_ms, dur_ms, ok, data";

fn span_from(row: &Row) -> rusqlite::Result<SpanRow> {
    let data: String = row.get(11)?;
    Ok(SpanRow {
        id: row.get(0)?,
        parent: row.get(1)?,
        root: row.get(2)?,
        session: row.get(3)?,
        agent: row.get(4)?,
        goal: row.get(5)?,
        kind: row.get(6)?,
        what: row.get(7)?,
        start_ms: row.get(8)?,
        dur_ms: row.get(9)?,
        ok: row.get::<_, i64>(10)? != 0,
        data: serde_json::from_str(&data).unwrap_or(Value::Null),
    })
}

/// What the index should hold for one entry.
pub struct IndexEntry<'a> {
    pub scope: &'a str,
    pub kind: &'a str,
    pub title: &'a str,
    pub body: &'a str,
    pub path: Option<&'a str>,
    pub mtime: i64,
    pub pending: i64,
    pub updated_at: i64,
    pub links: &'a [String],
}

fn memory_from(row: &Row) -> rusqlite::Result<Memory> {
    Ok(Memory {
        id: row.get(0)?,
        scope: row.get(1)?,
        kind: row.get(2)?,
        title: row.get(3)?,
        body: row.get(4)?,
        created_at: row.get(5)?,
        updated_at: row.get(6)?,
        path: row.get(7)?,
        mtime: row.get(8)?,
        pending: row.get(9)?,
    })
}

const MEMORY_COLS: &str =
    "m.id, m.scope, m.kind, m.title, m.body, m.created_at, m.updated_at, m.path, m.mtime, m.pending";

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Goal {
    pub id: String,
    pub objective: String,
    pub cwd: String,
    pub model: String,
    pub status: String,
    pub note: String,
    pub session_id: Option<String>,
    pub iterations: i64,
    pub created_at: i64,
    pub updated_at: i64,
}

const GOAL_COLS: &str = "id, objective, cwd, model, status, note, session_id, iterations, created_at, updated_at";

fn goal_from(row: &Row) -> rusqlite::Result<Goal> {
    Ok(Goal {
        id: row.get(0)?,
        objective: row.get(1)?,
        cwd: row.get(2)?,
        model: row.get(3)?,
        status: row.get(4)?,
        note: row.get(5)?,
        session_id: row.get(6)?,
        iterations: row.get(7)?,
        created_at: row.get(8)?,
        updated_at: row.get(9)?,
    })
}

/// A subagent as a UI shows it: who it works for, on what, and how far it is.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct AgentInfo {
    /// The subagent's own session id.
    pub id: String,
    /// The session that spawned it.
    pub parent: String,
    pub name: String,
    pub task: String,
    /// The parent's objective this agent serves; empty if it was not given one.
    pub objective: String,
    /// The tool call in the parent's transcript that created it.
    pub spawn_call: String,
    /// queued | running | done | failed | interrupted
    pub state: String,
    /// What it is doing right now, while running.
    #[serde(default)]
    pub activity: String,
    /// Tool calls started so far.
    pub calls: i64,
    /// Tokens processed so far (input plus output over all its model calls).
    pub tokens: i64,
    pub started_at: i64,
    pub ended_at: i64,
    /// Seconds of work in stretches that have ended. An agent that was stopped
    /// and resumed has several; time spent stopped does not count.
    #[serde(default)]
    pub worked: i64,
    /// The start of its final report, or why it stopped.
    pub summary: String,
    pub model: String,
}

impl AgentInfo {
    /// Seconds spent working so far, counting the stretch in progress up to `now`.
    pub fn elapsed(&self, now: i64) -> i64 {
        let current = if self.state == "running" && self.started_at > 0 { (now - self.started_at).max(0) } else { 0 };
        self.worked + current
    }

    pub fn active(&self) -> bool {
        matches!(self.state.as_str(), "queued" | "running")
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct GoalLog {
    pub ts: i64,
    pub kind: String,
    pub text: String,
}

fn message_from(row: &Row) -> rusqlite::Result<StoredMessage> {
    let role: String = row.get(1)?;
    let kind: String = row.get(2)?;
    let blocks: String = row.get(3)?;
    Ok(StoredMessage {
        id: row.get(0)?,
        role: Role::parse(&role),
        kind: MsgKind::parse(&kind),
        blocks: serde_json::from_str::<Vec<Block>>(&blocks).unwrap_or_default(),
        created_at: row.get(4)?,
    })
}

fn tool_call_from(row: &Row) -> rusqlite::Result<ToolCall> {
    let input: String = row.get(4)?;
    let state: Option<String> = row.get(6)?;
    Ok(ToolCall {
        id: row.get(0)?,
        message_id: row.get(1)?,
        seq: row.get(2)?,
        name: row.get(3)?,
        input: serde_json::from_str(&input).unwrap_or(Value::Null),
        status: row.get(5)?,
        state: state.and_then(|s| serde_json::from_str(&s).ok()),
        result: row.get(7)?,
        is_error: row.get::<_, i64>(8)? != 0,
    })
}

const TOOL_COLS: &str = "id, message_id, seq, name, input, status, state, result, is_error";

/// Columns added after a table first shipped. `CREATE TABLE IF NOT EXISTS`
/// leaves an existing table alone, so older databases get them here.
fn migrate(conn: &Connection) -> Result<()> {
    let added = [
        ("subagents", "worked", "INTEGER NOT NULL DEFAULT 0"),
        ("memories", "path", "TEXT"),
        ("memories", "mtime", "INTEGER NOT NULL DEFAULT 0"),
        ("memories", "pending", "INTEGER NOT NULL DEFAULT 0"),
    ];
    for (table, column, decl) in added {
        let present: bool = conn
            .prepare(&format!("SELECT 1 FROM pragma_table_info('{table}') WHERE name = ?1"))?
            .exists(params![column])?;
        if !present {
            conn.execute_batch(&format!("ALTER TABLE {table} ADD COLUMN {column} {decl}"))?;
        }
    }
    Ok(())
}

#[derive(Clone)]
pub struct Store {
    conn: Arc<Mutex<Connection>>,
}

impl Store {
    pub fn open(path: &Path) -> Result<Store> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let conn = Connection::open(path)?;
        conn.busy_timeout(Duration::from_secs(15))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.execute_batch(SCHEMA)?;
        migrate(&conn)?;
        Ok(Store { conn: Arc::new(Mutex::new(conn)) })
    }

    #[cfg(test)]
    pub fn open_memory() -> Result<Store> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch(SCHEMA)?;
        migrate(&conn)?;
        Ok(Store { conn: Arc::new(Mutex::new(conn)) })
    }

    /// The connection, for modules that keep their own tables here (`work.rs`).
    /// Hold it briefly, and begin any transaction with `Immediate`.
    pub(crate) fn conn(&self) -> MutexGuard<'_, Connection> {
        self.db()
    }

    // Every transaction here takes the write lock when it begins (`Immediate`).
    // The default takes it on the first write, and SQLite refuses that upgrade
    // outright, without waiting, if another connection has written since the
    // transaction's first read. Other connections do write: the telemetry
    // recorder, the TUI and the CLI each have their own.
    fn db(&self) -> MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(|e| e.into_inner())
    }

    // ---- sessions ---------------------------------------------------------

    pub fn create_session(&self, new: NewSession) -> Result<Session> {
        let id = new_id();
        let ts = now();
        let (parent_id, root_id, depth) = match &new.parent {
            Some(p) => (Some(p.id.clone()), p.root_id.clone(), p.depth + 1),
            None => (None, id.clone(), 0),
        };
        self.db().execute(
            "INSERT INTO sessions (id, parent_id, root_id, goal_id, kind, agent_name, depth, title, cwd, model, \
             perm_mode, created_at, updated_at) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?12)",
            params![
                id,
                parent_id,
                root_id,
                new.goal_id,
                new.kind,
                new.agent_name,
                depth,
                new.title,
                new.cwd,
                new.model,
                new.perm_mode,
                ts
            ],
        )?;
        self.get_session(&id)?.ok_or_else(|| anyhow!("session vanished after insert"))
    }

    pub fn get_session(&self, id: &str) -> Result<Option<Session>> {
        let db = self.db();
        let s = db
            .query_row(&format!("SELECT {SESSION_COLS} FROM sessions WHERE id = ?1"), params![id], session_from)
            .optional()?;
        Ok(s)
    }

    pub fn require_session(&self, id: &str) -> Result<Session> {
        self.get_session(id)?.ok_or_else(|| anyhow!("no such session: {id}"))
    }

    /// Resolve a unique id prefix to a full session id.
    pub fn resolve_session(&self, prefix: &str) -> Result<String> {
        self.resolve_prefix("sessions", prefix)
    }

    pub fn resolve_goal(&self, prefix: &str) -> Result<String> {
        self.resolve_prefix("goals", prefix)
    }

    fn resolve_prefix(&self, table: &str, prefix: &str) -> Result<String> {
        if prefix.is_empty() || !prefix.chars().all(|c| c.is_ascii_alphanumeric()) {
            bail!("invalid id: {prefix:?}");
        }
        let db = self.db();
        let mut stmt = db.prepare(&format!("SELECT id FROM {table} WHERE id LIKE ?1 LIMIT 2"))?;
        let ids: Vec<String> =
            stmt.query_map(params![format!("{prefix}%")], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
        match ids.len() {
            0 => bail!("nothing in {table} matches id {prefix:?}"),
            1 => Ok(ids.into_iter().next().unwrap()),
            _ => bail!("id prefix {prefix:?} is ambiguous"),
        }
    }

    /// Sessions with at least one message, newest first. Subagents are hidden
    /// unless `include_children`.
    pub fn list_sessions(&self, include_children: bool, limit: i64) -> Result<Vec<Session>> {
        let db = self.db();
        let mut stmt = db.prepare(&format!(
            "SELECT {SESSION_COLS} FROM sessions s WHERE (?1 OR parent_id IS NULL) \
             AND (EXISTS (SELECT 1 FROM messages m WHERE m.session_id = s.id) \
                  OR EXISTS (SELECT 1 FROM pending_inputs p WHERE p.session_id = s.id)) \
             ORDER BY updated_at DESC, created_at DESC LIMIT ?2"
        ))?;
        let rows = stmt.query_map(params![include_children, limit], session_from)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn children(&self, parent_id: &str) -> Result<Vec<Session>> {
        let db = self.db();
        let mut stmt =
            db.prepare(&format!("SELECT {SESSION_COLS} FROM sessions WHERE parent_id = ?1 ORDER BY created_at"))?;
        let rows = stmt.query_map(params![parent_id], session_from)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn set_run_state(&self, id: &str, state: &str) -> Result<()> {
        self.db().execute("UPDATE sessions SET run_state = ?2 WHERE id = ?1", params![id, state])?;
        Ok(())
    }

    pub fn set_title(&self, id: &str, title: &str) -> Result<()> {
        self.db().execute("UPDATE sessions SET title = ?2 WHERE id = ?1", params![id, title])?;
        Ok(())
    }

    /// Hand the session to another model. The system prompt is cleared so the
    /// next call builds a fresh one (it names the model), and reasoning blocks
    /// written so far are no longer replayed: they belong to the model that
    /// wrote them. Neither costs anything, since prompt caches are per model.
    pub fn switch_model(&self, id: &str, model: &str) -> Result<()> {
        self.db().execute(
            "UPDATE sessions SET model = ?2, system_prompt = '', \
             strip_thinking_before = (SELECT COALESCE(MAX(m.id), 0) + 1 FROM messages m WHERE m.session_id = ?1) \
             WHERE id = ?1",
            params![id, model],
        )?;
        Ok(())
    }

    /// Move a session to another directory. Like a change of model, the system
    /// prompt is rebuilt on the next call: the project's instructions, memory
    /// and work are those of the new directory.
    pub fn switch_directory(&self, id: &str, cwd: &str) -> Result<()> {
        self.db().execute(
            "UPDATE sessions SET cwd = ?2, system_prompt = '', \
             strip_thinking_before = (SELECT COALESCE(MAX(m.id), 0) + 1 FROM messages m WHERE m.session_id = ?1) \
             WHERE id = ?1",
            params![id, cwd],
        )?;
        Ok(())
    }

    pub fn goal_set_cwd(&self, id: &str, cwd: &str) -> Result<()> {
        self.db().execute("UPDATE goals SET cwd = ?2 WHERE id = ?1", params![id, cwd])?;
        Ok(())
    }

    pub fn set_perm_mode(&self, id: &str, mode: &str) -> Result<()> {
        self.db().execute("UPDATE sessions SET perm_mode = ?2 WHERE id = ?1", params![id, mode])?;
        Ok(())
    }

    pub fn set_system_prompt(&self, id: &str, prompt: &str) -> Result<()> {
        self.db().execute("UPDATE sessions SET system_prompt = ?2 WHERE id = ?1", params![id, prompt])?;
        Ok(())
    }

    pub fn set_strip_thinking_before(&self, id: &str, message_id: i64) -> Result<()> {
        self.db().execute(
            "UPDATE sessions SET strip_thinking_before = ?2 WHERE id = ?1",
            params![id, message_id],
        )?;
        Ok(())
    }

    /// Write down a request to a model that was answered.
    pub fn usage_add(&self, call: &ModelCall) -> Result<()> {
        self.db().execute(
            "INSERT INTO model_usage (session_id, root_id, request_id, purpose, provider, model, input, cache_read, cache_write, \
             output, estimated, started_ms, dur_ms) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
            params![
                call.session,
                call.root,
                call.request,
                call.purpose,
                call.provider,
                call.model,
                call.input,
                call.cache_read,
                call.cache_write,
                call.output,
                call.estimated,
                call.started_ms,
                call.dur_ms
            ],
        )?;
        Ok(())
    }

    /// Every answered request to a model in a session tree, in the order they were made.
    pub fn usage_for_root(&self, root: &str) -> Result<Vec<ModelCall>> {
        let db = self.db();
        let mut q = db.prepare(
            "SELECT session_id, root_id, request_id, purpose, provider, model, input, cache_read, cache_write, output, estimated, \
             started_ms, dur_ms FROM model_usage WHERE root_id = ?1 ORDER BY id",
        )?;
        let rows = q.query_map(params![root], |r| {
            Ok(ModelCall {
                session: r.get(0)?,
                root: r.get(1)?,
                request: r.get(2)?,
                purpose: r.get(3)?,
                provider: r.get(4)?,
                model: r.get(5)?,
                input: r.get(6)?,
                cache_read: r.get(7)?,
                cache_write: r.get(8)?,
                output: r.get(9)?,
                estimated: r.get(10)?,
                started_ms: r.get(11)?,
                dur_ms: r.get(12)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// The message that asked for the work a session is doing now: the last
    /// thing said to it that is not a tool's result or a note of narsil's own.
    /// 0 when nothing has been asked (a goal's session, which is given notes).
    pub fn current_request(&self, session: &str) -> Result<i64> {
        Ok(self.db().query_row(
            "SELECT COALESCE(MAX(id), 0) FROM messages WHERE session_id = ?1 AND role = 'user' AND kind = 'message' \
             AND blocks NOT LIKE '%\"type\":\"tool_result\"%'",
            params![session],
            |r| r.get(0),
        )?)
    }

    /// A tool call by its id: (the tool, what it was called with, what came back, whether that was a failure).
    pub fn tool_call(&self, call: &str) -> Result<Option<(String, Value, Option<String>, bool)>> {
        Ok(self
            .db()
            .query_row("SELECT name, input, result, is_error FROM tool_calls WHERE id = ?1", params![call], |r| {
                let input: String = r.get(1)?;
                Ok((r.get(0)?, serde_json::from_str(&input).unwrap_or(Value::Null), r.get(2)?, r.get::<_, i64>(3)? != 0))
            })
            .optional()?)
    }

    /// The messages with these ids, as their stored blocks.
    pub fn message_blocks(&self, ids: &[i64]) -> Result<Vec<(i64, String)>> {
        let db = self.db();
        let mut q = db.prepare("SELECT blocks FROM messages WHERE id = ?1")?;
        let mut out = Vec::new();
        for id in ids {
            if let Some(blocks) = q.query_row(params![id], |r| r.get::<_, String>(0)).optional()? {
                out.push((*id, blocks));
            }
        }
        Ok(out)
    }

    pub fn add_usage(&self, id: &str, input: i64, output: i64, context: i64) -> Result<()> {
        self.db().execute(
            "UPDATE sessions SET input_tokens = input_tokens + ?2, output_tokens = output_tokens + ?3, \
             context_tokens = ?4 WHERE id = ?1",
            params![id, input, output, context],
        )?;
        Ok(())
    }

    /// Called once at daemon start: nothing can be running yet, so any session
    /// still marked running was cut off by a crash or a kill.
    pub fn mark_running_as_interrupted(&self) -> Result<Vec<Session>> {
        let db = self.db();
        let mut stmt = db.prepare(&format!("SELECT {SESSION_COLS} FROM sessions WHERE run_state = 'running'"))?;
        let found: Vec<Session> = stmt.query_map([], session_from)?.collect::<rusqlite::Result<_>>()?;
        db.execute("UPDATE sessions SET run_state = 'interrupted' WHERE run_state = 'running'", [])?;
        Ok(found)
    }

    pub fn interrupted_sessions(&self) -> Result<Vec<Session>> {
        let db = self.db();
        let mut stmt =
            db.prepare(&format!("SELECT {SESSION_COLS} FROM sessions WHERE run_state = 'interrupted'"))?;
        let rows = stmt.query_map([], session_from)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Delete a session, its subagents and everything they own.
    pub fn delete_session(&self, id: &str) -> Result<()> {
        let mut ids = vec![id.to_string()];
        let mut i = 0;
        while i < ids.len() {
            for c in self.children(&ids[i])? {
                ids.push(c.id);
            }
            i += 1;
        }
        let mut db = self.db();
        let tx = db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        for sid in &ids {
            tx.execute("DELETE FROM messages WHERE session_id = ?1", params![sid])?;
            tx.execute("DELETE FROM tool_calls WHERE session_id = ?1", params![sid])?;
            tx.execute("DELETE FROM pending_inputs WHERE session_id = ?1", params![sid])?;
            tx.execute("DELETE FROM objectives WHERE session_id = ?1", params![sid])?;
            tx.execute("DELETE FROM subagents WHERE session_id = ?1", params![sid])?;
            tx.execute("DELETE FROM spans WHERE session = ?1", params![sid])?;
            tx.execute("DELETE FROM work_events WHERE session = ?1", params![sid])?;
            tx.execute("DELETE FROM memories WHERE scope = ?1", params![format!("task:{sid}")])?;
            tx.execute("DELETE FROM sessions WHERE id = ?1", params![sid])?;
        }
        tx.commit()?;
        Ok(())
    }

    // ---- messages ---------------------------------------------------------

    pub fn append_message(&self, session_id: &str, role: Role, kind: MsgKind, blocks: &[Block]) -> Result<StoredMessage> {
        let ts = now();
        let db = self.db();
        db.execute(
            "INSERT INTO messages (session_id, role, kind, blocks, created_at) VALUES (?1,?2,?3,?4,?5)",
            params![session_id, role.as_str(), kind.as_str(), serde_json::to_string(blocks)?, ts],
        )?;
        let id = db.last_insert_rowid();
        db.execute("UPDATE sessions SET updated_at = ?2 WHERE id = ?1", params![session_id, ts])?;
        Ok(StoredMessage { id, role, kind, blocks: blocks.to_vec(), created_at: ts })
    }

    /// Store a model turn and, in the same transaction, journal each tool call
    /// it requests as `pending`. After this commits, a crash at any point can
    /// be recovered: the journal says exactly which calls ran.
    pub fn append_assistant(&self, session_id: &str, blocks: &[Block]) -> Result<StoredMessage> {
        let ts = now();
        let mut db = self.db();
        let tx = db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        tx.execute(
            "INSERT INTO messages (session_id, role, kind, blocks, created_at) VALUES (?1,'assistant','message',?2,?3)",
            params![session_id, serde_json::to_string(blocks)?, ts],
        )?;
        let id = tx.last_insert_rowid();
        let mut seq = 0i64;
        for b in blocks {
            if let Block::ToolUse { id: call_id, name, input, .. } = b {
                tx.execute(
                    "INSERT INTO tool_calls (session_id, message_id, seq, id, name, input) VALUES (?1,?2,?3,?4,?5,?6)",
                    params![session_id, id, seq, call_id, name, serde_json::to_string(input)?],
                )?;
                seq += 1;
            }
        }
        tx.execute("UPDATE sessions SET updated_at = ?2 WHERE id = ?1", params![session_id, ts])?;
        tx.commit()?;
        Ok(StoredMessage { id, role: Role::Assistant, kind: MsgKind::Message, blocks: blocks.to_vec(), created_at: ts })
    }

    pub fn messages_from(&self, session_id: &str, from_id: i64) -> Result<Vec<StoredMessage>> {
        let db = self.db();
        let mut stmt = db.prepare(
            "SELECT id, role, kind, blocks, created_at FROM messages WHERE session_id = ?1 AND id >= ?2 ORDER BY id",
        )?;
        let rows = stmt.query_map(params![session_id, from_id], message_from)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// What the user last asked of a session, in their words: the first line of it.
    pub fn last_request(&self, session_id: &str) -> Option<String> {
        let db = self.db();
        let mut stmt = db
            .prepare("SELECT blocks FROM messages WHERE session_id = ?1 AND role = 'user' AND kind = 'message' ORDER BY id DESC LIMIT 20")
            .ok()?;
        let rows = stmt.query_map(params![session_id], |r| r.get::<_, String>(0)).ok()?;
        for blocks in rows.flatten() {
            let parsed: Value = serde_json::from_str(&blocks).unwrap_or_default();
            let text = parsed.as_array().and_then(|list| list.iter().find_map(|b| b.get("text").and_then(Value::as_str)));
            if let Some(line) = text.and_then(|t| t.lines().map(str::trim).find(|l| !l.is_empty())) {
                return Some(line.to_string());
            }
        }
        None
    }

    pub fn last_message(&self, session_id: &str) -> Result<Option<StoredMessage>> {
        let db = self.db();
        let m = db
            .query_row(
                "SELECT id, role, kind, blocks, created_at FROM messages WHERE session_id = ?1 ORDER BY id DESC LIMIT 1",
                params![session_id],
                message_from,
            )
            .optional()?;
        Ok(m)
    }

    pub fn message_count(&self, session_id: &str) -> Result<i64> {
        Ok(self.db().query_row(
            "SELECT COUNT(*) FROM messages WHERE session_id = ?1",
            params![session_id],
            |r| r.get(0),
        )?)
    }

    /// Start a new context epoch: the summary replaces everything before it,
    /// and the system prompt is re-snapshotted in the same transaction.
    pub fn compact(&self, session_id: &str, summary: &str, system_prompt: &str, est_tokens: i64) -> Result<StoredMessage> {
        let ts = now();
        let blocks = vec![Block::Text { text: summary.to_string() }];
        let mut db = self.db();
        let tx = db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        tx.execute(
            "INSERT INTO messages (session_id, role, kind, blocks, created_at) VALUES (?1,'user','compaction',?2,?3)",
            params![session_id, serde_json::to_string(&blocks)?, ts],
        )?;
        let id = tx.last_insert_rowid();
        tx.execute(
            "UPDATE sessions SET epoch_start = ?2, system_prompt = ?3, context_tokens = ?4, \
             strip_thinking_before = 0, updated_at = ?5 WHERE id = ?1",
            params![session_id, id, system_prompt, est_tokens, ts],
        )?;
        tx.commit()?;
        Ok(StoredMessage { id, role: Role::User, kind: MsgKind::Compaction, blocks, created_at: ts })
    }

    // ---- tool journal -----------------------------------------------------

    pub fn tool_calls(&self, session_id: &str, message_id: i64) -> Result<Vec<ToolCall>> {
        let db = self.db();
        let mut stmt = db.prepare(&format!(
            "SELECT {TOOL_COLS} FROM tool_calls WHERE session_id = ?1 AND message_id = ?2 ORDER BY seq"
        ))?;
        let rows = stmt.query_map(params![session_id, message_id], tool_call_from)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn tool_call_start(&self, session_id: &str, call: &ToolCall) -> Result<()> {
        self.db().execute(
            "UPDATE tool_calls SET status = 'running', started_at = ?4 WHERE session_id = ?1 AND message_id = ?2 AND seq = ?3",
            params![session_id, call.message_id, call.seq, now()],
        )?;
        Ok(())
    }

    /// Journal progress inside a running tool so it can be resumed after a crash.
    pub fn tool_call_set_state(&self, session_id: &str, call: &ToolCall, state: &Value) -> Result<()> {
        self.db().execute(
            "UPDATE tool_calls SET state = ?4 WHERE session_id = ?1 AND message_id = ?2 AND seq = ?3",
            params![session_id, call.message_id, call.seq, serde_json::to_string(state)?],
        )?;
        Ok(())
    }

    pub fn tool_call_finish(&self, session_id: &str, call: &ToolCall, result: &str, is_error: bool) -> Result<()> {
        self.db().execute(
            "UPDATE tool_calls SET status = 'done', result = ?4, is_error = ?5, finished_at = ?6 \
             WHERE session_id = ?1 AND message_id = ?2 AND seq = ?3",
            params![session_id, call.message_id, call.seq, result, is_error as i64, now()],
        )?;
        Ok(())
    }

    /// The journaled state of every call still marked as running, with its
    /// tool: after a crash, what the dead daemon had in hand.
    /// The calls to the file tools made by a session and its subagents, in
    /// order: tool, input, status, whether it failed, how its result begins,
    /// the message that asked for it, the session, that session's directory,
    /// and the file as it resolved when the call ran (if that was recorded).
    #[allow(clippy::type_complexity)]
    pub fn file_calls(&self, root: &str) -> Result<Vec<(String, Value, String, bool, String, i64, String, String, Option<String>)>> {
        let db = self.db();
        let mut stmt = db.prepare(
            "SELECT t.name, t.input, t.status, t.is_error, substr(COALESCE(t.result, ''), 1, 24), t.message_id, s.id, s.cwd, json_extract(t.state, '$.path') \
             FROM tool_calls t JOIN sessions s ON s.id = t.session_id \
             WHERE s.root_id = ?1 AND t.name IN ('read_file', 'write_file', 'edit_file') ORDER BY t.rowid",
        )?;
        let rows = stmt.query_map(params![root], |r| {
            let input: String = r.get(1)?;
            Ok((
                r.get(0)?,
                serde_json::from_str(&input).unwrap_or(Value::Null),
                r.get(2)?,
                r.get::<_, i64>(3)? != 0,
                r.get(4)?,
                r.get(5)?,
                r.get(6)?,
                r.get(7)?,
                r.get(8)?,
            ))
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// The same calls with all of their input and when they ran, latest first:
    /// for showing what was done to one file. Each: tool, input, status,
    /// whether it failed, how its result begins, when it started, the message
    /// that asked for it, the session, the subagent's name if it was one, that
    /// session's directory, and the file as it resolved then.
    #[allow(clippy::type_complexity)]
    pub fn file_history(&self, root: &str) -> Result<Vec<(String, Value, String, bool, String, i64, i64, String, String, String, Option<String>)>> {
        let db = self.db();
        let mut stmt = db.prepare(
            "SELECT t.name, t.input, t.status, t.is_error, substr(COALESCE(t.result, ''), 1, 200), COALESCE(t.started_at, 0), t.message_id, \
             s.id, COALESCE(s.agent_name, ''), s.cwd, json_extract(t.state, '$.path') \
             FROM tool_calls t JOIN sessions s ON s.id = t.session_id \
             WHERE s.root_id = ?1 AND t.name IN ('read_file', 'write_file', 'edit_file') ORDER BY t.rowid DESC",
        )?;
        let rows = stmt.query_map(params![root], |r| {
            let input: String = r.get(1)?;
            Ok((
                r.get(0)?,
                serde_json::from_str(&input).unwrap_or(Value::Null),
                r.get(2)?,
                r.get::<_, i64>(3)? != 0,
                r.get(4)?,
                r.get(5)?,
                r.get(6)?,
                r.get(7)?,
                r.get(8)?,
                r.get(9)?,
                r.get(10)?,
            ))
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn running_tool_states(&self) -> Result<Vec<(i64, String, Value)>> {
        let db = self.db();
        let mut stmt =
            db.prepare("SELECT rowid, name, state FROM tool_calls WHERE status = 'running' AND state IS NOT NULL")?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?)))?;
        let mut out = Vec::new();
        for row in rows {
            let (rowid, name, state) = row?;
            if let Ok(state) = serde_json::from_str(&state) {
                out.push((rowid, name, state));
            }
        }
        Ok(out)
    }

    /// Note on a journaled call that its command was stopped during recovery,
    /// so that whoever resumes the session can be told.
    pub fn tool_call_mark_stopped(&self, rowid: i64) -> Result<()> {
        self.db().execute("UPDATE tool_calls SET state = json_set(state, '$.stopped', 1) WHERE rowid = ?1", params![rowid])?;
        Ok(())
    }

    /// Resolve every call that has not finished with `reason` as its result.
    pub fn abort_unfinished_tools(&self, session_id: &str, message_id: i64, reason: &str) -> Result<()> {
        self.db().execute(
            "UPDATE tool_calls SET status = 'done', result = ?3, is_error = 1, finished_at = ?4 \
             WHERE session_id = ?1 AND message_id = ?2 AND status != 'done'",
            params![session_id, message_id, reason, now()],
        )?;
        Ok(())
    }

    /// Turn a fully resolved batch of tool calls into the user turn that
    /// carries their results. All results go in one message, in call order.
    pub fn commit_tool_results(&self, session_id: &str, message_id: i64) -> Result<StoredMessage> {
        let calls = self.tool_calls(session_id, message_id)?;
        if let Some(c) = calls.iter().find(|c| c.status != "done") {
            bail!("tool call {} ({}) has not finished", c.id, c.name);
        }
        let blocks: Vec<Block> = calls
            .into_iter()
            .map(|c| Block::ToolResult {
                tool_use_id: c.id,
                content: c.result.unwrap_or_default(),
                is_error: c.is_error,
            })
            .collect();
        self.append_message(session_id, Role::User, MsgKind::Message, &blocks)
    }

    // ---- pending inputs ---------------------------------------------------

    pub fn pending_add(&self, session_id: &str, text: &str) -> Result<()> {
        let ts = now();
        let db = self.db();
        db.execute(
            "INSERT INTO pending_inputs (session_id, text, created_at) VALUES (?1,?2,?3)",
            params![session_id, text, ts],
        )?;
        db.execute("UPDATE sessions SET updated_at = ?2 WHERE id = ?1", params![session_id, ts])?;
        Ok(())
    }

    pub fn pending_count(&self, session_id: &str) -> Result<i64> {
        Ok(self.db().query_row(
            "SELECT COUNT(*) FROM pending_inputs WHERE session_id = ?1",
            params![session_id],
            |r| r.get(0),
        )?)
    }

    /// Move queued user input into the transcript, atomically.
    pub fn pending_deliver(&self, session_id: &str) -> Result<Vec<StoredMessage>> {
        let ts = now();
        let mut db = self.db();
        let tx = db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let texts: Vec<String> = {
            let mut stmt = tx.prepare("SELECT text FROM pending_inputs WHERE session_id = ?1 ORDER BY id")?;
            stmt.query_map(params![session_id], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?
        };
        let mut out = Vec::new();
        for text in texts {
            let blocks = vec![Block::Text { text }];
            tx.execute(
                "INSERT INTO messages (session_id, role, kind, blocks, created_at) VALUES (?1,'user','message',?2,?3)",
                params![session_id, serde_json::to_string(&blocks)?, ts],
            )?;
            out.push(StoredMessage {
                id: tx.last_insert_rowid(),
                role: Role::User,
                kind: MsgKind::Message,
                blocks,
                created_at: ts,
            });
        }
        tx.execute("DELETE FROM pending_inputs WHERE session_id = ?1", params![session_id])?;
        tx.commit()?;
        Ok(out)
    }

    // ---- memory -----------------------------------------------------------

    /// Insert, or update the entry with the same scope and title.
    pub fn memory_save(&self, scope: &str, kind: &str, title: &str, body: &str, source: Option<&str>) -> Result<i64> {
        let ts = now();
        let db = self.db();
        db.execute(
            "INSERT INTO memories (scope, kind, title, body, source_session, created_at, updated_at) \
             VALUES (?1,?2,?3,?4,?5,?6,?6) \
             ON CONFLICT(scope, title) DO UPDATE SET body = excluded.body, kind = excluded.kind, \
             source_session = excluded.source_session, updated_at = excluded.updated_at",
            params![scope, kind, title, body, source, ts],
        )?;
        Ok(db.query_row(
            "SELECT id FROM memories WHERE scope = ?1 AND title = ?2",
            params![scope, title],
            |r| r.get(0),
        )?)
    }

    pub fn memory_list(&self, scopes: &[String], limit: i64) -> Result<Vec<Memory>> {
        let db = self.db();
        let (filter, mut vals) = scope_filter(scopes);
        vals.push(SqlValue::Integer(limit));
        let sql = format!(
            "SELECT {MEMORY_COLS} FROM memories m WHERE {filter} AND m.pending != 2 \
             ORDER BY m.updated_at DESC, m.id DESC LIMIT ?"
        );
        let mut stmt = db.prepare(&sql)?;
        let rows = stmt.query_map(params_from_iter(vals.iter()), memory_from)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Full-text search, best match first. Falls back to substring matching
    /// when the query has no indexable terms.
    pub fn memory_search(&self, query: &str, scopes: &[String], limit: i64) -> Result<Vec<Memory>> {
        let terms: Vec<String> = query
            .split(|c: char| !c.is_alphanumeric())
            .filter(|t| t.len() > 1)
            .map(|t| format!("\"{t}\""))
            .collect();
        let db = self.db();
        let (filter, scope_vals) = scope_filter(scopes);
        if !terms.is_empty() {
            let mut vals = vec![SqlValue::Text(terms.join(" OR "))];
            vals.extend(scope_vals.iter().cloned());
            vals.push(SqlValue::Integer(limit));
            let sql = format!(
                "SELECT {MEMORY_COLS} FROM memories_fts f JOIN memories m ON m.id = f.rowid \
                 WHERE memories_fts MATCH ? AND {filter} AND m.pending != 2 ORDER BY bm25(memories_fts) LIMIT ?"
            );
            let mut stmt = db.prepare(&sql)?;
            let found: rusqlite::Result<Vec<Memory>> =
                stmt.query_map(params_from_iter(vals.iter()), memory_from)?.collect();
            if let Ok(found) = found {
                if !found.is_empty() {
                    return Ok(found);
                }
            }
        }
        let mut vals = vec![SqlValue::Text(format!("%{}%", query.trim()))];
        vals.extend(scope_vals);
        vals.push(SqlValue::Integer(limit));
        let sql = format!(
            "SELECT {MEMORY_COLS} FROM memories m WHERE (m.title LIKE ?1 OR m.body LIKE ?1) AND {filter} AND m.pending != 2 \
             ORDER BY m.updated_at DESC LIMIT ?"
        );
        let mut stmt = db.prepare(&sql)?;
        let rows = stmt.query_map(params_from_iter(vals.iter()), memory_from)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn memory_get(&self, id: i64) -> Result<Option<Memory>> {
        let db = self.db();
        let m = db
            .query_row(&format!("SELECT {MEMORY_COLS} FROM memories m WHERE m.id = ?1"), params![id], memory_from)
            .optional()?;
        Ok(m)
    }

    /// The entry with this title in one of `scopes`, whatever its capitalisation.
    pub fn memory_find(&self, scopes: &[String], title: &str) -> Result<Option<Memory>> {
        let db = self.db();
        let (filter, scope_vals) = scope_filter(scopes);
        let mut vals = vec![SqlValue::Text(title.trim().to_string())];
        vals.extend(scope_vals);
        let sql = format!(
            "SELECT {MEMORY_COLS} FROM memories m WHERE m.title = ?1 COLLATE NOCASE AND {filter} AND m.pending != 2 \
             ORDER BY m.updated_at DESC LIMIT 1"
        );
        let m = db.query_row(&sql, params_from_iter(vals.iter()), memory_from).optional()?;
        Ok(m)
    }

    /// Global and project memory: everything that outlives a task and lives in a note.
    pub fn memory_durable(&self) -> Result<Vec<Memory>> {
        let db = self.db();
        let mut stmt = db.prepare(&format!(
            "SELECT {MEMORY_COLS} FROM memories m WHERE m.scope = 'global' OR m.scope LIKE 'project:%' ORDER BY m.id"
        ))?;
        let rows = stmt.query_map([], memory_from)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Record what a note holds (or, with `pending`, what one is going to hold),
    /// replacing the entry with the same scope and title, and its links.
    pub fn memory_index_put(&self, e: &IndexEntry<'_>) -> Result<i64> {
        let mut db = self.db();
        let tx = db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        tx.execute(
            "INSERT INTO memories (scope, kind, title, body, created_at, updated_at, path, mtime, pending) \
             VALUES (?1,?2,?3,?4,?5,?5,?6,?7,?8) \
             ON CONFLICT(scope, title) DO UPDATE SET kind = excluded.kind, body = excluded.body, \
             updated_at = excluded.updated_at, path = excluded.path, mtime = excluded.mtime, pending = excluded.pending",
            params![e.scope, e.kind, e.title, e.body, e.updated_at, e.path, e.mtime, e.pending],
        )?;
        let id: i64 = tx.query_row(
            "SELECT id FROM memories WHERE scope = ?1 AND title = ?2",
            params![e.scope, e.title],
            |r| r.get(0),
        )?;
        tx.execute("DELETE FROM memory_links WHERE from_id = ?1", params![id])?;
        for target in e.links {
            tx.execute("INSERT INTO memory_links (from_id, target) VALUES (?1,?2)", params![id, target])?;
        }
        tx.commit()?;
        Ok(id)
    }

    /// The entry's note was written: it is in step again.
    pub fn memory_set_file(&self, id: i64, path: &str, mtime: i64) -> Result<()> {
        self.db().execute(
            "UPDATE memories SET path = ?2, mtime = ?3, pending = 0 WHERE id = ?1",
            params![id, path, mtime],
        )?;
        Ok(())
    }

    pub fn memory_set_pending(&self, id: i64, pending: i64) -> Result<()> {
        self.db().execute("UPDATE memories SET pending = ?2 WHERE id = ?1", params![id, pending])?;
        Ok(())
    }

    /// The memory folder changed: every entry has to be written to the new one.
    pub fn memory_mark_all_pending(&self) -> Result<usize> {
        Ok(self.db().execute(
            "UPDATE memories SET path = NULL, mtime = 0, pending = 1 \
             WHERE (scope = 'global' OR scope LIKE 'project:%') AND pending != 2",
            [],
        )?)
    }

    /// Titles of the entries in `scopes` that link to `title`.
    pub fn memory_backlinks(&self, title: &str, scopes: &[String]) -> Result<Vec<String>> {
        let db = self.db();
        let (filter, scope_vals) = scope_filter(scopes);
        let mut vals = vec![SqlValue::Text(title.trim().to_string())];
        vals.extend(scope_vals);
        let sql = format!(
            "SELECT DISTINCT m.title FROM memory_links l JOIN memories m ON m.id = l.from_id \
             WHERE l.target = ?1 COLLATE NOCASE AND {filter} AND m.pending != 2 ORDER BY m.title"
        );
        let mut stmt = db.prepare(&sql)?;
        let rows = stmt.query_map(params_from_iter(vals.iter()), |r| r.get(0))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn memory_delete(&self, id: i64) -> Result<bool> {
        let db = self.db();
        db.execute("DELETE FROM memory_links WHERE from_id = ?1", params![id])?;
        Ok(db.execute("DELETE FROM memories WHERE id = ?1", params![id])? > 0)
    }

    pub fn meta_get(&self, key: &str) -> Result<Option<String>> {
        let db = self.db();
        Ok(db.query_row("SELECT value FROM meta WHERE key = ?1", params![key], |r| r.get(0)).optional()?)
    }

    pub fn meta_set(&self, key: &str, value: &str) -> Result<()> {
        self.db().execute(
            "INSERT INTO meta (key, value) VALUES (?1,?2) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    // ---- goals ------------------------------------------------------------

    pub fn goal_create(&self, objective: &str, cwd: &str, model: &str) -> Result<Goal> {
        let id = new_id();
        let ts = now();
        self.db().execute(
            "INSERT INTO goals (id, objective, cwd, model, status, created_at, updated_at) VALUES (?1,?2,?3,?4,'active',?5,?5)",
            params![id, objective, cwd, model, ts],
        )?;
        self.goal_log(&id, "status", "created")?;
        self.goal_get(&id)?.ok_or_else(|| anyhow!("goal vanished after insert"))
    }

    pub fn goal_get(&self, id: &str) -> Result<Option<Goal>> {
        let db = self.db();
        let g = db
            .query_row(&format!("SELECT {GOAL_COLS} FROM goals WHERE id = ?1"), params![id], goal_from)
            .optional()?;
        Ok(g)
    }

    pub fn goal_list(&self, include_finished: bool) -> Result<Vec<Goal>> {
        let db = self.db();
        let mut stmt = db.prepare(&format!(
            "SELECT {GOAL_COLS} FROM goals WHERE (?1 OR status IN ('active','paused','blocked')) ORDER BY created_at DESC"
        ))?;
        let rows = stmt.query_map(params![include_finished], goal_from)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn goal_set_status(&self, id: &str, status: &str, note: &str) -> Result<()> {
        self.db().execute(
            "UPDATE goals SET status = ?2, note = ?3, updated_at = ?4 WHERE id = ?1",
            params![id, status, note, now()],
        )?;
        self.goal_log(id, "status", &if note.is_empty() { status.to_string() } else { format!("{status}: {note}") })
    }

    pub fn goal_set_session(&self, id: &str, session_id: &str) -> Result<()> {
        self.db().execute("UPDATE goals SET session_id = ?2 WHERE id = ?1", params![id, session_id])?;
        Ok(())
    }

    pub fn goal_bump_iteration(&self, id: &str) -> Result<()> {
        self.db().execute(
            "UPDATE goals SET iterations = iterations + 1, updated_at = ?2 WHERE id = ?1",
            params![id, now()],
        )?;
        Ok(())
    }

    pub fn goal_log(&self, id: &str, kind: &str, text: &str) -> Result<()> {
        // The journal is shown, and written into the goal's note: no credentials in it.
        let text = &crate::secrets::kept(text);
        self.db().execute(
            "INSERT INTO goal_log (goal_id, ts, kind, text) VALUES (?1,?2,?3,?4)",
            params![id, now(), kind, text],
        )?;
        Ok(())
    }

    /// Most recent log entries, oldest first.
    pub fn goal_logs(&self, id: &str, limit: i64) -> Result<Vec<GoalLog>> {
        let db = self.db();
        let mut stmt = db.prepare(
            "SELECT ts, kind, text FROM (SELECT id, ts, kind, text FROM goal_log WHERE goal_id = ?1 \
             ORDER BY id DESC LIMIT ?2) ORDER BY id",
        )?;
        let rows = stmt.query_map(params![id, limit], |r| {
            Ok(GoalLog { ts: r.get(0)?, kind: r.get(1)?, text: r.get(2)? })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    // ---- subagents --------------------------------------------------------

    pub fn subagent_add(&self, session_id: &str, spawn_call: &str, task: &str, objective: &str) -> Result<()> {
        self.db().execute(
            "INSERT OR IGNORE INTO subagents (session_id, spawn_call, task, objective) VALUES (?1,?2,?3,?4)",
            params![session_id, spawn_call, task, objective],
        )?;
        Ok(())
    }

    /// The agent is starting, or starting again after an interruption. A
    /// stretch that a crash cut short is counted up to the session's last write.
    pub fn subagent_started(&self, session_id: &str) -> Result<()> {
        self.db().execute(
            "UPDATE subagents SET \
               worked = worked + CASE WHEN started_at > 0 AND ended_at = 0 \
                 THEN MAX(0, (SELECT updated_at FROM sessions WHERE id = ?1) - started_at) ELSE 0 END, \
               started_at = ?2, ended_at = 0, outcome = '', summary = '' WHERE session_id = ?1",
            params![session_id, now()],
        )?;
        Ok(())
    }

    pub fn subagent_finished(&self, session_id: &str, outcome: &str, summary: &str) -> Result<()> {
        self.db().execute(
            "UPDATE subagents SET \
               worked = worked + CASE WHEN started_at > 0 AND ended_at = 0 THEN MAX(0, ?2 - started_at) ELSE 0 END, \
               ended_at = ?2, outcome = ?3, summary = ?4 WHERE session_id = ?1",
            params![session_id, now(), outcome, summary],
        )?;
        Ok(())
    }

    fn subagents_where(&self, filter: &str, arg: &str) -> Result<Vec<AgentInfo>> {
        let db = self.db();
        let mut stmt = db.prepare(&format!(
            "SELECT s.id, s.parent_id, s.agent_name, s.model, s.input_tokens + s.output_tokens, \
                    a.task, a.objective, a.spawn_call, a.started_at, a.ended_at, a.outcome, a.summary, \
                    (SELECT COUNT(*) FROM tool_calls t WHERE t.session_id = s.id AND t.status != 'pending'), \
                    a.worked \
             FROM sessions s JOIN subagents a ON a.session_id = s.id WHERE {filter} ORDER BY s.rowid"
        ))?;
        let rows = stmt.query_map(params![arg], |r| {
            let started_at: i64 = r.get(8)?;
            let outcome: String = r.get(10)?;
            // The stored view. Whether it is running right now is the engine's to say.
            let state = if !outcome.is_empty() {
                outcome
            } else if started_at == 0 {
                "queued".to_string()
            } else {
                "interrupted".to_string()
            };
            Ok(AgentInfo {
                id: r.get(0)?,
                parent: r.get::<_, Option<String>>(1)?.unwrap_or_default(),
                name: r.get::<_, Option<String>>(2)?.unwrap_or_default(),
                model: r.get(3)?,
                tokens: r.get(4)?,
                task: r.get(5)?,
                objective: r.get(6)?,
                spawn_call: r.get(7)?,
                started_at,
                ended_at: r.get(9)?,
                state,
                summary: r.get(11)?,
                calls: r.get(12)?,
                worked: r.get(13)?,
                activity: String::new(),
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Every subagent in a session tree, in the order they were created.
    pub fn subagents(&self, root_id: &str) -> Result<Vec<AgentInfo>> {
        self.subagents_where("s.root_id = ?1", root_id)
    }

    pub fn subagent_get(&self, session_id: &str) -> Result<Option<AgentInfo>> {
        Ok(self.subagents_where("s.id = ?1", session_id)?.into_iter().next())
    }

    /// The tool call a session is in the middle of, if any: (name, input).
    pub fn running_tool(&self, session_id: &str) -> Result<Option<(String, Value)>> {
        let db = self.db();
        let found: Option<(String, String)> = db
            .query_row(
                "SELECT name, input FROM tool_calls WHERE session_id = ?1 AND status = 'running' \
                 ORDER BY message_id DESC, seq DESC LIMIT 1",
                params![session_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        Ok(found.map(|(name, input)| (name, serde_json::from_str(&input).unwrap_or(Value::Null))))
    }

    // ---- recorded spans ---------------------------------------------------

    fn spans_where(&self, filter: &str, args: &[SqlValue]) -> Result<Vec<SpanRow>> {
        let db = self.db();
        let mut stmt = db.prepare(&format!("SELECT {SPAN_COLS} FROM spans WHERE {filter} ORDER BY start_ms, id"))?;
        let rows = stmt.query_map(params_from_iter(args.iter()), span_from)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Everything recorded for one session tree, oldest first.
    pub fn spans_for_root(&self, root: &str) -> Result<Vec<SpanRow>> {
        self.spans_where("root = ?1", &[SqlValue::Text(root.to_string())])
    }

    pub fn spans_since(&self, start_ms: i64) -> Result<Vec<SpanRow>> {
        self.spans_where("start_ms >= ?1", &[SqlValue::Integer(start_ms)])
    }

    /// What is in flight right now.
    pub fn spans_open(&self) -> Result<Vec<SpanRow>> {
        self.spans_where("dur_ms IS NULL", &[])
    }

    /// The session tree that most recently did anything.
    /// The session tree that used a model most recently.
    pub fn latest_usage_root(&self) -> Result<Option<String>> {
        Ok(self.db().query_row("SELECT root_id FROM model_usage ORDER BY id DESC LIMIT 1", [], |r| r.get(0)).optional()?)
    }

    pub fn latest_span_root(&self) -> Result<Option<String>> {
        let db = self.db();
        Ok(db
            .query_row("SELECT root FROM spans WHERE root != '' ORDER BY start_ms DESC, id DESC LIMIT 1", [], |r| r.get(0))
            .optional()?)
    }

    // ---- input history ----------------------------------------------------

    pub fn history_add(&self, text: &str, cwd: &str) -> Result<()> {
        self.db().execute(
            "INSERT INTO input_history (text, cwd, ts) VALUES (?1,?2,?3)",
            params![text, cwd, now()],
        )?;
        Ok(())
    }

    /// Recent inputs, oldest first.
    pub fn history_recent(&self, limit: i64) -> Result<Vec<String>> {
        let db = self.db();
        let mut stmt =
            db.prepare("SELECT text FROM (SELECT id, text FROM input_history ORDER BY id DESC LIMIT ?1) ORDER BY id")?;
        let rows = stmt.query_map(params![limit], |r| r.get(0))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }
}

fn scope_filter(scopes: &[String]) -> (String, Vec<SqlValue>) {
    if scopes.is_empty() {
        return ("1=1".to_string(), Vec::new());
    }
    let marks = vec!["?"; scopes.len()].join(",");
    (format!("m.scope IN ({marks})"), scopes.iter().map(|s| SqlValue::Text(s.clone())).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn session(store: &Store) -> Session {
        store
            .create_session(NewSession {
                kind: "chat".into(),
                cwd: "/work".into(),
                model: "mock/m".into(),
                perm_mode: "auto".into(),
                ..Default::default()
            })
            .unwrap()
    }

    #[test]
    fn tool_journal_survives_and_commits_in_order() {
        let store = Store::open_memory().unwrap();
        let s = session(&store);
        store.pending_add(&s.id, "do two things").unwrap();
        assert_eq!(store.pending_deliver(&s.id).unwrap().len(), 1);
        assert_eq!(store.pending_count(&s.id).unwrap(), 0);

        let turn = store
            .append_assistant(
                &s.id,
                &[
                    Block::Text { text: "on it".into() },
                    Block::ToolUse { id: "a".into(), name: "bash".into(), input: json!({"command": "true"}), extra: None },
                    Block::ToolUse { id: "b".into(), name: "bash".into(), input: json!({"command": "false"}), extra: None },
                ],
            )
            .unwrap();
        let calls = store.tool_calls(&s.id, turn.id).unwrap();
        assert_eq!(calls.len(), 2);
        assert!(calls.iter().all(|c| c.status == "pending"));

        // A batch with an unfinished call cannot be committed.
        store.tool_call_start(&s.id, &calls[0]).unwrap();
        store.tool_call_finish(&s.id, &calls[0], "ok", false).unwrap();
        assert!(store.commit_tool_results(&s.id, turn.id).is_err());

        store.abort_unfinished_tools(&s.id, turn.id, "not run").unwrap();
        let results = store.commit_tool_results(&s.id, turn.id).unwrap();
        match &results.blocks[..] {
            [
                Block::ToolResult { tool_use_id: a, is_error: false, .. },
                Block::ToolResult { tool_use_id: b, is_error: true, content },
            ] => {
                assert_eq!((a.as_str(), b.as_str()), ("a", "b"));
                assert_eq!(content, "not run");
            }
            other => panic!("unexpected blocks: {other:?}"),
        }
    }

    #[test]
    fn compaction_starts_a_new_epoch() {
        let store = Store::open_memory().unwrap();
        let s = session(&store);
        store.append_message(&s.id, Role::User, MsgKind::Message, &[Block::Text { text: "old".into() }]).unwrap();
        store.set_strip_thinking_before(&s.id, 99).unwrap();
        let c = store.compact(&s.id, "summary", "new system", 42).unwrap();
        let s = store.require_session(&s.id).unwrap();
        assert_eq!(s.epoch_start, c.id);
        assert_eq!(s.system_prompt, "new system");
        assert_eq!(s.strip_thinking_before, 0);
        let live = store.messages_from(&s.id, s.epoch_start).unwrap();
        assert_eq!(live.len(), 1);
        assert_eq!(live[0].kind, MsgKind::Compaction);
    }

    #[test]
    fn memory_is_scoped_searchable_and_upserts() {
        let store = Store::open_memory().unwrap();
        let g = "global".to_string();
        let p = "project:/work".to_string();
        let a = store.memory_save(&g, "user", "Editor", "Prefers helix over vim", None).unwrap();
        store.memory_save(&p, "project", "Build", "Use cargo nextest for the test suite", None).unwrap();
        let again = store.memory_save(&g, "user", "Editor", "Prefers helix; never suggest vim", None).unwrap();
        assert_eq!(a, again, "same scope and title updates in place");

        let hits = store.memory_search("which editor: helix?", &[g.clone(), p.clone()], 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert!(hits[0].body.contains("never suggest vim"));

        assert!(store.memory_search("nextest", &[g.clone()], 10).unwrap().is_empty(), "scope filter applies");
        assert_eq!(store.memory_search("nextest", &[p.clone()], 10).unwrap().len(), 1);
        assert_eq!(store.memory_list(&[], 10).unwrap().len(), 2);
        assert!(store.memory_delete(a).unwrap());
        assert!(store.memory_search("helix", &[g], 10).unwrap().is_empty());
    }

    #[test]
    fn switching_model_refreshes_the_prompt_and_retires_old_reasoning() {
        let store = Store::open_memory().unwrap();
        let s = session(&store);
        store.set_system_prompt(&s.id, "You are model A").unwrap();
        let other = session(&store);
        let last = store.append_message(&s.id, Role::User, MsgKind::Message, &[Block::Text { text: "hi".into() }]).unwrap();
        store.append_message(&other.id, Role::User, MsgKind::Message, &[Block::Text { text: "elsewhere".into() }]).unwrap();
        store.switch_model(&s.id, "mock/b").unwrap();
        let after = store.require_session(&s.id).unwrap();
        assert_eq!(after.model, "mock/b");
        assert!(after.system_prompt.is_empty(), "rebuilt on the next call");
        assert_eq!(after.strip_thinking_before, last.id + 1, "only this session's messages count");
        assert_eq!(store.require_session(&other.id).unwrap().model, "mock/m");
    }

    #[test]
    fn crash_marks_running_sessions_interrupted() {
        let store = Store::open_memory().unwrap();
        let s = session(&store);
        store.set_run_state(&s.id, "running").unwrap();
        let hit = store.mark_running_as_interrupted().unwrap();
        assert_eq!(hit.len(), 1);
        assert_eq!(store.require_session(&s.id).unwrap().run_state, "interrupted");
    }

    #[test]
    fn subagent_records_follow_the_agent_from_queue_to_report() {
        let store = Store::open_memory().unwrap();
        let parent = session(&store);
        let child = store
            .create_session(NewSession {
                parent: Some(parent.clone()),
                kind: "subagent".into(),
                agent_name: Some("lexer".into()),
                cwd: "/work".into(),
                model: "mock/m".into(),
                perm_mode: "auto".into(),
                ..Default::default()
            })
            .unwrap();
        store.subagent_add(&child.id, "call_1", "write the lexer", "Tokenizer passes its tests").unwrap();
        store.subagent_add(&child.id, "call_1", "again", "").unwrap(); // a resumed call must not reset it

        let queued = store.subagents(&parent.id).unwrap();
        assert_eq!(queued.len(), 1);
        let a = &queued[0];
        assert_eq!((a.state.as_str(), a.name.as_str(), a.parent.as_str()), ("queued", "lexer", parent.id.as_str()));
        assert_eq!((a.task.as_str(), a.objective.as_str()), ("write the lexer", "Tokenizer passes its tests"));
        assert_eq!(a.elapsed(now()), 0);

        // Started but with no recorded outcome and nothing running: it was cut off.
        store.subagent_started(&child.id).unwrap();
        assert_eq!(store.subagent_get(&child.id).unwrap().unwrap().state, "interrupted");

        let turn = store
            .append_assistant(
                &child.id,
                &[Block::ToolUse { id: "t".into(), name: "bash".into(), input: json!({"command": "cargo test"}), extra: None }],
            )
            .unwrap();
        let calls = store.tool_calls(&child.id, turn.id).unwrap();
        assert!(store.running_tool(&child.id).unwrap().is_none());
        store.tool_call_start(&child.id, &calls[0]).unwrap();
        assert_eq!(store.running_tool(&child.id).unwrap().unwrap().0, "bash");

        store.subagent_finished(&child.id, "done", "14 tests pass").unwrap();
        let done = store.subagent_get(&child.id).unwrap().unwrap();
        assert_eq!((done.state.as_str(), done.summary.as_str(), done.calls), ("done", "14 tests pass", 1));
        assert!(!done.active());

        // Time is counted per stretch of work, so time spent stopped is left out.
        let db = store.db();
        db.execute("UPDATE subagents SET worked = 0, started_at = ?2 - 30, ended_at = 0 WHERE session_id = ?1", params![child.id, now()]).unwrap();
        drop(db);
        store.subagent_finished(&child.id, "interrupted", "").unwrap();
        let first = store.subagent_get(&child.id).unwrap().unwrap().worked;
        assert!((30..=31).contains(&first), "{first}");
        store.subagent_finished(&child.id, "interrupted", "").unwrap();
        assert_eq!(store.subagent_get(&child.id).unwrap().unwrap().worked, first, "an ended stretch is not counted twice");
        let db = store.db();
        db.execute("UPDATE subagents SET ended_at = ?2 - 1000 WHERE session_id = ?1", params![child.id, now()]).unwrap();
        drop(db);
        store.subagent_started(&child.id).unwrap();
        let mut resumed = store.subagent_get(&child.id).unwrap().unwrap();
        assert_eq!(resumed.worked, first);
        resumed.state = "running".into();
        assert_eq!(resumed.elapsed(now() + 5), first + 5);

        store.delete_session(&parent.id).unwrap();
        assert!(store.subagents(&parent.id).unwrap().is_empty());
    }

    #[test]
    fn ids_resolve_by_unique_prefix() {
        let store = Store::open_memory().unwrap();
        let s = session(&store);
        assert_eq!(store.resolve_session(&s.id[..4]).unwrap(), s.id);
        assert!(store.resolve_session("zzzzzzzz9").is_err());
        assert!(store.resolve_session("a%").is_err());
    }
}
