//! Telemetry: what narsil is doing, and how long each part takes.
//!
//! One set of instrumentation feeds three things:
//!
//!   - **The log.** Leveled lines on the daemon's stderr (which `narsil start`
//!     points at `daemon.log`), each prefixed with the spans it happened in, so
//!     a line says which session, model call or tool call it belongs to.
//!   - **Recorded spans.** A span whose name is in `RECORDED` is written to the
//!     `spans` table when it opens and again when it closes: every session run,
//!     model call, tool call, compaction, approval wait, retry wait and goal
//!     turn, with its timing, its parent and its fields (tokens, time to first
//!     token, errors...). Because the row is written at open, whatever was in
//!     flight when the daemon died is on record.
//!   - **Views over the recorded spans**: a timeline of one session tree, its
//!     totals, statistics over a period, a flamegraph and a Chrome trace.
//!
//! Functions are instrumented with `#[tracing::instrument]` or an explicit
//! span. Instrumenting something new and getting it recorded takes two steps:
//! give the span one of the names in `RECORDED` (or add a name there), and set
//! the fields that matter. `session`, `root`, `agent`, `goal`, `what` and `ok`
//! become columns and are inherited from the enclosing recorded span; every
//! other field goes into the row's JSON.
//!
//! Recording happens on a thread of its own with its own database connection,
//! so instrumented code never waits on the store and cannot deadlock with it.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::mpsc::{Sender, SyncSender, channel, sync_channel};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::Result;
use serde_json::{Map, Value, json};
use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id, Record};
use tracing::{Level, Subscriber};
use tracing_subscriber::filter::{EnvFilter, LevelFilter};
use tracing_subscriber::fmt::format::Writer;
use tracing_subscriber::fmt::time::FormatTime;
use tracing_subscriber::layer::{Context, SubscriberExt};
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{Layer, Registry};

use crate::config::{Config, Paths};
use crate::store::SpanRow;
use crate::util::{first_line, fmt_tokens};

/// Span names that are written to the `spans` table.
pub const RECORDED: [&str; 7] =
    ["session_run", "model_call", "tool_call", "compaction", "approval_wait", "retry_wait", "goal_turn"];

pub fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

// ---- recording ---------------------------------------------------------------

/// What the recorder keeps on a span while it is open.
struct Rec {
    id: i64,
    started: Instant,
    root: String,
    session: String,
    agent: String,
    goal: String,
    what: String,
    ok: bool,
    data: Map<String, Value>,
}

impl Rec {
    fn apply(&mut self, fields: Fields) {
        for (key, value) in fields.0 {
            let text = || match &value {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            match key.as_str() {
                "session" => self.session = text(),
                "root" => self.root = text(),
                "agent" => self.agent = text(),
                "goal" => self.goal = text(),
                "what" => self.what = text(),
                "ok" => self.ok = value.as_bool().unwrap_or(text() == "true"),
                _ => {
                    self.data.insert(key, value);
                }
            }
        }
    }
}

#[derive(Default)]
struct Fields(Vec<(String, Value)>);

impl Visit for Fields {
    fn record_str(&mut self, field: &Field, value: &str) {
        self.0.push((field.name().to_string(), json!(value)));
    }
    fn record_i64(&mut self, field: &Field, value: i64) {
        self.0.push((field.name().to_string(), json!(value)));
    }
    fn record_u64(&mut self, field: &Field, value: u64) {
        self.0.push((field.name().to_string(), json!(value)));
    }
    fn record_bool(&mut self, field: &Field, value: bool) {
        self.0.push((field.name().to_string(), json!(value)));
    }
    fn record_f64(&mut self, field: &Field, value: f64) {
        self.0.push((field.name().to_string(), json!(value)));
    }
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        // `%value` fields arrive here already formatted with Display.
        self.0.push((field.name().to_string(), json!(format!("{value:?}"))));
    }
}

enum Msg {
    Open { id: i64, parent: Option<i64>, kind: &'static str, start_ms: i64, rec: Box<Rec> },
    /// Who a span belongs to became known after it opened.
    Who { id: i64, root: String, session: String, agent: String, goal: String },
    Close { id: i64, dur_ms: i64, ok: bool, what: String, data: String },
    Flush(SyncSender<()>),
}

/// The layer that writes recorded spans.
pub struct Recorder {
    tx: Sender<Msg>,
    next: AtomicI64,
}

/// Keeps the recorder's writer reachable so pending rows can be flushed.
pub struct Guard {
    tx: Option<Sender<Msg>>,
}

impl Guard {
    /// Wait until everything recorded so far is in the database.
    pub fn flush(&self) {
        if let Some(tx) = &self.tx {
            let (done, wait) = sync_channel(1);
            if tx.send(Msg::Flush(done)).is_ok() {
                let _ = wait.recv_timeout(Duration::from_secs(5));
            }
        }
    }
}

impl Drop for Guard {
    fn drop(&mut self) {
        self.flush();
    }
}

const HEARTBEAT: &str = "telemetry_heartbeat";

fn writer(conn: rusqlite::Connection, rx: std::sync::mpsc::Receiver<Msg>) {
    // A timestamp refreshed every couple of seconds. If the daemon dies without
    // warning, it says when, which is how long the spans left open had run.
    let beat = |conn: &rusqlite::Connection| {
        let _ = conn.execute(
            "INSERT INTO meta (key, value) VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            rusqlite::params![HEARTBEAT, now_ms().to_string()],
        );
    };
    let mut last_beat = Instant::now();
    beat(&conn);
    loop {
        let msg = match rx.recv_timeout(Duration::from_secs(2)) {
            Ok(msg) => msg,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                beat(&conn);
                last_beat = Instant::now();
                continue;
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        };
        if last_beat.elapsed() >= Duration::from_secs(2) {
            beat(&conn);
            last_beat = Instant::now();
        }
        let result = match msg {
            Msg::Open { id, parent, kind, start_ms, rec } => conn.execute(
                "INSERT OR REPLACE INTO spans (id, parent, root, session, agent, goal, kind, what, start_ms, data) \
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
                rusqlite::params![
                    id,
                    parent,
                    rec.root,
                    rec.session,
                    rec.agent,
                    rec.goal,
                    kind,
                    rec.what,
                    start_ms,
                    Value::Object(rec.data).to_string()
                ],
            ),
            Msg::Who { id, root, session, agent, goal } => conn.execute(
                "UPDATE spans SET root = ?2, session = ?3, agent = ?4, goal = ?5 WHERE id = ?1",
                rusqlite::params![id, root, session, agent, goal],
            ),
            Msg::Close { id, dur_ms, ok, what, data } => conn.execute(
                "UPDATE spans SET dur_ms = ?2, ok = ?3, what = ?4, data = ?5 WHERE id = ?1",
                rusqlite::params![id, dur_ms, ok as i64, what, data],
            ),
            Msg::Flush(done) => {
                let _ = done.send(());
                Ok(0)
            }
        };
        if let Err(e) = result {
            // Not through tracing: a failure here must not feed back into itself.
            eprintln!("{} WARN telemetry: could not record a span: {e}", crate::util::timestamp());
        }
    }
}

/// Start recording spans into the database at `db`. Rows left open by a
/// previous daemon are closed off as cut short, and old rows are dropped.
pub fn recorder(db: &Path, keep_days: i64) -> Result<(Recorder, Guard)> {
    let conn = rusqlite::Connection::open(db)?;
    conn.busy_timeout(Duration::from_secs(15))?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    // Spans a previous daemon left open ran until it died: its last heartbeat,
    // or failing that the last moment anything was recorded.
    conn.execute(
        "UPDATE spans SET ok = 0, data = json_set(data, '$.cut_off', 1), dur_ms = MAX(0, MAX( \
           COALESCE((SELECT CAST(value AS INTEGER) FROM meta WHERE key = ?1), 0), \
           (SELECT MAX(start_ms + COALESCE(dur_ms, 0)) FROM spans)) - start_ms) WHERE dur_ms IS NULL",
        [HEARTBEAT],
    )?;
    if keep_days > 0 {
        conn.execute("DELETE FROM spans WHERE start_ms < ?1", [now_ms() - keep_days * 86_400_000])?;
    }
    let next: i64 = conn.query_row("SELECT COALESCE(MAX(id), 0) + 1 FROM spans", [], |r| r.get(0))?;
    let (tx, rx) = channel();
    std::thread::Builder::new().name("narsil-telemetry".into()).spawn(move || writer(conn, rx))?;
    Ok((Recorder { tx: tx.clone(), next: AtomicI64::new(next) }, Guard { tx: Some(tx) }))
}

impl<S> Layer<S> for Recorder
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_new_span(&self, attrs: &Attributes<'_>, id: &Id, ctx: Context<'_, S>) {
        let Some(span) = ctx.span(id) else { return };
        let Some(kind) = RECORDED.iter().copied().find(|k| *k == span.name()) else { return };
        let mut rec = Rec {
            id: self.next.fetch_add(1, Ordering::Relaxed),
            started: Instant::now(),
            root: String::new(),
            session: String::new(),
            agent: String::new(),
            goal: String::new(),
            what: String::new(),
            ok: true,
            data: Map::new(),
        };
        // Who this belongs to comes from the nearest recorded span around it.
        let mut parent = None;
        if let Some(outer) = span.parent() {
            for ancestor in outer.scope() {
                if let Some(up) = ancestor.extensions().get::<Rec>() {
                    parent = Some(up.id);
                    rec.root = up.root.clone();
                    rec.session = up.session.clone();
                    rec.agent = up.agent.clone();
                    rec.goal = up.goal.clone();
                    break;
                }
            }
        }
        let mut fields = Fields::default();
        attrs.record(&mut fields);
        rec.apply(fields);
        let opened = Rec {
            id: rec.id,
            started: rec.started,
            root: rec.root.clone(),
            session: rec.session.clone(),
            agent: rec.agent.clone(),
            goal: rec.goal.clone(),
            what: rec.what.clone(),
            ok: true,
            data: rec.data.clone(),
        };
        let _ = self.tx.send(Msg::Open { id: rec.id, parent, kind, start_ms: now_ms(), rec: Box::new(opened) });
        span.extensions_mut().insert(rec);
    }

