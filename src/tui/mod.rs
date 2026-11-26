//! The terminal UI: a client of the daemon. Closing it never stops any work;
//! reopening it shows the session exactly as the daemon has it.

mod complete;
mod input;
mod keys;
mod models;
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
use self::keys::{Action, KeyMap};
use self::models::{Picker, Target};
use self::render::{Entry, Item, SPINNER, agent_label, agent_mark, agent_status, markdown, wrap};
use crate::client::Conn;
use crate::config::{Config, Paths};
use crate::context;
use crate::proto::{ApprovalInfo, Command, Envelope, Event, ProviderModels};
use crate::store::{AgentInfo, Goal, Session, Store};
use crate::work::{Item as WorkItem, Overview};
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
Keys (f1 or /keys lists all of them as they are bound now; [keys] in config.toml rebinds them)
  enter            send                    alt+enter / ctrl+j   new line
  esc              interrupt the agent     ctrl+c               clear input, then quit
  pgup / pgdn      scroll                  up / down            input history
  ctrl+o           expand tool output      alt+t                show or hide thinking
  f2               voice input             ctrl+d               quit (work continues)
  tab              complete a command being typed; otherwise focus the side panel: up/down
                   jumps the conversation to where each work item, subagent or milestone
                   was last touched; enter opens a subagent; esc returns to the live end

Tabs (each tab is a session of its own; they all keep working at once)
  ctrl+t           new tab                 alt+w                close this tab
  alt+right/left   next / previous tab     ctrl+g then 1 … 9    go to that tab
  After ctrl+g, left/right, t and w also work, for terminals that keep the alt keys for themselves.
  A tab shows a spinner while it works, ! when it wants an approval, and • when it has finished
  since you last looked.

Sessions
  /new             start a fresh session   /sessions            pick a session to resume
  /resume <id>     open a session by id    /title <text>        rename this session
  /cd <directory>  move this session to another directory: its commands, project memory and
                   work plan are then that directory's
  /continue        pick up an interrupted or paused session where it stopped
  /compact         summarize the conversation and start a fresh context

Goals (the agent keeps working until done, with or without this window)
  /goal <text>     start a goal here       /goals               list goals
  /pause           pause this goal         /continue            resume it

Work (the plan: items that depend on each other, kept as notes you can edit)
  The side panel shows it: ✓ done  ▶ in progress  ○ ready  · blocked  ? waiting on you.
  It survives compaction, restarts and new sessions; narsil work prints it outside the TUI.

Subagents (the agent spawns them for independent pieces of work)
  They appear live in the conversation and in the side panel, under the work item each holds.
  /agents          list them; enter opens one    /back or esc         return from a subagent
  /pause           while viewing a subagent, stops just that one

Obsidian (when a vault is set under [obsidian] in config.toml)
  ◈ in purple marks everything that touches your vault: a search or read of your notes, a file
  tool or command that reaches into it, and the notes narsil names when you ask for something.
  The status line shows ◈ vault, how many parts of it you have closed, and when it is away.

Memory (a folder of linked Markdown notes; edit them in any editor, or in Obsidian)
  /memory [query]  list or search          /remember <text>     save a global memory
  /forget <id>     delete a memory and its note

Models
  /model           pick a model from every configured provider: type to filter, enter selects,
                   tab switches between this session, new sessions and subagents
  /model <text>    the same, already filtered; a full provider/model-id switches at once
  /provider [name] list the providers, or one provider's models (add providers: narsil setup)

Seeing what it did
  /trace           the tree that was walked: each decision the model made, the tools it then
                   ran and what came back, subagents under the call that started them, timings
  /stats           where the time and tokens went, per model and per tool
                   (outside the TUI: narsil trace [id] --flamegraph out.svg, narsil stats)

Settings
  /auto  /ask      permission mode         /reload              re-read config.toml
  /quit

Typing while the agent works queues your message; it is delivered at the next step.
Commands complete as you type: the rest of the best match shows dimmed, tab or right takes it,
and more tabs walk through the others.";

enum Overlay {
    None,
    Sessions { list: Vec<Session>, sel: usize },
    Goals { list: Vec<Goal>, sel: usize },
    /// The subagents of the session on screen, read live at draw time.
    Agents { sel: usize },
    Models(Picker),
    Providers { data: Picker, sel: usize },
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
    Models { result: Result<Value, String>, view: PickerView },
}

/// What to open once the model lists have arrived.
enum PickerView {
    Models { only: Option<String>, filter: String },
    Providers,
}

struct App {
    paths: Paths,
    cfg: Config,
    conn: Arc<Conn>,
    store: Store,
    cwd: String,
    session: Option<Session>,
    goal: Option<Goal>,
    /// The work items of the project on screen.
    work: Overview,
    /// Where in this session's transcript each item was last touched.
    work_anchors: std::collections::HashMap<String, i64>,
    /// The session that last did something to each item.
    work_owners: std::collections::HashMap<String, String>,
    /// Goal milestones found in the transcript: (text, message id, the call that recorded it).
    milestones: Vec<(String, i64, String)>,
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
    /// Time to draw the last frame and the slowest one, in milliseconds.
    frame_ms: (u64, u64),
    last_width: u16,
    dirty: bool,
    quit: bool,
    flash: Option<(String, Instant)>,
    last_ctrl_c: Option<Instant>,
    recorder: Option<voice::Recorder>,
    transcribing: bool,
    /// Permission mode chosen with /auto or /ask before the session exists.
    pending_mode: Option<String>,
    /// Model chosen with /model before the session exists.
    pending_model: Option<String>,
    /// While tab is cycling through commands: what was typed, and which match is shown.
    completing: Option<(String, usize)>,
    /// An open /trace or /stats: which, when it was last read, and whether it follows the latest steps.
    live_view: Option<(&'static str, Instant, bool)>,
    /// The Obsidian vault: None when none is set, else whether it can be reached, and when that was last checked.
    vault: Option<(bool, Instant)>,
    bg_tx: mpsc::UnboundedSender<Bg>,
    /// The key bindings in force, shared by every tab.
    keys: Arc<KeyMap>,
    /// Set by /keys: the window opens the list of keys.
    want_keys: bool,
    /// Its run ended while another tab was in front.
    unseen: bool,
}

/// What a tab's background work reports back, tagged with the tab it belongs to.
type Tagged<T> = (u64, T);

/// The window: one or more tabs, each a session of its own with its own
/// connection to the daemon, and what belongs to the window rather than to a
/// tab (the tab bar, the keys window).
struct Tabs {
    tabs: Vec<(u64, App)>,
    active: usize,
    next_id: u64,
    keys: Arc<KeyMap>,
    /// The keys window, with how far it is scrolled, when it is open.
    keys_window: Option<usize>,
    /// Set by the "go to tab" key: the next key says which tab, or what to do with tabs.
    going: Option<Instant>,
    paths: Paths,
    cfg: Config,
    store: Store,
    /// Events from every tab's connection. None: that connection was lost.
    push_tx: mpsc::UnboundedSender<Tagged<Option<Envelope>>>,
    bg_tx: mpsc::UnboundedSender<Tagged<Bg>>,
    dirty: bool,
}

impl Tabs {
    /// Open a tab on a fresh connection, in `cwd`. Returns its index.
    async fn open(&mut self, cwd: String) -> Result<usize> {
        let mut conn = Conn::connect_or_start(&self.paths).await?;
        let mut pushes = conn.take_events();
        let id = self.next_id;
        self.next_id += 1;
        let all = self.push_tx.clone();
        tokio::spawn(async move {
            while let Some(env) = pushes.recv().await {
                if all.send((id, Some(env))).is_err() {
                    return;
                }
            }
            let _ = all.send((id, None));
        });
        let (bg_tx, mut bg_rx) = mpsc::unbounded_channel::<Bg>();
        let all = self.bg_tx.clone();
        tokio::spawn(async move {
            while let Some(msg) = bg_rx.recv().await {
                if all.send((id, msg)).is_err() {
                    return;
                }
            }
        });
        let app = App::new(self.paths.clone(), self.cfg.clone(), self.store.clone(), cwd, conn, bg_tx, self.keys.clone());
        self.tabs.push((id, app));
        self.dirty = true;
        Ok(self.tabs.len() - 1)
    }

