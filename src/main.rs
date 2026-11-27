//! narsil: a persistent, provider-agnostic agent harness.

mod client;
mod config;
mod context;
mod daemon;
mod engine;
mod goals;
mod memory;
mod obsidian;
mod secrets;
mod proto;
mod provider;
mod runview;
mod service;
mod setup;
mod store;
mod telemetry;
mod tools;
mod tui;
mod types;
mod update;
mod usage;
mod util;
mod voice;
mod work;

use std::io::{IsTerminal, Read, Write};

use anyhow::{Context, Result, anyhow, bail};
use clap::{Args, Parser, Subcommand};

use crate::client::Conn;
use crate::config::{Config, Paths};
use crate::proto::{Command, Event};
use crate::store::{Session, Store};
use crate::types::{Block, MsgKind, Role};
use crate::util::{first_line, fmt_age, fmt_tokens};

/// The version this binary reports. A release is built by GoReleaser from a
/// git tag, and the tag is the version; a plain `cargo build` falls back to
/// the one in Cargo.toml.
pub const VERSION: &str = match option_env!("NARSIL_VERSION") {
    Some(tagged) => tagged,
    None => env!("CARGO_PKG_VERSION"),
};

#[derive(Parser)]
#[command(name = "narsil", version = VERSION, about = "A persistent agent harness: TUI, daemon, unified memory, autonomous goals")]
struct Cli {
    #[command(subcommand)]
    cmd: Option<Cmd>,
    /// Continue the most recent session started in this directory
    #[arg(short = 'c', long = "continue")]
    cont: bool,
    /// Open a specific session (id or unique prefix)
    #[arg(short, long)]
    session: Option<String>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Configure the model backend, credentials, permissions and sudo
    Setup(SetupArgs),
    /// Start the background daemon
    Start,
    /// Stop the daemon; unfinished work resumes on the next start
    Stop,
    /// Stop and start the daemon (picks up config changes)
    Restart,
    /// Show whether the daemon is running and what it is doing
    Status,
    /// Update narsil the way it was installed (Homebrew, cargo, or a downloaded binary) and restart the daemon
    Update {
        /// Say whether there is a newer release, and change nothing
        #[arg(long)]
        check: bool,
        /// Update the program but leave the running daemon as it is
        #[arg(long)]
        no_restart: bool,
    },
    /// Run the daemon in the foreground (used by `start` and systemd)
    #[command(hide = true)]
    Daemon,
    /// Open the TUI (the default when no command is given)
    Chat {
        #[arg(short = 'c', long = "continue")]
        cont: bool,
        #[arg(short, long)]
        session: Option<String>,
    },
    /// Resume a past session: by id, or pick from a list
    Resume { id: Option<String> },
    /// Send one prompt and print the reply, without the TUI
    Run(RunArgs),
    /// List and manage sessions
    Sessions {
        #[command(subcommand)]
        cmd: Option<SessionsCmd>,
    },
    /// Goals the agent works on unattended until they are done
    Goal {
        #[command(subcommand)]
        cmd: GoalCmd,
    },
    /// Durable memory shared by all sessions
    Memory {
        #[command(subcommand)]
        cmd: MemoryCmd,
    },
    /// Show where the configuration lives, print it, or open it in $EDITOR
    Config {
        #[command(subcommand)]
        cmd: Option<ConfigCmd>,
    },
    /// Check configuration, backend connectivity, the daemon and sudo
    Doctor,
    /// Show this directory's work: what is done, in progress, ready, blocked or waiting on you
    Work {
        /// An item's title, to print that item in full
        item: Vec<String>,
        /// Drop what is unfinished in this directory's plan (what was done stays on record)
        #[arg(long)]
        clear: bool,
    },
    /// Show the tree of what a session or goal did: each decision, the actions it led to, and the time they took
    Trace {
        /// A session or goal id (default: whatever ran most recently)
        id: Option<String>,
        /// A flat list in time order instead of the tree
        #[arg(long)]
        timeline: bool,
        /// Write a flamegraph of where the time went to this SVG file
        #[arg(long, value_name = "FILE")]
        flamegraph: Option<String>,
        /// Write a Chrome trace (open it at ui.perfetto.dev) to this file
        #[arg(long, value_name = "FILE")]
        chrome: Option<String>,
        /// Print folded stacks, the input other flamegraph tools take
        #[arg(long)]
        folded: bool,
        /// Print the recorded spans as JSON
        #[arg(long)]
        json: bool,
    },
    /// Tokens a session and its subagents used: in and out, cached, by request and by model
    Usage {
        /// A session or goal id (default: whatever used a model most recently)
        id: Option<String>,
        /// The numbers for a program to read
        #[arg(long)]
        json: bool,
    },
    /// Totals over a period: time and tokens per model, time and failures per tool
    Stats {
        /// How far back to look, e.g. 30m, 24h, 7d
        #[arg(long, default_value = "24h")]
        since: String,
    },
    /// Show the daemon log
    Logs {
        #[arg(short, long)]
        follow: bool,
        #[arg(short = 'n', long, default_value_t = 50)]
        lines: usize,
        /// Only the lines of this session (with its subagents) or goal, by id
        #[arg(short, long, value_name = "ID")]
        session: Option<String>,
    },
    /// Run the daemon under systemd: restart after crashes, start at login
    Service {
        #[command(subcommand)]
        cmd: ServiceCmd,
    },
}

#[derive(Args)]
struct SetupArgs {
    /// Backend preset: anthropic, openai, gemini, openrouter, deepseek, mistral, groq, xai, local, custom
    #[arg(long)]
    provider: Option<String>,
    /// Name for the [providers.*] entry (default: the preset name)
    #[arg(long)]
    name: Option<String>,
    #[arg(long)]
    base_url: Option<String>,
    /// Environment variable that holds the API key
    #[arg(long)]
    api_key_env: Option<String>,
    #[arg(long)]
    model: Option<String>,
    /// Do not prompt; take everything from flags and existing config
    #[arg(short, long)]
    yes: bool,
    /// Skip the test request to the backend
    #[arg(long)]
    no_test: bool,
    /// Obsidian vault to integrate with (a folder path), or `none` to turn it off
    #[arg(long)]
    obsidian: Option<String>,
}

#[derive(Args)]
struct RunArgs {
    /// The prompt (read from stdin when omitted)
    prompt: Vec<String>,
    /// Continue an existing session instead of starting a new one
    #[arg(short, long)]
    session: Option<String>,
    /// With --session and no prompt: pick the session up where it stopped
    #[arg(long = "continue")]
    resume: bool,
    /// Run tools without asking for approval
    #[arg(long)]
    auto: bool,
    /// Working directory for the session (default: the current directory)
    #[arg(long)]
    cwd: Option<String>,
    /// provider/model-id for this session
    #[arg(long)]
    model: Option<String>,
}