    fn on_record(&self, id: &Id, values: &Record<'_>, ctx: Context<'_, S>) {
        let Some(span) = ctx.span(id) else { return };
        if let Some(rec) = span.extensions_mut().get_mut::<Rec>() {
            let mut fields = Fields::default();
            values.record(&mut fields);
            let named = fields.0.iter().any(|(k, _)| matches!(k.as_str(), "session" | "root" | "agent" | "goal"));
            rec.apply(fields);
            if named {
                let _ = self.tx.send(Msg::Who {
                    id: rec.id,
                    root: rec.root.clone(),
                    session: rec.session.clone(),
                    agent: rec.agent.clone(),
                    goal: rec.goal.clone(),
                });
            }
        }
    }

    fn on_close(&self, id: Id, ctx: Context<'_, S>) {
        let Some(span) = ctx.span(&id) else { return };
        let Some(rec) = span.extensions_mut().remove::<Rec>() else { return };
        let _ = self.tx.send(Msg::Close {
            id: rec.id,
            dur_ms: rec.started.elapsed().as_millis() as i64,
            ok: rec.ok,
            what: rec.what,
            data: Value::Object(rec.data).to_string(),
        });
    }
}

// ---- setting up the log ------------------------------------------------------

#[derive(Clone, Copy, PartialEq)]
pub enum Role {
    /// Logs to stderr and records spans.
    Daemon,
    /// Logs to `tui.log`, since the terminal is the interface.
    Tui,
    /// Warnings and errors to stderr unless `NARSIL_LOG` asks for more.
    Cli,
}

struct LocalTime;

impl FormatTime for LocalTime {
    fn format_time(&self, w: &mut Writer<'_>) -> std::fmt::Result {
        write!(w, "{}", chrono::Local::now().format("%Y-%m-%d %H:%M:%S%.3f"))
    }
}

/// The value of `key=` in a span's formatted fields.
fn field_of<'a>(fields: &'a str, key: &str) -> Option<&'a str> {
    let at = fields.match_indices(key).find(|(i, _)| {
        (*i == 0 || fields[..*i].ends_with(' ')) && fields[*i + key.len()..].starts_with('=')
    })?;
    let rest = &fields[at.0 + key.len() + 1..];
    let value = match rest.strip_prefix('"') {
        Some(quoted) => &quoted[..quoted.find('"').unwrap_or(quoted.len())],
        None => rest.split(' ').next().unwrap_or(""),
    };
    (!value.is_empty()).then_some(value)
}

/// Who a log line, a recorded step or a line of status belongs to. Written the
/// same way everywhere, so that one id finds everything about a session
/// wherever it is looked for:
///
///   w31q3ky4                              a session
///   w31q3ky4 goal:x6j4z062                a goal's session
///   w31q3ky4>9xk2m1aa count-two           a subagent, after the session it works for
///   goal:x6j4z062                         a goal, when no session is at work on it
#[derive(Default, Clone, PartialEq, Debug)]
pub struct Who {
    pub root: String,
    pub session: String,
    pub agent: String,
    pub goal: String,
}

impl Who {
    pub fn of(span: &SpanRow) -> Who {
        Who { root: span.root.clone(), session: span.session.clone(), agent: span.agent.clone(), goal: span.goal.clone() }
    }

    /// Whether this is the session, subagent or goal with the given id, or
    /// belongs to it: a subagent's lines belong to the session it works for.
    pub fn is(&self, id: &str) -> bool {
        !id.is_empty() && (self.session == id || self.root == id || self.goal == id)
    }

    /// Read back from the bracketed start of a log line.
    pub fn parse(text: &str) -> Who {
        let mut who = Who::default();
        for (i, part) in text.split_whitespace().enumerate() {
            if let Some(goal) = part.strip_prefix("goal:") {
                who.goal = goal.to_string();
            } else if i == 0 {
                match part.split_once('>') {
                    Some((root, session)) => {
                        who.root = root.to_string();
                        who.session = session.to_string();
                    }
                    None => who.session = part.to_string(),
                }
            } else if who.agent.is_empty() {
                who.agent = part.to_string();
            }
        }
        who
    }
}

impl std::fmt::Display for Who {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut parts = Vec::new();
        if !self.session.is_empty() {
            let under = !self.root.is_empty() && self.root != self.session;
            parts.push(if under { format!("{}>{}", self.root, self.session) } else { self.session.clone() });
            if !self.agent.is_empty() {
                parts.push(self.agent.clone());
            }
        }
        if !self.goal.is_empty() {
            parts.push(format!("goal:{}", self.goal));
        }
        write!(f, "{}", parts.join(" "))
    }
}

/// Whose a line of the log is, from its text: the part in brackets straight
/// after the level. A line with none belongs to no session (the daemon
/// starting, the notes being read), or continues the line before it.
pub fn line_owner(line: &str) -> Who {
    static OWNER: std::sync::LazyLock<regex::Regex> =
        std::sync::LazyLock::new(|| regex::Regex::new(r"^\S+ \S+\s+[A-Z]+ \[([^\]]*)\] ").expect("a valid pattern"));
    OWNER.captures(line).map(|c| Who::parse(&c[1])).unwrap_or_default()
}

/// The lines of a log that belong to a session, a subagent or a goal, with
/// the lines that continue them (a message that runs over several lines).
pub fn lines_of<'a>(log: &'a str, id: &str) -> Vec<&'a str> {
    static STARTS: std::sync::LazyLock<regex::Regex> =
        std::sync::LazyLock::new(|| regex::Regex::new(r"^\d{4}-\d\d-\d\d \d\d:\d\d:\d\d").expect("a valid pattern"));
    let mut out = Vec::new();
    let mut taking = false;
    for line in log.lines() {
        if STARTS.is_match(line) {
            taking = line_owner(line).is(id);
        }
        if taking {
            out.push(line);
        }
    }
    out
}

/// How serious a line of the log is: 0 trace and debug, 1 info, 2 warn, 3 error.
pub fn line_level(line: &str) -> Option<u8> {
    static LEVEL: std::sync::LazyLock<regex::Regex> =
        std::sync::LazyLock::new(|| regex::Regex::new(r"^\d{4}-\d\d-\d\d \S+\s+(TRACE|DEBUG|INFO|WARN|ERROR) ").expect("a valid pattern"));
    LEVEL.captures(line).map(|c| match &c[1] {
        "ERROR" => 3,
        "WARN" => 2,
        "INFO" => 1,
        _ => 0,
    })
}

