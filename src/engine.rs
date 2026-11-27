//! The agent loop.
//!
//! A session's next action is a function of its transcript:
//!   last message is a model turn with tool calls  -> run (the rest of) the tools
//!   last message is a user turn                   -> call the model
//!   last message is a model turn without tools    -> the turn is over
//!
//! Nothing about a run lives only in memory, so "continue after a crash",
//! "resume a paused session" and "take the next step" are the same code path.

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use anyhow::{Result, bail};
use tokio::sync::{broadcast, oneshot};
use tokio_util::sync::CancellationToken;
use tracing::field::Empty;
use tracing::{Instrument, Span};

use crate::config::{Config, Paths, split_model_ref};
use crate::context;
use crate::logln;
use crate::proto::{ApprovalInfo, Envelope, Event, ProviderModels};
use crate::provider::{self, ChatRequest, ChatResponse, Provider, ProviderError, StopReason, StreamEvent};
use crate::store::{AgentInfo, NewSession, Session, Store, ToolCall};
use crate::tools::{self, ToolCtx};
use crate::types::{Block, Message, MsgKind, Role, StoredMessage, Usage, blocks_text};
use crate::util::{estimate_tokens, first_line, new_id, shell_quote, truncate_middle};

pub type BoxFut<T> = Pin<Box<dyn Future<Output = T> + Send + 'static>>;

const CRASHED: &str = "[narsil] This tool call was cut off by a crash or restart of the harness before it \
     finished, so its output was lost and its effects may be partial. Inspect the current state before \
     deciding whether to run it again.";
const NOT_RUN: &str = "[narsil] Not run: the user interrupted the turn.";
const SUPERSEDED: &str = "[narsil] Not run: the user sent a new message instead of continuing.";
const DECLINED: &str = "[narsil] The user declined this tool call. Do not retry it as is; ask what they \
     would prefer, or take a different approach.";
const TRUNCATED: &str = "[narsil] Your reply was cut off at the output-token limit, and any tool call in it \
     was discarded. Continue from where you stopped, splitting large outputs into smaller tool calls.";

#[derive(Debug)]
pub enum Outcome {
    /// The model ended its turn. `text` is its final message.
    Completed { text: String, tool_calls: usize },
    Interrupted,
    Refused(String),
}

pub enum Decision {
    Allow,
    Always,
    Deny,
}

enum Step {
    Continue,
    Truncated,
    Interrupted,
    Refused(String),
}

enum Tail {
    NeedsTools(i64),
    NeedsModel,
    Idle,
}

impl Tail {
    fn of(last: Option<&StoredMessage>) -> Tail {
        match last {
            None => Tail::Idle,
            Some(m) if m.role == Role::User => Tail::NeedsModel,
            Some(m) if m.blocks.iter().any(|b| matches!(b, Block::ToolUse { .. })) => Tail::NeedsTools(m.id),
            Some(_) => Tail::Idle,
        }
    }
}

struct RunHandle {
    cancel: CancellationToken,
    /// Set when the run is being paused or the daemon is stopping, as opposed
    /// to the user aborting the turn: unfinished work is left to be continued.
    suspend: Arc<AtomicBool>,
}

struct PendingApproval {
    info: ApprovalInfo,
    root: String,
    tx: oneshot::Sender<Decision>,
}

pub struct Engine {
    pub paths: Paths,
    pub store: Store,
    cfg: RwLock<Arc<Config>>,
    providers: Mutex<HashMap<String, Arc<Provider>>>,
    pub events: broadcast::Sender<Envelope>,
    runs: Mutex<HashMap<String, RunHandle>>,
    approvals: Mutex<HashMap<String, PendingApproval>>,
    always_allow: Mutex<HashSet<(String, String)>>,
    /// Per session: tool calls made since it last touched the work graph.
    off_the_books: Mutex<HashMap<String, u32>>,
    /// Per session: failed tool calls in a row.
    failing: Mutex<HashMap<String, u32>>,
    /// Commands that failed in a session and have not yet worked: what kind of command, and its text.
    stumbles: Mutex<HashMap<String, Vec<(String, String)>>>,
    /// How many files a session has written in this run with no work laid out (u32::MAX: no longer counting).
    unplanned: Mutex<HashMap<String, u32>>,
    /// The kinds of credential, and the credential stores, a session has already been told how to handle.
    handled: Mutex<HashMap<String, HashSet<String>>>,
    shutting_down: AtomicBool,
}

impl Engine {
    /// Write down what an answered request to a model used: the record that
    /// every report of token use is added up from. `timed` is when the request
    /// was made (unix ms) and how long it took.
    fn count(&self, session: &Session, purpose: &str, provider: &str, model: &str, usage: &Usage, timed: (i64, Duration)) {
        let call = crate::store::ModelCall {
            session: session.id.clone(),
            root: session.root_id.clone(),
            request: self.store.current_request(&session.root_id).unwrap_or(0),
            purpose: purpose.to_string(),
            provider: provider.to_string(),
            model: model.to_string(),
            input: usage.input,
            cache_read: usage.cache_read,
            cache_write: usage.cache_write,
            output: usage.output,
            estimated: usage.estimated,
            started_ms: timed.0,
            dur_ms: timed.1.as_millis() as i64,
        };
        if let Err(e) = self.store.usage_add(&call) {
            logln!("session {}: could not record token use: {e}", session.id);
        }
    }
}

/// The span for one request to a model. One per attempt, so retries show.
fn call_span(model: &str, provider: &str, attempt: u32, purpose: &str, messages: usize) -> Span {
    tracing::info_span!(
        "model_call",
        what = %model,
        provider = %provider,
        attempt,
        purpose,
        messages,
        ttft_ms = Empty,
        input_tokens = Empty,
        cache_read = Empty,
        cache_write = Empty,
        output_tokens = Empty,
        stop = Empty,
        request_id = Empty,
        message_id = Empty,
        error = Empty,
        ok = Empty,
    )
}

/// Put what came back on the call's span. `None` means it was cancelled.
fn record_call(span: &Span, result: Option<&Result<ChatResponse, ProviderError>>, first_token: Option<Duration>) {
    if let Some(t) = first_token {
        span.record("ttft_ms", t.as_millis() as u64);
    }
    match result {
        Some(Ok(resp)) => {
            let u = &resp.usage;
            span.record("input_tokens", u.input + u.cache_read + u.cache_write)
                .record("cache_read", u.cache_read)
                .record("cache_write", u.cache_write)
                .record("output_tokens", u.output)
                .record("stop", match &resp.stop {
                    StopReason::EndTurn => "end_turn",
                    StopReason::ToolUse => "tool_use",
                    StopReason::MaxTokens => "max_tokens",
                    StopReason::Refusal(_) => "refusal",
                })
                .record("ok", !matches!(resp.stop, StopReason::Refusal(_)));
        }
        Some(Err(e)) => {
            let class = match e {
                ProviderError::Retryable { .. } => "retryable",
                ProviderError::ContextOverflow(_) => "context overflow",
                ProviderError::ThinkingMismatch(_) => "reasoning rejected",
                ProviderError::Fatal(_) => "fatal",
            };
            span.record("ok", false).record("error", format!("{class}: {e}").as_str());
        }
        None => {
            span.record("ok", false).record("error", "cancelled");
        }
    }
}

/// Makes sure a session is released if its run ends by a panic, so it is not
/// left looking busy until the daemon restarts.
struct RunGuard<'a> {
    engine: &'a Engine,
    session: &'a Session,
    armed: bool,
}

impl Drop for RunGuard<'_> {
    fn drop(&mut self) {
        if self.armed {
            tracing::error!(session = %self.session.id, "the run ended abnormally; releasing the session as interrupted");
            self.engine.unregister(self.session, "interrupted");
        }
    }
}

fn strip_reasoning(msg: &mut Message) {
    msg.blocks.retain(|b| match b {
        Block::Thinking { signature: Some(_), .. } => false,
        Block::Raw { json, .. } => json["type"] != "redacted_thinking",
        _ => true,
    });
}

/// Plain-text rendering of a transcript, for summarizing a context that no
/// longer fits in the model's window as a conversation.
fn render_transcript(stored: &[StoredMessage]) -> String {
    let mut out = String::new();
    for m in stored {
        out.push_str(&format!("\n### {}\n", m.role.as_str()));
        for b in &m.blocks {
            match b {
                Block::Text { text } => {
                    out.push_str(text);
                    out.push('\n');
                }
                Block::ToolUse { name, input, .. } => {
                    out.push_str(&format!("[tool call {name}: {}]\n", truncate_middle(&input.to_string(), 1500)));
                }
                Block::ToolResult { content, is_error, .. } => {
                    let tag = if *is_error { "tool error" } else { "tool result" };
                    out.push_str(&format!("[{tag}: {}]\n", truncate_middle(content, 1500)));
                }
                _ => {}
            }
        }
    }
    out
}