#[derive(Subcommand)]
enum SessionsCmd {
    /// List sessions, most recent first
    List {
        /// Include subagent sessions
        #[arg(short, long)]
        all: bool,
        #[arg(short = 'n', long, default_value_t = 30)]
        limit: i64,
    },
    /// Print a session's transcript
    Show { id: String },
    /// Delete a session and its subagents
    Rm { id: String },
    /// Hand a session (or a goal's) to another model: at once, or at its next step if it is at work
    Model {
        /// A session or goal id
        id: String,
        /// provider/model-id
        model: String,
    },
}

#[derive(Subcommand)]
enum GoalCmd {
    /// Start working on an objective, unattended
    Add {
        objective: Vec<String>,
        /// Directory to work in (default: the current directory)
        #[arg(long)]
        dir: Option<String>,
        #[arg(long)]
        model: Option<String>,
    },
    /// List open goals
    List {
        /// Include completed and cancelled goals
        #[arg(short, long)]
        all: bool,
    },
    /// Show a goal's status and progress journal
    Show { id: String },
    Pause { id: String },
    Resume { id: String },
    Cancel { id: String },
    /// Say how long a goal may work: 30m, 2h, 5t (turns from here), or none (until you stop it).
    /// A paused goal is set going again.
    Limit { id: String, limit: String },
}

#[derive(Subcommand)]
enum MemoryCmd {
    /// List entries (global and this project by default)
    List {
        /// Every scope, including other projects and task notes
        #[arg(short, long)]
        all: bool,
    },
    /// Add an entry
    Add {
        text: Vec<String>,
        #[arg(long)]
        title: Option<String>,
        /// Scope it to the current project instead of global
        #[arg(long)]
        project: bool,
        #[arg(long, default_value = "user")]
        kind: String,
    },
    Search { query: Vec<String> },
    /// Forget an entry and remove its note (the id is the # shown by `list`)
    Rm { id: i64 },
    /// Show where the memory notes are, and bring the index up to date with them
    Sync,
    /// Write every entry out again, for when the memory folder was lost
    Restore,
}

#[derive(Subcommand)]
enum ConfigCmd {
    /// Print the paths of every file narsil reads
    Path,
    /// Print config.toml
    Show,
    /// Open a file in $EDITOR: config (default), soul, or agents
    Edit { which: Option<String> },
}

#[derive(Subcommand)]
enum ServiceCmd {
    Install,
    Uninstall,
}

fn main() {
    let cli = Cli::parse();
    let runtime = match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("narsil: could not start the async runtime: {e}");
            std::process::exit(1);
        }
    };
    let result = runtime.block_on(dispatch(cli));
    // Background tasks (spawned commands, readers) must not keep the process alive.
    runtime.shutdown_background();
    if let Err(e) = result {
        eprintln!("narsil: {e:#}");
        std::process::exit(1);
    }
}

fn cwd() -> Result<String> {
    Ok(std::env::current_dir().context("reading the current directory")?.to_string_lossy().into_owned())
}

fn require_configured(paths: &Paths) -> Result<Config> {
    let cfg = Config::load(paths)?;
    if !cfg.is_configured() {
        bail!("narsil is not configured yet; run `narsil setup`");
    }
    Ok(cfg)
}

async fn dispatch(cli: Cli) -> Result<()> {
    let paths = Paths::resolve();
    // The daemon sets up its own logging once its database is open, since it also records spans.
    let role = match &cli.cmd {
        Some(Cmd::Daemon) => None,
        None | Some(Cmd::Chat { .. }) | Some(Cmd::Resume { .. }) => Some(telemetry::Role::Tui),
        _ => Some(telemetry::Role::Cli),
    };
    let _telemetry = role.map(|role| {
        let _ = std::fs::create_dir_all(&paths.state_dir);
        telemetry::init(role, &paths, &Config::load(&paths).unwrap_or_default())
    });
    match cli.cmd {
        None => open_tui(&paths, cli.session, cli.cont, false).await,
        Some(Cmd::Chat { cont, session }) => open_tui(&paths, session.or(cli.session), cont || cli.cont, false).await,
        Some(Cmd::Resume { id }) => {
            let pick = id.is_none();
            open_tui(&paths, id, false, pick).await
        }
        Some(Cmd::Setup(a)) => {
            setup::run(
                &paths,
                setup::SetupOpts {
                    provider: a.provider,
                    name: a.name,
                    base_url: a.base_url,
                    api_key_env: a.api_key_env,
                    model: a.model,
                    yes: a.yes,
                    no_test: a.no_test,
                    obsidian: a.obsidian,
                },
            )
            .await
        }
        Some(Cmd::Start) => service::start(&paths, false).await,
        Some(Cmd::Stop) => service::stop(&paths).await,
        Some(Cmd::Restart) => {
            service::stop(&paths).await?;
            service::start(&paths, false).await
        }
        Some(Cmd::Status) => service::status(&paths).await,
        Some(Cmd::Update { check, no_restart }) => update::update(&paths, update::Opts { check, no_restart }).await,
        Some(Cmd::Daemon) => daemon::run(paths).await,
        Some(Cmd::Run(a)) => cmd_run(&paths, a).await,
        Some(Cmd::Sessions { cmd }) => cmd_sessions(&paths, cmd.unwrap_or(SessionsCmd::List { all: false, limit: 30 })).await,
        Some(Cmd::Goal { cmd }) => cmd_goal(&paths, cmd).await,
        Some(Cmd::Memory { cmd }) => cmd_memory(&paths, cmd),
        Some(Cmd::Config { cmd }) => cmd_config(&paths, cmd.unwrap_or(ConfigCmd::Path)),
        Some(Cmd::Doctor) => cmd_doctor(&paths).await,
        Some(Cmd::Trace { id, timeline, flamegraph, chrome, folded, json }) => {
            cmd_trace(&paths, id, TraceOut { timeline, flamegraph, chrome, folded, json })
        }
        Some(Cmd::Stats { since }) => cmd_stats(&paths, &since),
        Some(Cmd::Usage { id, json }) => cmd_usage(&paths, id, json),
        Some(Cmd::Work { clear: true, .. }) => {
            let conn = client::Conn::connect_or_start(&paths).await?;
            let said = conn.call(proto::Command::WorkClear { cwd: cwd()? }).await?;
            let dropped: Vec<String> = serde_json::from_value(said["dropped"].clone()).unwrap_or_default();
            match dropped.len() {
                0 => println!("nothing unfinished here"),
                n => println!("dropped {n} unfinished item{}: {}", if n == 1 { "" } else { "s" }, dropped.join(", ")),
            }
            Ok(())
        }
        Some(Cmd::Work { item, .. }) => cmd_work(&paths, &item.join(" ")),
        Some(Cmd::Logs { follow, lines, session }) => cmd_logs(&paths, follow, lines, session),
        Some(Cmd::Service { cmd }) => match cmd {
            ServiceCmd::Install => service::install(&paths),
            ServiceCmd::Uninstall => service::uninstall(),
        },
    }
}