/// The lines of a log to show: at or above a level, of one session (with its
/// subagents) or goal if `owner` is given, and holding `find` if that is not
/// empty. A line that continues another goes or stays with it. The date is
/// dropped from each line: it is the time that is wanted on screen.
pub fn log_view(log: &str, min_level: u8, owner: Option<&str>, find: &str) -> Vec<String> {
    let find = find.trim().to_lowercase();
    let mut out = Vec::new();
    // Each entry: its first line and the lines that continue it.
    let mut entry: Vec<&str> = Vec::new();
    let mut flush = |entry: &mut Vec<&str>| {
        let Some(first) = entry.first() else { return };
        let wanted = line_level(first).unwrap_or(1) >= min_level
            && owner.is_none_or(|id| line_owner(first).is(id))
            && (find.is_empty() || entry.iter().any(|l| l.to_lowercase().contains(&find)));
        if wanted {
            for (i, line) in entry.iter().enumerate() {
                out.push(if i == 0 { line.get(11..).unwrap_or(line).to_string() } else { line.to_string() });
            }
        }
        entry.clear();
    };
    for line in log.lines() {
        if line_level(line).is_some() {
            flush(&mut entry);
        }
        if !entry.is_empty() || line_level(line).is_some() {
            entry.push(line);
        }
    }
    flush(&mut entry);
    out
}

/// What an event says, apart from whose it is: its message, then its other fields.
#[derive(Default)]
struct Said {
    who: Who,
    message: String,
    rest: String,
}

impl Said {
    fn put(&mut self, name: &str, value: String) {
        let plain = value.trim_matches('"').to_string();
        match name {
            "message" => self.message = plain,
            "session" => self.who.session = plain,
            "root" => self.who.root = plain,
            "agent" => self.who.agent = plain,
            "goal" => self.who.goal = plain,
            _ => self.rest.push_str(&format!(" {name}={value}")),
        }
    }
}

impl tracing::field::Visit for Said {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        self.put(field.name(), format!("{value:?}"));
    }
    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        self.put(field.name(), format!("{value:?}"));
    }
}

/// One line per event: time, level, whose it is, what happened.
///
///   2026-11-26 14:38:47.168  INFO [w31q3ky4 count-two] model call used 1225 input tokens ...
///
/// The spans an event happened in can be long (a subagent's tool call sits five
/// deep); what a reader needs from them is the session and, for a subagent, its name.
struct Line;

impl<S, N> tracing_subscriber::fmt::FormatEvent<S, N> for Line
where
    S: Subscriber + for<'a> LookupSpan<'a>,
    N: for<'a> tracing_subscriber::fmt::FormatFields<'a> + 'static,
{
    fn format_event(
        &self,
        ctx: &tracing_subscriber::fmt::FmtContext<'_, S, N>,
        mut w: Writer<'_>,
        event: &tracing::Event<'_>,
    ) -> std::fmt::Result {
        LocalTime.format_time(&mut w)?;
        write!(w, " {:>5} ", event.metadata().level())?;
        // Whose line this is: what the event itself names, else the innermost span that names a session.
        let mut said = Said::default();
        event.record(&mut said);
        let mut who = said.who;
        if who.session.is_empty() {
            if let Some(scope) = ctx.event_scope() {
                for span in scope {
                    let ext = span.extensions();
                    let Some(fields) = ext.get::<tracing_subscriber::fmt::FormattedFields<N>>() else { continue };
                    let field = |key: &str| field_of(&fields.fields, key).unwrap_or("").to_string();
                    if who.goal.is_empty() {
                        who.goal = field("goal");
                    }
                    if !field("session").is_empty() {
                        who.session = field("session");
                        who.root = field("root");
                        who.agent = field("agent");
                        break;
                    }
                }
            }
        }
        let whose = who.to_string();
        if !whose.is_empty() {
            write!(w, "[{whose}] ")?;
        }
        write!(w, "{}{}", said.message, said.rest)?;
        writeln!(w)
    }
}

type BoxedLayer = Box<dyn Layer<Registry> + Send + Sync>;

/// Install logging for this process, and span recording for the daemon.
/// Returns a guard to keep until the process ends.
pub fn init(role: Role, paths: &Paths, cfg: &Config) -> Guard {
    // Every process that shows or keeps anything starts here.
    crate::secrets::set_masking(cfg.permissions.mask_secrets);
    let configured = if role == Role::Cli { "warn" } else { cfg.telemetry.level.as_str() };
    let wanted = std::env::var("NARSIL_LOG").ok().filter(|s| !s.trim().is_empty()).unwrap_or_else(|| configured.to_string());
    // A bare `debug` or `trace` means narsil's own detail, not the HTTP library's connection chatter.
    let wanted = match wanted.trim() {
        level @ ("debug" | "trace") => format!("info,narsil={level}"),
        other => other.to_string(),
    };
    let filter = EnvFilter::try_new(&wanted).unwrap_or_else(|_| EnvFilter::new("info"));
    let json = std::env::var("NARSIL_LOG_FORMAT").unwrap_or_else(|_| cfg.telemetry.log_format.clone()) == "json";

    let to_file = (role == Role::Tui)
        .then(|| std::fs::OpenOptions::new().create(true).append(true).open(paths.state_dir.join("tui.log")).ok())
        .flatten();
    let log: BoxedLayer = match (json, to_file) {
        (true, Some(file)) => tracing_subscriber::fmt::layer().json().with_writer(std::sync::Mutex::new(file)).boxed(),
        (true, None) => tracing_subscriber::fmt::layer().json().with_writer(std::io::stderr).boxed(),
        (false, Some(file)) => {
            tracing_subscriber::fmt::layer().with_ansi(false).event_format(Line).with_writer(std::sync::Mutex::new(file)).boxed()
        }
        (false, None) => tracing_subscriber::fmt::layer().with_ansi(false).event_format(Line).with_writer(std::io::stderr).boxed(),
    };
    let mut layers: Vec<BoxedLayer> = vec![log.with_filter(filter).boxed()];
    let mut guard = Guard { tx: None };
    if role == Role::Daemon {
        match recorder(&paths.db_file(), cfg.telemetry.keep_days) {
            Ok((rec, g)) => {
                layers.push(rec.with_filter(LevelFilter::from_level(Level::INFO)).boxed());
                guard = g;
            }
            Err(e) => eprintln!("{} WARN telemetry: spans will not be recorded: {e:#}", crate::util::timestamp()),
        }
    }
    let _ = tracing_subscriber::registry().with(layers).try_init();
    guard
}

/// Send panics to the log with a backtrace, instead of only to stderr.
pub fn log_panics() {
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let backtrace = std::backtrace::Backtrace::force_capture();
        tracing::error!(panic = %info, %backtrace, "a task panicked");
        default(info);
    }));
}

// ---- reading it back ---------------------------------------------------------

pub fn fmt_ms(ms: i64) -> String {
    let ms = ms.max(0);
    if ms < 1000 {
        format!("{ms}ms")
    } else if ms < 60_000 {
        format!("{:.2}s", ms as f64 / 1000.0)
    } else if ms < 3_600_000 {
        format!("{}m{:02}s", ms / 60_000, (ms % 60_000) / 1000)
    } else {
        format!("{}h{:02}m", ms / 3_600_000, (ms % 3_600_000) / 60_000)
    }
}

/// How long a span has run: its recorded duration, or the time so far.
fn span_ms(s: &SpanRow, now: i64) -> i64 {
    s.dur_ms.unwrap_or_else(|| (now - s.start_ms).max(0))
}

fn label(s: &SpanRow) -> String {
    if s.what.is_empty() { s.kind.clone() } else { format!("{} {}", s.kind, s.what) }
}

/// The interesting facts of a span, on one line.
fn details(s: &SpanRow) -> String {
    let mut parts: Vec<String> = Vec::new();
    match s.kind.as_str() {
        "model_call" => {
            if s.num("input_tokens") + s.num("output_tokens") > 0 {
                parts.push(format!(
                    "{} in ({} cached) · {} out",
                    fmt_tokens(s.num("input_tokens")),
                    fmt_tokens(s.num("cache_read")),
                    fmt_tokens(s.num("output_tokens"))
                ));
            }
            if s.num("ttft_ms") > 0 {
                parts.push(format!("first token {}", fmt_ms(s.num("ttft_ms"))));
            }
            if s.num("attempt") > 1 {
                parts.push(format!("attempt {}", s.num("attempt")));
            }
            for key in ["purpose", "stop", "request_id"] {
                if !s.text(key).is_empty() && !(key == "stop" && matches!(s.text(key).as_str(), "end_turn" | "tool_use")) {
                    parts.push(s.text(key));
                }
            }
        }
        "tool_call" => parts.push(s.text("summary")),
        "session_run" => {
            // Named as in the log, so that one id finds a session in both.
            parts.push(format!("[{}]", Who::of(s)));
            parts.push(s.text("outcome"));
        }
        "compaction" => parts.push(format!("{} messages to {} tokens", s.num("messages"), fmt_tokens(s.num("summary_tokens")))),
        "approval_wait" => parts.push(s.text("decision")),
        "retry_wait" => parts.push(s.text("reason")),
        "goal_turn" => parts.push(format!("turn {}", s.num("turn"))),
        _ => {}
    }
    if !s.text("error").is_empty() {
        parts.push(format!("error: {}", first_line(&s.text("error"), 160)));
    }
    if s.num("cut_off") == 1 {
        parts.push("cut off: the daemon stopped while this was running".to_string());
    }
    parts.retain(|p| !p.trim().is_empty());
    parts.join(" · ")
}