/// Commands whose exit status is an answer, not a result: "no match", "not there", "they differ".
const ASKS_A_QUESTION: [&str; 20] = [
    "test", "[", "[[", "grep", "egrep", "fgrep", "rg", "diff", "cmp", "which", "type", "ls", "find", "cat", "stat", "pgrep", "pidof", "git diff",
    "git grep", "head",
];

/// Whether a shell command that was asked to print its exit status (`...; echo
/// exit=$?`, a habit of models) printed one other than zero. The line as a
/// whole then returns success, and the failure would otherwise go unseen.
fn reports_failure(call: &ToolCall, output: &str) -> bool {
    static STATUS: std::sync::LazyLock<regex::Regex> =
        std::sync::LazyLock::new(|| regex::Regex::new(r"(?im)\b(?:exit|status|rc|code)\b[^0-9\n]{0,12}([0-9]{1,3})\b").expect("a valid pattern"));
    call.name == "bash"
        && call.input.get("command").and_then(|c| c.as_str()).is_some_and(|c| c.contains("$?"))
        && STATUS.captures_iter(output).any(|c| c[1].parse::<u32>().is_ok_and(|n| n != 0))
}

/// What a shell command is a run of, for telling two forms of the same thing
/// apart from two different things: the program, and for programs that are
/// many tools in one (`cargo`, `make`, `git`), what it was asked to do.
/// `BUILD_MODE=fast ./build.sh --all` and `cd app && ./build.sh` are both `./build.sh`.
fn command_kind(command: &str) -> Option<String> {
    const SETUP: [&str; 6] = ["cd", "export", "set", "source", ".", "echo"];
    const WRAPPERS: [&str; 5] = ["sudo", "env", "time", "nice", "command"];
    const MANY_IN_ONE: [&str; 18] =
        ["cargo", "make", "git", "npm", "pnpm", "yarn", "npx", "go", "docker", "kubectl", "python", "python3", "node", "uv", "pip", "just", "bash", "sh"];
    // The first step that does something: past any `cd dir &&`, and before any pipe.
    let step = command.split("&&").flat_map(|s| s.split(';')).map(|s| s.split('|').next().unwrap_or(s).trim()).find(|s| {
        let first = s.split_whitespace().next().unwrap_or("");
        !first.is_empty() && !SETUP.contains(&first)
    })?;
    let mut words = step.split_whitespace().skip_while(|w| WRAPPERS.contains(w) || (w.contains('=') && !w.starts_with(['-', '/', '.'])));
    let program = words.next()?;
    if !MANY_IN_ONE.contains(&program) {
        return Some(program.to_string());
    }
    Some(match words.find(|w| !w.starts_with('-')) {
        Some(what) => format!("{program} {what}"),
        None => program.to_string(),
    })
}

impl Engine {
    pub fn new(paths: Paths, cfg: Config, store: Store) -> Arc<Engine> {
        let (events, _) = broadcast::channel(8192);
        Arc::new(Engine {
            paths,
            store,
            cfg: RwLock::new(Arc::new(cfg)),
            providers: Mutex::new(HashMap::new()),
            events,
            runs: Mutex::new(HashMap::new()),
            approvals: Mutex::new(HashMap::new()),
            always_allow: Mutex::new(HashSet::new()),
            off_the_books: Mutex::new(HashMap::new()),
            failing: Mutex::new(HashMap::new()),
            stumbles: Mutex::new(HashMap::new()),
            unplanned: Mutex::new(HashMap::new()),
            handled: Mutex::new(HashMap::new()),
            shutting_down: AtomicBool::new(false),
        })
    }