async fn open_tui(paths: &Paths, session: Option<String>, cont: bool, pick: bool) -> Result<()> {
    let cfg = require_configured(paths)?;
    if !std::io::stdout().is_terminal() {
        bail!("the TUI needs a terminal; use `narsil run \"...\"` for scripted use");
    }
    tui::run(paths.clone(), cfg, tui::Opts { session, cont, pick, cwd: cwd()? }).await
}

fn open_store(paths: &Paths) -> Result<Store> {
    paths.ensure()?;
    Store::open(&paths.db_file())
}

// ---- run ------------------------------------------------------------------

async fn cmd_run(paths: &Paths, a: RunArgs) -> Result<()> {
    require_configured(paths)?;
    let mut prompt = a.prompt.join(" ");
    let resume = a.resume && prompt.trim().is_empty();
    if resume && a.session.is_none() {
        bail!("--continue needs --session <id>");
    }
    if prompt.trim().is_empty() && !resume {
        if std::io::stdin().is_terminal() {
            bail!("give a prompt: narsil run \"how many files are in this directory?\"");
        }
        std::io::stdin().read_to_string(&mut prompt)?;
    }
    if prompt.trim().is_empty() && !resume {
        bail!("empty prompt");
    }
    let mut conn = Conn::connect_or_start(paths).await?;
    let mut events = conn.take_events();
    let dir = match a.cwd {
        Some(d) => std::fs::canonicalize(&d).with_context(|| d.clone())?.to_string_lossy().into_owned(),
        None => cwd()?,
    };
    let sid = match &a.session {
        Some(s) => {
            let v = conn.call(Command::SessionGet { id: s.clone() }).await?;
            let id = v["session"]["id"].as_str().unwrap_or_default().to_string();
            // `--model` on an existing session hands it to that model from here on.
            if let Some(model) = &a.model {
                conn.call(Command::SetModel { session: id.clone(), model: model.clone() }).await?;
            }
            id
        }
        None => {
            let v = conn.call(Command::SessionCreate { cwd: dir, title: None, model: a.model.clone() }).await?;
            v["session"]["id"].as_str().unwrap_or_default().to_string()
        }
    };
    if sid.is_empty() {
        bail!("the daemon returned no session");
    }
    if a.auto {
        conn.call(Command::SetMode { session: sid.clone(), mode: "auto".into() }).await?;
    }
    conn.call(Command::Attach { id: sid.clone() }).await?;
    if resume {
        conn.call(Command::Resume { session: sid.clone() }).await?;
    } else {
        conn.call(Command::Send { session: sid.clone(), text: prompt }).await?;
    }

    let mut started = false;
    let mut mid_line = false;
    let mut streamed = false;
    let mut failed = false;
    let mut out = std::io::stdout();
    // On a terminal the steps and the answer are laid out to be read; into a pipe they stay
    // as plain as scripts expect (see runview.rs).
    let colour = std::env::var_os("NO_COLOR").is_none();
    let width = crossterm::terminal::size().map(|(w, _)| w as usize).unwrap_or(80).clamp(40, 100);
    let pretty = std::io::stderr().is_terminal();
    let mut flow = out.is_terminal().then(|| runview::Flow::new(width, colour));
    // What was written last: 0 nothing, 1 a step, 2 some of the answer. A blank line goes between the two kinds.
    let mut last = 0u8;
    macro_rules! before_step {
        () => {
            if let Some(flow) = &mut flow {
                write!(out, "{}", flow.finish())?;
                if !flow.at_line_start() || mid_line {
                    writeln!(out)?;
                }
                out.flush()?;
                *flow = runview::Flow::new(width, colour);
                mid_line = false;
            }
            if pretty && last != 1 {
                eprintln!();
            }
            last = 1;
        };
    }
    loop {
        let Some(env) = events.recv().await else { bail!("lost the connection to the daemon") };
        if env.root.is_empty() {
            continue;
        }
        let sub = env.session != sid;
        let who = env.agent.as_deref().map(|a| format!("[{a}] ")).unwrap_or_default();
        match env.event {
            Event::TextDelta { text } if !sub => {
                if pretty && last == 1 {
                    eprintln!();
                }
                last = 2;
                match &mut flow {
                    Some(flow) => {
                        write!(out, "{}", flow.push(&text))?;
                        mid_line = !flow.at_line_start();
                    }
                    None => {
                        write!(out, "{text}")?;
                        mid_line = !text.ends_with('\n');
                    }
                }
                out.flush()?;
                streamed = true;
            }
            Event::StreamReset if !sub => {
                if let Some(flow) = &mut flow {
                    write!(out, "{}", flow.finish())?;
                    *flow = runview::Flow::new(width, colour);
                }
                if mid_line {
                    writeln!(out)?;
                    mid_line = false;
                }
                streamed = false;
            }
            Event::Message { msg } if !sub && msg.role == Role::Assistant => {
                let text = if streamed { String::new() } else { types::blocks_text(&msg.blocks) };
                if !text.is_empty() {
                    if pretty && last == 1 {
                        eprintln!();
                    }
                    last = 2;
                }
                match &mut flow {
                    // The end of what was said: whatever the flow was holding back, and the line it was on.
                    Some(flow) => {
                        write!(out, "{}{}", flow.push(&text), flow.finish())?;
                        if !flow.at_line_start() {
                            writeln!(out)?;
                        }
                        *flow = runview::Flow::new(width, colour);
                    }
                    None if !text.is_empty() => writeln!(out, "{text}")?,
                    None if mid_line => writeln!(out)?,
                    None => {}
                }
                out.flush()?;
                mid_line = false;
                streamed = false;
            }
            Event::ToolStart { name, input, .. } => {
                // What is shown can be seen over a shoulder: credentials are masked here as in the TUI.
                let summary = secrets::kept(&tools::summarize_call(&name, &input));
                if pretty {
                    before_step!();
                    eprintln!("{}", runview::step(&who, &name, &summary, width, colour));
                } else {
                    eprintln!("{who}> {name} {summary}");
                }
            }
            Event::ToolEnd { is_error, output, .. } => {
                let output = secrets::kept(&output);
                if pretty {
                    eprintln!("{}", runview::outcome(&who, &output, is_error, width, colour));
                    last = 1;
                } else {
                    let lines = output.lines().count();
                    let more = if lines > 1 { format!(" (+{} lines)", lines - 1) } else { String::new() };
                    eprintln!("{who}  {} {}{more}", if is_error { "!" } else { "=" }, first_line(&output, 160));
                }
            }
            Event::Approval { id, tool, summary } => {
                let decision = if std::io::stdin().is_terminal() {
                    if pretty {
                        before_step!();
                        let asked = runview::approval(&who, &tool, &secrets::kept(&summary), width, colour);
                        let (prompt, body) = asked.split_last().expect("an approval always has its prompt");
                        body.iter().for_each(|line| eprintln!("{line}"));
                        // The step that follows belongs with the question: no blank line between them.
                        eprint!("{prompt}");
                    } else {
                        eprintln!("\n{who}wants to run {tool}:\n{summary}");
                        eprint!("allow? [y]es / [a]lways / [N]o: ");
                    }
                    let answer = tokio::task::spawn_blocking(|| {
                        let mut line = String::new();
                        let _ = std::io::stdin().read_line(&mut line);
                        line.trim().to_lowercase()
                    })
                    .await?;
                    match answer.as_str() {
                        "y" | "yes" => "allow",
                        "a" | "always" => "always",
                        _ => "deny",
                    }
                } else {
                    eprintln!("{who}{tool} needs approval and there is no terminal to ask on; denied (use --auto)");
                    "deny"
                };
                let _ = conn.call(Command::Approve { id, decision: decision.into() }).await;
            }
            Event::Notice { level, text } => {
                if level == "error" {
                    failed = true;
                }
                if pretty {
                    before_step!();
                    let tint = if !colour { "" } else if level == "error" { runview::RED } else { runview::DIM };
                    eprintln!("  {tint}{who}{} {text}{}", if level == "error" { "✗" } else { "·" }, if colour { runview::RESET } else { "" });
                } else {
                    eprintln!("{who}[{level}] {text}");
                }
            }
            Event::Agent { info } => match info.state.as_str() {
                "queued" => {}
                "running" => {
                    let what = if info.objective.is_empty() { first_line(&info.task, 100) } else { info.objective.clone() };
                    eprintln!("{who}started: {what}");
                }
                state => eprintln!(
                    "{who}{state} after {} ({} tool call{}): {}",
                    util::fmt_duration(info.elapsed(util::now())),
                    info.calls,
                    if info.calls == 1 { "" } else { "s" },
                    first_line(&info.summary, 160)
                ),
            },
            Event::RunState { state } if !sub => {
                if state == "running" {
                    started = true;
                } else if started {
                    break;
                }
            }
            _ => {}
        }
    }
    if let Some(flow) = &mut flow {
        write!(out, "{}", flow.finish())?;
        if !flow.at_line_start() {
            writeln!(out)?;
        }
        out.flush()?;
    }
    // What this request used, as the backend counted it: the same line the TUI puts under a request.
    let used = open_store(paths).ok().and_then(|store| {
        let root = store.get_session(&sid).ok().flatten()?.root_id;
        usage::receipts(&store, &root).pop().map(|(_, tokens)| usage::receipt_line(&tokens))
    });
    if pretty {
        eprintln!();
        if let Some(used) = &used {
            eprintln!("{}", runview::receipt(used, colour));
        }
        eprintln!("{}", runview::footer(&sid, colour));
    } else {
        if let Some(used) = &used {
            eprintln!("({used})");
        }
        // Scripts read this line for the session's id.
        eprintln!("(session {sid})");
    }
    if failed {
        bail!("the run ended with an error");
    }
    Ok(())
}

