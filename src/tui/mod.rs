//! The terminal UI: a client of the daemon. Closing it never stops any work;
//! reopening it shows the session exactly as the daemon has it.

mod input;
mod render;

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture, Event as TermEvent,
    EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEventKind,
};
use crossterm::execute;
use futures_util::StreamExt;
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block as UiBlock, Clear, Paragraph};
use serde_json::Value;
use tokio::sync::mpsc;

use self::input::InputBox;
use self::render::{Entry, Item, SPINNER, agent_label, agent_mark, agent_status, markdown, wrap};
use crate::client::Conn;
use crate::config::{Config, Paths};
use crate::context;
use crate::proto::{ApprovalInfo, Command, Envelope, Event};
use crate::store::{AgentInfo, Goal, Objective, Session, Store};
use crate::types::{Block, MsgKind, Role, StoredMessage};
use crate::util::{first_line, fmt_age, fmt_duration, fmt_tokens, home, now};
use crate::voice;

pub struct Opts {
    pub session: Option<String>,
    pub cont: bool,
    pub pick: bool,
    pub cwd: String,
}

const HELP: &str = "\
Keys
  enter            send                    alt+enter / ctrl+j   new line
  esc              interrupt the agent     ctrl+c               clear input, then quit
  pgup / pgdn      scroll                  up / down            input history
  ctrl+o           expand tool output      ctrl+t               show or hide thinking
  F2               voice input             ctrl+d               quit (work continues)
  tab              focus the side panel: up/down jumps the conversation to each objective,
                   subagent or milestone; enter opens a subagent; esc returns to the live
                   end (/panel hides it)

Sessions
  /new             start a fresh session   /sessions            pick a session to resume
  /resume <id>     open a session by id    /title <text>        rename this session
  /continue        pick up an interrupted or paused session where it stopped
  /compact         summarize the conversation and start a fresh context

Goals (the agent keeps working until done, with or without this window)
  /goal <text>     start a goal here       /goals               list goals
  /pause           pause this goal         /continue            resume it

Subagents (the agent spawns them for independent pieces of work)
  They appear live in the conversation and in the side panel, under the objective each serves.
  /agents          list them; enter opens one    /back or esc         return from a subagent
  /pause           while viewing a subagent, stops just that one

Memory
  /memory [query]  list or search          /remember <text>     save a global memory
  /forget <id>     delete a memory

Settings
  /model [p/m]     show or change model    /auto  /ask          permission mode
  /reload          re-read config.toml     /quit

Typing while the agent works queues your message; it is delivered at the next step.";

enum Overlay {
    None,
    Sessions { list: Vec<Session>, sel: usize },
    Goals { list: Vec<Goal>, sel: usize },
    /// The subagents of the session on screen, read live at draw time.
    Agents { sel: usize },
    Text { title: String, lines: Vec<String>, scroll: usize },
}

/// One row of the side panel.
struct Nav {
    text: String,
    /// More to show while the row is selected.
    detail: String,
    mark: &'static str,
    mark_style: Style,
    style: Style,
    /// The message to jump to; None for a section heading.
    anchor: Option<i64>,
    indent: usize,
    /// Which row to start on and keep in view: the objective in progress (2),
    /// else a subagent that is running (1).
    rank: u8,
    /// A subagent session that enter opens.
    open: Option<String>,
}

enum Bg {
    Done { label: String, result: Result<Value, String> },
    Voice(Result<String, String>),
}

struct App {
    paths: Paths,
    cfg: Config,
    conn: Arc<Conn>,
    store: Store,
    cwd: String,
    session: Option<Session>,
    goal: Option<Goal>,
    objectives: Vec<Objective>,
    /// Goal milestones found in the transcript: (text, message id).
    milestones: Vec<(String, i64)>,
    /// Every subagent in this session tree, in creation order.
    agents: Vec<AgentInfo>,
    /// When a subagent was opened from a scrolled-back view: the spawn call to return to.
    back_to: Option<String>,
    panel_visible: bool,
    panel_hidden: bool,
    panel_focus: bool,
    panel_sel: usize,
    /// Scroll the transcript to this message on the next draw.
    jump: Option<i64>,
    /// Message currently being turned into entries.
    cur_msg: i64,
    entries: Vec<Entry>,
    last_msg_id: i64,
    stream: String,
    stream_thinking: String,
    input: InputBox,
    scroll: usize,
    view_h: usize,
    run_state: String,
    overlay: Overlay,
    approvals: VecDeque<ApprovalInfo>,
    expanded: bool,
    show_thinking: bool,
    spin: usize,
    dirty: bool,
    quit: bool,
    flash: Option<(String, Instant)>,
    last_ctrl_c: Option<Instant>,
    recorder: Option<voice::Recorder>,
    transcribing: bool,
    /// Permission mode chosen with /auto or /ask before the session exists.
    pending_mode: Option<String>,
    bg_tx: mpsc::UnboundedSender<Bg>,
}

pub async fn run(paths: Paths, cfg: Config, opts: Opts) -> Result<()> {
    let mut conn = Conn::connect_or_start(&paths).await?;
    let pushes = conn.take_events();
    let store = Store::open(&paths.db_file())?;
    let history = store.history_recent(500).unwrap_or_default();
    let (bg_tx, bg_rx) = mpsc::unbounded_channel();
    let mut app = App {
        show_thinking: cfg.tui.show_thinking,
        paths,
        cfg,
        conn: Arc::new(conn),
        store,
        cwd: opts.cwd.clone(),
        session: None,
        goal: None,
        objectives: Vec::new(),
        milestones: Vec::new(),
        agents: Vec::new(),
        back_to: None,
        panel_visible: false,
        panel_hidden: false,
        panel_focus: false,
        panel_sel: 0,
        jump: None,
        cur_msg: 0,
        entries: Vec::new(),
        last_msg_id: 0,
        stream: String::new(),
        stream_thinking: String::new(),
        input: InputBox::new(history),
        scroll: 0,
        view_h: 20,
        run_state: "idle".into(),
        overlay: Overlay::None,
        approvals: VecDeque::new(),
        expanded: false,
        spin: 0,
        dirty: true,
        quit: false,
        flash: None,
        last_ctrl_c: None,
        recorder: None,
        transcribing: false,
        pending_mode: None,
        bg_tx,
    };
    // Resolve the starting session before taking over the terminal, so that
    // a bad id is an ordinary error message.
    if let Some(id) = &opts.session {
        app.attach(id).await?;
    } else if opts.cont {
        let recent = app.store.list_sessions(false, 300)?.into_iter().find(|s| s.cwd == opts.cwd);
        match recent {
            Some(s) => app.attach(&s.id).await?,
            None => app.flash("no earlier session in this directory; starting a new one"),
        }
    } else if opts.pick {
        app.open_sessions();
    }

    let mut terminal = ratatui::init();
    let _ = execute!(std::io::stdout(), EnableBracketedPaste);
    if app.cfg.tui.mouse {
        let _ = execute!(std::io::stdout(), EnableMouseCapture);
    }
    let result = app.event_loop(&mut terminal, pushes, bg_rx).await;
    let _ = execute!(std::io::stdout(), DisableMouseCapture, DisableBracketedPaste);
    ratatui::restore();
    if let Some(s) = &app.session {
        let working = if app.run_state == "running" { " (still working in the background)" } else { "" };
        println!("session {}{working}: narsil resume {}", s.id, s.id);
    }
    result
}

impl App {
    async fn event_loop(
        &mut self,
        terminal: &mut ratatui::DefaultTerminal,
        mut pushes: mpsc::UnboundedReceiver<Envelope>,
        mut bg_rx: mpsc::UnboundedReceiver<Bg>,
    ) -> Result<()> {
        let mut term_events = EventStream::new();
        let mut tick = tokio::time::interval(Duration::from_millis(100));
        loop {
            if self.dirty {
                terminal.draw(|f| self.draw(f))?;
                self.dirty = false;
            }
            tokio::select! {
                ev = term_events.next() => match ev {
                    Some(Ok(ev)) => self.on_term(ev).await,
                    Some(Err(e)) => return Err(e.into()),
                    None => break,
                },
                env = pushes.recv() => match env {
                    Some(env) => {
                        self.on_push(env);
                        while let Ok(more) = pushes.try_recv() {
                            self.on_push(more);
                        }
                    }
                    None => bail!("lost the connection to the narsil daemon (sessions are safe; reopen with `narsil -c`)"),
                },
                Some(msg) = bg_rx.recv() => self.on_background(msg).await,
                _ = tick.tick() => {
                    if self.busy() {
                        self.spin += 1;
                        self.dirty = true;
                    }
                    if self.flash.as_ref().is_some_and(|(_, at)| at.elapsed() > Duration::from_secs(5)) {
                        self.flash = None;
                        self.dirty = true;
                    }
                }
            }
            if self.quit {
                break;
            }
        }
        Ok(())
    }