/// Depth of each span below the recorded spans that contain it.
fn depths(spans: &[SpanRow]) -> HashMap<i64, usize> {
    let parents: HashMap<i64, Option<i64>> = spans.iter().map(|s| (s.id, s.parent)).collect();
    spans
        .iter()
        .map(|s| {
            let (mut depth, mut up) = (0, s.parent);
            while let Some(p) = up.filter(|p| parents.contains_key(p) && depth < 64) {
                depth += 1;
                up = parents[&p];
            }
            (s.id, depth)
        })
        .collect()
}

/// One line per span, in the order things happened, indented by nesting.
pub fn timeline(spans: &[SpanRow], now: i64) -> Vec<String> {
    in_order(spans, now).iter().map(|line| crate::secrets::kept(line)).collect()
}

fn in_order(spans: &[SpanRow], now: i64) -> Vec<String> {
    let Some(first) = spans.iter().map(|s| s.start_ms).min() else { return vec!["nothing recorded".to_string()] };
    let depth = depths(spans);
    spans
        .iter()
        .map(|s| {
            let offset = s.start_ms - first;
            let at = if offset < 600_000 { format!("{:>8.3}s", offset as f64 / 1000.0) } else { format!("{:>9}", fmt_ms(offset)) };
            let mark = match (s.dur_ms.is_none(), s.ok) {
                (true, _) => "…",
                (false, false) => "✗",
                (false, true) => " ",
            };
            let name = format!("{}{}", "  ".repeat(depth[&s.id]), label(s));
            format!("{at} {mark} {name:<44} {:>8}  {}", fmt_ms(span_ms(s, now)), details(s)).trim_end().to_string()
        })
        .collect()
}

fn percentile(sorted: &[i64], p: f64) -> i64 {
    if sorted.is_empty() {
        return 0;
    }
    sorted[(((sorted.len() - 1) as f64) * p).round() as usize]
}

/// Totals for a set of spans: where the time and the tokens went.
pub fn summary(spans: &[SpanRow], now: i64) -> Vec<String> {
    if spans.is_empty() {
        return vec!["nothing recorded".to_string()];
    }
    let ids: std::collections::HashSet<i64> = spans.iter().map(|s| s.id).collect();
    fn pick<'a>(spans: &'a [SpanRow], kind: &'a str) -> impl Iterator<Item = &'a SpanRow> {
        spans.iter().filter(move |s| s.kind == kind)
    }
    let of = |kind: &'static str| pick(spans, kind);
    // Time on the clock: the outermost spans, not the gaps between them.
    let clock: i64 = spans.iter().filter(|s| s.parent.is_none_or(|p| !ids.contains(&p))).map(|s| span_ms(s, now)).sum();
    // Each span's own time is what the spans inside it do not account for, so
    // a tool call that runs subagents is not counted on top of what they did.
    let mut inside: HashMap<i64, i64> = HashMap::new();
    for s in spans {
        if let Some(p) = s.parent.filter(|p| ids.contains(p)) {
            *inside.entry(p).or_default() += span_ms(s, now);
        }
    }
    let own = |s: &SpanRow| (span_ms(s, now) - inside.get(&s.id).copied().unwrap_or(0)).max(0);
    let spent = |kinds: &[&str]| spans.iter().filter(|s| kinds.contains(&s.kind.as_str())).map(own).sum::<i64>();
    let (model, tool, approval, retry) = (spent(&["model_call"]), spent(&["tool_call"]), spent(&["approval_wait"]), spent(&["retry_wait"]));
    let overhead = spent(&["session_run", "goal_turn", "compaction"]);
    let work = model + tool + approval + retry + overhead;
    let share = |ms: i64| if work > 0 { format!("{}%", ms * 100 / work) } else { "-".to_string() };
    let mut out = vec![format!(
        "{} on the clock · {} model calls {} ({}) · {} tool calls {} ({}) · waiting on approval {} · waiting to retry {} · narsil itself {}",
        fmt_ms(clock),
        of("model_call").count(),
        fmt_ms(model),
        share(model),
        of("tool_call").count(),
        fmt_ms(tool),
        share(tool),
        fmt_ms(approval),
        fmt_ms(retry),
        fmt_ms(overhead)
    )];
    if work > clock + clock / 20 {
        out.push(format!("{} of work in that time, because subagents ran in parallel", fmt_ms(work)));
    }
    let sum = |key: &str| of("model_call").map(|s| s.num(key)).sum::<i64>();
    let (input, cached, output) = (sum("input_tokens"), sum("cache_read"), sum("output_tokens"));
    if input + output > 0 {
        let hit = if input > 0 { cached * 100 / input } else { 0 };
        out.push(format!("tokens {} in ({} read from cache, {hit}%) · {} out", fmt_tokens(input), fmt_tokens(cached), fmt_tokens(output)));
    }
    let mut ttft: Vec<i64> = of("model_call").map(|s| s.num("ttft_ms")).filter(|t| *t > 0).collect();
    ttft.sort_unstable();
    if !ttft.is_empty() {
        out.push(format!("first token after {} typically, {} at worst", fmt_ms(percentile(&ttft, 0.5)), fmt_ms(ttft[ttft.len() - 1])));
    }
    let sessions = of("session_run").map(|s| s.session.as_str()).collect::<std::collections::HashSet<_>>().len();
    let agents = of("session_run").filter(|s| !s.agent.is_empty()).map(|s| s.session.as_str()).collect::<std::collections::HashSet<_>>().len();
    out.push(format!(
        "{sessions} session{} ({agents} subagent{}) · {} compaction{} · {} goal turn{}",
        if sessions == 1 { "" } else { "s" },
        if agents == 1 { "" } else { "s" },
        of("compaction").count(),
        if of("compaction").count() == 1 { "" } else { "s" },
        of("goal_turn").count(),
        if of("goal_turn").count() == 1 { "" } else { "s" }
    ));
    let failed: Vec<&SpanRow> = spans.iter().filter(|s| !s.ok && s.dur_ms.is_some()).collect();
    if !failed.is_empty() {
        let shown: Vec<String> = failed.iter().take(5).map(|s| first_line(&format!("{} ({})", label(s), details(s)), 110)).collect();
        out.push(format!("{} failed: {}", failed.len(), shown.join("; ")));
    }
    let running: Vec<String> = spans.iter().filter(|s| s.dur_ms.is_none()).map(|s| format!("{} for {}", label(s), fmt_ms(span_ms(s, now)))).collect();
    if !running.is_empty() {
        out.push(format!("in flight: {}", running.join("; ")));
    }
    let mut slow: Vec<&SpanRow> = spans.iter().filter(|s| matches!(s.kind.as_str(), "model_call" | "tool_call")).collect();
    slow.sort_by_key(|s| std::cmp::Reverse(span_ms(s, now)));
    let slowest: Vec<String> = slow.iter().take(3).map(|s| format!("{} {}", first_line(&format!("{} {}", label(s), s.text("summary")), 60), fmt_ms(span_ms(s, now)))).collect();
    if !slowest.is_empty() {
        out.push(format!("slowest: {}", slowest.join("; ")));
    }
    out
}