    fn app(&mut self) -> &mut App {
        &mut self.tabs[self.active].1
    }

    fn go_to(&mut self, index: usize) {
        if index < self.tabs.len() {
            self.active = index;
            let app = self.app();
            app.unseen = false;
            app.dirty = true;
            self.dirty = true;
        }
    }

    /// Go to a tab by its number in the bar (0 for the first), or say there is no such tab.
    fn go_to_number(&mut self, n: usize) {
        if n < self.tabs.len() {
            self.go_to(n);
        } else {
            let label = self.keys.label(Action::NewTab);
            self.app().flash(format!("there is no tab {}; {label} opens one", n + 1));
        }
    }

    /// Open a tab and bring it to the front. It starts where the tab in front is working.
    async fn new_tab(&mut self) {
        let cwd = self.app().session.as_ref().map(|s| s.cwd.clone()).unwrap_or_else(|| self.app().cwd.clone());
        match self.open(cwd).await {
            Ok(index) => self.go_to(index),
            Err(e) => self.app().error(format!("could not open a tab: {e:#}")),
        }
    }

    fn close_tab(&mut self) {
        if !self.close() {
            let quit = self.keys.label(Action::Quit);
            self.app().flash(format!("this is the only tab; {quit} leaves narsil"));
        }
    }

    /// Close the tab in front. Whatever its session is doing carries on in the
    /// daemon. Returns false when it was the last one.
    fn close(&mut self) -> bool {
        if self.tabs.len() <= 1 {
            return false;
        }
        let (_, closed) = self.tabs.remove(self.active);
        let left = closed.session.as_ref().map(|s| {
            let working = if closed.run_state == "running" { ", still working" } else { "" };
            format!("closed {}{working}: /resume {} reopens it", first_line(&s.title, 30), s.id)
        });
        self.active = self.active.min(self.tabs.len() - 1);
        self.go_to(self.active);
        if let Some(text) = left {
            self.app().flash(text);
        }
        true
    }

    async fn on_term(&mut self, ev: TermEvent) {
        if let TermEvent::Key(key) = &ev {
            if key.kind == KeyEventKind::Release {
                return;
            }
            self.dirty = true;
            if let Some(scroll) = &mut self.keys_window {
                match key.code {
                    KeyCode::Up | KeyCode::Char('k') => *scroll = scroll.saturating_sub(1),
                    KeyCode::Down | KeyCode::Char('j') => *scroll += 1,
                    KeyCode::PageUp => *scroll = scroll.saturating_sub(10),
                    KeyCode::PageDown | KeyCode::Char(' ') => *scroll += 10,
                    _ => self.keys_window = None,
                }
                return;
            }
            if self.going.take().is_some() {
                // The key after "go to tab". Plain keys, so no terminal can have taken them.
                let app = self.app();
                app.flash = None;
                app.dirty = true;
                match key.code {
                    KeyCode::Char(c @ '1'..='9') => self.go_to_number(c as usize - '1' as usize),
                    KeyCode::Right | KeyCode::Char('n') => self.go_to((self.active + 1) % self.tabs.len()),
                    KeyCode::Left | KeyCode::Char('p') => self.go_to((self.active + self.tabs.len() - 1) % self.tabs.len()),
                    KeyCode::Char('t') => self.new_tab().await,
                    KeyCode::Char('w') => self.close_tab(),
                    // Anything else calls it off.
                    _ => {}
                }
                return;
            }
            if let Some(n) = self.keys.tab_number(key) {
                self.go_to_number(n);
                return;
            }
            match self.keys.action(key) {
                Some(Action::GotoTab) => {
                    self.going = Some(Instant::now());
                    self.app().flash("go to tab: 1–9 · ←/→ previous/next · t new · w close · any other key cancels");
                    return;
                }
                Some(Action::NewTab) => return self.new_tab().await,
                Some(Action::CloseTab) => return self.close_tab(),
                Some(Action::NextTab) => return self.go_to((self.active + 1) % self.tabs.len()),
                Some(Action::PrevTab) => return self.go_to((self.active + self.tabs.len() - 1) % self.tabs.len()),
                Some(Action::Keys) => {
                    self.keys_window = Some(0);
                    return;
                }
                _ => {}
            }
        }
        self.app().on_term(ev).await;
        if std::mem::take(&mut self.app().want_keys) {
            self.keys_window = Some(0);
            self.dirty = true;
        }
    }