    fn busy(&self) -> bool {
        self.run_state == "running" || self.transcribing || self.recorder.is_some()
    }

    fn flash(&mut self, text: impl Into<String>) {
        self.flash = Some((text.into(), Instant::now()));
        self.dirty = true;
    }

    fn push(&mut self, item: Item) {
        self.entries.push(Entry::at(item, self.cur_msg));
        self.dirty = true;
    }

    fn note(&mut self, text: impl Into<String>) {
        self.push(Item::Note { text: text.into() });
    }

    fn error(&mut self, text: impl Into<String>) {
        self.push(Item::Error { text: text.into() });
    }

    fn sid(&self) -> Option<String> {
        self.session.as_ref().map(|s| s.id.clone())
    }

    // ---- session plumbing -------------------------------------------------

    fn tool_entry(&mut self, id: &str) -> Option<&mut Entry> {
        self.entries.iter_mut().rev().find(|e| matches!(&e.item, Item::Tool { id: i, .. } if i == id))
    }

    fn push_message(&mut self, m: &StoredMessage) {
        self.last_msg_id = self.last_msg_id.max(m.id);
        self.cur_msg = m.id;
        self.push_message_items(m);
        self.cur_msg = 0;
        self.dirty = true;
    }

    fn push_message_items(&mut self, m: &StoredMessage) {
        match (m.role, m.kind) {
            (Role::User, MsgKind::Compaction) => self.push(Item::Divider { text: "context compacted".into() }),
            (Role::User, MsgKind::Note) => {
                let text = crate::types::blocks_text(&m.blocks);
                self.note(first_line(text.trim_start_matches("[narsil] "), 160));
            }
            (Role::User, MsgKind::Message) => {
                for b in &m.blocks {
                    match b {
                        Block::Text { text } => self.push(Item::User { text: text.clone() }),
                        Block::ToolResult { tool_use_id, content, is_error } => {
                            if let Some(e) = self.tool_entry(tool_use_id) {
                                if let Item::Tool { output, is_error: err, running, .. } = &mut e.item {
                                    if output.is_none() {
                                        *output = Some(content.clone());
                                        *err = *is_error;
                                    }
                                    *running = false;
                                }
                                e.touch();
                            }
                        }
                        _ => {}
                    }
                }
            }
            (Role::Assistant, _) => {
                for b in &m.blocks {
                    match b {
                        Block::Thinking { text, .. } if !text.trim().is_empty() => {
                            self.push(Item::Thinking { text: text.clone() })
                        }
                        Block::Text { text } if !text.trim().is_empty() => self.push(Item::Assistant { text: text.clone() }),
                        Block::ToolUse { id, name, input, .. } => {
                            let field = |k: &str| input.get(k).and_then(Value::as_str).unwrap_or("").trim().to_string();
                            match name.as_str() {
                                "goal_update" => self.milestones.push((field("note"), m.id)),
                                "goal_complete" => self.milestones.push((format!("completed: {}", field("summary")), m.id)),
                                "goal_blocked" => self.milestones.push((format!("blocked: {}", field("reason")), m.id)),
                                _ => {}
                            }
                            if self.tool_entry(id).is_none() {
                                self.push(Item::Tool {
                                    id: id.clone(),
                                    name: name.clone(),
                                    input: input.clone(),
                                    output: None,
                                    is_error: false,
                                    running: false,
                                    agents: Vec::new(),
                                });
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
        self.dirty = true;
    }

    /// Show a session and follow its live events.
    async fn attach(&mut self, id: &str) -> Result<()> {
        // Subscribe first, then snapshot: an event in between is deduplicated by message id.
        let root = self.conn.call(Command::Attach { id: id.to_string() }).await?;
        let _ = root;
        let v = self.conn.call(Command::SessionGet { id: id.to_string() }).await?;
        let session: Session = serde_json::from_value(v["session"].clone())?;
        let messages: Vec<StoredMessage> = serde_json::from_value(v["messages"].clone())?;
        self.entries.clear();
        self.stream.clear();
        self.stream_thinking.clear();
        self.last_msg_id = 0;
        self.scroll = 0;
        self.milestones.clear();
        self.panel_focus = false;
        self.panel_sel = 0;
        self.objectives = serde_json::from_value(v["objectives"].clone()).unwrap_or_default();
        self.agents = serde_json::from_value(v["agents"].clone()).unwrap_or_default();
        for m in &messages {
            self.push_message(m);
        }
        self.sync_all_spawns();
        self.approvals = serde_json::from_value::<Vec<ApprovalInfo>>(v["approvals"].clone()).unwrap_or_default().into();
        self.goal = serde_json::from_value(v["goal"].clone()).ok();
        self.run_state = if v["running"] == true { "running".into() } else { session.run_state.clone() };
        if self.run_state == "running" && !self.approvals.iter().any(|a| a.session == session.id) {
            // Calls run one at a time, in order: the first of the last turn without a result is in flight.
            let last = self.last_msg_id;
            let in_flight = self.entries.iter_mut().find(|e| {
                e.msg_id == last && matches!(&e.item, Item::Tool { output: None, .. })
            });
            if let Some(e) = in_flight {
                if let Item::Tool { running, .. } = &mut e.item {
                    *running = true;
                }
                e.touch();
            }
        }
        let queued = v["queued"].as_i64().unwrap_or(0);
        if self.run_state == "interrupted" {
            self.note(
                "This session was interrupted mid-task (a crash, or narsil was stopped). /continue picks it up \
                 exactly where it stopped; sending a new message replaces the unfinished step.",
            );
        } else if queued > 0 && self.run_state != "running" {
            self.note(format!("{queued} message(s) queued; /continue to deliver them"));
        }
        if let Some(g) = &self.goal {
            if g.status != "active" && g.status != "completed" {
                self.note(format!("goal is {}: {}", g.status, first_line(&g.note, 200)));
            }
        }
        self.session = Some(session);
        self.overlay = Overlay::None;
        self.dirty = true;
        Ok(())
    }

    async fn refresh_session(&mut self) {
        let Some(id) = self.sid() else { return };
        if let Ok(v) = self.conn.call(Command::SessionGet { id }).await {
            if let Ok(s) = serde_json::from_value::<Session>(v["session"].clone()) {
                self.session = Some(s);
            }
            self.goal = serde_json::from_value(v["goal"].clone()).ok();
        }
        self.dirty = true;
    }

    // ---- subagents --------------------------------------------------------

    /// The subagents spawned by the session on screen.
    fn my_agents(&self) -> Vec<&AgentInfo> {
        let Some(sid) = self.session.as_ref().map(|s| s.id.as_str()) else { return Vec::new() };
        self.agents.iter().filter(|a| a.parent == sid).collect()
    }

    /// Copy the current state of a spawn call's agents into its transcript entry.
    fn sync_spawn(&mut self, call: &str) {
        let rows: Vec<AgentInfo> = self.agents.iter().filter(|a| a.spawn_call == call).cloned().collect();
        if let Some(e) = self.tool_entry(call) {
            if let Item::Tool { agents, .. } = &mut e.item {
                *agents = rows;
            }
            e.touch();
        }
    }

    fn sync_all_spawns(&mut self) {
        let mut calls: Vec<String> = self.agents.iter().map(|a| a.spawn_call.clone()).collect();
        calls.dedup();
        for call in calls {
            self.sync_spawn(&call);
        }
    }

    /// A subagent's record changed: created, started or finished.
    fn agent_update(&mut self, mut info: AgentInfo) {
        let call = info.spawn_call.clone();
        match self.agents.iter_mut().find(|a| a.id == info.id) {
            Some(a) => {
                if info.state == "running" && info.activity.is_empty() {
                    info.activity = std::mem::take(&mut a.activity);
                }
                *a = info;
            }
            None => self.agents.push(info),
        }
        self.sync_spawn(&call);
    }

    /// Apply a live detail to a subagent; `change` says whether anything changed.
    fn agent_touch(&mut self, id: &str, change: impl FnOnce(&mut AgentInfo) -> bool) {
        let Some(a) = self.agents.iter_mut().find(|a| a.id == id) else { return };
        if change(a) {
            let call = a.spawn_call.clone();
            self.sync_spawn(&call);
        }
    }

    /// The transcript message in which an agent was spawned.
    fn spawn_anchor(&self, call: &str) -> i64 {
        self.entries
            .iter()
            .find(|e| matches!(&e.item, Item::Tool { id, .. } if id == call))
            .map(|e| e.msg_id)
            .filter(|id| *id > 0)
            .unwrap_or(self.last_msg_id)
    }

    /// Show a subagent's own transcript.
    async fn open_agent(&mut self, id: &str) {
        let scrolled = self.scroll > 0 || self.panel_focus;
        let call = self.agents.iter().find(|a| a.id == id).map(|a| a.spawn_call.clone());
        match self.attach(id).await {
            Ok(()) => self.back_to = call.filter(|_| scrolled),
            Err(e) => self.error(format!("{e:#}")),
        }
    }

    /// Return from a subagent to the session that spawned it.
    async fn back(&mut self) {
        let Some(parent) = self.session.as_ref().and_then(|s| s.parent_id.clone()) else {
            self.flash("this is not a subagent's session");
            return;
        };
        let back_to = self.back_to.take();
        match self.attach(&parent).await {
            Ok(()) => {
                if let Some(call) = back_to {
                    self.jump = Some(self.spawn_anchor(&call));
                }
            }
            Err(e) => self.error(format!("{e:#}")),
        }
    }

    fn on_push(&mut self, env: Envelope) {
        self.dirty = true;
        if env.root.is_empty() {
            match env.event {
                Event::Goal { goal } => {
                    let mine = self.session.as_ref().is_some_and(|s| s.goal_id.as_deref() == Some(goal.id.as_str()));
                    let line = format!("goal {} is {}{}", goal.id, goal.status, if goal.note.is_empty() { String::new() } else { format!(": {}", first_line(&goal.note, 200)) });
                    if mine {
                        self.note(line);
                        self.goal = Some(goal);
                    } else {
                        self.flash(line);
                    }
                }
                Event::Notice { text, .. } => self.flash(text),
                _ => {}
            }
            return;
        }
        let Some(session) = &self.session else { return };
        if env.root != session.root_id {
            return;
        }
        let sid = session.id.clone();
        if env.session != sid {
            self.on_tree_push(env);
            return;
        }
        match env.event {
            Event::TextDelta { text } => self.stream.push_str(&text),
            Event::ThinkingDelta { text } => self.stream_thinking.push_str(&text),
            Event::StreamReset => {
                self.stream.clear();
                self.stream_thinking.clear();
            }
            Event::Message { msg } => {
                if msg.role == Role::Assistant {
                    self.stream.clear();
                    self.stream_thinking.clear();
                }
                if msg.id > self.last_msg_id {
                    self.push_message(&msg);
                }
            }
            Event::ToolStart { id, name, input } => {
                if let Some(e) = self.tool_entry(&id) {
                    if let Item::Tool { running, .. } = &mut e.item {
                        *running = true;
                    }
                    e.touch();
                } else {
                    self.push(Item::Tool { id, name, input, output: None, is_error: false, running: true, agents: Vec::new() });
                }
            }
            Event::ToolEnd { id, is_error, output } => {
                if let Some(e) = self.tool_entry(&id) {
                    if let Item::Tool { output: o, is_error: err, running, .. } = &mut e.item {
                        *o = Some(output);
                        *err = is_error;
                        *running = false;
                    }
                    e.touch();
                }
            }
            Event::RunState { state } => {
                if state != "running" {
                    self.stream.clear();
                    self.stream_thinking.clear();
                    for e in &mut self.entries {
                        if let Item::Tool { running, .. } = &mut e.item {
                            if *running {
                                *running = false;
                                e.touch();
                            }
                        }
                    }
                    if state == "idle" && self.run_state == "running" && !self.cfg.voice.speak_command.is_empty() {
                        let last = self.entries.iter().rev().find_map(|e| match &e.item {
                            Item::Assistant { text } => Some(text.clone()),
                            _ => None,
                        });
                        if let Some(text) = last {
                            voice::speak(&self.cfg.voice, text);
                        }
                    }
                }
                self.run_state = state;
            }
            Event::Usage { context_tokens, input_tokens, output_tokens } => {
                if let Some(s) = &mut self.session {
                    s.context_tokens = context_tokens;
                    s.input_tokens = input_tokens;
                    s.output_tokens = output_tokens;
                }
            }
            Event::Approval { id, tool, summary } => {
                self.approvals.push_back(ApprovalInfo { id, session: env.session, agent: env.agent, tool, summary });
            }
            Event::ApprovalDone { id } => self.approvals.retain(|a| a.id != id),
            Event::Objectives { items } => self.objectives = items,
            Event::Agent { info } => self.agent_update(*info),
            Event::Notice { level, text } => {
                if level == "error" {
                    self.error(text);
                } else {
                    self.note(text);
                }
            }
            Event::SessionChanged => {
                // Model or mode changed elsewhere; fetch the new values.
                let tx = self.bg_tx.clone();
                let conn = self.conn.clone();
                tokio::spawn(async move {
                    let result = conn.call(Command::SessionGet { id: sid }).await.map_err(|e| format!("{e:#}"));
                    let _ = tx.send(Bg::Done { label: "refresh".into(), result });
                });
            }
            // Goals are announced globally, not per session.
            Event::Goal { .. } => {}
        }
    }

    /// Events from other sessions in this tree: the subagents (or, when a
    /// subagent is on screen, its parent and siblings). They never write into
    /// this transcript; they update the agent roster.
    fn on_tree_push(&mut self, env: Envelope) {
        let from = env.session.clone();
        match env.event {
            Event::Agent { info } => self.agent_update(*info),
            Event::ToolStart { name, input, .. } => {
                let doing = format!("{name} {}", crate::tools::summarize_call(&name, &input)).trim().to_string();
                self.agent_touch(&from, |a| {
                    a.calls += 1;
                    a.activity = doing;
                    true
                });
            }
            Event::ToolEnd { .. } => self.agent_touch(&from, |a| {
                let changed = !a.activity.is_empty();
                a.activity.clear();
                changed
            }),
            Event::TextDelta { .. } => self.agent_touch(&from, |a| {
                let changed = a.activity.is_empty();
                if changed {
                    a.activity = "writing…".into();
                }
                changed
            }),
            Event::Usage { input_tokens, output_tokens, .. } => self.agent_touch(&from, |a| {
                a.tokens = input_tokens + output_tokens;
                false
            }),
            Event::Approval { id, tool, summary } => {
                let waiting = format!("waiting for your approval: {tool}");
                self.agent_touch(&from, |a| {
                    a.activity = waiting;
                    true
                });
                self.approvals.push_back(ApprovalInfo { id, session: env.session, agent: env.agent, tool, summary });
            }
            Event::ApprovalDone { id } => self.approvals.retain(|a| a.id != id),
            Event::Notice { level, text } => {
                // Only this session's own subagents report here, not its parent or siblings.
                let mine = self.my_agents().iter().any(|a| a.id == from);
                let who = env.agent.map(|a| format!("[{a}] ")).unwrap_or_default();
                match (mine, level.as_str()) {
                    (false, _) => {}
                    (true, "error") => self.error(format!("{who}{text}")),
                    (true, _) => self.note(format!("{who}{text}")),
                }
            }
            _ => {}
        }
    }

    async fn on_background(&mut self, msg: Bg) {
        self.dirty = true;
        match msg {
            Bg::Done { label, result } => match (label.as_str(), result) {
                ("refresh", Ok(v)) => {
                    if let Ok(s) = serde_json::from_value::<Session>(v["session"].clone()) {
                        self.session = Some(s);
                    }
                }
                ("refresh", Err(_)) => {}
                (label, Ok(_)) => self.flash(format!("{label}: done")),
                (label, Err(e)) => self.error(format!("{label}: {e}")),
            },
            Bg::Voice(result) => {
                self.transcribing = false;
                match result {
                    Ok(text) if text.trim().is_empty() => self.flash("heard nothing"),
                    Ok(text) => {
                        if self.cfg.voice.auto_send && self.input.is_empty() {
                            self.send(text).await;
                        } else {
                            if !self.input.is_empty() {
                                self.input.insert(" ");
                            }
                            self.input.insert(text.trim());
                        }
                    }
                    Err(e) => self.error(format!("voice: {e}")),
                }
            }
        }
    }

    fn background(&self, label: &str, cmd: Command) {
        let tx = self.bg_tx.clone();
        let conn = self.conn.clone();
        let label = label.to_string();
        tokio::spawn(async move {
            let result = conn.call(cmd).await.map_err(|e| format!("{e:#}"));
            let _ = tx.send(Bg::Done { label, result });
        });
    }

    // ---- actions ----------------------------------------------------------

    async fn send(&mut self, text: String) {
        if self.session.is_none() {
            let created = self
                .conn
                .call(Command::SessionCreate { cwd: self.cwd.clone(), title: None, model: None })
                .await
                .and_then(|v| Ok(serde_json::from_value::<Session>(v["session"].clone())?));
            match created {
                Ok(s) => {
                    if let Some(mode) = self.pending_mode.take() {
                        let _ = self.conn.call(Command::SetMode { session: s.id.clone(), mode }).await;
                    }
                    if let Err(e) = self.attach(&s.id).await {
                        self.error(format!("{e:#}"));
                        return;
                    }
                }
                Err(e) => {
                    self.error(format!("{e:#}"));
                    self.input.set(text);
                    return;
                }
            }
        }
        let Some(sid) = self.sid() else { return };
        let queued = self.run_state == "running";
        match self.conn.call(Command::Send { session: sid, text: text.clone() }).await {
            Ok(_) => {
                self.scroll = 0;
                if queued {
                    self.note(format!("queued for the next step: {}", first_line(&text, 100)));
                }
                if self.session.as_ref().is_some_and(|s| s.title.is_empty()) {
                    self.refresh_session().await; // the first message names the session
                }
            }
            Err(e) => {
                self.error(format!("{e:#}"));
                self.input.set(text);
            }
        }
    }

    async fn submit(&mut self) {
        let text = self.input.take();
        if text.trim().is_empty() {
            return;
        }
        let _ = self.store.history_add(&text, &self.cwd);
        if text.starts_with('/') {
            self.command(text.trim()).await;
        } else {
            self.send(text).await;
        }
    }

    async fn interrupt(&mut self) {
        let Some(sid) = self.sid() else { return };
        match self.conn.call(Command::Interrupt { session: sid }).await {
            Ok(v) if v["paused_goal"] == true => self.note("goal paused; /continue resumes it"),
            Ok(v) if v["stopped"] == true => self.note("interrupted"),
            Ok(_) => {}
            Err(e) => self.error(format!("{e:#}")),
        }
    }

    fn open_sessions(&mut self) {
        match self.store.list_sessions(false, 100) {
            Ok(list) if list.is_empty() => self.flash("no sessions yet"),
            Ok(list) => self.overlay = Overlay::Sessions { list, sel: 0 },
            Err(e) => self.error(format!("{e:#}")),
        }
        self.dirty = true;
    }

    fn open_goals(&mut self) {
        match self.store.goal_list(true) {
            Ok(list) if list.is_empty() => self.flash("no goals yet; start one with /goal <objective>"),
            Ok(list) => self.overlay = Overlay::Goals { list, sel: 0 },
            Err(e) => self.error(format!("{e:#}")),
        }
        self.dirty = true;
    }

    fn toggle_voice(&mut self) {
        if self.transcribing {
            return;
        }
        if let Some(rec) = self.recorder.take() {
            self.transcribing = true;
            let cfg = self.cfg.voice.clone();
            let tx = self.bg_tx.clone();
            tokio::spawn(async move {
                let result = rec.finish(&cfg).await.map_err(|e| format!("{e:#}"));
                let _ = tx.send(Bg::Voice(result));
            });
        } else {
            match voice::Recorder::start(&self.cfg.voice, &self.paths) {
                Ok(rec) => self.recorder = Some(rec),
                Err(e) => self.flash(format!("{e:#}")),
            }
        }
        self.dirty = true;
    }

    async fn command(&mut self, line: &str) {
        let (cmd, arg) = line.split_once(char::is_whitespace).map(|(c, a)| (c, a.trim())).unwrap_or((line, ""));
        let need_session = |app: &mut App| -> Option<String> {
            let id = app.sid();
            if id.is_none() {
                app.flash("no session open yet");
            }
            id
        };
        match cmd {
            "/help" | "/?" => {
                self.overlay = Overlay::Text { title: "help".into(), lines: HELP.lines().map(str::to_string).collect(), scroll: 0 }
            }
            "/quit" | "/exit" | "/q" => self.quit = true,
            "/new" => {
                self.session = None;
                self.goal = None;
                self.objectives.clear();
                self.milestones.clear();
                self.agents.clear();
                self.back_to = None;
                self.panel_focus = false;
                self.entries.clear();
                self.stream.clear();
                self.run_state = "idle".into();
                self.approvals.clear();
                self.flash("new session: it is created when you send the first message");
            }
            "/sessions" => self.open_sessions(),
            "/resume" => {
                if arg.is_empty() {
                    self.open_sessions();
                } else if let Err(e) = self.attach(arg).await {
                    self.error(format!("{e:#}"));
                }
            }
            "/goals" => self.open_goals(),
            "/goal" => {
                if arg.is_empty() {
                    self.flash("usage: /goal <what the agent should achieve>");
                    return;
                }
                let added = self.conn.call(Command::GoalAdd { objective: arg.to_string(), cwd: self.cwd.clone(), model: None }).await;
                match added {
                    Ok(v) => {
                        let sid = v["session"]["id"].as_str().unwrap_or_default().to_string();
                        if let Err(e) = self.attach(&sid).await {
                            self.error(format!("{e:#}"));
                        }
                        self.note("goal started: the agent keeps working on it whether or not this window is open");
                    }
                    Err(e) => self.error(format!("{e:#}")),
                }
            }
            "/pause" => self.interrupt().await,
            "/continue" | "/c" => {
                let Some(sid) = need_session(self) else { return };
                match self.conn.call(Command::Resume { session: sid }).await {
                    Ok(v) if v["resumed"] == "goal" => self.note("goal resumed"),
                    Ok(_) => {}
                    Err(e) => self.error(format!("{e:#}")),
                }
            }
            "/compact" => {
                let Some(sid) = need_session(self) else { return };
                self.background("compact", Command::Compact { session: sid });
            }
            "/auto" | "/ask" => {
                let mode = cmd.trim_start_matches('/').to_string();
                let Some(sid) = self.sid() else {
                    self.flash(format!("the next session will start in {mode} mode"));
                    self.pending_mode = Some(mode);
                    return;
                };
                match self.conn.call(Command::SetMode { session: sid, mode: mode.clone() }).await {
                    Ok(_) => {
                        self.refresh_session().await;
                        self.flash(if mode == "auto" { "tools now run without asking in this session" } else { "tools now ask before running in this session" });
                    }
                    Err(e) => self.error(format!("{e:#}")),
                }
            }
            "/model" => {
                if arg.is_empty() {
                    let current = self.session.as_ref().map(|s| s.model.clone()).unwrap_or_else(|| self.cfg.model.default.clone());
                    let providers: Vec<&str> = self.cfg.providers.keys().map(String::as_str).collect();
                    self.note(format!("model: {current}. Change with /model provider/model-id (providers: {})", providers.join(", ")));
                    return;
                }
                let Some(sid) = need_session(self) else { return };
                match self.conn.call(Command::SetModel { session: sid, model: arg.to_string() }).await {
                    Ok(_) => {
                        self.refresh_session().await;
                        self.note(format!("model for this session is now {arg}"));
                    }
                    Err(e) => self.error(format!("{e:#}")),
                }
            }
            "/reload" => match self.conn.call(Command::Reload).await {
                Ok(_) => {
                    if let Ok(cfg) = Config::load(&self.paths) {
                        self.cfg = cfg;
                    }
                    self.flash("configuration reloaded");
                }
                Err(e) => self.error(format!("{e:#}")),
            },
            "/title" => {
                let Some(sid) = need_session(self) else { return };
                if !arg.is_empty() && self.store.set_title(&sid, arg).is_ok() {
                    self.refresh_session().await;
                }
            }
            "/memory" | "/mem" => {
                let scopes = match &self.session {
                    Some(s) => context::scopes_for(s),
                    None => vec!["global".to_string(), context::project_scope(&self.cwd)],
                };
                let found = if arg.is_empty() { self.store.memory_list(&scopes, 200) } else { self.store.memory_search(arg, &scopes, 30) };
                let mut lines = Vec::new();
                for m in found.unwrap_or_default() {
                    let scope = m.scope.split(':').next().unwrap_or("").to_string();
                    lines.push(format!("#{}  [{} · {}]  {}", m.id, scope, m.kind, m.title));
                    lines.extend(m.body.trim().lines().map(|l| format!("    {l}")));
                    lines.push(String::new());
                }
                if lines.is_empty() {
                    lines.push("Nothing here yet. /remember <text> saves a global memory; the agent saves its own as it learns.".into());
                }
                self.overlay = Overlay::Text { title: "memory".into(), lines, scroll: 0 };
            }
            "/remember" => {
                if arg.is_empty() {
                    self.flash("usage: /remember <something that should always be known>");
                    return;
                }
                match self.store.memory_save("global", "user", &first_line(arg, 60), arg, self.sid().as_deref()) {
                    Ok(id) => self.note(format!("remembered as #{id}; new sessions will see it")),
                    Err(e) => self.error(format!("{e:#}")),
                }
            }
            "/forget" => match arg.trim_start_matches('#').parse::<i64>() {
                Ok(id) => match self.store.memory_delete(id) {
                    Ok(true) => self.note(format!("forgot #{id}")),
                    _ => self.flash(format!("there is no memory #{id}")),
                },
                Err(_) => self.flash("usage: /forget <id>"),
            },
            "/agents" => {
                if self.my_agents().is_empty() {
                    self.flash("no subagents in this session; the agent spawns them for independent pieces of work");
                } else {
                    let first_active = self.my_agents().iter().position(|a| a.active()).unwrap_or(0);
                    self.overlay = Overlay::Agents { sel: first_active };
                }
            }
            "/back" => self.back().await,
            "/voice" => self.toggle_voice(),
            "/panel" => {
                self.panel_hidden = !self.panel_hidden;
                self.panel_focus = false;
            }
            other => self.flash(format!("unknown command {other}; /help lists them")),
        }
        self.dirty = true;
    }

    // ---- input ------------------------------------------------------------

    async fn on_term(&mut self, ev: TermEvent) {
        match ev {
            TermEvent::Key(key) if key.kind != KeyEventKind::Release => self.on_key(key).await,
            TermEvent::Paste(text) => {
                if matches!(self.overlay, Overlay::None) && self.approvals.is_empty() {
                    self.input.insert(&text);
                }
            }
            TermEvent::Mouse(m) => match m.kind {
                MouseEventKind::ScrollUp => self.scroll_by(3),
                MouseEventKind::ScrollDown => self.scroll_by(-3),
                _ => return,
            },
            TermEvent::Resize(..) => {}
            _ => return,
        }
        self.dirty = true;
    }

    fn scroll_by(&mut self, delta: i64) {
        if let Overlay::Text { scroll, .. } = &mut self.overlay {
            *scroll = (*scroll as i64 - delta).max(0) as usize;
        } else {
            self.scroll = (self.scroll as i64 + delta).max(0) as usize;
        }
    }

    async fn on_key(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);

        if let Some(approval) = self.approvals.front().cloned() {
            let decision = match key.code {
                KeyCode::Char('y') | KeyCode::Enter => Some("allow"),
                KeyCode::Char('a') => Some("always"),
                KeyCode::Char('n') | KeyCode::Esc => Some("deny"),
                KeyCode::Char('c') if ctrl => Some("deny"),
                _ => None,
            };
            if let Some(decision) = decision {
                self.approvals.pop_front();
                if let Err(e) = self.conn.call(Command::Approve { id: approval.id, decision: decision.into() }).await {
                    self.flash(format!("{e:#}"));
                }
            }
            return;
        }
        if !matches!(self.overlay, Overlay::None) {
            self.overlay_key(key).await;
            return;
        }
        if self.panel_focus {
            if key.code == KeyCode::Enter {
                let target = self.nav().into_iter().filter(|n| n.anchor.is_some()).nth(self.panel_sel).and_then(|n| n.open);
                if let Some(id) = target {
                    self.open_agent(&id).await;
                    return;
                }
            }
            if self.panel_key(key) {
                return;
            }
            // Anything else (typing, ctrl keys) goes back to the input.
            self.panel_focus = false;
        }

        let page = self.view_h.saturating_sub(2).max(1) as i64;
        match key.code {
            KeyCode::Enter if alt || shift || ctrl => self.input.insert("\n"),
            KeyCode::Char('j') if ctrl => self.input.insert("\n"),
            KeyCode::Enter => self.submit().await,
            KeyCode::Char('c') if ctrl => {
                if !self.input.is_empty() {
                    self.input.clear();
                } else if self.run_state == "running" && self.last_ctrl_c.is_none_or(|t| t.elapsed() > Duration::from_secs(2)) {
                    self.last_ctrl_c = Some(Instant::now());
                    self.interrupt().await;
                } else if self.last_ctrl_c.is_some_and(|t| t.elapsed() < Duration::from_secs(2)) {
                    self.quit = true;
                } else {
                    self.last_ctrl_c = Some(Instant::now());
                    self.flash("press ctrl+c again to quit; the daemon and any running work continue");
                }
            }
            KeyCode::Char('d') if ctrl => {
                if self.input.is_empty() {
                    self.quit = true;
                } else {
                    self.input.delete();
                }
            }
            KeyCode::Char('o') if ctrl => self.expanded = !self.expanded,
            KeyCode::Char('t') if ctrl => self.show_thinking = !self.show_thinking,
            KeyCode::Char('a') if ctrl => self.input.home(),
            KeyCode::Char('e') if ctrl => self.input.end(),
            KeyCode::Char('u') if ctrl => self.input.kill_to_start(),
            KeyCode::Char('k') if ctrl => self.input.kill_to_end(),
            KeyCode::Char('w') if ctrl => self.input.delete_word(),
            KeyCode::Esc => {
                if self.session.as_ref().is_some_and(|s| s.parent_id.is_some()) {
                    // Looking at a subagent: esc goes back to whoever spawned it.
                    self.back().await;
                } else if self.run_state == "running" {
                    self.interrupt().await;
                }
            }
            KeyCode::F(2) => self.toggle_voice(),
            KeyCode::PageUp => self.scroll_by(page),
            KeyCode::PageDown => self.scroll_by(-page),
            KeyCode::Up => self.input.up(),
            KeyCode::Down => self.input.down(),
            KeyCode::Left if ctrl || alt => self.input.word_left(),
            KeyCode::Right if ctrl || alt => self.input.word_right(),
            KeyCode::Left => self.input.left(),
            KeyCode::Right => self.input.right(),
            KeyCode::Home => self.input.home(),
            KeyCode::End => self.input.end(),
            KeyCode::Backspace if ctrl || alt => self.input.delete_word(),
            KeyCode::Backspace => self.input.backspace(),
            KeyCode::Delete => self.input.delete(),
            KeyCode::Tab | KeyCode::BackTab => {
                if self.panel_visible {
                    self.focus_panel();
                } else if self.panel_hidden {
                    self.flash("the objectives panel is hidden; /panel shows it");
                } else {
                    self.flash("nothing in the side panel yet: it fills in with goals, objectives and subagents");
                }
            }
            KeyCode::Char(c) if !ctrl && !alt => {
                let mut buf = [0u8; 4];
                self.input.insert(c.encode_utf8(&mut buf));
            }
            _ => {}
        }
    }

    async fn overlay_key(&mut self, key: KeyEvent) {
        let code = key.code;
        let close = matches!(code, KeyCode::Esc | KeyCode::Char('q'))
            || (code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL));
        if close {
            self.overlay = Overlay::None;
            return;
        }
        let page = self.view_h.saturating_sub(4).max(1);
        let mut open: Option<String> = None;
        let mut goal_action: Option<(String, &'static str)> = None;
        let agent_ids: Vec<String> = self.my_agents().iter().map(|a| a.id.clone()).collect();
        let mut open_agent: Option<String> = None;
        match &mut self.overlay {
            Overlay::Agents { sel } => match code {
                KeyCode::Up | KeyCode::Char('k') => *sel = sel.saturating_sub(1),
                KeyCode::Down | KeyCode::Char('j') => *sel = (*sel + 1).min(agent_ids.len().saturating_sub(1)),
                KeyCode::Enter => open_agent = agent_ids.get(*sel).cloned(),
                _ => {}
            },
            Overlay::Sessions { list, sel } => match code {
                KeyCode::Up | KeyCode::Char('k') => *sel = sel.saturating_sub(1),
                KeyCode::Down | KeyCode::Char('j') => *sel = (*sel + 1).min(list.len().saturating_sub(1)),
                KeyCode::PageUp => *sel = sel.saturating_sub(page),
                KeyCode::PageDown => *sel = (*sel + page).min(list.len().saturating_sub(1)),
                KeyCode::Enter => open = list.get(*sel).map(|s| s.id.clone()),
                _ => {}
            },
            Overlay::Goals { list, sel } => match code {
                KeyCode::Up | KeyCode::Char('k') => *sel = sel.saturating_sub(1),
                KeyCode::Down | KeyCode::Char('j') => *sel = (*sel + 1).min(list.len().saturating_sub(1)),
                KeyCode::Enter => open = list.get(*sel).and_then(|g| g.session_id.clone()),
                KeyCode::Char('p') => {
                    if let Some(g) = list.get(*sel) {
                        goal_action = Some((g.id.clone(), if g.status == "active" { "pause" } else { "resume" }));
                    }
                }
                KeyCode::Char('x') => goal_action = list.get(*sel).map(|g| (g.id.clone(), "cancel")),
                _ => {}
            },
            Overlay::Text { scroll, lines, .. } => match code {
                KeyCode::Up | KeyCode::Char('k') => *scroll = scroll.saturating_sub(1),
                KeyCode::Down | KeyCode::Char('j') => *scroll = (*scroll + 1).min(lines.len().saturating_sub(1)),
                KeyCode::PageUp => *scroll = scroll.saturating_sub(page),
                KeyCode::PageDown | KeyCode::Char(' ') => *scroll = (*scroll + page).min(lines.len().saturating_sub(1)),
                KeyCode::Enter => self.overlay = Overlay::None,
                _ => {}
            },
            Overlay::None => {}
        }
        if let Some(id) = open {
            self.back_to = None;
            if let Err(e) = self.attach(&id).await {
                self.overlay = Overlay::None;
                self.error(format!("{e:#}"));
            }
        }
        if let Some(id) = open_agent {
            self.overlay = Overlay::None;
            self.open_agent(&id).await;
        }
        if let Some((id, action)) = goal_action {
            match self.conn.call(Command::GoalAction { id, action: action.into() }).await {
                Ok(_) => self.open_goals(),
                Err(e) => self.flash(format!("{e:#}")),
            }
        }
    }

    // ---- objectives panel -------------------------------------------------

    fn agent_row(&self, a: &AgentInfo, indent: usize) -> Nav {
        let (mark, mark_style) = agent_mark(&a.state, self.spin);
        let status = match a.state.as_str() {
            "running" => agent_status(a),
            "done" => format!("done in {}", fmt_duration(a.elapsed(now()))),
            other => other.to_string(),
        };
        let mut detail = format!("task: {}", first_line(&a.task, 400));
        if !a.active() && !a.summary.is_empty() {
            detail.push_str(&format!("\nresult: {}", a.summary));
        }
        Nav {
            text: format!("{} · {status}", a.name),
            detail,
            mark,
            mark_style,
            style: if a.state == "running" { Style::new() } else { Style::new().fg(Color::DarkGray) },
            anchor: Some(self.spawn_anchor(&a.spawn_call)),
            indent,
            rank: if a.state == "running" { 1 } else { 0 },
            open: Some(a.id.clone()),
        }
    }

    /// The rows of the side panel: the goal, each objective with the subagents
    /// working on it beneath, subagents that serve no listed objective, and
    /// recorded milestones.
    fn nav(&self) -> Vec<Nav> {
        let heading_style = Style::new().fg(Color::LightBlue).add_modifier(Modifier::BOLD);
        let dim = Style::new().fg(Color::DarkGray);
        let heading = |text: String| Nav {
            text,
            detail: String::new(),
            mark: "",
            mark_style: heading_style,
            style: heading_style,
            anchor: None,
            indent: 0,
            rank: 0,
            open: None,
        };
        let row = |text: String, mark: &'static str, style: Style, anchor: i64| Nav {
            text,
            detail: String::new(),
            mark,
            mark_style: style,
            style,
            anchor: Some(anchor),
            indent: 0,
            rank: 0,
            open: None,
        };
        let agents = self.my_agents();
        let mut rows = Vec::new();
        if let Some(g) = &self.goal {
            rows.push(heading(format!("GOAL · {} · turn {}", g.status, g.iterations)));
            rows.push(row(g.objective.trim().to_string(), "◆", Style::new(), 0));
        }
        if !self.objectives.is_empty() {
            let done = self.objectives.iter().filter(|o| o.status == "done").count();
            rows.push(heading(format!("OBJECTIVES · {done}/{}", self.objectives.len())));
            for o in &self.objectives {
                let (mark, style) = match o.status.as_str() {
                    "done" => ("✓", Style::new().fg(Color::Green)),
                    "active" => ("▶", Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD)),
                    "dropped" => ("✗", dim.add_modifier(Modifier::CROSSED_OUT)),
                    _ => ("○", Style::new()),
                };
                let mine: Vec<&AgentInfo> = agents.iter().copied().filter(|a| a.objective == o.text).collect();
                let mut nav = row(o.text.clone(), mark, style, o.anchor);
                if mine.iter().any(|a| a.state == "running") {
                    // Someone is on it right now, whatever status the list last gave it.
                    nav.mark = SPINNER[self.spin % SPINNER.len()];
                    nav.mark_style = Style::new().fg(Color::Yellow);
                }
                nav.rank = if o.status == "active" { 2 } else { 0 };
                rows.push(nav);
                rows.extend(mine.into_iter().map(|a| self.agent_row(a, 2)));
            }
        }
        let loose: Vec<&AgentInfo> =
            agents.iter().copied().filter(|a| !self.objectives.iter().any(|o| o.text == a.objective)).collect();
        if !loose.is_empty() {
            // Everything still in flight, and the most recent few that are not.
            const RECENT: usize = 6;
            let finished = loose.iter().filter(|a| !a.active()).count();
            let hidden = finished.saturating_sub(RECENT);
            let running = loose.iter().filter(|a| a.state == "running").count();
            let mut title = "AGENTS".to_string();
            if running > 0 {
                title.push_str(&format!(" · {running} running"));
            }
            if hidden > 0 {
                title.push_str(&format!(" · +{hidden} earlier (/agents)"));
            }
            rows.push(heading(title));
            let mut skipped = 0;
            for a in loose {
                if !a.active() && skipped < hidden {
                    skipped += 1;
                    continue;
                }
                rows.push(self.agent_row(a, 0));
            }
        }
        if !self.milestones.is_empty() {
            rows.push(heading("MILESTONES".into()));
            for (text, anchor) in &self.milestones {
                rows.push(row(text.clone(), "•", dim, *anchor));
            }
        }
        rows
    }

    fn nav_anchors(&self) -> Vec<i64> {
        self.nav().into_iter().filter_map(|n| n.anchor).collect()
    }

    fn panel_width(&self, total: u16) -> u16 {
        if self.panel_hidden || total < 84 || self.nav().is_empty() { 0 } else { (total / 3).clamp(30, 46) }
    }

    /// Move the keyboard to the panel, starting on the objective in progress.
    fn focus_panel(&mut self) {
        let rows: Vec<Nav> = self.nav().into_iter().filter(|n| n.anchor.is_some()).collect();
        if rows.is_empty() {
            return;
        }
        let top = rows.iter().map(|n| n.rank).max().unwrap_or(0);
        let start = if top > 0 { rows.iter().position(|n| n.rank == top).unwrap_or(0) } else { rows.len() - 1 };
        self.panel_focus = true;
        self.panel_sel = start;
        self.jump = rows[start].anchor;
    }

    /// Keys while the panel has focus. Returns false for keys it does not use.
    fn panel_key(&mut self, key: KeyEvent) -> bool {
        let anchors = self.nav_anchors();
        if anchors.is_empty() || key.modifiers.contains(KeyModifiers::CONTROL) {
            return false;
        }
        let last = anchors.len() - 1;
        let page = self.view_h.saturating_sub(2).max(1) as i64;
        let before = self.panel_sel;
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => self.panel_sel = self.panel_sel.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => self.panel_sel = (self.panel_sel + 1).min(last),
            KeyCode::Home | KeyCode::Char('g') => self.panel_sel = 0,
            KeyCode::End | KeyCode::Char('G') => self.panel_sel = last,
            KeyCode::PageUp => self.scroll_by(page),
            KeyCode::PageDown => self.scroll_by(-page),
            KeyCode::Tab | KeyCode::BackTab | KeyCode::Enter => self.panel_focus = false,
            KeyCode::Esc => {
                self.panel_focus = false;
                self.scroll = 0;
            }
            _ => return false,
        }
        self.panel_sel = self.panel_sel.min(last);
        if self.panel_sel != before || matches!(key.code, KeyCode::Home | KeyCode::End | KeyCode::Up | KeyCode::Down) {
            self.jump = Some(anchors[self.panel_sel]);
        }
        true
    }

    fn draw_panel(&mut self, f: &mut Frame, area: Rect) {
        let rows = self.nav();
        let count = rows.iter().filter(|n| n.anchor.is_some()).count();
        self.panel_sel = self.panel_sel.min(count.saturating_sub(1));
        let focus = self.panel_focus;
        let on_agent =
            focus && rows.iter().filter(|n| n.anchor.is_some()).nth(self.panel_sel).is_some_and(|n| n.open.is_some());
        let hint = match (focus, on_agent) {
            (false, _) => " tab ",
            (true, true) => " ↑↓ jump · enter open · esc live ",
            (true, false) => " ↑↓ jump · esc live ",
        };
        let title = if self.objectives.is_empty() && self.goal.is_none() { " agents " } else { " objectives " };
        let block = UiBlock::bordered()
            .border_style(Style::new().fg(if focus { Color::LightBlue } else { Color::DarkGray }))
            .title(title)
            .title_bottom(hint);
        let inner = block.inner(area);
        let width = inner.width.saturating_sub(1) as usize;
        let top_rank = rows.iter().map(|n| n.rank).max().unwrap_or(0);
        let mut lines: Vec<Line<'static>> = Vec::new();
        // The rows to keep in view: the selection, or the work in progress.
        let mut keep: Option<(usize, usize)> = None;
        let mut index = 0usize;
        for row in rows {
            if row.anchor.is_none() {
                if !lines.is_empty() {
                    lines.push(Line::default());
                }
                lines.push(Line::styled(format!(" {}", first_line(&row.text, width)), row.style));
                continue;
            }
            let selected = focus && index == self.panel_sel;
            let pad = " ".repeat(row.indent);
            let room = width.saturating_sub(3 + row.indent).max(8);
            let mut pieces = wrap(&row.text, room);
            if selected && !row.detail.is_empty() {
                pieces.extend(wrap(&row.detail, room));
            }
            // A subagent takes one line until it is selected.
            let limit = if selected { 12 } else if row.open.is_some() { 1 } else { 3 };
            let start = lines.len();
            for (i, piece) in pieces.iter().take(limit).enumerate() {
                let cut = i + 1 == limit && pieces.len() > limit;
                let mark = if i == 0 { row.mark } else { " " };
                let body = format!("{piece}{}", if cut { "…" } else { "" });
                if selected {
                    let text = format!(" {pad}{mark} {body}");
                    let padded = format!("{text:<w$}", w = inner.width as usize);
                    lines.push(Line::styled(padded, Style::new().fg(Color::Black).bg(Color::LightBlue)));
                } else {
                    lines.push(Line::from(vec![
                        Span::raw(format!(" {pad}")),
                        Span::styled(mark, row.mark_style),
                        Span::styled(format!(" {body}"), row.style),
                    ]));
                }
            }
            if selected || (!focus && keep.is_none() && top_rank > 0 && row.rank == top_rank) {
                keep = Some((start, lines.len()));
            }
            index += 1;
        }
        let room = inner.height as usize;
        let offset = match keep {
            Some((first, end)) if end > room => (end - room).min(first),
            _ => 0,
        };
        let shown: Vec<Line<'static>> = lines.into_iter().skip(offset).take(room).collect();
        f.render_widget(Paragraph::new(shown).block(block), area);
    }

    // ---- drawing ----------------------------------------------------------

    fn draw(&mut self, f: &mut Frame) {
        let area = f.area();
        let (input_lines, cursor) = self.input.layout(area.width.saturating_sub(4) as usize);
        let input_h = input_lines.len().clamp(1, 8) as u16 + 2;
        let approval_h = match self.approvals.front() {
            Some(a) => (wrap(&a.summary, area.width.saturating_sub(2) as usize).len().min(14) as u16 + 3).min(area.height / 2),
            None => 0,
        };
        let [header, body, approval, input, status] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Min(3),
            Constraint::Length(approval_h),
            Constraint::Length(input_h),
            Constraint::Length(1),
        ])
        .areas(area);
        self.draw_header(f, header);
        let panel_w = self.panel_width(area.width);
        self.panel_visible = panel_w > 0;
        if !self.panel_visible {
            self.panel_focus = false;
        }
        if self.panel_visible {
            let [main, side] = Layout::horizontal([Constraint::Min(40), Constraint::Length(panel_w)]).areas(body);
            self.draw_transcript(f, main);
            self.draw_panel(f, side);
        } else {
            self.draw_transcript(f, body);
        }
        if approval_h > 0 {
            self.draw_approval(f, approval);
        }
        self.draw_input(f, input, &input_lines, cursor);
        self.draw_status(f, status);
        self.draw_overlay(f, area);
    }