// ---- sessions -------------------------------------------------------------

fn session_line(s: &Session) -> String {
    let who = match (&s.kind[..], &s.agent_name) {
        ("subagent", Some(n)) => format!("sub:{n}"),
        (k, _) => k.to_string(),
    };
    format!(
        "{}  {:<11} {:<12} {:>4}  {:>6}  {}",
        s.id,
        s.run_state,
        first_line(&who, 12),
        fmt_age(s.updated_at),
        fmt_tokens(s.context_tokens),
        first_line(if s.title.is_empty() { "(untitled)" } else { &s.title }, 60)
    )
}

async fn cmd_sessions(paths: &Paths, cmd: SessionsCmd) -> Result<()> {
    let store = open_store(paths)?;
    match cmd {
        SessionsCmd::List { all, limit } => {
            let sessions = store.list_sessions(all, limit)?;
            if sessions.is_empty() {
                println!("no sessions yet");
                return Ok(());
            }
            println!("{:<8}  {:<11} {:<12} {:>4}  {:>6}  {}", "id", "state", "kind", "age", "ctx", "title");
            for s in sessions {
                println!("{}", session_line(&s));
            }
            println!("\nresume with: narsil resume <id>");
        }
        SessionsCmd::Show { id } => {
            let id = store.resolve_session(&id)?;
            let s = store.require_session(&id)?;
            println!("session {}  [{}]  {}  {}", s.id, s.run_state, s.model, s.cwd);
            for m in store.messages_from(&id, 0)? {
                for b in &m.blocks {
                    match (m.role, m.kind, b) {
                        (Role::User, MsgKind::Message, Block::Text { text }) => println!("\n> {text}"),
                        (Role::User, MsgKind::Compaction, Block::Text { .. }) => println!("\n--- context compacted ---"),
                        (Role::User, MsgKind::Note, Block::Text { text }) => println!("\n({})", first_line(text, 120)),
                        (Role::Assistant, _, Block::Text { text }) => println!("\n{text}"),
                        (_, _, Block::ToolUse { name, input, .. }) => {
                            println!("  * {name} {}", tools::summarize_call(name, input))
                        }
                        (_, _, Block::ToolResult { content, is_error, .. }) => {
                            println!("    {} {}", if *is_error { "!" } else { "=" }, first_line(content, 140))
                        }
                        _ => {}
                    }
                }
            }
        }
        SessionsCmd::Model { id, model } => {
            // A goal's id stands for the session that works on it.
            let id = match store.resolve_goal(&id) {
                Ok(goal) => store.goal_get(&goal)?.and_then(|g| g.session_id).ok_or_else(|| anyhow!("goal {goal} has not started a session yet"))?,
                Err(_) => store.resolve_session(&id)?,
            };
            let conn = client::Conn::connect_or_start(paths).await?;
            let said = conn.call(proto::Command::SetModel { session: id.clone(), model: model.clone() }).await?;
            match said["deferred"] == true {
                true => println!("session {id} is at work: {model} takes over at its next step"),
                false => println!("session {id} now uses {model}"),
            }
        }
        SessionsCmd::Rm { id } => {
            let id = store.resolve_session(&id)?;
            if let Some(st) = service::ping(paths).await {
                let running = st["running"].as_array().cloned().unwrap_or_default();
                if running.iter().any(|r| r.as_str() == Some(id.as_str())) {
                    bail!("session {id} is running; interrupt it first");
                }
            }
            store.delete_session(&id)?;
            println!("deleted session {id}");
        }
    }
    Ok(())
}