    async fn run_loop(
        &mut self,
        terminal: &mut ratatui::DefaultTerminal,
        mut pushes: mpsc::UnboundedReceiver<Tagged<Option<Envelope>>>,
        mut background: mpsc::UnboundedReceiver<Tagged<Bg>>,
    ) -> Result<()> {
        let mut term_events = EventStream::new();
        let mut tick = tokio::time::interval(Duration::from_millis(100));
        loop {
            if self.dirty || self.app().dirty {
                let started = Instant::now();
                terminal.draw(|f| self.draw(f))?;
                self.dirty = false;
                let width = terminal.size().map(|s| s.width).ok();
                let app = self.app();
                app.dirty = false;
                let ms = started.elapsed().as_millis() as u64;
                app.frame_ms = (ms, app.frame_ms.1.max(ms));
                app.last_width = width.unwrap_or(app.last_width);
                if ms > 50 {
                    // A slow frame makes typing feel laggy; worth knowing which view caused it.
                    tracing::warn!(session = app.session.as_ref().map(|s| s.id.as_str()).unwrap_or(""), ms, entries = app.entries.len(), "slow frame");
                }
            }
            tokio::select! {
                ev = term_events.next() => match ev {
                    Some(Ok(ev)) => self.on_term(ev).await,
                    Some(Err(e)) => return Err(e.into()),
                    None => break,
                },
                Some((id, env)) = pushes.recv() => {
                    let mut next = Some((id, env));
                    while let Some((id, env)) = next {
                        let front = self.tabs[self.active].0;
                        match (self.tabs.iter_mut().find(|(tab, _)| *tab == id), env) {
                            (Some((_, app)), Some(env)) => {
                                let was = app.run_state.clone();
                                app.on_push(env);
                                // A run that ends behind another tab leaves a mark on its tab.
                                if id != front && was == "running" && app.run_state != "running" {
                                    app.unseen = true;
                                }
                            }
                            (Some(_), None) => bail!("lost the connection to the narsil daemon (sessions are safe; reopen with `narsil -c`)"),
                            // A tab that has been closed.
                            (None, _) => {}
                        }
                        // What happens in any tab can change the tab bar.
                        self.dirty = true;
                        next = pushes.try_recv().ok();
                    }
                }
                Some((id, msg)) = background.recv() => {
                    if let Some((_, app)) = self.tabs.iter_mut().find(|(tab, _)| *tab == id) {
                        app.on_background(msg).await;
                    }
                    self.dirty = true;
                }
                _ = tick.tick() => {
                    for (_, app) in &mut self.tabs {
                        app.on_tick();
                    }
                    // "Go to tab" does not wait for ever: its hint is gone after a few seconds.
                    if self.going.is_some_and(|at| at.elapsed() > Duration::from_secs(4)) {
                        self.going = None;
                        let app = self.app();
                        app.flash = None;
                        app.dirty = true;
                    }
                    // A spinner in the tab bar, for work going on behind the tab in front.
                    if self.tabs.len() > 1 && self.tabs.iter().any(|(_, a)| a.run_state == "running") {
                        self.dirty = true;
                    }
                }
            }
            if self.app().quit {
                break;
            }
        }
        Ok(())
    }

    fn draw(&mut self, f: &mut Frame) {
        let area = f.area();
        let below = if self.tabs.len() > 1 {
            let [bar, rest] = Layout::vertical([Constraint::Length(1), Constraint::Min(4)]).areas(area);
            self.draw_bar(f, bar);
            rest
        } else {
            area
        };
        self.app().draw(f, below);
        if let Some(scroll) = self.keys_window {
            let lines = self.keys.listing();
            let w = area.width.saturating_sub(6).clamp(20, 104).min(area.width);
            let h = (lines.len() as u16 + 2).min(area.height.saturating_sub(2)).max(5).min(area.height);
            let rect = Rect { x: area.x + (area.width - w) / 2, y: area.y + (area.height - h) / 2, width: w, height: h };
            let block = UiBlock::bordered()
                .border_style(Style::new().fg(Color::LightBlue))
                .title(" keys ")
                .title_bottom(" up/down scroll · any other key closes · rebind under [keys] in config.toml ");
            let room = block.inner(rect).height as usize;
            let scroll = scroll.min(lines.len().saturating_sub(room));
            self.keys_window = Some(scroll);
            let heading = Style::new().fg(Color::LightBlue).add_modifier(Modifier::BOLD);
            let shown: Vec<Line> = lines
                .iter()
                .skip(scroll)
                .take(room)
                // Section names are the short lines that start at the margin.
                .map(|l| if !l.starts_with(' ') && !l.is_empty() && l.len() < 20 { Line::styled(format!(" {l}"), heading) } else { Line::raw(format!(" {l}")) })
                .collect();
            f.render_widget(Clear, rect);
            f.render_widget(Paragraph::new(shown).block(block), rect);
        }
    }