    /// The working directory, home-relative and cut from the left to fit.
    fn short_cwd(&self, max: usize) -> String {
        let dir = self.session.as_ref().map(|s| s.cwd.as_str()).unwrap_or(&self.cwd);
        let home = home().to_string_lossy().into_owned();
        let shown = match dir.strip_prefix(&home) {
            Some(rest) if !home.is_empty() && home != "/" => format!("~{rest}"),
            _ => dir.to_string(),
        };
        let len = shown.chars().count();
        if len <= max.max(8) {
            shown
        } else {
            format!("…{}", shown.chars().skip(len - max.max(8) + 1).collect::<String>())
        }
    }

    fn draw_header(&self, f: &mut Frame, area: Rect) {
        let title_room = (area.width as usize / 2).saturating_sub(12).max(10);
        let title = match &self.session {
            Some(s) if s.parent_id.is_some() => {
                first_line(&format!("⤷ subagent {} · {}", s.agent_name.as_deref().unwrap_or("?"), s.title), title_room)
            }
            Some(s) if !s.title.is_empty() => first_line(&s.title, title_room),
            Some(_) => "untitled".into(),
            None => "new session".into(),
        };
        let room = (area.width as usize / 2).saturating_sub(12);
        let right = match &self.session {
            Some(s) => format!("{} · {} ", s.id, self.short_cwd(room)),
            None => format!("{} ", self.short_cwd(room)),
        };
        let left = format!(" narsil  {title}");
        let gap = (area.width as usize).saturating_sub(left.chars().count() + right.chars().count());
        let line = Line::from(vec![
            Span::styled(" narsil ", Style::new().fg(Color::Black).bg(Color::LightBlue).add_modifier(Modifier::BOLD)),
            Span::styled(format!(" {title}"), Style::new().add_modifier(Modifier::BOLD)),
            Span::raw(" ".repeat(gap)),
            Span::styled(right, Style::new().fg(Color::DarkGray)),
        ]);
        f.render_widget(Paragraph::new(line), area);
    }