// ---- goals ----------------------------------------------------------------

async fn cmd_goal(paths: &Paths, cmd: GoalCmd) -> Result<()> {
    match cmd {
        GoalCmd::Add { objective, dir, model } => {
            require_configured(paths)?;
            let objective = objective.join(" ");
            let dir = match dir {
                Some(d) => std::fs::canonicalize(&d).with_context(|| d.clone())?.to_string_lossy().into_owned(),
                None => cwd()?,
            };
            let conn = Conn::connect_or_start(paths).await?;
            let v = conn.call(Command::GoalAdd { objective, cwd: dir, model }).await?;
            let gid = v["goal"]["id"].as_str().unwrap_or("?");
            let sid = v["session"]["id"].as_str().unwrap_or("?");
            println!("goal {gid} started; the agent will work on it until it is done or blocked");
            println!("  watch:   narsil -s {sid}");
            println!("  status:  narsil goal show {gid}");
            println!("  pause:   narsil goal pause {gid}");
        }
        GoalCmd::List { all } => {
            let goals = open_store(paths)?.goal_list(all)?;
            if goals.is_empty() {
                println!("no {}goals", if all { "" } else { "open " });
                return Ok(());
            }
            println!("{:<8}  {:<9} {:>5}  {:>4}  {}", "id", "status", "turns", "age", "objective");
            for g in goals {
                println!(
                    "{}  {:<9} {:>5}  {:>4}  {}",
                    g.id,
                    g.status,
                    g.iterations,
                    fmt_age(g.created_at),
                    first_line(&format!("{}{}", if g.open_ended() { "[open-ended, runs until stopped] " } else { "" }, g.objective), 90)
                );
            }
            if service::ping(paths).await.is_none() {
                println!("\nthe daemon is not running, so no goal is being worked on: narsil start");
            }
        }
        GoalCmd::Show { id } => {
            let store = open_store(paths)?;
            let id = store.resolve_goal(&id)?;
            let g = store.goal_get(&id)?.ok_or_else(|| anyhow!("no such goal"))?;
            println!("goal {}  [{}]  turn {}  started {} ago", g.id, g.status, g.iterations, fmt_age(g.created_at));
            println!("  dir:     {}", g.cwd);
            if let Some(s) = &g.session_id {
                println!("  session: {s}   (narsil -s {s})");
            }
            println!("\n{}\n", g.objective.trim());
            if !g.note.is_empty() {
                println!("{}: {}\n", g.status, g.note.trim());
            }
            for l in store.goal_logs(&id, 200)? {
                let when = chrono::DateTime::from_timestamp(l.ts, 0)
                    .map(|t| t.with_timezone(&chrono::Local).format("%m-%d %H:%M").to_string())
                    .unwrap_or_default();
                println!("{when}  {:<8} {}", l.kind, l.text.trim().replace('\n', "\n                      "));
            }
        }
        GoalCmd::Pause { id } => goal_action(paths, &id, "pause", "paused").await?,
        GoalCmd::Resume { id } => goal_action(paths, &id, "resume", "active").await?,
        GoalCmd::Cancel { id } => goal_action(paths, &id, "cancel", "cancelled").await?,
        GoalCmd::Limit { id, limit } => {
            let conn = client::Conn::connect_or_start(paths).await?;
            let said = conn.call(proto::Command::GoalAction { id: id.clone(), action: format!("limit {limit}") }).await?;
            let goal: store::Goal = serde_json::from_value(said["goal"].clone())?;
            println!("goal {} may work {}", goal.id, goal.allowance(util::now()));
        }
    }
    Ok(())
}

async fn goal_action(paths: &Paths, id: &str, action: &str, status: &str) -> Result<()> {
    if let Ok(conn) = Conn::connect(paths).await {
        conn.call(Command::GoalAction { id: id.to_string(), action: action.to_string() }).await?;
    } else {
        // No daemon: the goals table is the control plane, so edit it directly.
        let store = open_store(paths)?;
        let id = store.resolve_goal(id)?;
        store.goal_set_status(&id, status, &format!("{action} by the user"))?;
        if status == "active" {
            println!("the daemon is not running; start it to resume work: narsil start");
        }
    }
    println!("goal {id}: {status}");
    Ok(())
}

// ---- memory ---------------------------------------------------------------

fn cmd_memory(paths: &Paths, cmd: MemoryCmd) -> Result<()> {
    let store = open_store(paths)?;
    let cfg = Config::load(paths)?;
    let project = context::project_scope(&cwd()?);
    // Say so when the notes cannot be reached, since what follows is then the index's copy.
    let report = |r: &memory::Refresh| {
        if !r.available {
            eprintln!("note: {}", r.why);
        }
        r.problems.iter().for_each(|p| eprintln!("note: {p}"));
    };
    let show = |m: &store::Memory| {
        let scope = if m.scope == "global" { "global".to_string() } else { m.scope.replacen(':', " ", 1) };
        println!("#{}  [{} · {}]  {}", m.id, scope, m.kind, m.title);
        for line in m.body.trim().lines() {
            println!("      {line}");
        }
    };
    match cmd {
        MemoryCmd::List { all } => {
            let scopes = if all { Vec::new() } else { vec!["global".to_string(), project] };
            report(&memory::refresh(&store, &cfg, paths));
            let items = store.memory_list(&scopes, 500)?;
            if items.is_empty() {
                println!("memory is empty");
            }
            items.iter().for_each(show);
        }
        MemoryCmd::Add { text, title, project: in_project, kind } => {
            let body = text.join(" ");
            if body.trim().is_empty() {
                bail!("nothing to remember");
            }
            let title = title.unwrap_or_else(|| first_line(&body, 60));
            let scope = if in_project { project } else { "global".to_string() };
            let entry = memory::Entry { scope: &scope, kind: &kind, title: &title, body: body.trim(), source: None };
            let saved = memory::save(&store, &cfg, paths, &entry)?;
            println!(
                "saved as #{}{}; new sessions will see it",
                saved.id,
                if saved.held { " (held: the memory folder is not reachable right now)" } else { "" }
            );
        }
        MemoryCmd::Search { query } => {
            report(&memory::refresh(&store, &cfg, paths));
            let items = store.memory_search(&query.join(" "), &[], 20)?;
            if items.is_empty() {
                println!("no matches");
            }
            items.iter().for_each(show);
        }
        MemoryCmd::Rm { id } => {
            if memory::delete(&store, &cfg, paths, id)? {
                println!("forgot #{id}");
            } else {
                bail!("there is no memory #{id}");
            }
        }
        MemoryCmd::Sync => {
            let root = memory::root(&cfg, paths);
            println!("memory notes: {}{}", root.dir.display(), if root.vault.is_some() { " (in the Obsidian vault)" } else { "" });
            let r = memory::refresh(&store, &cfg, paths);
            println!("{}", r.summary());
            r.problems.iter().for_each(|p| println!("  ! {p}"));
            println!("{} entries indexed", store.memory_durable()?.len());
        }
        MemoryCmd::Restore => {
            let r = memory::restore(&store, &cfg, paths)?;
            println!("{}", r.summary());
            r.problems.iter().for_each(|p| println!("  ! {p}"));
        }
    }
    Ok(())
}