/// Figures per model and per tool over a period.
/// Where the time and the tokens went, session by session: with several
/// sessions at work at once, the totals alone do not say whose they are.
fn by_session(spans: &[SpanRow], now: i64) -> Vec<String> {
    let mut roots: Vec<&str> = Vec::new();
    for s in spans {
        if !s.root.is_empty() && !roots.contains(&s.root.as_str()) {
            roots.push(&s.root);
        }
    }
    let mut rows: Vec<(i64, String)> = roots
        .iter()
        .map(|root| {
            let mine: Vec<&SpanRow> = spans.iter().filter(|s| s.root == *root).collect();
            let goal = mine.iter().map(|s| s.goal.as_str()).find(|g| !g.is_empty()).unwrap_or("");
            let who = Who { root: root.to_string(), session: root.to_string(), agent: String::new(), goal: goal.to_string() };
            let mut subagents: Vec<&str> = mine.iter().filter(|s| s.session != *root).map(|s| s.session.as_str()).collect();
            subagents.sort_unstable();
            subagents.dedup();
            let of = |kind: &str| mine.iter().filter(|s| s.kind == kind).copied().collect::<Vec<_>>();
            let (models, tools) = (of("model_call"), of("tool_call"));
            let time: i64 = of("session_run").iter().filter(|s| s.session == *root).map(|s| span_ms(s, now)).sum();
            let line = format!(
                "{:<34} {:>6} {:>6} {:>9} {:>9}  {} / {}",
                first_line(&who.to_string(), 34),
                models.len(),
                tools.len(),
                subagents.len(),
                fmt_ms(time),
                fmt_tokens(models.iter().map(|s| s.num("input_tokens")).sum()),
                fmt_tokens(models.iter().map(|s| s.num("output_tokens")).sum())
            );
            (time, line)
        })
        .collect();
    if rows.len() < 2 {
        return Vec::new();
    }
    rows.sort_by_key(|(time, _)| std::cmp::Reverse(*time));
    let mut out = vec![String::new(), format!("{:<34} {:>6} {:>6} {:>9} {:>9}  {}", "session", "model", "tool", "subagents", "running", "tokens in / out")];
    let more = rows.len().saturating_sub(15);
    out.extend(rows.into_iter().take(15).map(|(_, line)| line));
    if more > 0 {
        out.push(format!("… and {more} more"));
    }
    out
}

pub fn stats(spans: &[SpanRow], now: i64) -> Vec<String> {
    let mut out = summary(spans, now);
    out.extend(by_session(spans, now));
    for (kind, title) in [("model_call", "model"), ("tool_call", "tool")] {
        let mut groups: HashMap<&str, Vec<&SpanRow>> = HashMap::new();
        for s in spans.iter().filter(|s| s.kind == kind) {
            groups.entry(s.what.as_str()).or_default().push(s);
        }
        if groups.is_empty() {
            continue;
        }
        let mut rows: Vec<(&str, Vec<&SpanRow>)> = groups.into_iter().collect();
        rows.sort_by_key(|(_, g)| std::cmp::Reverse(g.iter().map(|s| span_ms(s, now)).sum::<i64>()));
        out.push(String::new());
        out.push(format!("{:<34} {:>6} {:>6} {:>9} {:>9} {:>9}  {}", title, "calls", "failed", "typical", "p95", "total", if kind == "model_call" { "tokens in / out" } else { "" }).trim_end().to_string());
        for (name, group) in rows {
            let mut times: Vec<i64> = group.iter().map(|s| span_ms(s, now)).collect();
            times.sort_unstable();
            let tokens = if kind == "model_call" {
                format!(
                    "{} / {}",
                    fmt_tokens(group.iter().map(|s| s.num("input_tokens")).sum()),
                    fmt_tokens(group.iter().map(|s| s.num("output_tokens")).sum())
                )
            } else {
                String::new()
            };
            out.push(
                format!(
                    "{:<34} {:>6} {:>6} {:>9} {:>9} {:>9}  {tokens}",
                    first_line(name, 34),
                    group.len(),
                    group.iter().filter(|s| !s.ok).count(),
                    fmt_ms(percentile(&times, 0.5)),
                    fmt_ms(percentile(&times, 0.95)),
                    fmt_ms(times.iter().sum())
                )
                .trim_end()
                .to_string(),
            );
        }
    }
    out
}

fn frame(s: &SpanRow) -> String {
    let name = match (s.kind.as_str(), s.agent.is_empty()) {
        ("session_run", false) => format!("subagent {}", s.agent),
        ("session_run", true) => format!("session {}", if s.what.is_empty() { &s.session } else { &s.what }),
        _ => label(s),
    };
    name.replace([';', '\n'], " ")
}

/// Folded stacks (`outer;inner milliseconds`), the input flamegraph tools take.
/// Each span contributes the time not spent in the spans inside it. Work done
/// in parallel by subagents adds up, so the total can exceed the time on the clock.
pub fn folded(spans: &[SpanRow], now: i64) -> Vec<String> {
    let by_id: HashMap<i64, &SpanRow> = spans.iter().map(|s| (s.id, s)).collect();
    let mut inside: HashMap<i64, i64> = HashMap::new();
    for s in spans {
        if let Some(p) = s.parent.filter(|p| by_id.contains_key(p)) {
            *inside.entry(p).or_default() += span_ms(s, now);
        }
    }
    let mut totals: HashMap<String, i64> = HashMap::new();
    for s in spans {
        let own = (span_ms(s, now) - inside.get(&s.id).copied().unwrap_or(0)).max(0);
        if own == 0 {
            continue;
        }
        let mut stack = vec![frame(s)];
        let mut up = s.parent;
        while let Some(p) = up.and_then(|p| by_id.get(&p)).filter(|_| stack.len() < 64) {
            stack.push(frame(p));
            up = p.parent;
        }
        stack.reverse();
        *totals.entry(stack.join(";")).or_default() += own;
    }
    let mut lines: Vec<String> = totals.into_iter().map(|(stack, ms)| format!("{stack} {ms}")).collect();
    lines.sort();
    lines
}

/// A flamegraph of where the time went, as an SVG document.
pub fn flamegraph(spans: &[SpanRow], now: i64, title: &str) -> Result<String> {
    let lines = folded(spans, now);
    if lines.is_empty() {
        anyhow::bail!("nothing recorded to draw");
    }
    let mut options = inferno::flamegraph::Options::default();
    options.title = title.to_string();
    options.count_name = "ms".to_string();
    options.subtitle = Some("time on the clock, by what narsil was doing; parallel subagents add up".to_string());
    let mut svg = Vec::new();
    inferno::flamegraph::from_lines(&mut options, lines.iter().map(String::as_str), &mut svg)?;
    Ok(String::from_utf8(svg)?)
}

/// The spans as a Chrome trace (open it in ui.perfetto.dev or chrome://tracing):
/// one track per session, so parallel subagents sit side by side.
pub fn chrome_trace(spans: &[SpanRow], now: i64) -> Value {
    let mut tracks: Vec<&str> = Vec::new();
    let mut events = Vec::new();
    for s in spans {
        let track = match tracks.iter().position(|t| *t == s.session) {
            Some(i) => i,
            None => {
                tracks.push(&s.session);
                let name = match (s.session.is_empty(), s.agent.is_empty()) {
                    (true, _) => "daemon".to_string(),
                    (false, true) => format!("session {}", Who::of(s)),
                    (false, false) => format!("subagent {}", Who::of(s)),
                };
                events.push(json!({ "ph": "M", "name": "thread_name", "pid": 1, "tid": tracks.len(), "args": { "name": name } }));
                tracks.len() - 1
            }
        };
        let mut args = s.data.as_object().cloned().unwrap_or_default();
        args.insert("ok".into(), json!(s.ok));
        events.push(json!({
            "ph": "X",
            "name": label(s),
            "cat": s.kind,
            "pid": 1,
            "tid": track + 1,
            "ts": s.start_ms * 1000,
            "dur": span_ms(s, now) * 1000,
            "args": args,
        }));
    }
    json!({ "traceEvents": events, "displayTimeUnit": "ms" })
}

// ---- the tree that was walked ------------------------------------------------

/// What the transcript adds to the recorded spans: the words behind each step.
#[derive(Default)]
pub struct Notes {
    /// Stored message id -> (what the model said, what it was thinking), first lines.
    pub said: HashMap<i64, (String, String)>,
    /// Tool call id -> (failed, the start of its result).
    pub results: HashMap<String, (bool, String)>,
    /// Session -> its user messages as (message id, first line), in order.
    pub prompts: HashMap<String, Vec<(i64, String)>>,
}

impl Notes {
    /// Read what each session in `spans` said and was told.
    pub fn load(store: &crate::store::Store, spans: &[SpanRow]) -> Notes {
        use crate::types::{Block, MsgKind, Role};
        let mut notes = Notes::default();
        let mut sessions: Vec<&str> = spans.iter().map(|s| s.session.as_str()).filter(|s| !s.is_empty()).collect();
        sessions.sort_unstable();
        sessions.dedup();
        let brief = |text: &str| first_line(text.trim(), 200);
        for sid in sessions {
            for m in store.messages_from(sid, 0).unwrap_or_default() {
                let mut text = String::new();
                let mut thinking = String::new();
                for b in &m.blocks {
                    match b {
                        Block::Text { text: t } if text.is_empty() => text = brief(t),
                        Block::Thinking { text: t, .. } if thinking.is_empty() => thinking = brief(t),
                        Block::ToolResult { tool_use_id, content, is_error } => {
                            let more = content.trim().lines().count().saturating_sub(1);
                            let shown = if more > 0 { format!("{} (+{more} lines)", brief(content)) } else { brief(content) };
                            notes.results.insert(tool_use_id.clone(), (*is_error, shown));
                        }
                        _ => {}
                    }
                }
                match m.role {
                    Role::Assistant => {
                        notes.said.insert(m.id, (text, thinking));
                    }
                    // What was asked, not what narsil added around it (recalled memory, continuation notes).
                    Role::User if !text.is_empty() && m.kind == MsgKind::Message => {
                        notes.prompts.entry(sid.to_string()).or_default().push((m.id, text));
                    }
                    _ => {}
                }
            }
        }
        notes
    }
}