    fn draw_transcript(&mut self, f: &mut Frame, area: Rect) {
        let inner = Rect { x: area.x + 1, y: area.y, width: area.width.saturating_sub(2), height: area.height };
        let width = inner.width as usize;
        let height = inner.height as usize;
        self.view_h = height;
        // Where the panel's selection lives in the transcript: the entries of the
        // first message at or after its anchor.
        let jump = self.jump.take();
        let selected = if self.panel_focus { self.nav_anchors().get(self.panel_sel).copied() } else { None };
        let anchor = jump.or(selected);
        let mut target: Option<(i64, usize, usize)> = None; // (message id, first line, end line)
        let cwd = self.session.as_ref().map(|s| s.cwd.clone()).unwrap_or_default();
        let mut all: Vec<Line<'static>> = Vec::new();
        let mut prev_compact = true;
        for e in &mut self.entries {
            if !self.show_thinking && matches!(e.item, Item::Thinking { .. }) {
                continue;
            }
            let compact = matches!(e.item, Item::Tool { .. } | Item::Note { .. } | Item::Thinking { .. });
            let msg_id = e.msg_id;
            let lines = e.lines(width, self.expanded, self.spin, &cwd);
            if lines.is_empty() {
                continue;
            }
            let grouped = compact && prev_compact;
            if !all.is_empty() && !grouped {
                all.push(Line::default());
            }
            let start = all.len();
            all.extend(lines.iter().cloned());
            prev_compact = compact;
            if let Some(a) = anchor {
                match &mut target {
                    None if msg_id != 0 && msg_id >= a => target = Some((msg_id, start, all.len())),
                    Some((id, _, end)) if *id == msg_id => *end = all.len(),
                    _ => {}
                }
            }
        }
        if self.show_thinking && !self.stream_thinking.trim().is_empty() && self.stream.is_empty() {
            let tail = self.stream_thinking.trim().lines().last().unwrap_or("");
            all.push(Line::styled(
                format!("∴ {}", first_line(tail, width.saturating_sub(4))),
                Style::new().fg(Color::DarkGray).add_modifier(Modifier::ITALIC),
            ));
        }
        if !self.stream.is_empty() {
            if !all.is_empty() {
                all.push(Line::default());
            }
            all.extend(markdown(&self.stream, width));
        }
        if all.is_empty() {
            all = self.welcome(width);
        }
        let max_scroll = all.len().saturating_sub(height);
        if let (Some(_), Some((_, first, _))) = (jump, target) {
            // Put the target near the top of the view, with a line of context above it.
            self.scroll = max_scroll.saturating_sub(first.saturating_sub(1));
        }
        self.scroll = self.scroll.min(max_scroll);
        let start = max_scroll - self.scroll;
        let visible: Vec<Line<'static>> = all.into_iter().skip(start).take(height).collect();
        f.render_widget(Paragraph::new(visible), inner);
        if let (true, Some((_, first, end))) = (self.panel_focus, target) {
            // Mark the selected stretch of conversation in the left margin.
            for line in first.max(start)..end.min(start + height) {
                let spot = Rect { x: area.x, y: area.y + (line - start) as u16, width: 1, height: 1 };
                f.render_widget(Paragraph::new(Span::styled("▌", Style::new().fg(Color::LightBlue))), spot);
            }
        }
        if self.scroll > 0 {
            let tag = format!(" ↓ {} more ", self.scroll);
            let w = tag.chars().count() as u16;
            if area.width > w + 2 && area.height > 1 {
                let spot = Rect { x: area.x + area.width - w - 1, y: area.y + area.height - 1, width: w, height: 1 };
                f.render_widget(Paragraph::new(Line::styled(tag, Style::new().fg(Color::Black).bg(Color::Yellow))), spot);
            }
        }
    }