// ---- config, doctor, logs -------------------------------------------------

fn cmd_config(paths: &Paths, cmd: ConfigCmd) -> Result<()> {
    match cmd {
        ConfigCmd::Path => {
            let mark = |p: &std::path::Path| if p.exists() { "" } else { "  (not created yet)" };
            for (label, p) in [
                ("config", paths.config_file()),
                ("soul", paths.soul_file()),
                ("agents", paths.agents_file()),
                ("credentials", paths.credentials_file()),
                ("database", paths.db_file()),
                ("log", paths.log_file()),
                ("socket", paths.socket()),
            ] {
                println!("{label:<12} {}{}", p.display(), mark(&p));
            }
        }
        ConfigCmd::Show => {
            let file = paths.config_file();
            print!("{}", std::fs::read_to_string(&file).with_context(|| format!("{} (run `narsil setup`)", file.display()))?);
        }
        ConfigCmd::Edit { which } => {
            let file = match which.as_deref().unwrap_or("config") {
                "config" => paths.config_file(),
                "soul" => paths.soul_file(),
                "agents" => paths.agents_file(),
                other => bail!("unknown file {other:?}; choose config, soul or agents"),
            };
            if !file.exists() {
                config::write_default_guidance(paths)?;
                if !file.exists() {
                    bail!("{} does not exist yet; run `narsil setup`", file.display());
                }
            }
            let editor = std::env::var("VISUAL").or_else(|_| std::env::var("EDITOR")).unwrap_or_else(|_| "vi".into());
            let status = std::process::Command::new("sh")
                .arg("-c")
                .arg(format!("{editor} {}", util::shell_quote(&file.to_string_lossy())))
                .status()?;
            if !status.success() {
                bail!("{editor} exited with an error");
            }
            println!("SOUL.md and AGENTS.md apply to new sessions (and after the next compaction);");
            println!("config.toml applies after `narsil restart` or /reload.");
        }
    }
    Ok(())
}

async fn cmd_doctor(paths: &Paths) -> Result<()> {
    println!("narsil {}", env!("CARGO_PKG_VERSION"));
    let cfg = Config::load(paths)?;
    let file = paths.config_file();
    println!("config      {}{}", file.display(), if file.exists() { "" } else { "  MISSING: run `narsil setup`" });
    for (label, p) in [("SOUL.md", paths.soul_file()), ("AGENTS.md", paths.agents_file())] {
        println!("{label:<11} {}", if p.exists() { "present" } else { "missing (setup creates it)" });
    }
    println!("model       {}", if cfg.model.default.is_empty() { "(none)" } else { &cfg.model.default });
    let (default_provider, _) = config::split_model_ref(&cfg.model.default);
    for (name, p) in &cfg.providers {
        let key = config::api_key(paths, name, p);
        let key_state = if !key.is_empty() {
            "key found"
        } else if p.base_url.contains("127.0.0.1") || p.base_url.contains("localhost") || p.kind == "mock" {
            "no key (local)"
        } else {
            "NO KEY"
        };
        print!("provider    {name} ({}) {} [{key_state}] ... ", p.kind, p.base_url);
        std::io::stdout().flush()?;
        match provider::build(&cfg, paths, name) {
            Ok(client) => {
                let t = std::time::Instant::now();
                match tokio::time::timeout(std::time::Duration::from_secs(15), client.list_models()).await {
                    Ok(Ok(models)) => println!("reachable, {} models, {} ms", models.len(), t.elapsed().as_millis()),
                    Ok(Err(e)) => println!("ERROR: {}", first_line(&format!("{e:#}"), 120)),
                    Err(_) => println!("ERROR: no answer in 15s"),
                }
            }
            Err(e) => println!("ERROR: {e:#}"),
        }
        if name == default_provider {
            println!("            compacts at {} tokens", fmt_tokens(cfg.compact_limit(name)));
        }
    }
    match service::ping(paths).await {
        Some(st) => println!(
            "daemon      running, pid {}, {} session(s) active",
            st["pid"],
            st["running"].as_array().map(|a| a.len()).unwrap_or(0)
        ),
        None => println!("daemon      not running (starts on demand, or `narsil start`)"),
    }
    println!("service     {}", if service::unit_path().exists() { "systemd unit installed" } else { "not installed (`narsil service install`)" });
    let groups = std::process::Command::new("id").arg("-nG").output().map(|o| String::from_utf8_lossy(&o.stdout).to_string()).unwrap_or_default();
    let in_admin = groups.split_whitespace().any(|g| g == "wheel" || g == "sudo");
    let nopasswd = std::process::Command::new("sudo")
        .args(["-n", "true"])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    println!(
        "sudo        {}; account {} wheel/sudo; {}",
        if cfg.privilege.sudo { "allowed in config" } else { "disabled in config" },
        if in_admin { "is in" } else { "is NOT in" },
        if nopasswd { "passwordless (usable unattended)" } else { "needs a password (fails fast inside narsil)" }
    );
    println!("permissions chats: {}, goals: {}", cfg.permissions.mode, cfg.permissions.goal_mode);
    {
        let root = memory::root(&cfg, paths);
        let state = match open_store(paths) {
            Ok(store) => {
                // Looking only: a health check must not move the folder under a running daemon.
                let n = store.memory_durable().map(|m| m.len()).unwrap_or(0);
                match memory::folder_state(&root) {
                    Some(true) => format!("{n} notes"),
                    Some(false) => format!("{n} entries; the folder is set up when the daemon next starts or reloads"),
                    None => format!("NOT REACHABLE, {n} entries served from the index"),
                }
            }
            Err(e) => format!("{e:#}"),
        };
        println!("memory      {} ({state})", root.dir.display());
        let recorded = open_store(paths).and_then(|s| s.spans_since(0)).map(|s| s.len()).unwrap_or(0);
        println!(
            "telemetry   log level {}, {} format; {recorded} spans recorded, kept {} days (`narsil trace`, `narsil stats`)",
            std::env::var("NARSIL_LOG").unwrap_or_else(|_| cfg.telemetry.level.clone()),
            cfg.telemetry.log_format,
            cfg.telemetry.keep_days
        );
        for (what, path) in telemetry::locations(paths) {
            println!("            {what}: {}", path.display());
        }
        println!(
            "llmfit      {}",
            match (cfg.llmfit.command.trim().is_empty(), tools::local::program(&cfg)) {
                (true, _) => "switched off ([llmfit] command is empty)".to_string(),
                (false, Some(found)) => format!("{} (the agent can ask it which local models this machine runs)", found.display()),
                (false, None) => "not installed (`brew install llmfit`): the agent cannot say which local models fit this machine".to_string(),
            }
        );
        let vault = cfg.obsidian.vault.trim();
        println!(
            "obsidian    {}",
            match (vault.is_empty(), obsidian::Vault::open(&cfg)) {
                (true, _) => "not set up (`narsil setup --obsidian <vault folder>`)".to_string(),
                (false, Some(v)) => {
                    let mut line =
                        format!("{} ({} notes open to narsil), narsil's notes under {}/", v.root.display(), v.notes().len(), v.folder);
                    if v.closed.by_user > 0 {
                        line.push_str(&format!("\n            closed to narsil: {}", cfg.obsidian.exclude.join(", ")));
                    }
                    for p in &v.closed.problems {
                        line.push_str(&format!("\n            ! exclude {p}"));
                    }
                    if let Some(inner) = v.vault_inside() {
                        line.push_str(&format!(
                            "\n            ! this folder is not a vault itself, but {} is. Set [obsidian] vault to that: as it \
                             stands, narsil's notes are outside the vault Obsidian opens, and the folders beside it are \
                             searched as if they were your notes.",
                            inner.display()
                        ));
                    }
                    line
                }
                (false, None) => format!("{vault} is NOT REACHABLE (is its drive mounted?)"),
            }
        );
    }
    println!(
        "voice       {}",
        match voice::configured(&cfg.voice) {
            Ok(()) => "configured (F2 in the TUI)".to_string(),
            Err(e) => e.to_string(),
        }
    );
    Ok(())
}