/// The steps as a tree of decisions and actions: each model call with what the
/// model said, the tool calls it asked for beneath it with what came back, and
/// a subagent's whole run beneath the call that started it.
pub fn tree(spans: &[SpanRow], notes: &Notes, now: i64, width: usize) -> Vec<String> {
    // A trace is read by people and pasted into issues: no credentials in it.
    walked(spans, notes, now, width).iter().map(|line| crate::secrets::kept(line)).collect()
}

fn walked(spans: &[SpanRow], notes: &Notes, now: i64, width: usize) -> Vec<String> {
    if spans.is_empty() {
        return vec!["nothing recorded".to_string()];
    }
    let known: std::collections::HashSet<i64> = spans.iter().map(|s| s.id).collect();
    // A tool call hangs under the model call that asked for it, not beside it.
    let asked_by: HashMap<(&str, i64), i64> = spans
        .iter()
        .filter(|s| s.kind == "model_call" && s.num("message_id") > 0)
        .map(|s| ((s.session.as_str(), s.num("message_id")), s.id))
        .collect();
    let mut children: HashMap<Option<i64>, Vec<&SpanRow>> = HashMap::new();
    for s in spans {
        let asked = matches!(s.kind.as_str(), "tool_call" | "approval_wait")
            .then(|| asked_by.get(&(s.session.as_str(), s.num("asked_in"))).copied())
            .flatten();
        let parent = asked.or(s.parent.filter(|p| known.contains(p)));
        children.entry(parent).or_default().push(s);
    }

    let describe = |s: &SpanRow, first_call_of_run: Option<i64>| -> String {
        let quote = |t: &str| format!("\u{201c}{t}\u{201d}");
        match s.kind.as_str() {
            "goal_turn" => format!("goal {} turn {}", s.goal, s.num("turn")),
            "session_run" => {
                // What it was asked: the last thing said to it before its first reply.
                let asked = first_call_of_run
                    .and_then(|mid| notes.prompts.get(&s.session).and_then(|p| p.iter().rev().find(|(id, _)| *id < mid)))
                    .map(|(_, text)| format!(" \u{203a} {}", quote(text)))
                    .unwrap_or_default();
                let who = if s.agent.is_empty() { format!("session {}", Who::of(s)) } else { format!("subagent {}", Who::of(s)) };
                let ended = match (s.text("outcome").as_str(), s.text("error")) {
                    ("" | "completed", _) => String::new(),
                    (other, error) if error.is_empty() => format!(" ({other})"),
                    (other, error) => format!(" ({other}: {})", first_line(&error, 120)),
                };
                format!("{who}{asked}{ended}")
            }
            "model_call" => {
                let (text, thinking) = notes.said.get(&s.num("message_id")).cloned().unwrap_or_default();
                let words = if !text.is_empty() && text != "(no reply)" {
                    quote(&text)
                } else if !thinking.is_empty() {
                    format!("thinks: {thinking}")
                } else if !s.ok {
                    format!("model call failed: {}", first_line(&s.text("error"), 160))
                } else if s.text("purpose") == "compaction" {
                    "writes the handoff summary".to_string()
                } else {
                    "decides what to do next".to_string()
                };
                let mut facts = vec![s.what.clone()];
                if s.num("input_tokens") + s.num("output_tokens") > 0 {
                    facts.push(format!("{} in, {} out", fmt_tokens(s.num("input_tokens")), fmt_tokens(s.num("output_tokens"))));
                }
                if s.num("attempt") > 1 {
                    facts.push(format!("attempt {}", s.num("attempt")));
                }
                format!("{words}  [{}]", facts.join(" · "))
            }
            "tool_call" => {
                let came_back = match notes.results.get(&s.text("call")) {
                    Some((_, text)) if !text.is_empty() => format!("  \u{2192} {text}"),
                    _ => String::new(),
                };
                format!("{} {}{came_back}", s.what, s.text("summary"))
            }
            "compaction" => format!("compacted {} messages to {} tokens ({})", s.num("messages"), fmt_tokens(s.num("summary_tokens")), s.what),
            "approval_wait" => format!("waited for approval of {}: {}", s.what, s.text("decision")),
            "retry_wait" => format!("waited to retry: {}", s.text("reason")),
            _ => label(s),
        }
    };
    let glyph = |s: &SpanRow| match (s.dur_ms.is_none(), s.ok, s.kind.as_str()) {
        (true, _, _) => "\u{2026}",
        (false, false, _) => "\u{2717}",
        (_, _, "model_call") => "\u{25c6}",
        (_, _, "tool_call") => "\u{25b8}",
        (_, _, "session_run") => "\u{25cf}",
        (_, _, "approval_wait" | "retry_wait") => "\u{23f8}",
        _ => "\u{00b7}",
    };

    fn walk<'a>(
        parent: Option<i64>,
        prefix: &str,
        children: &HashMap<Option<i64>, Vec<&'a SpanRow>>,
        render: &dyn Fn(&SpanRow, Option<i64>, &str) -> String,
        out: &mut Vec<String>,
    ) {
        let Some(list) = children.get(&parent) else { return };
        for (i, s) in list.iter().enumerate() {
            let last = i + 1 == list.len();
            let (branch, below) = match (parent.is_none(), last) {
                (true, _) => ("", ""),
                (false, true) => ("\u{2514}\u{2500} ", "   "),
                (false, false) => ("\u{251c}\u{2500} ", "\u{2502}  "),
            };
            // The first reply of a run tells us which prompt started it.
            let first_call = children
                .get(&Some(s.id))
                .and_then(|c| c.iter().find(|c| c.kind == "model_call" && c.num("message_id") > 0))
                .map(|c| c.num("message_id"));
            out.push(render(s, first_call, &format!("{prefix}{branch}")));
            walk(Some(s.id), &format!("{prefix}{below}"), children, render, out);
        }
    }

    let render = |s: &SpanRow, first_call: Option<i64>, lead: &str| -> String {
        let took = fmt_ms(span_ms(s, now));
        let text = format!("{lead}{} {}", glyph(s), describe(s, first_call));
        let room = width.saturating_sub(took.chars().count() + 2).max(40);
        let text = if text.chars().count() > room { format!("{}\u{2026}", text.chars().take(room - 1).collect::<String>()) } else { text };
        format!("{text:<room$}  {took}")
    };
    let mut out = Vec::new();
    walk(None, "", &children, &render, &mut out);
    out
}