    fn welcome(&self, width: usize) -> Vec<Line<'static>> {
        let dim = Style::new().fg(Color::DarkGray);
        let mut lines = vec![
            Line::default(),
            Line::styled("narsil", Style::new().add_modifier(Modifier::BOLD).fg(Color::LightBlue)),
            Line::styled(format!("model {}   ·   {}", self.cfg.model.default, self.short_cwd(width.saturating_sub(30))), dim),
            Line::default(),
        ];
        let tips = "Type a message and press enter. The agent can run shell commands and edit files here.\n\
                    /goal <objective> sets it working unattended.  /sessions resumes earlier work.  /help lists everything.";
        lines.extend(wrap(tips, width).into_iter().map(|l| Line::styled(l, dim)));
        lines
    }

    fn draw_approval(&self, f: &mut Frame, area: Rect) {
        let Some(a) = self.approvals.front() else { return };
        let who = a.agent.as_deref().map(|n| format!("subagent {n} ")).unwrap_or_default();
        let more = if self.approvals.len() > 1 { format!(" (+{} waiting)", self.approvals.len() - 1) } else { String::new() };
        let block = UiBlock::bordered()
            .border_style(Style::new().fg(Color::Yellow))
            .title(format!(" {who}wants to run {}{more} ", a.tool));
        let inner = block.inner(area);
        let room = inner.height.saturating_sub(1) as usize;
        let mut lines: Vec<Line> = wrap(&a.summary, inner.width as usize).into_iter().take(room).map(Line::raw).collect();
        lines.push(Line::from(vec![
            Span::styled("y", Style::new().fg(Color::Green).add_modifier(Modifier::BOLD)),
            Span::raw(" allow   "),
            Span::styled("a", Style::new().fg(Color::Green).add_modifier(Modifier::BOLD)),
            Span::raw(format!(" always allow {} in this session   ", a.tool)),
            Span::styled("n", Style::new().fg(Color::Red).add_modifier(Modifier::BOLD)),
            Span::raw(" deny"),
        ]));
        f.render_widget(Paragraph::new(lines).block(block), area);
    }

    fn draw_input(&self, f: &mut Frame, area: Rect, lines: &[String], cursor: (usize, usize)) {
        let (title, color) = if self.recorder.is_some() {
            (" ● recording: F2 to stop ".to_string(), Color::Red)
        } else if self.transcribing {
            (" transcribing… ".to_string(), Color::Yellow)
        } else if !self.approvals.is_empty() {
            (" waiting for your decision above ".to_string(), Color::Yellow)
        } else if let Some(s) = self.session.as_ref().filter(|s| s.parent_id.is_some()) {
            let name = s.agent_name.as_deref().unwrap_or("?");
            let stop = if self.run_state == "running" { " · /pause stops it" } else { "" };
            (format!(" subagent {name}: esc back to its parent{stop} · enter sends it a message "), Color::Magenta)
        } else if self.run_state == "running" {
            (" working: esc interrupts, enter queues a message ".to_string(), Color::DarkGray)
        } else {
            (" enter send · alt+enter new line · /help ".to_string(), Color::DarkGray)
        };
        let block = UiBlock::bordered().border_style(Style::new().fg(color)).title(title);
        let inner = block.inner(area);
        let room = inner.height as usize;
        let first = cursor.0.saturating_sub(room.saturating_sub(1));
        let shown: Vec<Line> = lines.iter().skip(first).take(room).map(|l| Line::raw(format!(" {l}"))).collect();
        f.render_widget(Paragraph::new(shown).block(block), area);
        if matches!(self.overlay, Overlay::None) && self.approvals.is_empty() {
            let x = inner.x + 1 + cursor.1 as u16;
            let y = inner.y + (cursor.0 - first) as u16;
            if x < inner.x + inner.width && y < inner.y + inner.height {
                f.set_cursor_position(Position::new(x, y));
            }
        }
    }

    fn draw_status(&self, f: &mut Frame, area: Rect) {
        let (mut left, style) = match self.run_state.as_str() {
            "running" => (format!(" {} working", SPINNER[self.spin % SPINNER.len()]), Style::new().fg(Color::Yellow)),
            "interrupted" => (" ⏸ interrupted: /continue to resume".to_string(), Style::new().fg(Color::Magenta)),
            _ => (" ● ready".to_string(), Style::new().fg(Color::Green)),
        };
        if let Some(g) = &self.goal {
            left.push_str(&format!("  ·  goal {} (turn {})", g.status, g.iterations));
        }
        let agents = self.my_agents();
        let (working, queued) = (
            agents.iter().filter(|a| a.state == "running").count(),
            agents.iter().filter(|a| a.state == "queued").count(),
        );
        if working + queued > 0 {
            left.push_str(&format!("  ·  {working} subagent{} working", if working == 1 { "" } else { "s" }));
            if queued > 0 {
                left.push_str(&format!(", {queued} queued"));
            }
        }
        if let Some((text, _)) = &self.flash {
            left.push_str(&format!("  ·  {text}"));
        }
        let right = match &self.session {
            Some(s) => format!("ctx {} · {} · {} ", fmt_tokens(s.context_tokens), s.perm_mode, s.model),
            None => format!("{} · {} ", self.pending_mode.as_deref().unwrap_or(&self.cfg.permissions.mode), self.cfg.model.default),
        };
        let width = area.width as usize;
        let room = width.saturating_sub(right.chars().count() + 1);
        let left = if left.chars().count() > room { format!("{}…", left.chars().take(room.saturating_sub(1)).collect::<String>()) } else { left };
        let gap = width.saturating_sub(left.chars().count() + right.chars().count());
        let line = Line::from(vec![
            Span::styled(left, style),
            Span::raw(" ".repeat(gap)),
            Span::styled(right, Style::new().fg(Color::DarkGray)),
        ]);
        f.render_widget(Paragraph::new(line), area);
    }

    fn draw_overlay(&self, f: &mut Frame, area: Rect) {
        let (title, lines, sel, hint): (String, Vec<String>, Option<usize>, &str) = match &self.overlay {
            Overlay::None => return,
            Overlay::Sessions { list, sel } => (
                "sessions".into(),
                list.iter()
                    .map(|s| {
                        format!(
                            "{}  {:<11} {:>4}  {}  {}",
                            s.id,
                            s.run_state,
                            fmt_age(s.updated_at),
                            first_line(if s.title.is_empty() { "(untitled)" } else { &s.title }, 50),
                            if s.kind == "goal" { "[goal]" } else { "" }
                        )
                    })
                    .collect(),
                Some(*sel),
                "enter open · esc close",
            ),
            Overlay::Goals { list, sel } => (
                "goals".into(),
                list.iter()
                    .map(|g| format!("{}  {:<9} turn {:<4} {}", g.id, g.status, g.iterations, first_line(&g.objective, 60)))
                    .collect(),
                Some(*sel),
                "enter open · p pause/resume · x cancel · esc close",
            ),
            Overlay::Agents { sel } => {
                let clock = now();
                let agents = self.my_agents();
                let name_w = agents.iter().map(|a| a.name.chars().count()).max().unwrap_or(4).clamp(4, 18);
                (
                    "subagents".into(),
                    agents
                        .iter()
                        .map(|a| {
                            let (mark, _) = agent_mark(&a.state, self.spin);
                            let time = if a.state == "queued" { "-".to_string() } else { fmt_duration(a.elapsed(clock)) };
                            format!(
                                "{mark} {:<name_w$}  {:<11} {time:>7}  {:>3} {:<5}  {:>6}  {}",
                                first_line(&a.name, name_w),
                                a.state,
                                a.calls,
                                if a.calls == 1 { "call" } else { "calls" },
                                fmt_tokens(a.tokens),
                                agent_label(a)
                            )
                        })
                        .collect(),
                    Some((*sel).min(agents.len().saturating_sub(1))),
                    "enter open · esc close",
                )
            }
            Overlay::Text { title, lines, .. } => (title.clone(), lines.clone(), None, "↑↓ scroll · esc close"),
        };
        let w = area.width.saturating_sub(6).clamp(20, 100).min(area.width);
        let h = (lines.len() as u16 + 2).min(area.height.saturating_sub(4)).max(5).min(area.height);
        let rect = Rect { x: area.x + (area.width - w) / 2, y: area.y + (area.height - h) / 2, width: w, height: h };
        let block = UiBlock::bordered()
            .border_style(Style::new().fg(Color::LightBlue))
            .title(format!(" {title} "))
            .title_bottom(format!(" {hint} "));
        let inner = block.inner(rect);
        let room = inner.height as usize;
        let offset = match (&self.overlay, sel) {
            (_, Some(sel)) => sel.saturating_sub(room.saturating_sub(1)),
            (Overlay::Text { scroll, .. }, _) => (*scroll).min(lines.len().saturating_sub(room)),
            _ => 0,
        };
        let shown: Vec<Line> = lines
            .iter()
            .enumerate()
            .skip(offset)
            .take(room)
            .map(|(i, l)| {
                // Clip without trimming: indentation is part of the layout here.
                let room = inner.width.saturating_sub(2) as usize;
                let text: String = if l.chars().count() > room {
                    format!("{}…", l.chars().take(room.saturating_sub(1)).collect::<String>())
                } else {
                    l.clone()
                };
                if Some(i) == sel {
                    Line::styled(format!(" {text:<w$}", w = inner.width.saturating_sub(1) as usize), Style::new().bg(Color::LightBlue).fg(Color::Black))
                } else {
                    Line::raw(format!(" {text}"))
                }
            })
            .collect();
        f.render_widget(Clear, rect);
        f.render_widget(Paragraph::new(shown).block(block), rect);
    }
}