fn cmd_work(paths: &Paths, item: &str) -> Result<()> {
    let store = open_store(paths)?;
    let cfg = Config::load(paths)?;
    let project = context::project_dir(&cwd()?);
    let state = work::refresh(&store, &cfg, paths);
    if !state.available {
        eprintln!("note: {}", state.why);
    }
    state.problems.iter().for_each(|p| eprintln!("note: {p}"));
    let view = work::Overview::of(&store, &project)?;
    if !item.trim().is_empty() {
        let found = view.find(item).ok_or_else(|| anyhow!("there is no work item called {item:?} in {project}"))?;
        println!("[{}] {}", found.status.replace('_', " "), view.bundle(found));
        if !found.result.trim().is_empty() {
            println!("Result:\n{}", found.result.trim());
        }
        if let Some(path) = &found.path {
            println!("\nnote: {}", work::root(&cfg, paths).dir.join(path).display());
        }
        return Ok(());
    }
    if view.items.is_empty() {
        println!("no work is laid out for {project}");
        return Ok(());
    }
    fn show(view: &work::Overview, item: &work::Item, depth: usize) {
        let blockers = view.open_blockers(item);
        let mark = match item.status.as_str() {
            "done" => "✓",
            "dropped" => "✗",
            "waiting" => "?",
            "in_progress" => "▶",
            _ if !view.children(&item.id).is_empty() => "▾",
            _ if !blockers.is_empty() => "·",
            _ => "○",
        };
        let mut extra = String::new();
        if item.status == "in_progress" && !item.claimed_by.is_empty() {
            extra = if item.claimed_agent.is_empty() { format!("  (session {})", item.claimed_by) } else { format!("  (subagent {})", item.claimed_agent) };
        } else if !blockers.is_empty() && !item.closed() {
            extra = format!("  (after {})", blockers.iter().map(|b| b.title.as_str()).collect::<Vec<_>>().join(", "));
        }
        println!("{}{mark} {}{extra}", "  ".repeat(depth), item.title);
        for part in view.children(&item.id) {
            show(view, part, depth + 1);
        }
    }
    for top in view.items.iter().filter(|i| i.parent.is_none()) {
        show(&view, top, 0);
    }
    let brief = view.brief();
    if !brief.is_empty() {
        println!("\n{brief}");
    }
    println!("notes: {}", work::root(&cfg, paths).dir.display());
    Ok(())
}

struct TraceOut {
    timeline: bool,
    flamegraph: Option<String>,
    chrome: Option<String>,
    folded: bool,
    json: bool,
}

/// The session tree a trace is about: a goal's, a session's, or the latest one.
fn trace_root(store: &Store, id: Option<&str>) -> Result<String> {
    match id {
        None => store.latest_span_root()?.ok_or_else(|| anyhow!("nothing has been recorded yet; the daemon records what it does as it runs")),
        Some(id) => {
            if let Ok(goal) = store.resolve_goal(id) {
                let session = store.goal_get(&goal)?.and_then(|g| g.session_id).ok_or_else(|| anyhow!("goal {goal} has not started a session yet"))?;
                return Ok(store.require_session(&session)?.root_id);
            }
            Ok(store.require_session(&store.resolve_session(id)?)?.root_id)
        }
    }
}

fn cmd_usage(paths: &Paths, id: Option<String>, as_json: bool) -> Result<()> {
    let store = open_store(paths)?;
    let root = match &id {
        Some(_) => trace_root(&store, id.as_deref())?,
        None => store.latest_usage_root()?.ok_or_else(|| anyhow!("no request to a model has been recorded yet"))?,
    };
    let session = store.require_session(&root)?;
    let report = usage::report(&store, &Config::load(paths)?, &session)?;
    if as_json {
        println!("{}", serde_json::to_string_pretty(&usage::json(&report))?);
        return Ok(());
    }
    let width = crossterm::terminal::size().map(|(w, _)| w as usize).unwrap_or(90).clamp(60, 100);
    println!("session {root}  {}", first_line(&session.title, 60));
    println!();
    usage::lines(&report, width).iter().for_each(|l| println!("{l}"));
    Ok(())
}