    pub fn cfg(&self) -> Arc<Config> {
        self.cfg.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    pub fn reload_config(&self) -> Result<()> {
        let cfg = Config::load(&self.paths)?;
        *self.cfg.write().unwrap_or_else(|e| e.into_inner()) = Arc::new(cfg);
        self.providers.lock().unwrap_or_else(|e| e.into_inner()).clear();
        Ok(())
    }

    fn provider(&self, name: &str) -> Result<Arc<Provider>> {
        let cfg = self.cfg();
        let mut map = self.providers.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(p) = map.get(name) {
            return Ok(p.clone());
        }
        let p = Arc::new(provider::build(&cfg, &self.paths, name)?);
        map.insert(name.to_string(), p.clone());
        Ok(p)
    }

    pub fn emit(&self, s: &Session, event: Event) {
        let _ = self.events.send(Envelope {
            root: s.root_id.clone(),
            session: s.id.clone(),
            agent: s.agent_name.clone(),
            event,
        });
    }

    pub fn emit_global(&self, event: Event) {
        let _ = self.events.send(Envelope { root: String::new(), session: String::new(), agent: None, event });
    }

    pub fn is_shutting_down(&self) -> bool {
        self.shutting_down.load(Ordering::SeqCst)
    }

    pub fn is_running(&self, sid: &str) -> bool {
        self.runs.lock().unwrap_or_else(|e| e.into_inner()).contains_key(sid)
    }

    pub fn running_sessions(&self) -> Vec<String> {
        self.runs.lock().unwrap_or_else(|e| e.into_inner()).keys().cloned().collect()
    }

    /// Check that `provider/model` names a configured backend.
    pub fn validate_model(&self, model: &str) -> Result<()> {
        let cfg = self.cfg();
        let (p, m) = split_model_ref(model);
        if m.is_empty() {
            bail!("model must be written as provider/model-id, got {model:?}");
        }
        if !cfg.providers.contains_key(p) {
            let known: Vec<&str> = cfg.providers.keys().map(String::as_str).collect();
            bail!("unknown provider {p:?} in model {model:?}; configured providers: {}", known.join(", "));
        }
        Ok(())
    }

    /// Ask every configured provider for its models, all at once. A provider
    /// that cannot answer is still listed, with the reason.
    pub async fn list_models(&self) -> Vec<ProviderModels> {
        let cfg = self.cfg();
        let lookups = cfg.providers.iter().map(|(name, p)| {
            let client = self.provider(name);
            let mut row = ProviderModels {
                name: name.clone(),
                kind: p.kind.clone(),
                base_url: p.base_url.clone(),
                ..Default::default()
            };
            async move {
                match client {
                    Err(e) => row.error = format!("{e:#}"),
                    Ok(client) => match tokio::time::timeout(Duration::from_secs(12), client.list_models()).await {
                        Ok(Ok(models)) => row.models = models,
                        Ok(Err(e)) => row.error = first_line(&format!("{e:#}"), 200),
                        Err(_) => row.error = "no answer within 12 seconds".into(),
                    },
                }
                row
            }
        });
        futures_util::future::join_all(lookups).await
    }

    /// Hand a session to another model, between turns.
    pub fn switch_model(&self, sid: &str, model: &str) -> Result<()> {
        self.validate_model(model)?;
        let session = self.store.require_session(sid)?;
        if self.is_running(sid) {
            bail!("the session is busy; change the model between turns");
        }
        if session.model == model {
            return Ok(());
        }
        self.store.switch_model(sid, model)?;
        if self.store.message_count(sid)? > 0 {
            // In the transcript, so the new model knows the earlier turns were not its own.
            let note = [Block::Text {
                text: format!(
                    "[narsil] The model for this conversation changed from {} to {model}. Earlier replies were \
                     written by the previous model.",
                    session.model
                ),
            }];
            let msg = self.store.append_message(sid, Role::User, MsgKind::Note, &note)?;
            self.emit(&session, Event::Message { msg });
        }
        self.emit(&session, Event::SessionChanged);
        Ok(())
    }

    /// Change `[model] default` or `[model] subagent` in config.toml and apply it.
    pub fn set_config_model(&self, role: &str, model: &str) -> Result<()> {
        if !(role == "subagent" && model.is_empty()) {
            self.validate_model(model)?;
        }
        // From disk, so edits made by hand since the daemon started are kept.
        let mut cfg = Config::load(&self.paths)?;
        match role {
            "default" => cfg.model.default = model.to_string(),
            "subagent" => cfg.model.subagent = model.to_string(),
            other => bail!("unknown model role {other:?} (expected default or subagent)"),
        }
        cfg.save(&self.paths)?;
        self.reload_config()
    }

    pub fn default_model(&self, requested: Option<String>) -> Result<String> {
        let model = requested.filter(|m| !m.is_empty()).unwrap_or_else(|| self.cfg().model.default.clone());
        if model.is_empty() {
            bail!("no model is configured yet; run `narsil setup`");
        }
        self.validate_model(&model)?;
        Ok(model)
    }

    pub fn create_chat_session(&self, cwd: &str, title: Option<String>, model: Option<String>) -> Result<Session> {
        let model = self.default_model(model)?;
        self.store.create_session(NewSession {
            kind: "chat".into(),
            title: title.unwrap_or_default(),
            cwd: cwd.to_string(),
            model,
            perm_mode: self.cfg().permissions.mode.clone(),
            ..Default::default()
        })
    }

    // ---- subagents --------------------------------------------------------

    /// Overlay what only the engine knows on a stored record: whether the agent
    /// is running right now, and what it is in the middle of.
    fn live_agent(&self, mut info: AgentInfo) -> AgentInfo {
        if self.is_running(&info.id) {
            info.state = "running".into();
            if let Ok(Some((name, input))) = self.store.running_tool(&info.id) {
                info.activity = format!("{name} {}", tools::summarize_call(&name, &input)).trim().to_string();
            }
        }
        info
    }

    /// Every subagent in a session tree, as it stands now.
    pub fn agents(&self, root: &str) -> Vec<AgentInfo> {
        self.store.subagents(root).unwrap_or_default().into_iter().map(|a| self.live_agent(a)).collect()
    }

    /// Tell attached clients where a subagent stands. `state` overrides the
    /// derived one for the moments the store cannot see (about to start).
    pub fn emit_agent(&self, child: &Session, state: Option<&str>) {
        if let Ok(Some(info)) = self.store.subagent_get(&child.id) {
            let mut info = self.live_agent(info);
            if let Some(state) = state {
                info.state = state.to_string();
            }
            self.emit(child, Event::Agent { info: Box::new(info) });
        }
    }

    // ---- goals ------------------------------------------------------------

    /// Move a session to another directory (see `Store::switch_directory`).
    /// `note` puts the change in the transcript; a tool call reports it in its
    /// own result instead, since nothing may come between a call and its result.
    pub fn change_directory(&self, sid: &str, path: &str, create: bool, note: bool) -> Result<String> {
        let session = self.store.require_session(sid)?;
        let target = tools::resolve(&session.cwd, path.trim());
        if !target.exists() {
            if !create {
                bail!("{} does not exist; pass `create` to make it", target.display());
            }
            std::fs::create_dir_all(&target).map_err(|e| anyhow::anyhow!("creating {}: {e}", target.display()))?;
        }
        if !target.is_dir() {
            bail!("{} is not a directory", target.display());
        }
        let target = std::fs::canonicalize(&target)?.to_string_lossy().into_owned();
        if target == session.cwd {
            return Ok(target);
        }
        self.store.switch_directory(sid, &target)?;
        if let Some(goal) = session.goal_id.as_ref().filter(|_| session.kind == "goal") {
            // The goal's work, its note and its questions are looked up by its directory.
            self.store.goal_set_cwd(goal, &target)?;
        }
        if note && self.store.message_count(sid)? > 0 {
            let text = format!("[narsil] The working directory is now {target} (it was {}).", session.cwd);
            let msg = self.store.append_message(sid, Role::User, MsgKind::Note, &[Block::Text { text }])?;
            self.emit(&session, Event::Message { msg });
        }
        tracing::info!(session = %sid, "working directory is now {target}");
        self.emit(&session, Event::SessionChanged);
        Ok(target)
    }

    /// What memory holds that bears on `text`, leaving out what the session's
    /// prompt already shows it.
    pub fn recall(&self, session: &Session, text: &str) -> Vec<crate::store::Memory> {
        let cfg = self.cfg();
        crate::memory::refresh(&self.store, &cfg, &self.paths);
        let scopes = vec!["global".to_string(), context::project_scope(&session.cwd)];
        let prompt = self.store.get_session(&session.id).ok().flatten().map(|s| s.system_prompt).unwrap_or_default();
        let have = |title: &str| prompt.contains(&format!("## {title} ["));
        crate::memory::recall(&self.store, &scopes, text, &have, 5)
    }

    /// When something new is asked of a session, put what memory holds on the
    /// subject in front of the agent before it starts: above all, what was
    /// tried before and failed. Costs a search, not a model call.
    pub fn recall_note(&self, session: &Session, text: &str) -> Option<StoredMessage> {
        let from_vault = self.vault_note(session, text);
        let found = self.recall(session, text);
        if found.is_empty() {
            return from_vault;
        }
        let titles: Vec<&str> = found.iter().map(|m| m.title.as_str()).collect();
        let body = format!(
            "[narsil] Recalled from memory: {}. Read these before starting; do not retry what is marked as tried and \
             failed unless something has changed since, and say so to the user instead.\n{}",
            titles.join("; "),
            crate::memory::recalled_text(&found)
        );
        tracing::info!(session = %session.id, "recalled {} memory entries for the new request", found.len());
        let msg = self.store.append_message(&session.id, Role::User, MsgKind::Note, &[Block::Text { text: body }]).ok()?;
        self.emit(session, Event::Message { msg: msg.clone() });
        Some(msg)
    }

    /// Name the user's own notes on the subject of a new request, so that their
    /// Obsidian vault is where the agent looks first. A search of the vault,
    /// not a model call; only the notes' paths are given, and the agent reads
    /// what it needs.
    fn vault_note(&self, session: &Session, text: &str) -> Option<StoredMessage> {
        let cfg = self.cfg();
        if !cfg.obsidian.lookup {
            return None;
        }
        let notes = crate::obsidian::Vault::open(&cfg)?.lookup(text, 5);
        if notes.is_empty() {
            return None;
        }
        let body = format!(
            "[narsil] Obsidian: notes of the user's that may bear on this: {}. For what the user has written down, \
             their vault is the first place to look: read the relevant ones with vault_read before answering from \
             general knowledge, searching the web, or asking them.",
            notes.join("; ")
        );
        tracing::info!(session = %session.id, "named {} vault notes for the new request", notes.len());
        let msg = self.store.append_message(&session.id, Role::User, MsgKind::Note, &[Block::Text { text: body }]).ok()?;
        self.emit(session, Event::Message { msg: msg.clone() });
        Some(msg)
    }

    /// After two failed tool calls in a row, a line asking whether this is a
    /// dead end worth recording. The moment of failure is when the reason is
    /// freshest, and when a model left alone tends to just try something else.
    fn failure_reminder(&self, session: &Session, tool: &str, failed: bool) -> Option<String> {
        let mut streaks = self.failing.lock().unwrap_or_else(|e| e.into_inner());
        if !failed || tool.starts_with("work_") || tool.starts_with("memory_") || tool.starts_with("goal_") {
            if !failed {
                streaks.remove(&session.id);
            }
            return None;
        }
        let n = streaks.entry(session.id.clone()).or_insert(0);
        *n += 1;
        (*n == 2).then(|| {
            "\n\n[narsil] Two attempts in a row have failed. If this path is a dead end for a reason that will still hold \
             next time (the data does not exist, a tool is missing, a service needs a key), record it now with memory_save, \
             kind dead_end: what you were after, what you tried, why it failed. Then take a different path, or tell the user."
                .to_string()
        })
    }

    /// When a command works where another form of the same command failed, a
    /// line asking for what made the difference to be written down. A model
    /// that finds the way through on its second try tends to carry straight
    /// on, and then has to find it again next time. The very same command
    /// passing after it failed means something was fixed in between, which
    /// is ordinary work and gets no reminder.
    fn lesson_reminder(&self, session: &Session, call: &ToolCall, failed: bool, output: &str) -> Option<String> {
        let mut all = self.stumbles.lock().unwrap_or_else(|e| e.into_inner());
        // Writing something down settles what had failed up to now.
        if matches!(call.name.as_str(), "memory_save" | "work_done" | "goal_blocked" | "goal_complete") && !failed {
            all.remove(&session.id);
            return None;
        }
        let (kind, command) = match call.name.as_str() {
            "bash" => {
                let command = call.input.get("command").and_then(|c| c.as_str())?.trim();
                (command_kind(command)?, command)
            }
            // A page that could not be had: another page from the same site settles it.
            "web_fetch" => {
                let url = call.input.get("url").and_then(|u| u.as_str())?.trim();
                let host = url.split("://").nth(1).unwrap_or(url).split('/').next().unwrap_or(url);
                (format!("web_fetch {host}"), url)
            }
            _ => return None,
        };
        // A command that answers a question says no with its exit status, and what narsil
        // itself refused was never tried: neither is a path that failed.
        if failed && (ASKS_A_QUESTION.contains(&kind.as_str()) || output.starts_with("Refused:")) {
            return None;
        }
        let mine = all.entry(session.id.clone()).or_default();
        if failed {
            if !mine.iter().any(|(k, c)| *k == kind && c == command) {
                mine.push((kind, command.to_string()));
                if mine.len() > 24 {
                    mine.remove(0);
                }
            }
            return None;
        }
        let earlier: Vec<String> = mine.iter().filter(|(k, _)| *k == kind).map(|(_, c)| c.clone()).collect();
        mine.retain(|(k, _)| *k != kind);
        let other = earlier.iter().find(|c| c.as_str() != command)?;
        if call.name != "bash" {
            return None;
        }
        Some(format!(
            "\n\n[narsil] This worked where `{}` failed a moment ago. If what made the difference would have to be worked \
             out again next time (a flag, a variable to set, a step that must come first), record it now with memory_save, \
             kind lesson: what fails, what works, and why. Scope it to the project unless it holds everywhere.",
            first_line(other, 100)
        ))
    }

    /// When a session that was simply asked for something (no goal, no plan)
    /// turns out to be building: on its third file written in one run, a line
    /// saying to lay the work out. A request such as "build me a game" deserves
    /// the same steps, visible in the panel and safe against interruption, as
    /// one given as a goal; a one-file fix does not, and never sees this.
    fn plan_reminder(&self, session: &Session, tool: &str) -> Option<String> {
        const FILES: u32 = 3;
        if session.parent_id.is_some() {
            return None;
        }
        let mut counts = self.unplanned.lock().unwrap_or_else(|e| e.into_inner());
        if tool.starts_with("work_") || tool == "spawn_agents" {
            counts.insert(session.id.clone(), u32::MAX);
            return None;
        }
        if !matches!(tool, "write_file" | "edit_file") {
            return None;
        }
        let n = counts.entry(session.id.clone()).or_insert(0);
        if *n == u32::MAX {
            return None;
        }
        *n += 1;
        if *n < FILES {
            return None;
        }
        *n = u32::MAX;
        drop(counts);
        // Not if work is already laid out here and still going.
        let view = crate::work::Overview::of(&self.store, &context::project_dir(&session.cwd)).ok()?;
        if view.items.iter().any(|i| !i.closed()) {
            return None;
        }
        Some(
            "\n\n[narsil] This has become more than a couple of steps. Lay it out now with work_plan: one item per outcome, \
             in the order they must happen, including what is already done. The user sees the plan and its progress in the \
             side panel, and it is what lets the work be picked up again if this run is cut short. Then take each item with \
             work_next as you start it and close it with work_done as it is finished."
                .to_string(),
        )
    }

    /// Drop what is unfinished in a directory's plan, at the user's word: the
    /// plan is abandoned or the work has moved on, and they want the slate
    /// clean. What was done stays on record as done. Returns the titles dropped.
    pub fn clear_work(&self, cwd: &str) -> Result<Vec<String>> {
        let cfg = self.cfg();
        let project = context::project_dir(cwd);
        crate::work::refresh(&self.store, &cfg, &self.paths);
        let view = crate::work::Overview::of(&self.store, &project)?;
        let open: Vec<&crate::work::Item> = view.items.iter().filter(|i| !i.closed()).collect();
        if let Some(busy) = open.iter().find(|i| !i.claimed_by.is_empty() && self.is_running(&i.claimed_by)) {
            bail!("{:?} is being worked on right now (session {}); interrupt it first", busy.title, busy.claimed_by);
        }
        let titles: Vec<String> = open.iter().map(|i| i.title.clone()).collect();
        {
            let graph = crate::work::Graph::open(&self.store, &cfg, &self.paths);
            let by = crate::work::By { session: "", agent: "", message_id: 0 };
            for title in &titles {
                graph.update(by, &project, title, Some("dropped"), Some("cleared by the user"), &[])?;
            }
        }
        self.emit_work(&project);
        tracing::info!("cleared the work in {project}: {} unfinished item(s) dropped", titles.len());
        Ok(titles)
    }

    /// When what a tool returned holds a credential, or a credential store was
    /// read, a line saying how to handle it: use it for the task and the
    /// service it belongs to, and do not let it spread. The agent is not
    /// stopped from reading or using it. Said once in a session for each kind
    /// of credential and each store.
    fn credential_reminder(&self, session: &Session, call: &ToolCall, output: &str) -> Option<String> {
        let text = |key: &str| call.input.get(key).and_then(|v| v.as_str());
        let store = match call.name.as_str() {
            "bash" => text("command").and_then(|c| crate::secrets::named_in(c, std::path::Path::new(&session.cwd))),
            "read_file" | "grep" => text("path").filter(|p| crate::secrets::is_store(&tools::resolve(&session.cwd, p))).map(str::to_string),
            _ => None,
        };
        let holds = crate::secrets::kinds(output);
        if holds.is_empty() && store.is_none() {
            return None;
        }
        let mut told = self.handled.lock().unwrap_or_else(|e| e.into_inner());
        let mine = told.entry(session.root_id.clone()).or_default();
        let new_kinds: Vec<&str> = holds.iter().copied().filter(|k| mine.insert(k.to_string())).collect();
        let new_store = store.filter(|s| mine.insert(format!("store:{s}")));
        if new_kinds.is_empty() && new_store.is_none() {
            return None;
        }
        tracing::info!(session = %session.id, "a tool result held a credential; the agent was told how to handle it");
        Some(crate::secrets::reminder(&new_kinds, new_store.as_deref()))
    }

    /// When a chat turn is about to end with something having failed and
    /// nothing written down, the question to put before it ends: is this a
    /// dead end? One decisive failure is exactly when a model gives up and
    /// tells the user, and exactly what the next session should not have to
    /// find out again. Goals and subagents are asked the same thing by the
    /// tools they finish with.
    fn unrecorded_failure(&self, session: &Session) -> Option<String> {
        let left = self.stumbles.lock().unwrap_or_else(|e| e.into_inner()).remove(&session.id)?;
        if session.kind != "chat" {
            return None;
        }
        let (_, command) = left.last()?;
        Some(format!(
            "[narsil] Before you finish: `{}` failed in this turn and nothing was recorded. If it failed for a reason that \
             will still hold next time (the source is gone, a tool or a key is missing, the data does not exist), record it \
             now with memory_save, kind dead_end: what was wanted, what was tried, and why it failed. If the reason will \
             not hold (a slip, something since fixed, a passing fault), do nothing more. Either way, do not repeat your answer.",
            first_line(command, 100)
        ))
    }

    /// If a session has work laid out but has gone many tool calls without
    /// taking or closing anything, a line to add to its next tool result.
    /// Progress recorded only at the end is no record at all if the run is cut
    /// short, and shows the user nothing meanwhile.
    fn work_reminder(&self, session: &Session, tool: &str) -> Option<String> {
        const EVERY: u32 = 8;
        let mut counts = self.off_the_books.lock().unwrap_or_else(|e| e.into_inner());
        if tool.starts_with("work_") || tool == "spawn_agents" || tool.starts_with("goal_") {
            counts.remove(&session.id);
            return None;
        }
        let n = counts.entry(session.id.clone()).or_insert(0);
        *n += 1;
        if *n < EVERY {
            return None;
        }
        *n = 0;
        drop(counts);
        let view = crate::work::Overview::of(&self.store, &context::project_dir(&session.cwd)).ok()?;
        let live: Vec<&crate::work::Item> = view.items.iter().filter(|i| !view.root_of(i).closed() && !i.closed()).collect();
        let open = live.iter().filter(|i| view.is_ready(i)).count();
        let held = live.iter().any(|i| i.status == "in_progress" && (i.claimed_by == session.id || self.is_running(&i.claimed_by)));
        (open > 0 && !held).then(|| {
            "\n\n[narsil] Work is laid out for this project, but no item is in progress. If you are working on one, take \
             it with work_next (claim) so progress is visible and survives a restart, and close each finished item with \
             work_done as you go, not all at the end."
                .to_string()
        })
    }

    /// Tell clients where a project's work stands.
    pub fn emit_work(&self, project: &str) {
        if let Ok(items) = crate::work::load(&self.store, Some(project)) {
            self.emit_global(Event::Work { project: project.to_string(), items });
        }
    }

    /// Refresh the goal's note in the Obsidian vault, if that is switched on.
    pub fn write_goal_note(&self, goal_id: &str) {
        let cfg = self.cfg();
        if !cfg.obsidian.goal_notes {
            return;
        }
        let (Some(vault), Ok(Some(goal))) = (crate::obsidian::Vault::open(&cfg), self.store.goal_get(goal_id)) else { return };
        if let Err(e) = crate::obsidian::write_goal_note(&self.store, &vault, &goal) {
            tracing::warn!(goal = %goal_id, "could not write the goal's note in the vault: {e:#}");
        }
    }

    pub fn set_goal_status(&self, id: &str, status: &str, note: &str) -> Result<()> {
        self.store.goal_set_status(id, status, note)?;
        self.write_goal_note(id);
        if let Some(goal) = self.store.goal_get(id)? {
            tracing::info!(goal = %id, "goal is now {status}{}", if note.is_empty() { String::new() } else { format!(": {}", first_line(note, 200)) });
            if matches!(status, "completed" | "blocked" | "paused") {
                self.notify(&format!("Goal {status}: {} ({})", first_line(&goal.objective, 80), first_line(note, 160)));
            }
            self.emit_global(Event::Goal { goal });
        }
        Ok(())
    }

    fn notify(&self, message: &str) {
        let template = self.cfg().notify.command.clone();
        if template.trim().is_empty() {
            return;
        }
        let line = template.replace("{message}", &shell_quote(message));
        let _ = std::process::Command::new("sh")
            .arg("-c")
            .arg(line)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
    }

    // ---- approvals --------------------------------------------------------

    fn needs_approval(&self, session: &Session, call: &ToolCall, cfg: &Config) -> bool {
        // A subagent follows the permission mode of the session it works for.
        let mode = if session.parent_id.is_some() {
            self.store.get_session(&session.root_id).ok().flatten().map(|r| r.perm_mode).unwrap_or_else(|| "ask".into())
        } else {
            session.perm_mode.clone()
        };
        if mode != "ask" {
            return false;
        }
        let key = (session.root_id.clone(), call.name.clone());
        if self.always_allow.lock().unwrap_or_else(|e| e.into_inner()).contains(&key) {
            return false;
        }
        tools::needs_approval(&call.name, &call.input, cfg)
    }

    async fn ask_approval(&self, session: &Session, call: &ToolCall, cancel: &CancellationToken) -> Option<Decision> {
        let id = new_id();
        let (tx, rx) = oneshot::channel();
        let info = ApprovalInfo {
            id: id.clone(),
            session: session.id.clone(),
            agent: session.agent_name.clone(),
            tool: call.name.clone(),
            summary: tools::describe_call(&call.name, &call.input),
        };
        self.approvals.lock().unwrap_or_else(|e| e.into_inner()).insert(
            id.clone(),
            PendingApproval { info: info.clone(), root: session.root_id.clone(), tx },
        );
        self.emit(session, Event::Approval { id: id.clone(), tool: info.tool, summary: info.summary });
        // Time spent waiting for a person is its own span, so it is not mistaken for slow work.
        let waiting = tracing::info_span!("approval_wait", what = %call.name, asked_in = call.message_id, decision = Empty);
        let decision = tokio::select! {
            d = rx => d.ok(),
            _ = cancel.cancelled() => None,
        };
        waiting.record("decision", match &decision {
            Some(Decision::Allow) => "allowed",
            Some(Decision::Always) => "always allowed",
            Some(Decision::Deny) => "denied",
            None => "cancelled",
        });
        drop(waiting);
        self.approvals.lock().unwrap_or_else(|e| e.into_inner()).remove(&id);
        self.emit(session, Event::ApprovalDone { id });
        decision
    }

    pub fn resolve_approval(&self, id: &str, decision: Decision) -> bool {
        let pending = self.approvals.lock().unwrap_or_else(|e| e.into_inner()).remove(id);
        match pending {
            Some(p) => p.tx.send(decision).is_ok(),
            None => false,
        }
    }

    pub fn pending_approvals(&self, root: &str) -> Vec<ApprovalInfo> {
        self.approvals
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .filter(|p| p.root == root)
            .map(|p| p.info.clone())
            .collect()
    }

    // ---- run control ------------------------------------------------------

    /// Claim the session for a run. Returns None if it is already running.
    fn try_register(&self, session: &Session, cancel: &CancellationToken) -> Option<Arc<AtomicBool>> {
        if self.is_shutting_down() {
            return None;
        }
        let mut runs = self.runs.lock().unwrap_or_else(|e| e.into_inner());
        if runs.contains_key(&session.id) {
            return None;
        }
        // A subagent shares its parent's suspend flag, so pausing the parent
        // suspends the whole tree in a resumable state.
        let suspend = session
            .parent_id
            .as_ref()
            .and_then(|p| runs.get(p))
            .map(|h| h.suspend.clone())
            .unwrap_or_default();
        runs.insert(session.id.clone(), RunHandle { cancel: cancel.clone(), suspend: suspend.clone() });
        let _ = self.store.set_run_state(&session.id, "running");
        self.emit(session, Event::RunState { state: "running".into() });
        Some(suspend)
    }

    fn unregister(&self, session: &Session, state: &str) {
        let mut runs = self.runs.lock().unwrap_or_else(|e| e.into_inner());
        runs.remove(&session.id);
        let _ = self.store.set_run_state(&session.id, state);
        self.emit(session, Event::RunState { state: state.into() });
    }

    /// Stop a run. With `suspend`, work in flight is left resumable; without,
    /// the turn is closed out so the user can say something else.
    pub fn interrupt(&self, sid: &str, suspend: bool) -> bool {
        let runs = self.runs.lock().unwrap_or_else(|e| e.into_inner());
        match runs.get(sid) {
            Some(h) => {
                if suspend {
                    h.suspend.store(true, Ordering::SeqCst);
                }
                h.cancel.cancel();
                true
            }
            None => false,
        }
    }

    /// Suspend every run and wait for them to wind down.
    pub async fn shutdown(&self) {
        self.shutting_down.store(true, Ordering::SeqCst);
        {
            let runs = self.runs.lock().unwrap_or_else(|e| e.into_inner());
            for h in runs.values() {
                h.suspend.store(true, Ordering::SeqCst);
                h.cancel.cancel();
            }
        }
        for _ in 0..150 {
            if self.runs.lock().unwrap_or_else(|e| e.into_inner()).is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    /// Queue a user message and make sure something will act on it.
    pub fn send_user_message(self: &Arc<Self>, sid: &str, text: &str) -> Result<()> {
        let session = self.store.require_session(sid)?;
        if session.title.is_empty() {
            self.store.set_title(sid, &first_line(text, 60))?;
        }
        self.store.pending_add(sid, text)?;
        if session.kind == "goal" {
            if let Some(goal) = session.goal_id.as_ref().and_then(|g| self.store.goal_get(g).ok().flatten()) {
                match goal.status.as_str() {
                    // The goal supervisor drives this session and delivers the message.
                    "active" => return Ok(()),
                    "paused" | "blocked" => {
                        self.set_goal_status(&goal.id, "active", "resumed by a message from the user")?;
                        return Ok(());
                    }
                    _ => {}
                }
            }
        }
        self.start_run(sid);
        Ok(())
    }

    /// Run a session in the background until its turn ends.
    pub fn start_run(self: &Arc<Self>, sid: &str) {
        let this = self.clone();
        let sid = sid.to_string();
        tokio::spawn(async move {
            loop {
                if this.is_running(&sid) || this.is_shutting_down() {
                    break;
                }
                let result = this.run_session(sid.clone(), CancellationToken::new()).await;
                // A message may have arrived just as the run was finishing.
                let more = this.store.pending_count(&sid).unwrap_or(0) > 0;
                if !more || !matches!(result, Ok(Outcome::Completed { .. })) {
                    break;
                }
            }
        });
    }

    /// Drive a session until the model ends its turn, the run is cancelled, or
    /// it fails. Safe to call on any session at any time: it continues from
    /// whatever the transcript says is next.
    pub fn run_session(self: &Arc<Self>, sid: String, cancel: CancellationToken) -> BoxFut<Result<Outcome>> {
        let this = self.clone();
        Box::pin(async move {
            let session = this.store.require_session(&sid)?;
            let span = tracing::info_span!(
                "session_run",
                session = %session.id,
                root = %session.root_id,
                agent = Empty,
                goal = Empty,
                what = %session.kind,
                outcome = Empty,
                tool_calls = Empty,
                error = Empty,
                ok = Empty,
            );
            if let Some(agent) = &session.agent_name {
                span.record("agent", agent.as_str());
            }
            if let Some(goal) = &session.goal_id {
                span.record("goal", goal.as_str());
            }
            async {
                let Some(suspend) = this.try_register(&session, &cancel) else {
                    Span::current().record("ok", false).record("outcome", "not started");
                    bail!("session {sid} is already running, or narsil is shutting down");
                };
                let mut guard = RunGuard { engine: &this, session: &session, armed: true };
                let result = this.run_loop(&session, &cancel, &suspend).await;
                guard.armed = false;
                let suspended = suspend.load(Ordering::SeqCst);
                let state = match &result {
                    Ok(Outcome::Interrupted) if suspended => "interrupted",
                    _ => "idle",
                };
                let run = Span::current();
                match &result {
                    Ok(Outcome::Completed { tool_calls, .. }) => {
                        run.record("outcome", "completed").record("tool_calls", *tool_calls as u64);
                    }
                    Ok(Outcome::Interrupted) => {
                        run.record("outcome", if suspended { "suspended, resumable" } else { "interrupted by the user" });
                    }
                    Ok(Outcome::Refused(why)) => {
                        run.record("ok", false).record("outcome", "refused").record("error", why.as_str());
                    }
                    Err(e) => {
                        run.record("ok", false).record("outcome", "failed").record("error", format!("{e:#}").as_str());
                        tracing::error!(session = %session.id, "session run failed: {e:#}");
                        this.emit(&session, Event::Notice { level: "error".into(), text: format!("{e:#}") });
                    }
                }
                this.unregister(&session, state);
                result
            }
            .instrument(span)
            .await
        })
    }

    fn deliver_inputs(&self, session: &Session) -> Result<usize> {
        let delivered = self.store.pending_deliver(&session.id)?;
        let n = delivered.len();
        let mut asked = String::new();
        for msg in delivered {
            asked.push_str(&blocks_text(&msg.blocks));
            asked.push('\n');
            self.emit(session, Event::Message { msg });
        }
        // Subagents are briefed by their parent, which has already been through this.
        if n > 0 && session.parent_id.is_none() {
            self.ensure_prompt(session)?;
            self.recall_note(session, &asked);
        }
        Ok(n)
    }

    /// Take the system prompt snapshot for this context epoch, if it has not been taken.
    fn ensure_prompt(&self, session: &Session) -> Result<String> {
        let fresh = self.store.require_session(&session.id)?;
        if !fresh.system_prompt.is_empty() {
            return Ok(fresh.system_prompt);
        }
        let prompt = context::build_system_prompt(&self.cfg(), &self.paths, &self.store, &fresh);
        self.store.set_system_prompt(&fresh.id, &prompt)?;
        Ok(prompt)
    }

    async fn run_loop(
        self: &Arc<Self>,
        session: &Session,
        cancel: &CancellationToken,
        suspend: &Arc<AtomicBool>,
    ) -> Result<Outcome> {
        let sid = &session.id;
        let mut tool_calls = 0usize;
        let mut truncations = 0;
        let mut first = true;
        // What failed in an earlier turn is that turn's business.
        self.stumbles.lock().unwrap_or_else(|e| e.into_inner()).remove(sid);
        self.unplanned.lock().unwrap_or_else(|e| e.into_inner()).remove(sid);
        loop {
            if cancel.is_cancelled() {
                return Ok(Outcome::Interrupted);
            }
            let last = self.store.last_message(sid)?;
            let tail = Tail::of(last.as_ref());
            if std::mem::take(&mut first) {
                if let Tail::NeedsTools(mid) = tail {
                    if self.store.pending_count(sid)? > 0 {
                        // The user moved on instead of continuing an interrupted turn.
                        self.store.abort_unfinished_tools(sid, mid, SUPERSEDED)?;
                        let msg = self.store.commit_tool_results(sid, mid)?;
                        self.emit(session, Event::Message { msg });
                        continue;
                    }
                }
            }
            match tail {
                Tail::NeedsTools(mid) => {
                    tool_calls += self.run_tools(session, mid, cancel, suspend).await?;
                }
                Tail::NeedsModel => {
                    self.deliver_inputs(session)?;
                    match self.model_step(session, cancel).await? {
                        Step::Continue => truncations = 0,
                        Step::Truncated => {
                            truncations += 1;
                            if truncations >= 3 {
                                bail!(
                                    "the model hit its output limit three times in a row; raise \
                                     [model] max_output_tokens or ask for smaller steps"
                                );
                            }
                        }
                        Step::Interrupted => return Ok(Outcome::Interrupted),
                        Step::Refused(why) => return Ok(Outcome::Refused(why)),
                    }
                }
                Tail::Idle => {
                    if self.deliver_inputs(session)? == 0 {
                        // About to stop with a failure unrecorded: ask, once, whether it is a dead end.
                        if let Some(question) = self.unrecorded_failure(session) {
                            let msg = self.store.append_message(sid, Role::User, MsgKind::Note, &[Block::Text { text: question }])?;
                            self.emit(session, Event::Message { msg });
                            continue;
                        }
                        let text = last.map(|m| blocks_text(&m.blocks)).unwrap_or_default();
                        return Ok(Outcome::Completed { text, tool_calls });
                    }
                }
            }
        }
    }

    /// Execute the tool calls of one model turn that have not finished yet,
    /// then commit all results as a single user turn.
    async fn run_tools(
        self: &Arc<Self>,
        s0: &Session,
        mid: i64,
        cancel: &CancellationToken,
        suspend: &Arc<AtomicBool>,
    ) -> Result<usize> {
        let cfg = self.cfg();
        let sid = &s0.id;
        let calls = self.store.tool_calls(sid, mid)?;
        let mut ran = 0;
        for call in &calls {
            if call.status == "done" {
                continue;
            }
            if cancel.is_cancelled() {
                break;
            }
            let session = self.store.require_session(sid)?;
            let resuming = call.status == "running";
            if resuming && !(call.name == "spawn_agents" && call.state.is_some()) {
                // Journaled as started but never finished: the harness died mid-call.
                // Stopped just now, or already when the daemon started.
                let orphan = call.name == "bash"
                    && call.state.as_ref().is_some_and(|s| s["stopped"] == 1 || tools::shell::kill_orphan(s));
                let text = if orphan {
                    format!("{CRASHED} The command was still running and has now been stopped.")
                } else {
                    CRASHED.to_string()
                };
                self.store.tool_call_finish(sid, call, &text, true)?;
                self.emit(&session, Event::ToolEnd { id: call.id.clone(), is_error: true, output: text });
                continue;
            }
            if !resuming && self.needs_approval(&session, call, &cfg) {
                match self.ask_approval(&session, call, cancel).await {
                    None => break,
                    Some(Decision::Deny) => {
                        self.store.tool_call_finish(sid, call, DECLINED, true)?;
                        self.emit(&session, Event::ToolStart { id: call.id.clone(), name: call.name.clone(), input: call.input.clone() });
                        self.emit(&session, Event::ToolEnd { id: call.id.clone(), is_error: true, output: "declined".into() });
                        continue;
                    }
                    Some(Decision::Always) => {
                        self.always_allow
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .insert((session.root_id.clone(), call.name.clone()));
                    }
                    Some(Decision::Allow) => {}
                }
            }
            self.store.tool_call_start(sid, call)?;
            self.emit(&session, Event::ToolStart { id: call.id.clone(), name: call.name.clone(), input: call.input.clone() });
            let ctx = ToolCtx { engine: self, session: &session, cancel, call, cfg: &cfg };
            let span = tracing::info_span!(
                "tool_call",
                what = %call.name,
                call = %call.id,
                asked_in = mid,
                summary = %first_line(&tools::summarize_call(&call.name, &call.input), 160),
                is_error = Empty,
                output_bytes = Empty,
                ok = Empty,
            );
            // Run inside the span, so a subagent this call starts is recorded beneath it.
            let out = tools::execute(&ctx).instrument(span.clone()).await;
            let mut out = out;
            if let Some(reminder) = self.work_reminder(&session, &call.name) {
                out.content.push_str(&reminder);
            }
            // For the reminders, a command that printed its own non-zero status failed,
            // whatever the shell returned for the line as a whole.
            let failed = out.is_error || reports_failure(&call, &out.content);
            if let Some(reminder) = self.failure_reminder(&session, &call.name, failed) {
                out.content.push_str(&reminder);
            }
            if let Some(reminder) = self.lesson_reminder(&session, &call, failed, &out.content) {
                out.content.push_str(&reminder);
            }
            if let Some(reminder) = self.credential_reminder(&session, &call, &out.content) {
                out.content.push_str(&reminder);
            }
            if let Some(reminder) = self.plan_reminder(&session, &call.name).filter(|_| !out.is_error) {
                out.content.push_str(&reminder);
            }
            span.record("is_error", out.is_error).record("ok", !out.is_error).record("output_bytes", out.content.len() as u64);
            tracing::debug!(parent: &span, tool = %call.name, is_error = out.is_error, bytes = out.content.len(), "tool call finished");
            drop(span);
            if cancel.is_cancelled() && suspend.load(Ordering::SeqCst) && call.name == "spawn_agents" {
                // Left as running: on resume the same children are picked back up.
                break;
            }
            self.store.tool_call_finish(sid, call, &out.content, out.is_error)?;
            self.emit(
                &session,
                Event::ToolEnd { id: call.id.clone(), is_error: out.is_error, output: truncate_middle(&out.content, 6000) },
            );
            ran += 1;
        }
        if cancel.is_cancelled() {
            if suspend.load(Ordering::SeqCst) {
                return Ok(ran);
            }
            self.store.abort_unfinished_tools(sid, mid, NOT_RUN)?;
        }
        let msg = self.store.commit_tool_results(sid, mid)?;
        self.emit(s0, Event::Message { msg });
        Ok(ran)
    }

    /// One model call: assemble context, stream the reply, store it.
    async fn model_step(self: &Arc<Self>, s0: &Session, cancel: &CancellationToken) -> Result<Step> {
        let cfg = self.cfg();
        let mut session = self.store.require_session(&s0.id)?;
        let (pname, model_id) = {
            let (p, m) = split_model_ref(&session.model);
            (p.to_string(), m.to_string())
        };
        let provider = self.provider(&pname)?;
        let mut compacted = false;
        if session.context_tokens > cfg.compact_limit(&pname)
            && self.store.messages_from(&session.id, session.epoch_start)?.len() > 2
        {
            if !self.compact(&session, &provider, &model_id, cancel, false).await? {
                return Ok(Step::Interrupted);
            }
            session = self.store.require_session(&s0.id)?;
            compacted = true;
        }
        if session.system_prompt.is_empty() {
            // First call of this session (or after a change of model or directory):
            // take the snapshot it will keep for the epoch.
            session.system_prompt = self.ensure_prompt(&session)?;
        }
        let tools = tools::specs(&session, &cfg);
        let retry_forever = session.goal_id.is_some();
        let mut attempt: u32 = 0;
        let mut stripped = false;
        // The span of the latest request, kept so the reply's message id can be put on it.
        let mut call: Option<Span>;
        // When the latest request was made, and how long it took.
        let mut timed: (i64, Duration);

        let response = loop {
            let result = {
                let stored = self.store.messages_from(&session.id, session.epoch_start)?;
                let messages: Vec<Message> = stored
                    .iter()
                    .map(|m| {
                        let mut msg = m.to_message();
                        if m.id < session.strip_thinking_before {
                            strip_reasoning(&mut msg);
                        }
                        msg
                    })
                    .collect();
                let req = ChatRequest {
                    model: &model_id,
                    system: &session.system_prompt,
                    messages: &messages,
                    tools: &tools,
                    max_tokens: cfg.model.max_output_tokens,
                    effort: &cfg.model.effort,
                    thinking: &cfg.model.thinking,
                    strip_thinking: false,
                };
                let span = call_span(&model_id, &pname, attempt + 1, "turn", messages.len());
                let started = std::time::Instant::now();
                let began = crate::telemetry::now_ms();
                let first_token: std::sync::OnceLock<Duration> = std::sync::OnceLock::new();
                let sink = |ev: StreamEvent| {
                    first_token.get_or_init(|| started.elapsed());
                    match ev {
                        StreamEvent::Text(text) => self.emit(&session, Event::TextDelta { text }),
                        StreamEvent::Thinking(text) => self.emit(&session, Event::ThinkingDelta { text }),
                    }
                };
                let result = tokio::select! {
                    r = provider.chat(&req, &sink).instrument(span.clone()) => Some(r),
                    _ = cancel.cancelled() => None,
                };
                record_call(&span, result.as_ref(), first_token.get().copied());
                call = Some(span);
                timed = (began, started.elapsed());
                result
            };
            match result {
                None => {
                    self.emit(&session, Event::StreamReset);
                    return Ok(Step::Interrupted);
                }
                Some(Ok(resp)) => break resp,
                Some(Err(ProviderError::Retryable { message, retry_after })) => {
                    attempt += 1;
                    self.emit(&session, Event::StreamReset);
                    if !retry_forever && attempt >= cfg.retry.max_attempts.max(1) {
                        bail!("the model backend kept failing ({attempt} attempts): {message}");
                    }
                    let cap = Duration::from_secs(cfg.retry.max_backoff_secs.max(1));
                    let delay = retry_after.unwrap_or_else(|| Duration::from_secs(1u64 << attempt.min(9))).min(cap);
                    if !delay.is_zero() {
                        let text = format!("{message}; retrying in {}s (attempt {attempt})", delay.as_secs().max(1));
                        tracing::warn!("{text}");
                        self.emit(&session, Event::Notice { level: "warn".into(), text });
                    }
                    let _waiting = tracing::info_span!("retry_wait", what = "model backend", reason = %first_line(&message, 160), secs = delay.as_secs());
                    tokio::select! {
                        _ = tokio::time::sleep(delay) => {}
                        _ = cancel.cancelled() => return Ok(Step::Interrupted),
                    }
                }
                Some(Err(ProviderError::ContextOverflow(m))) => {
                    self.emit(&session, Event::StreamReset);
                    if compacted {
                        bail!("the conversation does not fit the model's context even after compaction: {m}");
                    }
                    if !self.compact(&session, &provider, &model_id, cancel, true).await? {
                        return Ok(Step::Interrupted);
                    }
                    session = self.store.require_session(&s0.id)?;
                    compacted = true;
                }
                Some(Err(ProviderError::ThinkingMismatch(m))) => {
                    self.emit(&session, Event::StreamReset);
                    if stripped {
                        bail!("{m}");
                    }
                    stripped = true;
                    // The API will not accept the stored reasoning for this history
                    // (model switch, edited prompt). Drop it once and carry on.
                    let next = self.store.last_message(&session.id)?.map(|m| m.id + 1).unwrap_or(0);
                    self.store.set_strip_thinking_before(&session.id, next)?;
                    session.strip_thinking_before = next;
                    self.emit(
                        &session,
                        Event::Notice {
                            level: "info".into(),
                            text: "earlier reasoning blocks were not accepted for this history and were dropped".into(),
                        },
                    );
                }
                Some(Err(ProviderError::Fatal(m))) => {
                    self.emit(&session, Event::StreamReset);
                    bail!("{m}");
                }
            }
        };

        let usage = response.usage.clone();
        self.count(&session, "turn", &pname, &model_id, &usage, timed);
        logln!(
            "model call used {} input tokens ({} read from cache, {} written to it), {} output",
            usage.input + usage.cache_read + usage.cache_write,
            usage.cache_read,
            usage.cache_write,
            usage.output
        );
        // Ties the call to the reply it produced, so a trace can show what the model said.
        let link = |msg: &StoredMessage| {
            if let Some(span) = &call {
                span.record("message_id", msg.id);
            }
        };
        let record = |context: i64| {
            let input = usage.input + usage.cache_read + usage.cache_write;
            let _ = self.store.add_usage(&session.id, input, usage.output, context);
            if let Ok(Some(s)) = self.store.get_session(&session.id) {
                self.emit(
                    &session,
                    Event::Usage { context_tokens: s.context_tokens, input_tokens: s.input_tokens, output_tokens: s.output_tokens },
                );
            }
        };
        let mut blocks = response.blocks;
        match response.stop {
            StopReason::Refusal(detail) => {
                // Never act on a declined turn: discard it, tool calls included.
                self.emit(&session, Event::StreamReset);
                let why = if detail.is_empty() { String::new() } else { format!(" ({detail})") };
                let text = format!("[No reply: the model declined this request{why}.]");
                let msg = self.store.append_assistant(&session.id, &[Block::Text { text }])?;
                link(&msg);
                self.emit(&session, Event::Message { msg });
                record(usage.context_tokens());
                Ok(Step::Refused(detail))
            }
            StopReason::MaxTokens => {
                // A truncated tool call would run with half its arguments.
                blocks.retain(|b| !matches!(b, Block::ToolUse { .. }));
                if !blocks.is_empty() {
                    let msg = self.store.append_assistant(&session.id, &blocks)?;
                    link(&msg);
                    self.emit(&session, Event::Message { msg });
                } else {
                    self.emit(&session, Event::StreamReset);
                }
                let note = [Block::Text { text: TRUNCATED.into() }];
                let msg = self.store.append_message(&session.id, Role::User, MsgKind::Note, &note)?;
                self.emit(&session, Event::Message { msg });
                record(usage.context_tokens());
                Ok(Step::Truncated)
            }
            StopReason::EndTurn | StopReason::ToolUse => {
                let substantive = blocks.iter().any(|b| match b {
                    Block::Text { text } => !text.trim().is_empty(),
                    Block::ToolUse { .. } => true,
                    _ => false,
                });
                if !substantive {
                    blocks.push(Block::Text { text: "(no reply)".into() });
                }
                let msg = self.store.append_assistant(&session.id, &blocks)?;
                link(&msg);
                self.emit(&session, Event::Message { msg });
                record(usage.context_tokens());
                Ok(Step::Continue)
            }
        }
    }

    /// Replace the current context with a summary and start a new epoch.
    /// Returns false if cancelled. `overflowed` means the conversation no
    /// longer fits as-is and is summarized from a plain-text rendering.
    #[tracing::instrument(
        name = "compaction",
        skip_all,
        fields(session = %session.id, root = %session.root_id, what = if overflowed { "context overflow" } else { "over the limit" }, messages = Empty, summary_tokens = Empty, ok = Empty)
    )]
    pub async fn compact(
        self: &Arc<Self>,
        session: &Session,
        provider: &Provider,
        model_id: &str,
        cancel: &CancellationToken,
        overflowed: bool,
    ) -> Result<bool> {
        let cfg = self.cfg();
        self.emit(session, Event::Notice { level: "info".into(), text: "compacting the conversation…".into() });
        let stored = self.store.messages_from(&session.id, session.epoch_start)?;
        if stored.is_empty() {
            return Ok(true);
        }
        Span::current().record("messages", stored.len() as u64);
        let last_user_text = stored
            .last()
            .filter(|m| m.role == Role::User && m.kind == MsgKind::Message)
            .map(|m| blocks_text(&m.blocks))
            .filter(|t| !t.trim().is_empty());

        let (pname, _) = split_model_ref(&session.model);
        let (system, mut messages, tools) = if overflowed {
            let budget = (cfg.compact_limit(pname).max(4000) * 2) as usize;
            let rendered = truncate_middle(&render_transcript(&stored), budget);
            (
                "You write faithful, complete handoff summaries of agent transcripts.".to_string(),
                vec![Message::user_text(format!("Transcript of the conversation so far:\n{rendered}"))],
                Vec::new(),
            )
        } else {
            // Same prefix as the conversation itself, so the provider's prompt
            // cache covers everything except the request for a summary.
            let messages = stored
                .iter()
                .map(|m| {
                    let mut msg = m.to_message();
                    if m.id < session.strip_thinking_before {
                        strip_reasoning(&mut msg);
                    }
                    msg
                })
                .collect();
            (session.system_prompt.clone(), messages, tools::specs(session, &cfg))
        };
        messages.push(Message::user_text(context::COMPACTION_REQUEST));
        let req = ChatRequest {
            model: model_id,
            system: &system,
            messages: &messages,
            tools: &tools,
            max_tokens: cfg.model.max_output_tokens,
            effort: &cfg.model.effort,
            thinking: &cfg.model.thinking,
            strip_thinking: false,
        };
        let sink = |_ev: StreamEvent| {};
        let mut attempt = 0u32;
        let (pname, _) = split_model_ref(&session.model);
        let mut timed: (i64, Duration);
        let response = loop {
            let span = call_span(model_id, pname, attempt + 1, "compaction", messages.len());
            let (began, started) = (crate::telemetry::now_ms(), std::time::Instant::now());
            let result = tokio::select! {
                r = provider.chat(&req, &sink).instrument(span.clone()) => r,
                _ = cancel.cancelled() => {
                    record_call(&span, None, None);
                    return Ok(false);
                }
            };
            record_call(&span, Some(&result), None);
            drop(span);
            timed = (began, started.elapsed());
            match result {
                Ok(r) => break Some(r),
                Err(ProviderError::Retryable { message, retry_after }) if attempt < 6 => {
                    attempt += 1;
                    logln!("session {}: compaction retry {attempt}: {message}", session.id);
                    let delay = retry_after.unwrap_or_else(|| Duration::from_secs(1u64 << attempt));
                    tokio::select! {
                        _ = tokio::time::sleep(delay) => {}
                        _ = cancel.cancelled() => return Ok(false),
                    }
                }
                Err(ProviderError::ContextOverflow(_)) | Err(ProviderError::ThinkingMismatch(_)) if !overflowed => break None,
                Err(e) => bail!("compaction failed: {e}"),
            }
        };
        if let Some(r) = &response {
            self.count(session, "compaction", pname, model_id, &r.usage, timed);
        }
        let summary = response.as_ref().map(|r| blocks_text(&r.blocks)).unwrap_or_default();
        if summary.trim().is_empty() {
            if overflowed {
                bail!("compaction produced no summary");
            }
            return Box::pin(self.compact(session, provider, model_id, cancel, true)).await;
        }

        let text = context::compaction_message(&summary, last_user_text.as_deref());
        let fresh = self.store.require_session(&session.id)?;
        let system_prompt = context::build_system_prompt(&cfg, &self.paths, &self.store, &fresh);
        let estimate = estimate_tokens(text.len() + system_prompt.len());
        let msg = self.store.compact(&session.id, &text, &system_prompt, estimate)?;
        if let Some(r) = &response {
            let input = r.usage.input + r.usage.cache_read + r.usage.cache_write;
            let _ = self.store.add_usage(&session.id, input, r.usage.output, estimate);
        }
        Span::current().record("summary_tokens", estimate);
        logln!("compacted: {} messages -> {} tokens", stored.len(), estimate);
        self.emit(session, Event::Message { msg });
        Ok(true)
    }

    /// Compact on request (the /compact command).
    pub async fn compact_now(self: &Arc<Self>, sid: &str) -> Result<()> {
        let session = self.store.require_session(sid)?;
        if self.is_running(sid) {
            bail!("the session is busy; interrupt it or wait for the turn to end");
        }
        if !matches!(Tail::of(self.store.last_message(sid)?.as_ref()), Tail::Idle) {
            bail!("the session is in the middle of a turn; /continue it first");
        }
        let (pname, model_id) = split_model_ref(&session.model);
        let provider = self.provider(pname)?;
        let cancel = CancellationToken::new();
        let Some(_claim) = self.try_register(&session, &cancel) else {
            bail!("the session is busy");
        };
        let result = self.compact(&session, &provider, model_id, &cancel, false).await;
        // The summary is a user turn; without this the next run would answer it unprompted.
        let closed = result.as_ref().is_ok_and(|done| *done);
        if closed {
            let ack = [Block::Text { text: "(context compacted; ready to continue)".into() }];
            if let Ok(msg) = self.store.append_assistant(sid, &ack) {
                self.emit(&session, Event::Message { msg });
            }
        }
        self.unregister(&session, "idle");
        result.map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn two_forms_of_one_command_are_told_from_two_commands() {
        let kind = |c: &str| command_kind(c).unwrap_or_default();
        assert_eq!(kind("./build.sh"), "./build.sh");
        assert_eq!(kind("BUILD_MODE=frugal ./build.sh --all"), "./build.sh");
        assert_eq!(kind("cd app && ./build.sh 2>&1 | tail -5"), "./build.sh");
        assert_eq!(kind("cargo test --release"), "cargo test");
        assert_eq!(kind("cargo build"), "cargo build");
        assert_eq!(kind("sudo make install"), "make install");
        assert_eq!(kind("python3 -u scripts/load.py --dry-run"), "python3 scripts/load.py");
        assert_eq!(kind("ls -la; echo ---; cat fetch.sh"), "ls");
        assert_eq!(kind("export A=1; cd x"), "");
        assert_eq!(kind("./build.sh; echo exit=$?"), "./build.sh");
    }

    #[test]
    fn a_command_that_prints_its_own_failure_has_failed() {
        let said = |command: &str, output: &str| {
            let call = ToolCall {
                id: "c".into(),
                message_id: 1,
                seq: 0,
                name: "bash".into(),
                input: serde_json::json!({ "command": command }),
                status: "done".into(),
                state: None,
                result: None,
                is_error: false,
            };
            reports_failure(&call, output)
        };
        assert!(said("./build.sh; echo exit=$?", "exit=37\n[stderr]\nbuild failed"));
        assert!(said("make test; echo \"exit code: $?\"", "FAIL\nexit code: 2"));
        assert!(said("./run; echo \"status $?\"", "status 1"));
        assert!(!said("./build.sh; echo exit=$?", "build ok: 14 targets\nexit=0"));
        assert!(!said("cat build.sh", "if x; then exit 37; fi"), "a script that mentions an exit status was not asked for its own");
        assert!(!said("./build.sh", "exit=37"), "only a command that asked for the status");
    }
}