    /// One line of tabs: number, title, and a mark for what needs knowing
    /// about a tab that is not in front.
    fn draw_bar(&self, f: &mut Frame, area: Rect) {
        let mut spans = Vec::new();
        let each = (area.width as usize / self.tabs.len().max(1)).clamp(10, 28);
        for (i, (_, app)) in self.tabs.iter().enumerate() {
            let title = match &app.session {
                Some(s) if !s.title.is_empty() => s.title.clone(),
                Some(_) => "untitled".to_string(),
                None => "new".to_string(),
            };
            let mark = if !app.approvals.is_empty() {
                " !".to_string()
            } else if app.run_state == "running" {
                format!(" {}", SPINNER[app.spin % SPINNER.len()])
            } else if app.unseen {
                " •".to_string()
            } else {
                String::new()
            };
            let label = format!(" {} {}{mark} ", i + 1, first_line(&title, each.saturating_sub(6)));
            let style = if i == self.active {
                Style::new().fg(Color::Black).bg(Color::LightBlue).add_modifier(Modifier::BOLD)
            } else if !app.approvals.is_empty() {
                Style::new().fg(Color::Yellow)
            } else {
                Style::new().fg(Color::DarkGray)
            };
            spans.push(Span::styled(label, style));
            spans.push(Span::styled("│", Style::new().fg(Color::DarkGray)));
        }
        spans.push(Span::styled(
            format!(
                " {} then 1–9 · {} new · {} keys",
                self.keys.label(Action::GotoTab),
                self.keys.label(Action::NewTab),
                self.keys.label(Action::Keys)
            ),
            Style::new().fg(Color::DarkGray),
        ));
        f.render_widget(Paragraph::new(Line::from(spans)), area);
    }
}

pub async fn run(paths: Paths, cfg: Config, opts: Opts) -> Result<()> {
    let store = Store::open(&paths.db_file())?;
    let keys = Arc::new(KeyMap::from_config(&cfg.keys));
    let (push_tx, pushes) = mpsc::unbounded_channel();
    let (bg_tx, background) = mpsc::unbounded_channel();
    let mut tabs = Tabs {
        tabs: Vec::new(),
        active: 0,
        next_id: 1,
        keys: keys.clone(),
        keys_window: None,
        going: None,
        paths,
        cfg,
        store,
        push_tx,
        bg_tx,
        dirty: true,
    };
    tabs.open(opts.cwd.clone()).await?;
    let app = tabs.app();
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
    if let Some(problem) = keys.problems.first() {
        app.flash(problem.clone());
    }
    if let Some(inner) = crate::obsidian::Vault::open(&app.cfg).and_then(|v| v.vault_inside()) {
        app.note(format!(
            "obsidian: [obsidian] vault names the folder above your vault; the vault itself is {}. `narsil doctor` says more.",
            inner.display()
        ));
    }

    let mut terminal = ratatui::init();
    let _ = execute!(std::io::stdout(), EnableBracketedPaste);
    if tabs.cfg.tui.mouse {
        let _ = execute!(std::io::stdout(), EnableMouseCapture);
    }
    let result = tabs.run_loop(&mut terminal, pushes, background).await;
    let _ = execute!(std::io::stdout(), DisableMouseCapture, DisableBracketedPaste);
    ratatui::restore();
    for (_, app) in &tabs.tabs {
        if let Some(s) = &app.session {
            let working = if app.run_state == "running" { " (still working in the background)" } else { "" };
            println!("session {}{working}: narsil resume {}", s.id, s.id);
        }
    }
    result
}

impl App {
    fn new(paths: Paths, cfg: Config, store: Store, cwd: String, conn: Conn, bg_tx: mpsc::UnboundedSender<Bg>, keys: Arc<KeyMap>) -> App {
        let history = store.history_recent(500).unwrap_or_default();
        let mut app = App {
            show_thinking: cfg.tui.show_thinking,
            paths,
            cfg,
            conn: Arc::new(conn),
            store,
            cwd,
            session: None,
            goal: None,
            work: Overview { items: Vec::new() },
            work_anchors: Default::default(),
            work_owners: Default::default(),
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
            frame_ms: (0, 0),
            last_width: 120,
            dirty: true,
            quit: false,
            flash: None,
            last_ctrl_c: None,
            recorder: None,
            transcribing: false,
            pending_mode: None,
            pending_model: None,
            completing: None,
            live_view: None,
            vault: None,
            bg_tx,
            keys,
            want_keys: false,
            unseen: false,
        };
        app.check_vault();
        // Work still unfinished in this directory shows at once, before any session is open.
        let here = crate::work::load(&app.store, Some(&app.project())).unwrap_or_default();
        app.set_work(here);
        app
    }

    /// See whether the Obsidian vault is there, for the status line and for
    /// marking what touches it.
    fn check_vault(&mut self) {
        let set = self.cfg.obsidian.vault.trim();
        let was = self.vault.map(|(there, _)| there);
        self.vault = (!set.is_empty()).then(|| (crate::obsidian::Vault::present(&crate::util::expand_tilde(set)), Instant::now()));
        render::set_vault(&if set.is_empty() { String::new() } else { crate::util::expand_tilde(set).to_string_lossy().into_owned() });
        if was != self.vault.map(|(there, _)| there) {
            self.dirty = true;
        }
    }