fn cmd_trace(paths: &Paths, id: Option<String>, out: TraceOut) -> Result<()> {
    let store = open_store(paths)?;
    let root = trace_root(&store, id.as_deref())?;
    let spans = store.spans_for_root(&root)?;
    if spans.is_empty() {
        bail!("nothing is recorded for session {root} (it ran before recording existed, or its records were pruned)");
    }
    let now = telemetry::now_ms();
    if out.json {
        println!("{}", serde_json::to_string_pretty(&spans)?);
        return Ok(());
    }
    if out.folded {
        telemetry::folded(&spans, now).iter().for_each(|l| println!("{l}"));
        return Ok(());
    }
    let mut wrote = false;
    if let Some(file) = &out.flamegraph {
        let title = store.get_session(&root)?.map(|s| first_line(&s.title, 80)).filter(|t| !t.is_empty()).unwrap_or_else(|| root.clone());
        std::fs::write(file, telemetry::flamegraph(&spans, now, &format!("narsil: {title}"))?).with_context(|| format!("writing {file}"))?;
        println!("flamegraph written to {file} (open it in a browser)");
        wrote = true;
    }
    if let Some(file) = &out.chrome {
        std::fs::write(file, telemetry::chrome_trace(&spans, now).to_string()).with_context(|| format!("writing {file}"))?;
        println!("trace written to {file} (open it at ui.perfetto.dev or chrome://tracing)");
        wrote = true;
    }
    if wrote {
        return Ok(());
    }
    let width = if std::io::stdout().is_terminal() { crossterm::terminal::size().map(|(w, _)| w as usize).unwrap_or(120) } else { 140 };
    let lines = if out.timeline { telemetry::timeline(&spans, now) } else { telemetry::tree(&spans, &telemetry::Notes::load(&store, &spans), now, width.clamp(60, 200)) };
    lines.iter().for_each(|l| println!("{l}"));
    println!();
    telemetry::summary(&spans, now).iter().for_each(|l| println!("{l}"));
    Ok(())
}

fn cmd_stats(paths: &Paths, since: &str) -> Result<()> {
    let (digits, unit) = since.trim().split_at(since.trim().find(|c: char| !c.is_ascii_digit()).unwrap_or(since.trim().len()));
    let n: i64 = digits.parse().map_err(|_| anyhow!("--since takes a number and a unit, such as 30m, 24h or 7d"))?;
    let ms = n * match unit {
        "m" | "min" => 60_000,
        "h" | "" => 3_600_000,
        "d" => 86_400_000,
        other => bail!("unknown unit {other:?}; use m, h or d"),
    };
    let store = open_store(paths)?;
    let now = telemetry::now_ms();
    let spans = store.spans_since(now - ms)?;
    println!("the last {since}:");
    telemetry::stats(&spans, now).iter().for_each(|l| println!("{l}"));
    Ok(())
}

/// A log line as written to a terminal: each part in its own colour, as in the TUI's /logs.
/// `level` is the level of the line before, for a line that continues it; the level of this one is returned.
fn coloured_log_line(line: &str, level: u8) -> (String, u8) {
    use telemetry::LogPart::*;
    let (parts, level) = telemetry::log_parts(line, level);
    let mut out = String::new();
    for (part, piece) in parts {
        let tint = match part {
            Time | Mark | Key | Text(0) => "\x1b[90m",
            Level(3) => "\x1b[1;31m",
            Level(2) => "\x1b[1;33m",
            Level(1) => "\x1b[32m",
            Level(_) | Value => "\x1b[36m",
            Session => "\x1b[94m",
            Agent | Text(2) => "\x1b[33m",
            Goal => "\x1b[35m",
            Text(3) => "\x1b[31m",
            Text(_) => "",
        };
        if tint.is_empty() { out.push_str(&piece) } else { out.push_str(&format!("{tint}{piece}\x1b[0m")) }
    }
    (out, level)
}

fn cmd_logs(paths: &Paths, follow: bool, lines: usize, session: Option<String>) -> Result<()> {
    let file = paths.log_file();
    if !file.exists() {
        bail!("no log yet at {}", file.display());
    }
    // In colour on a terminal; as written when it goes to a file or another program.
    let colour = std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none();
    let mut level = 1u8;
    let mut show = |line: &str| {
        if colour {
            let (painted, now) = coloured_log_line(line, level);
            level = now;
            println!("{painted}");
        } else {
            println!("{line}");
        }
    };
    if let Some(id) = session.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        // A session id may be given in short, as elsewhere; a goal id is taken as it is.
        let id = open_store(paths).ok().and_then(|s| s.resolve_session(id).ok()).unwrap_or_else(|| id.to_string());
        let mut seen: Option<usize> = None;
        loop {
            let text = std::fs::read_to_string(&file)?;
            let mine = telemetry::lines_of(&text, &id);
            // The last few to begin with; after that, whatever is new.
            let from = match seen {
                None => mine.len().saturating_sub(lines),
                Some(n) => n.min(mine.len()),
            };
            for line in &mine[from..] {
                show(line);
            }
            seen = Some(mine.len());
            if !follow {
                if mine.is_empty() {
                    eprintln!("no lines for {id} in {}", file.display());
                }
                return Ok(());
            }
            std::thread::sleep(std::time::Duration::from_millis(500));
        }
    }
    // The last lines, and then, if asked to follow, each new one as it is written.
    let mut text = std::fs::read_to_string(&file)?;
    let all: Vec<&str> = text.lines().collect();
    for line in &all[all.len().saturating_sub(lines)..] {
        show(line);
    }
    if !follow {
        return Ok(());
    }
    let mut read = text.len() as u64;
    loop {
        std::thread::sleep(std::time::Duration::from_millis(300));
        let size = std::fs::metadata(&file).map(|m| m.len()).unwrap_or(0);
        if size < read {
            // The file was started afresh.
            read = 0;
        }
        if size > read {
            use std::io::{Read as _, Seek as _};
            let mut f = std::fs::File::open(&file)?;
            f.seek(std::io::SeekFrom::Start(read))?;
            text.clear();
            f.read_to_string(&mut text)?;
            // Only whole lines: one still being written waits for its end.
            let whole = text.rfind('\n').map(|at| at + 1).unwrap_or(0);
            text[..whole].lines().for_each(&mut show);
            read += whole as u64;
        }
    }
}