/// Where the `spans` table and the logs are, for `narsil doctor`.
pub fn locations(paths: &Paths) -> Vec<(String, PathBuf)> {
    vec![
        ("daemon log".to_string(), paths.log_file()),
        ("interface log".to_string(), paths.state_dir.join("tui.log")),
        ("recorded spans".to_string(), paths.db_file()),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Store;
    use tracing::Instrument;

    fn row(id: i64, parent: Option<i64>, kind: &str, what: &str, start_ms: i64, dur_ms: Option<i64>, data: Value) -> SpanRow {
        SpanRow {
            id,
            parent,
            root: "r".into(),
            session: if kind == "session_run" && parent.is_some() { "child".into() } else { "r".into() },
            agent: if kind == "session_run" && parent.is_some() { "lexer".into() } else { String::new() },
            goal: String::new(),
            kind: kind.into(),
            what: what.into(),
            start_ms,
            dur_ms,
            ok: !data.get("failed").is_some_and(|v| v == true),
            data,
        }
    }

    fn sample() -> Vec<SpanRow> {
        let mut spans = sample_rows();
        // What runs inside the subagent's session belongs to that session.
        for s in spans.iter_mut().filter(|s| matches!(s.id, 5 | 6)) {
            s.session = "child".into();
            s.agent = "lexer".into();
        }
        spans
    }

    fn sample_rows() -> Vec<SpanRow> {
        vec![
            row(1, None, "session_run", "chat", 1000, Some(10_000), json!({ "outcome": "completed" })),
            row(2, Some(1), "model_call", "opus", 1010, Some(2000), json!({ "input_tokens": 5000, "cache_read": 4000, "output_tokens": 100, "ttft_ms": 700 })),
            row(3, Some(1), "tool_call", "spawn_agents", 3050, Some(6000), json!({ "summary": "lexer" })),
            row(4, Some(3), "session_run", "subagent", 3060, Some(5900), json!({})),
            row(5, Some(4), "model_call", "haiku", 3070, Some(900), json!({ "input_tokens": 1000, "output_tokens": 50, "ttft_ms": 300 })),
            row(6, Some(4), "tool_call", "bash", 4000, Some(4900), json!({ "summary": "cargo test", "failed": true, "error": "exit 101" })),
            row(7, Some(1), "model_call", "opus", 9100, None, json!({})),
        ]
    }

    #[test]
    fn everything_names_its_owner_the_same_way() {
        let who = |root: &str, session: &str, agent: &str, goal: &str| Who { root: root.into(), session: session.into(), agent: agent.into(), goal: goal.into() };
        for (w, text) in [
            (who("aaaa1111", "aaaa1111", "", ""), "aaaa1111"),
            (who("", "aaaa1111", "", ""), "aaaa1111"),
            (who("aaaa1111", "aaaa1111", "", "gggg9999"), "aaaa1111 goal:gggg9999"),
            (who("aaaa1111", "bbbb2222", "count-two", ""), "aaaa1111>bbbb2222 count-two"),
            (who("aaaa1111", "bbbb2222", "count-two", "gggg9999"), "aaaa1111>bbbb2222 count-two goal:gggg9999"),
            (who("", "", "", "gggg9999"), "goal:gggg9999"),
            (who("", "", "", ""), ""),
        ] {
            assert_eq!(w.to_string(), text);
            // And it reads back as written.
            let back = Who::parse(text);
            assert_eq!((back.session.as_str(), back.agent.as_str(), back.goal.as_str()), (w.session.as_str(), w.agent.as_str(), w.goal.as_str()), "{text}");
        }
        // A subagent's line belongs to it, to the session it works for, and to the goal; not to another session.
        let sub = Who::parse("aaaa1111>bbbb2222 count-two goal:gggg9999");
        assert!(sub.is("bbbb2222") && sub.is("aaaa1111") && sub.is("gggg9999"));
        assert!(!sub.is("cccc3333") && !sub.is("") && !sub.is("count-two"));
    }

    #[test]
    fn a_log_is_filtered_to_one_session_with_its_subagents_and_its_continuation_lines() {
        let log = "\
2026-11-26 14:00:00.000  INFO narsil daemon 0.1.0 starting (pid 1)
2026-11-26 14:00:01.000  INFO [aaaa1111] model call used 10 input tokens
2026-11-26 14:00:02.000 DEBUG [cccc3333] tool call finished tool=bash
2026-11-26 14:00:03.000  WARN [aaaa1111>bbbb2222 count-two] the backend said no
  and went on to explain why
  over several lines
2026-11-26 14:00:04.000  INFO [cccc3333 goal:gggg9999] goal is now completed: mentions [aaaa1111] in passing
2026-11-26 14:00:05.000  INFO [goal:gggg9999] goal starting (x)
2026-11-26 14:00:06.000  INFO work: 3 notes read
";
        let of = |id: &str| lines_of(log, id).iter().map(|l| l.split_whitespace().nth(1).unwrap_or("").to_string()).collect::<Vec<_>>();
        assert_eq!(lines_of(log, "aaaa1111").len(), 4, "its own line, its subagent's, and the two that continue it");
        assert_eq!(of("aaaa1111")[..2], ["14:00:01.000", "14:00:03.000"]);
        assert_eq!(of("bbbb2222"), ["14:00:03.000", "went", "several"]);
        assert_eq!(of("cccc3333"), ["14:00:02.000", "14:00:04.000"], "a session named inside another's message is not that line's owner");
        assert_eq!(of("gggg9999"), ["14:00:04.000", "14:00:05.000"]);
        assert!(lines_of(log, "zzzz0000").is_empty());
        assert_eq!(line_owner("2026-11-26 14:00:06.000  INFO work: 3 notes read"), Who::default());
    }

    #[test]
    fn a_log_is_narrowed_by_level_by_owner_and_by_what_it_says() {
        let log = "\
2026-11-26 14:00:00.000  INFO narsil daemon 0.1.0 starting (pid 1)
2026-11-26 14:00:01.000 DEBUG [aaaa1111] tool call finished tool=bash
2026-11-26 14:00:02.000  INFO [aaaa1111] model call used 10 input tokens
2026-11-26 14:00:03.000  WARN [aaaa1111>bbbb2222 count-two] the backend said no
  Timeout after 30s
2026-11-26 14:00:04.000 ERROR [cccc3333] session run failed: boom
2026-11-26 14:00:05.000  INFO [cccc3333] model call used 20 input tokens
";
        let times = |lines: Vec<String>| lines.iter().map(|l| l.split_whitespace().next().unwrap_or("").to_string()).collect::<Vec<_>>();
        assert_eq!(log_view(log, 0, None, "").len(), 7, "everything, with the line that continues another");
        assert!(log_view(log, 0, None, "")[0].starts_with("14:00:00.000  INFO narsil daemon"), "the date is dropped");
        assert_eq!(times(log_view(log, 1, None, "")), ["14:00:00.000", "14:00:02.000", "14:00:03.000", "Timeout", "14:00:04.000", "14:00:05.000"]);
        assert_eq!(times(log_view(log, 2, None, "")), ["14:00:03.000", "Timeout", "14:00:04.000"], "warnings and errors");
        assert_eq!(times(log_view(log, 3, None, "")), ["14:00:04.000"], "errors only");
        assert_eq!(times(log_view(log, 0, Some("aaaa1111"), "")), ["14:00:01.000", "14:00:02.000", "14:00:03.000", "Timeout"], "a session with its subagent");
        assert_eq!(times(log_view(log, 2, Some("aaaa1111"), "")), ["14:00:03.000", "Timeout"]);
        assert_eq!(times(log_view(log, 0, None, "TIMEOUT")), ["14:00:03.000", "Timeout"], "found in a continuation line, whatever the case");
        assert_eq!(times(log_view(log, 0, Some("cccc3333"), "model call")), ["14:00:05.000"]);
        assert!(log_view(log, 3, Some("aaaa1111"), "").is_empty() && log_view("", 0, None, "").is_empty());
        assert_eq!((line_level("2026-11-26 14:00:04.000 ERROR x"), line_level("  Timeout after 30s")), (Some(3), None));
    }

    #[test]
    fn a_timeline_shows_nesting_timing_and_what_matters() {
        let lines = timeline(&sample(), 9600);
        assert_eq!(lines.len(), 7);
        assert!(lines[0].starts_with("   0.000s   session_run chat") && lines[0].contains("10.00s") && lines[0].ends_with("[r] · completed"), "{}", lines[0]);
        assert!(lines[1].contains("  model_call opus") && lines[1].contains("5.0k in (4.0k cached) · 100 out · first token 700ms"), "{}", lines[1]);
        assert!(lines[3].contains("    session_run subagent") && lines[3].contains("[r>child lexer]"), "named as in the log: {}", lines[3]);
        assert!(lines[5].contains("✗       tool_call bash") && lines[5].contains("cargo test · error: exit 101"), "{}", lines[5]);
        assert!(lines[6].contains("…   model_call opus") && lines[6].contains("500ms"), "a span still open shows its time so far: {}", lines[6]);
        assert_eq!(timeline(&[], 0), vec!["nothing recorded"]);
    }

    #[test]
    fn a_summary_says_where_the_time_and_tokens_went() {
        let lines = summary(&sample(), 9600);
        // Own time only: the spawn call is not counted on top of what its subagent did.
        assert!(lines[0].starts_with("10.00s on the clock · 3 model calls 3.40s (34%) · 2 tool calls 5.00s (50%)"), "{}", lines[0]);
        assert!(lines[0].ends_with("narsil itself 1.60s"), "{}", lines[0]);
        assert_eq!(lines[1], "tokens 6.0k in (4.0k read from cache, 66%) · 150 out");
        assert_eq!(lines[2], "first token after 700ms typically, 700ms at worst");
        assert!(lines[3].starts_with("2 sessions (1 subagent)"), "{}", lines[3]);
        assert!(lines[4].starts_with("1 failed: tool_call bash"), "{}", lines[4]);
        assert!(lines[5].starts_with("in flight: model_call opus for 500ms"), "{}", lines[5]);
        assert!(lines[6].starts_with("slowest: tool_call spawn_agents lexer 6.00s"), "{}", lines[6]);
        assert_eq!(field_of("session=abc root=abc agent=\"lex er\" what=chat", "agent"), Some("lex er"));
        assert_eq!(field_of("subsession=zzz session=abc", "session"), Some("abc"));
        assert_eq!(field_of("what=chat", "agent"), None);
        let per = stats(&sample(), 9600);
        assert!(per.iter().any(|l| l.starts_with("opus") && l.contains("5.0k / 100")), "{per:#?}");
        assert!(per.iter().any(|l| l.starts_with("bash") && l.split_whitespace().nth(2) == Some("1")), "{per:#?}");
    }

    #[test]
    fn spans_fold_into_stacks_and_draw_as_a_flamegraph() {
        let lines = folded(&sample(), 9600);
        assert!(lines.contains(&"session chat;tool_call spawn_agents;subagent lexer;tool_call bash 4900".to_string()), "{lines:#?}");
        assert!(lines.contains(&"session chat;model_call opus 2500".to_string()), "identical stacks are added up: {lines:#?}");
        // The session's own time is what its children do not account for.
        assert!(lines.contains(&"session chat 1500".to_string()), "{lines:#?}");
        let svg = flamegraph(&sample(), 9600, "test").unwrap();
        assert!(svg.starts_with("<?xml") && svg.contains("tool_call bash") && svg.contains("</svg>"));
        let trace = chrome_trace(&sample(), 9600);
        let events = trace["traceEvents"].as_array().unwrap();
        assert_eq!(events.iter().filter(|e| e["ph"] == "M").count(), 2, "one track per session");
        assert!(events.iter().any(|e| e["name"] == "tool_call bash" && e["dur"] == 4_900_000 && e["tid"] == 2));
    }

    #[test]
    fn the_walked_tree_reads_as_decisions_and_their_actions() {
        let mut spans = vec![
            row(1, None, "session_run", "chat", 0, Some(9000), json!({ "outcome": "completed" })),
            row(2, Some(1), "model_call", "opus", 10, Some(1400), json!({ "message_id": 11, "input_tokens": 4000, "output_tokens": 90 })),
            row(3, Some(1), "approval_wait", "bash", 1450, Some(3000), json!({ "asked_in": 11, "decision": "allowed" })),
            row(4, Some(1), "tool_call", "bash", 4460, Some(40), json!({ "asked_in": 11, "call": "c1", "summary": "ls -A | wc -l" })),
            row(5, Some(1), "tool_call", "spawn_agents", 4510, Some(3000), json!({ "asked_in": 11, "call": "c2", "summary": "lexer" })),
            row(6, Some(5), "session_run", "subagent", 4520, Some(2900), json!({ "outcome": "completed" })),
            row(7, Some(6), "model_call", "haiku", 4530, Some(800), json!({ "message_id": 21, "failed": true, "error": "fatal: HTTP 400" })),
            row(8, Some(1), "model_call", "opus", 7600, Some(1300), json!({ "message_id": 13 })),
        ];
        spans[6].session = "child".into();
        let mut notes = Notes::default();
        notes.prompts.insert("r".into(), vec![(10, "How many files are here?".into())]);
        notes.prompts.insert("child".into(), vec![(20, "Count the lexer files".into())]);
        notes.said.insert(11, (String::new(), "I should list the directory first.".into()));
        notes.said.insert(13, ("There are 8 files.".into(), String::new()));
        notes.results.insert("c1".into(), (false, "10 (+14 lines)".into()));
        let lines = tree(&spans, &notes, 9000, 120);
        let shape: Vec<String> = lines.iter().map(|l| l.split("  ").next().unwrap_or("").chars().take(46).collect()).collect();
        assert_eq!(lines.len(), 8, "{lines:#?}");
        assert!(lines[0].starts_with("\u{25cf} session r \u{203a} \u{201c}How many files are here?\u{201d}"), "{}", lines[0]);
        assert!(lines[1].starts_with("\u{251c}\u{2500} \u{25c6} thinks: I should list the directory first."), "{}", lines[1]);
        assert!(lines[1].contains("[opus \u{00b7} 4.0k in, 90 out]") && lines[1].ends_with("1.40s"), "{}", lines[1]);
        // What the model asked for hangs beneath it, with what came back.
        assert!(lines[2].starts_with("\u{2502}  \u{251c}\u{2500} \u{23f8} waited for approval of bash: allowed"), "{}", lines[2]);
        assert!(lines[3].starts_with("\u{2502}  \u{251c}\u{2500} \u{25b8} bash ls -A | wc -l  \u{2192} 10 (+14 lines)"), "{}", lines[3]);
        assert!(lines[4].starts_with("\u{2502}  \u{2514}\u{2500} \u{25b8} spawn_agents lexer"), "{}", lines[4]);
        assert!(lines[5].contains("\u{25cf} subagent r>child lexer \u{203a} \u{201c}Count the lexer files\u{201d}"), "{}", lines[5]);
        assert!(lines[6].contains("\u{2717} model call failed: fatal: HTTP 400"), "{}", lines[6]);
        assert!(lines[7].starts_with("\u{2514}\u{2500} \u{25c6} \u{201c}There are 8 files.\u{201d}"), "{}  shape {shape:#?}", lines[7]);
        assert!(lines.iter().all(|l| l.chars().count() <= 120), "{lines:#?}");
    }

    #[tokio::test]
    async fn instrumented_code_is_recorded_with_its_parent_and_fields() {
        let dir = std::env::temp_dir().join(format!("narsil-telemetry-{}-{}", std::process::id(), crate::util::new_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("narsil.db");
        let store = Store::open(&db).unwrap();
        // A span left open by a daemon that died.
        rusqlite::Connection::open(&db)
            .unwrap()
            .execute_batch(
                "INSERT INTO spans (id, kind, what, start_ms, dur_ms) VALUES (1, 'tool_call', 'bash', 1000, 500);
                 INSERT INTO spans (id, kind, what, start_ms) VALUES (2, 'model_call', 'old', 1200);",
            )
            .unwrap();
        let (rec, guard) = recorder(&db, 0).unwrap();
        let subscriber = tracing_subscriber::registry().with(rec);
        let _scope = tracing::subscriber::set_default(subscriber);

        #[tracing::instrument(name = "tool_call", skip_all, fields(what = "bash", summary = %command, is_error = tracing::field::Empty, ok = tracing::field::Empty))]
        async fn run_tool(command: &str, fail: bool) {
            tokio::time::sleep(Duration::from_millis(15)).await;
            tracing::Span::current().record("is_error", fail).record("ok", !fail);
        }
        let run = tracing::info_span!("session_run", session = "s1", root = "s1", agent = tracing::field::Empty, what = "chat", outcome = tracing::field::Empty);
        run.record("agent", "lexer");
        async {
            tracing::info_span!("not_recorded").in_scope(|| ());
            run_tool("echo one", false).await;
            // A task of its own keeps its place in the tree when it is handed the current span.
            tokio::spawn(run_tool("false", true).in_current_span()).await.unwrap();
            tracing::Span::current().record("outcome", "completed");
        }
        .instrument(run)
        .await;
        guard.flush();

        let spans = store.spans_for_root("s1").unwrap();
        assert_eq!(spans.iter().map(|s| s.kind.as_str()).collect::<Vec<_>>(), vec!["session_run", "tool_call", "tool_call"]);
        let (session, first, second) = (&spans[0], &spans[1], &spans[2]);
        assert_eq!(session.text("outcome"), "completed");
        assert!(session.id > 2, "ids continue after what is already there");
        assert_eq!((first.parent, second.parent), (Some(session.id), Some(session.id)));
        assert_eq!((first.session.as_str(), first.root.as_str(), first.what.as_str()), ("s1", "s1", "bash"), "inherited from the session");
        assert_eq!((session.agent.as_str(), second.agent.as_str()), ("lexer", "lexer"), "a name given after the span opened still counts");
        assert_eq!(first.text("summary"), "echo one");
        assert!(first.ok && !second.ok && second.data["is_error"] == true);
        assert!(first.dur_ms.unwrap() >= 15 && session.dur_ms.unwrap() >= 30);
        assert!(store.spans_open().unwrap().is_empty());
        // The span the dead daemon left open was closed off at the last recorded moment.
        let old = store.spans_since(0).unwrap().into_iter().find(|s| s.id == 2).unwrap();
        assert_eq!((old.ok, old.dur_ms, old.num("cut_off")), (false, Some(300), 1));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