    /// Ten times a second: spinners, messages that time out, views that follow a run.
    fn on_tick(&mut self) {
        // A vault on a drive that comes and goes: look again now and then.
        if self.vault.is_some_and(|(_, at)| at.elapsed() > Duration::from_secs(15)) {
            self.check_vault();
        }
        if self.busy() {
            self.spin += 1;
            self.dirty = true;
        }
        if self.flash.as_ref().is_some_and(|(_, at)| at.elapsed() > Duration::from_secs(5)) {
            self.flash = None;
            self.dirty = true;
        }
        self.refresh_live_view();
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
                            if *is_error {
                                // The call was refused (a goal declared complete with work still open, say).
                                self.milestones.retain(|(_, _, call)| call != tool_use_id);
                            }
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
                                "goal_update" => self.milestones.push((field("note"), m.id, id.clone())),
                                "goal_complete" => self.milestones.push((format!("completed: {}", field("summary")), m.id, id.clone())),
                                "goal_blocked" => self.milestones.push((format!("blocked: {}", field("reason")), m.id, id.clone())),
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
        let work: Vec<WorkItem> = serde_json::from_value(v["work"].clone()).unwrap_or_default();
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
        self.set_work(work);
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
                Event::Work { project, items } => {
                    if project == self.project() {
                        self.set_work(items);
                    }
                }
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
            // Work is announced to everyone, not per session; see the top of this function.
            Event::Work { .. } => {}
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
            Bg::Models { result, view } => {
                self.flash = None;
                match result {
                    Ok(v) => self.show_picker(v, view),
                    Err(e) => self.error(format!("could not list models: {e}")),
                }
            }
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

    /// Keep an open /trace or /stats current, about once a second while the session works.
    fn refresh_live_view(&mut self) {
        let Some((cmd, at, follow)) = self.live_view else { return };
        if !matches!(self.overlay, Overlay::Text { .. }) {
            self.live_view = None;
            return;
        }
        if self.run_state != "running" || at.elapsed() < Duration::from_millis(1000) {
            return;
        }
        let Some(fresh) = self.live_lines(cmd) else { return };
        if let Overlay::Text { lines, scroll, .. } = &mut self.overlay {
            if follow {
                *scroll = fresh.len();
            }
            *lines = fresh;
        }
        self.live_view = Some((cmd, Instant::now(), follow));
        self.dirty = true;
    }

    /// Move the keyboard to the side panel, or say why there is nothing to move to.
    fn to_panel(&mut self) {
        if self.panel_visible {
            self.focus_panel();
        } else if self.panel_hidden {
            self.flash("the side panel is hidden; /panel shows it");
        } else {
            self.flash("nothing in the side panel yet: it fills in with goals, work items and subagents");
        }
    }

    /// The suggestion to show after the cursor, when it sits at the end of a command being typed.
    fn ghost(&self) -> Option<String> {
        if self.input.cursor != self.input.text.len() || self.completing.is_some() {
            return None;
        }
        complete::ghost(&self.input.text)
    }

    /// Complete the command being typed. The first tab takes the best match;
    /// more tabs walk through the others (shift+tab walks back). Returns false
    /// when there is no command being typed, so tab can mean something else.
    fn complete_command(&mut self, backward: bool) -> bool {
        let typed = match &self.completing {
            Some((typed, _)) => typed.clone(),
            None if complete::typing_command(&self.input.text) && self.input.cursor == self.input.text.len() => self.input.text.clone(),
            None => return false,
        };
        let found = complete::matches(&typed);
        if found.is_empty() {
            self.flash(format!("no command starts with {typed}; /help lists them"));
            return true;
        }
        let at = match &self.completing {
            None => 0,
            Some((_, at)) if backward => (*at + found.len() - 1) % found.len(),
            Some((_, at)) => (*at + 1) % found.len(),
        };
        self.input.set(complete::completed(found[at], found.len() == 1));
        // With one match there is nothing to walk through: the next tab is free again.
        self.completing = (found.len() > 1).then_some((typed, at));
        self.dirty = true;
        true
    }

    /// The lines of the /trace or /stats view, read from what the daemon has recorded so far.
    fn live_lines(&self, cmd: &str) -> Option<Vec<String>> {
        let root = self.session.as_ref().map(|s| s.root_id.clone())?;
        let spans = self.store.spans_for_root(&root).unwrap_or_default();
        let now = crate::telemetry::now_ms();
        let mut lines = Vec::new();
        if spans.is_empty() {
            lines.push("Nothing is recorded for this session yet. Steps are recorded from the first message on.".into());
        } else if cmd == "/trace" {
            let notes = crate::telemetry::Notes::load(&self.store, &spans);
            lines.extend(crate::telemetry::tree(&spans, &notes, now, self.overlay_text_width()));
            lines.push(String::new());
            lines.extend(crate::telemetry::summary(&spans, now));
        } else {
            lines.extend(crate::telemetry::stats(&spans, now));
            lines.push(String::new());
            lines.push(format!(
                "interface: last frame drawn in {} ms, slowest {} ms, {} transcript entries",
                self.frame_ms.0, self.frame_ms.1, self.entries.len()
            ));
        }
        Some(lines)
    }

    /// Characters of text that fit across the text overlay.
    fn overlay_text_width(&self) -> usize {
        (self.last_width.saturating_sub(6).clamp(20, 100) as usize).saturating_sub(4)
    }

    // ---- choosing a model -------------------------------------------------

    /// The model a message sent now would go to.
    fn model_in_use(&self) -> String {
        match (&self.session, &self.pending_model) {
            (Some(s), _) => s.model.clone(),
            (None, Some(m)) => m.clone(),
            (None, None) => self.cfg.model.default.clone(),
        }
    }

    /// Ask the daemon what every provider offers, then open a picker.
    fn fetch_models(&mut self, view: PickerView) {
        self.flash("asking the providers for their models…");
        let tx = self.bg_tx.clone();
        let conn = self.conn.clone();
        tokio::spawn(async move {
            let result = conn.call(Command::Models).await.map_err(|e| format!("{e:#}"));
            let _ = tx.send(Bg::Models { result, view });
        });
    }

    fn show_picker(&mut self, v: Value, view: PickerView) {
        let all: Vec<ProviderModels> = serde_json::from_value(v["providers"].clone()).unwrap_or_default();
        let text = |k: &str| v[k].as_str().unwrap_or_default().to_string();
        let mut picker = Picker::new(all, self.model_in_use(), text("default"), text("subagent"));
        match view {
            PickerView::Providers => {
                let in_use = picker.session.split('/').next().unwrap_or_default().to_string();
                let sel = picker.all.iter().position(|p| p.name == in_use).unwrap_or(0);
                self.overlay = Overlay::Providers { data: picker, sel };
            }
            PickerView::Models { only, filter } => {
                if let Some(name) = &only {
                    if !picker.all.iter().any(|p| p.name == *name) {
                        self.flash(format!("there is no provider named {name}; /provider lists them"));
                        return;
                    }
                }
                picker.only = only;
                picker.filter = filter;
                picker.seek_current();
                self.overlay = Overlay::Models(picker);
            }
        }
    }

    /// Use `model` for the session on screen, for new sessions, or for subagents.
    async fn apply_model(&mut self, target: Target, model: String) {
        match target {
            Target::Session => match self.sid() {
                Some(sid) => match self.conn.call(Command::SetModel { session: sid, model: model.clone() }).await {
                    // The transcript records the change; the status line shows the new model.
                    Ok(_) => self.refresh_session().await,
                    Err(e) => self.error(format!("{e:#}")),
                },
                None => {
                    self.flash(format!("the next session will use {model}"));
                    self.pending_model = Some(model);
                }
            },
            Target::Default | Target::Subagent => {
                let role = if target == Target::Default { "default" } else { "subagent" };
                match self.conn.call(Command::SetConfigModel { role: role.into(), model: model.clone() }).await {
                    Ok(_) => {
                        if let Ok(cfg) = Config::load(&self.paths) {
                            self.cfg = cfg;
                        }
                        self.flash(match (target, model.is_empty()) {
                            (Target::Default, _) => format!("new sessions will use {model} (saved to config.toml)"),
                            (_, true) => "subagents will use the same model as their session (saved to config.toml)".into(),
                            (_, false) => format!("subagents will use {model} (saved to config.toml)"),
                        });
                    }
                    Err(e) => self.error(format!("{e:#}")),
                }
            }
        }
    }

    // ---- actions ----------------------------------------------------------

    async fn send(&mut self, text: String) {
        if self.session.is_none() {
            let created = self
                .conn
                .call(Command::SessionCreate { cwd: self.cwd.clone(), title: None, model: self.pending_model.clone() })
                .await
                .and_then(|v| Ok(serde_json::from_value::<Session>(v["session"].clone())?));
            match created {
                Ok(s) => {
                    self.pending_model = None;
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
                self.work_anchors.clear();
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
            "/model" | "/models" => {
                // A complete `provider/model-id` switches at once; anything else opens the list.
                let provider = arg.split_once('/').map(|(p, _)| p).unwrap_or_default();
                if self.cfg.providers.contains_key(provider) && !arg.contains(char::is_whitespace) {
                    self.apply_model(Target::Session, arg.to_string()).await;
                } else {
                    self.fetch_models(PickerView::Models { only: None, filter: arg.to_string() });
                }
            }
            "/provider" | "/providers" => {
                if arg.is_empty() {
                    self.fetch_models(PickerView::Providers);
                } else {
                    self.fetch_models(PickerView::Models { only: Some(arg.to_string()), filter: String::new() });
                }
            }
            "/reload" => match self.conn.call(Command::Reload).await {
                Ok(_) => {
                    if let Ok(cfg) = Config::load(&self.paths) {
                        self.cfg = cfg;
                    }
                    self.check_vault();
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
                let state = crate::memory::refresh(&self.store, &self.cfg, &self.paths);
                let found = if arg.is_empty() { self.store.memory_list(&scopes, 200) } else { self.store.memory_search(arg, &scopes, 30) };
                let mut lines = Vec::new();
                let root = crate::memory::root(&self.cfg, &self.paths);
                lines.push(format!("Notes in {} (edit them with any editor)", root.dir.display()));
                if !state.available {
                    lines.push(format!("Not reachable right now: showing the index's copy. {}", state.why));
                }
                lines.extend(state.problems.iter().map(|p| format!("! {p}")));
                lines.push(String::new());
                let preamble = lines.len();
                for m in found.unwrap_or_default() {
                    let scope = m.scope.split(':').next().unwrap_or("").to_string();
                    lines.push(format!("#{}  [{} · {}]  {}", m.id, scope, m.kind, m.title));
                    lines.extend(m.body.trim().lines().map(|l| format!("    {l}")));
                    lines.push(String::new());
                }
                if lines.len() == preamble {
                    lines.push("Nothing here yet. /remember <text> saves a global memory; the agent saves its own as it learns.".into());
                }
                self.overlay = Overlay::Text { title: "memory".into(), lines, scroll: 0 };
            }
            "/remember" => {
                if arg.is_empty() {
                    self.flash("usage: /remember <something that should always be known>");
                    return;
                }
                let sid = self.sid();
                let title = first_line(arg, 60);
                let entry = crate::memory::Entry { scope: "global", kind: "user", title: &title, body: arg, source: sid.as_deref() };
                let saved = crate::memory::save(&self.store, &self.cfg, &self.paths, &entry);
                match saved {
                    Ok(s) if s.held => self.note(format!("remembered as #{} (held: the memory folder is not reachable right now)", s.id)),
                    Ok(s) => self.note(format!("remembered as #{}; new sessions will see it", s.id)),
                    Err(e) => self.error(format!("{e:#}")),
                }
            }
            "/forget" => match arg.trim_start_matches('#').parse::<i64>() {
                Ok(id) => match crate::memory::delete(&self.store, &self.cfg, &self.paths, id) {
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
            "/keys" => self.want_keys = true,
            "/back" => self.back().await,
            "/cd" => {
                if arg.is_empty() {
                    let dir = self.session.as_ref().map(|s| s.cwd.clone()).unwrap_or_else(|| self.cwd.clone());
                    self.flash(format!("working in {dir}; /cd <directory> moves this session"));
                    return;
                }
                let Some(sid) = self.sid() else {
                    // No session yet: the next one starts there.
                    let target = crate::tools::resolve(&self.cwd, arg);
                    if target.is_dir() {
                        self.cwd = std::fs::canonicalize(&target).unwrap_or(target).to_string_lossy().into_owned();
                        let here = crate::work::load(&self.store, Some(&self.project())).unwrap_or_default();
                        self.set_work(here);
                        self.flash(format!("the next session will work in {}", self.cwd));
                    } else {
                        self.flash(format!("{} is not a directory", target.display()));
                    }
                    return;
                };
                match self.conn.call(Command::SetDir { session: sid.clone(), dir: arg.to_string() }).await {
                    // Reopen it: the header, the project's work and its anchors all follow the directory.
                    Ok(_) => {
                        if let Err(e) = self.attach(&sid).await {
                            self.error(format!("{e:#}"));
                        }
                    }
                    Err(e) => self.error(format!("{e:#}")),
                }
            }
            "/voice" => self.toggle_voice(),
            "/panel" => {
                self.panel_hidden = !self.panel_hidden;
                self.panel_focus = false;
            }
            "/trace" | "/stats" => {
                let cmd = if cmd == "/trace" { "/trace" } else { "/stats" };
                let Some(lines) = self.live_lines(cmd) else {
                    self.flash("no session open yet");
                    return;
                };
                let title = if cmd == "/trace" { "what this session did" } else { "time and tokens in this session" };
                // While it is working, open on the latest steps and keep up with them.
                let working = self.run_state == "running";
                self.live_view = Some((cmd, Instant::now(), working));
                self.overlay = Overlay::Text { title: title.into(), scroll: if working { lines.len() } else { 0 }, lines };
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

        let action = self.keys.action(&key);
        if action != Some(Action::Panel) && key.code != KeyCode::BackTab {
            // Any other key ends a run through the matching commands.
            self.completing = None;
        }
        let page = self.view_h.saturating_sub(2).max(1) as i64;
        // What [keys] binds comes first; everything after is typing and editing.
        match action {
            Some(Action::Interrupt) => {
                if self.session.as_ref().is_some_and(|s| s.parent_id.is_some()) {
                    // Looking at a subagent: this goes back to whoever spawned it.
                    self.back().await;
                } else if self.run_state == "running" {
                    self.interrupt().await;
                }
                return;
            }
            Some(Action::Panel) => {
                if !self.complete_command(false) {
                    self.to_panel();
                }
                return;
            }
            Some(Action::ExpandTools) => return self.expanded = !self.expanded,
            Some(Action::Thinking) => return self.show_thinking = !self.show_thinking,
            Some(Action::Voice) => return self.toggle_voice(),
            Some(Action::ScrollUp) => return self.scroll_by(page),
            Some(Action::ScrollDown) => return self.scroll_by(-page),
            Some(Action::Quit) => {
                // With text in the box this is the usual "delete forward"; empty, it leaves.
                if self.input.is_empty() {
                    self.quit = true;
                } else {
                    self.input.delete();
                }
                return;
            }
            _ => {}
        }
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
            KeyCode::Char('a') if ctrl => self.input.home(),
            KeyCode::Char('e') if ctrl => self.input.end(),
            KeyCode::Char('u') if ctrl => self.input.kill_to_start(),
            KeyCode::Char('k') if ctrl => self.input.kill_to_end(),
            KeyCode::Char('w') if ctrl => self.input.delete_word(),
            KeyCode::Up => self.input.up(),
            KeyCode::Down => self.input.down(),
            KeyCode::Left if ctrl || alt => self.input.word_left(),
            KeyCode::Right if ctrl || alt => self.input.word_right(),
            KeyCode::Left => self.input.left(),
            // At the end of the line, right takes the suggestion, as in a shell.
            KeyCode::Right if self.ghost().is_some() => {
                self.complete_command(false);
            }
            KeyCode::Right => self.input.right(),
            KeyCode::Home => self.input.home(),
            KeyCode::End => self.input.end(),
            KeyCode::Backspace if ctrl || alt => self.input.delete_word(),
            KeyCode::Backspace => self.input.backspace(),
            KeyCode::Delete => self.input.delete(),
            // Shift+tab walks back through the matching commands, or goes to the panel like tab.
            KeyCode::BackTab => {
                if !self.complete_command(true) {
                    self.to_panel();
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
        if let Overlay::Models(p) = &mut self.overlay {
            // Letters go to the filter here, so this list has its own keys.
            let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
            let page = self.view_h.saturating_sub(4).max(1) as i64;
            let mut chosen: Option<(Target, String)> = None;
            let mut close = false;
            match code {
                KeyCode::Esc if p.filter.is_empty() => close = true,
                KeyCode::Esc => {
                    p.filter.clear();
                    p.seek_current();
                }
                KeyCode::Char('c') if ctrl => close = true,
                KeyCode::Up => p.move_by(-1),
                KeyCode::Down => p.move_by(1),
                KeyCode::PageUp => p.move_by(-page),
                KeyCode::PageDown => p.move_by(page),
                KeyCode::Tab => p.cycle_target(true),
                KeyCode::BackTab => p.cycle_target(false),
                KeyCode::Backspace => p.erase(),
                KeyCode::Enter => chosen = p.selected().map(|m| (p.target, m)),
                KeyCode::Char(c) if !ctrl && !key.modifiers.contains(KeyModifiers::ALT) => p.type_char(c),
                _ => {}
            }
            if let Some((target, model)) = chosen {
                self.overlay = Overlay::None;
                self.apply_model(target, model).await;
            } else if close {
                self.overlay = Overlay::None;
            }
            return;
        }
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
        let mut open_provider: Option<String> = None;
        match &mut self.overlay {
            Overlay::Agents { sel } => match code {
                KeyCode::Up | KeyCode::Char('k') => *sel = sel.saturating_sub(1),
                KeyCode::Down | KeyCode::Char('j') => *sel = (*sel + 1).min(agent_ids.len().saturating_sub(1)),
                KeyCode::Enter => open_agent = agent_ids.get(*sel).cloned(),
                _ => {}
            },
            Overlay::Providers { data, sel } => match code {
                KeyCode::Up | KeyCode::Char('k') => *sel = sel.saturating_sub(1),
                KeyCode::Down | KeyCode::Char('j') => *sel = (*sel + 1).min(data.all.len().saturating_sub(1)),
                KeyCode::Enter => open_provider = data.all.get(*sel).map(|p| p.name.clone()),
                _ => {}
            },
            Overlay::Models(_) => {}
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
                KeyCode::Up | KeyCode::Char('k') | KeyCode::PageUp if self.live_view.is_some_and(|v| v.2) => {
                    // Stop following the end, and move from where the view actually is.
                    self.live_view = self.live_view.map(|(cmd, at, _)| (cmd, at, false));
                    let step = if code == KeyCode::PageUp { page } else { 1 };
                    *scroll = lines.len().saturating_sub(page + 2).saturating_sub(step);
                }
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
        if let Some(name) = open_provider {
            // The lists are already here: narrow the same data to this provider.
            if let Overlay::Providers { mut data, .. } = std::mem::replace(&mut self.overlay, Overlay::None) {
                data.only = Some(name);
                data.seek_current();
                self.overlay = Overlay::Models(data);
            }
        }
        if let Some((id, action)) = goal_action {
            match self.conn.call(Command::GoalAction { id, action: action.into() }).await {
                Ok(_) => self.open_goals(),
                Err(e) => self.flash(format!("{e:#}")),
            }
        }
    }

    // ---- side panel ---------------------------------------------------------

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

    /// Take the project's work items in, with where this session touched each
    /// and which session last touched each.
    fn set_work(&mut self, items: Vec<WorkItem>) {
        self.work = Overview { items };
        self.work_anchors = match self.sid() {
            Some(sid) => crate::work::anchors(&self.store, &sid).unwrap_or_default(),
            None => Default::default(),
        };
        self.work_owners = crate::work::last_sessions(&self.store, &self.project()).unwrap_or_default();
    }

    /// The project the screen is about: the open session's, or this directory's.
    fn project(&self) -> String {
        context::project_dir(self.session.as_ref().map(|s| s.cwd.as_str()).unwrap_or(&self.cwd))
    }

    /// One work item as a panel row, then its subagents and its parts.
    fn work_rows(&self, item: &WorkItem, depth: usize, agents: &[&AgentInfo], rows: &mut Vec<Nav>) {
        let dim = Style::new().fg(Color::DarkGray);
        let parts = self.work.children(&item.id);
        let blockers = self.work.open_blockers(item);
        let mine: Vec<&AgentInfo> = agents.iter().copied().filter(|a| a.objective == item.title).collect();
        // Someone is on it right now: a running subagent, or this session itself.
        let active = mine.iter().any(|a| a.state == "running")
            || (item.status == "in_progress" && self.run_state == "running" && self.sid().as_deref() == Some(item.claimed_by.as_str()));
        let (mark, style): (&'static str, Style) = match item.status.as_str() {
            "done" => ("✓", Style::new().fg(Color::Green)),
            "dropped" => ("✗", dim.add_modifier(Modifier::CROSSED_OUT)),
            "waiting" => ("?", Style::new().fg(Color::Magenta).add_modifier(Modifier::BOLD)),
            "in_progress" => ("▶", Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD)),
            _ if !parts.is_empty() => ("▾", Style::new().add_modifier(Modifier::BOLD)),
            _ if !blockers.is_empty() => ("·", dim),
            _ => ("○", Style::new()),
        };
        let mut text = item.title.clone();
        if !parts.is_empty() {
            let live: Vec<&WorkItem> = self.work.descendants(&item.id).into_iter().filter(|p| p.status != "dropped" && self.work.children(&p.id).is_empty()).collect();
            text.push_str(&format!(" · {}/{}", live.iter().filter(|p| p.status == "done").count(), live.len()));
        }
        let mut detail: Vec<String> = Vec::new();
        if !blockers.is_empty() && !item.closed() {
            detail.push(format!("after: {}", blockers.iter().map(|b| b.title.as_str()).collect::<Vec<_>>().join(", ")));
        }
        if item.status == "in_progress" && !item.claimed_by.is_empty() {
            detail.push(if item.claimed_agent.is_empty() { format!("held by session {}", item.claimed_by) } else { format!("held by subagent {}", item.claimed_agent) });
        }
        if !item.body.trim().is_empty() {
            detail.push(first_line(&item.body, 300));
        }
        if !item.result.trim().is_empty() {
            detail.push(format!("result: {}", first_line(&item.result, 300)));
        }
        // Not touched in this conversation, so there is nowhere here to jump to: say where its history is.
        let here = self.work_anchors.get(&item.id).copied();
        if here.is_none() {
            if let Some(owner) = self.work_owners.get(&item.id).filter(|o| Some(o.as_str()) != self.sid().as_deref()) {
                detail.push(format!("from another session: narsil resume {owner}"));
            }
        }
        let mut nav = Nav {
            text,
            detail: detail.join("\n"),
            mark,
            mark_style: style,
            style,
            // i64::MAX is past every message: selecting such a row moves nothing.
            anchor: Some(here.unwrap_or(i64::MAX)),
            indent: depth * 2,
            rank: match (item.status.as_str(), active) {
                ("in_progress", true) => 3,
                ("in_progress", false) => 2,
                _ => 0,
            },
            open: None,
        };
        if active {
            nav.mark = SPINNER[self.spin % SPINNER.len()];
            nav.mark_style = Style::new().fg(Color::Yellow);
        }
        rows.push(nav);
        rows.extend(mine.into_iter().map(|a| self.agent_row(a, depth * 2 + 2)));
        for part in parts {
            self.work_rows(part, depth + 1, agents, rows);
        }
    }

    /// The rows of the side panel: the goal, the work graph with the subagents
    /// holding each item beneath it, subagents that hold no item, and recorded
    /// milestones.
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
        // The work graph, top-level items with their parts beneath: what is unfinished in
        // this directory, and what this session itself worked on. Work finished in earlier
        // sessions belongs to their history, which `narsil resume` opens; it is not shown
        // here, where there would be nothing in the conversation to go to.
        let mine = |i: &WorkItem| self.work_anchors.contains_key(&i.id) || self.work.descendants(&i.id).iter().any(|d| self.work_anchors.contains_key(&d.id));
        let shown: Vec<&WorkItem> = self.work.items.iter().filter(|i| i.parent.is_none() && (!i.closed() || mine(i))).collect();
        if !shown.is_empty() {
            let leaves: Vec<&WorkItem> = shown
                .iter()
                .flat_map(|t| {
                    let mut all = self.work.descendants(&t.id);
                    all.push(t);
                    all
                })
                .filter(|i| self.work.children(&i.id).is_empty() && i.status != "dropped")
                .collect();
            let done = leaves.iter().filter(|i| i.status == "done").count();
            rows.push(heading(format!("WORK · {done}/{} done", leaves.len())));
            for top in shown {
                self.work_rows(top, 0, &agents, &mut rows);
            }
        }
        let loose: Vec<&AgentInfo> =
            agents.iter().copied().filter(|a| !self.work.items.iter().any(|i| i.title == a.objective)).collect();
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
            for (text, anchor, _) in &self.milestones {
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
        let title = if self.work.items.is_empty() && self.goal.is_none() { " agents " } else { " work " };
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

    fn draw(&mut self, f: &mut Frame, area: Rect) {
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
            Line::styled(format!("model {}   ·   {}", self.model_in_use(), self.short_cwd(width.saturating_sub(30))), dim),
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
        // While a command is being typed, the title lists what fits instead.
        let typed = self.completing.as_ref().map(|(typed, _)| typed.as_str()).unwrap_or(&self.input.text);
        let (title, color) = match complete::hint(typed, area.width.saturating_sub(6) as usize) {
            Some(hint) if self.approvals.is_empty() => (format!(" {hint} "), Color::LightBlue),
            _ => (title, color),
        };
        let block = UiBlock::bordered().border_style(Style::new().fg(color)).title(title);
        let inner = block.inner(area);
        let room = inner.height as usize;
        let first = cursor.0.saturating_sub(room.saturating_sub(1));
        let ghost = self.ghost().filter(|_| lines.len() == 1);
        let shown: Vec<Line> = lines
            .iter()
            .skip(first)
            .take(room)
            .map(|l| match &ghost {
                Some(rest) => Line::from(vec![Span::raw(format!(" {l}")), Span::styled(rest.clone(), Style::new().fg(Color::DarkGray))]),
                None => Line::raw(format!(" {l}")),
            })
            .collect();
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
            None => format!("{} · {} ", self.pending_mode.as_deref().unwrap_or(&self.cfg.permissions.mode), self.model_in_use()),
        };
        // The Obsidian vault, when one is set: there, away, and how much of it is closed.
        let closed = self.cfg.obsidian.exclude.iter().filter(|e| !e.trim().is_empty()).count();
        let (vault, vault_style) = match self.vault {
            Some((true, _)) if closed > 0 => (format!("{} vault, {closed} closed · ", render::VAULT_MARK), Style::new().fg(render::VAULT)),
            Some((true, _)) => (format!("{} vault · ", render::VAULT_MARK), Style::new().fg(render::VAULT)),
            Some((false, _)) => (format!("{} vault away · ", render::VAULT_MARK), Style::new().fg(Color::Yellow)),
            None => (String::new(), Style::new()),
        };
        let width = area.width as usize;
        let room = width.saturating_sub(right.chars().count() + vault.chars().count() + 1);
        let left = if left.chars().count() > room { format!("{}…", left.chars().take(room.saturating_sub(1)).collect::<String>()) } else { left };
        let gap = width.saturating_sub(left.chars().count() + vault.chars().count() + right.chars().count());
        let line = Line::from(vec![
            Span::styled(left, style),
            Span::raw(" ".repeat(gap)),
            Span::styled(vault, vault_style),
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
            Overlay::Models(p) => {
                let (lines, sel) = p.lines();
                (p.title(), lines, sel, "type to filter · enter select · tab session/new sessions/subagents · esc")
            }
            Overlay::Providers { data, sel } => (
                "providers".into(),
                data.provider_lines(),
                Some((*sel).min(data.all.len().saturating_sub(1))),
                "enter show its models · esc close · add one with `narsil setup`",
            ),
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
