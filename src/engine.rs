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

use crate::config::{Config, Paths, split_model_ref};
use crate::context;
use crate::logln;
use crate::proto::{ApprovalInfo, Envelope, Event, ProviderModels};
use crate::provider::{self, ChatRequest, Provider, ProviderError, StopReason, StreamEvent};
use crate::store::{AgentInfo, NewSession, Session, Store, ToolCall};
use crate::tools::{self, ToolCtx};
use crate::types::{Block, Message, MsgKind, Role, StoredMessage, blocks_text};
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
    shutting_down: AtomicBool,
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

    pub fn set_goal_status(&self, id: &str, status: &str, note: &str) -> Result<()> {
        self.store.goal_set_status(id, status, note)?;
        if let Some(goal) = self.store.goal_get(id)? {
            logln!("goal {id} -> {status}{}", if note.is_empty() { String::new() } else { format!(": {}", first_line(note, 200)) });
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
        let decision = tokio::select! {
            d = rx => d.ok(),
            _ = cancel.cancelled() => None,
        };
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
            let Some(suspend) = this.try_register(&session, &cancel) else {
                bail!("session {sid} is already running, or narsil is shutting down");
            };
            let result = this.run_loop(&session, &cancel, &suspend).await;
            let state = match &result {
                Ok(Outcome::Interrupted) if suspend.load(Ordering::SeqCst) => "interrupted",
                _ => "idle",
            };
            if let Err(e) = &result {
                logln!("session {sid} failed: {e:#}");
                this.emit(&session, Event::Notice { level: "error".into(), text: format!("{e:#}") });
            }
            this.unregister(&session, state);
            result
        })
    }

    fn deliver_inputs(&self, session: &Session) -> Result<usize> {
        let delivered = self.store.pending_deliver(&session.id)?;
        let n = delivered.len();
        for msg in delivered {
            self.emit(session, Event::Message { msg });
        }
        Ok(n)
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
                let orphan = call.name == "bash" && call.state.as_ref().is_some_and(tools::shell::kill_orphan);
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
            let out = tools::execute(&ctx).await;
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
            // First call of this session: take the snapshot it will keep for the epoch.
            let prompt = context::build_system_prompt(&cfg, &self.paths, &self.store, &session);
            self.store.set_system_prompt(&session.id, &prompt)?;
            session.system_prompt = prompt;
        }
        let tools = tools::specs(&session, &cfg);
        let retry_forever = session.goal_id.is_some();
        let mut attempt: u32 = 0;
        let mut stripped = false;

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
                let sink = |ev: StreamEvent| match ev {
                    StreamEvent::Text(text) => self.emit(&session, Event::TextDelta { text }),
                    StreamEvent::Thinking(text) => self.emit(&session, Event::ThinkingDelta { text }),
                };
                tokio::select! {
                    r = provider.chat(&req, &sink) => Some(r),
                    _ = cancel.cancelled() => None,
                }
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
                        logln!("session {}: {text}", session.id);
                        self.emit(&session, Event::Notice { level: "warn".into(), text });
                    }
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
        logln!(
            "session {}: model call used {} input tokens ({} read from cache, {} written to it), {} output",
            session.id,
            usage.input + usage.cache_read + usage.cache_write,
            usage.cache_read,
            usage.cache_write,
            usage.output
        );
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
                self.emit(&session, Event::Message { msg });
                record(usage.context_tokens());
                Ok(Step::Refused(detail))
            }
            StopReason::MaxTokens => {
                // A truncated tool call would run with half its arguments.
                blocks.retain(|b| !matches!(b, Block::ToolUse { .. }));
                if !blocks.is_empty() {
                    let msg = self.store.append_assistant(&session.id, &blocks)?;
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
                self.emit(&session, Event::Message { msg });
                record(usage.context_tokens());
                Ok(Step::Continue)
            }
        }
    }

    /// Replace the current context with a summary and start a new epoch.
    /// Returns false if cancelled. `overflowed` means the conversation no
    /// longer fits as-is and is summarized from a plain-text rendering.
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
        let response = loop {
            let result = tokio::select! {
                r = provider.chat(&req, &sink) => r,
                _ = cancel.cancelled() => return Ok(false),
            };
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
            logln!(
                "session {}: compaction call used {} input tokens ({} read from cache, {} written to it), {} output",
                session.id,
                r.usage.input + r.usage.cache_read + r.usage.cache_write,
                r.usage.cache_read,
                r.usage.cache_write,
                r.usage.output
            );
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
        logln!("session {} compacted: {} messages -> {} tokens", session.id, stored.len(), estimate);
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
