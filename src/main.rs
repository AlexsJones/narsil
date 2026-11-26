//! narsil: a persistent, provider-agnostic agent harness.

mod client;
mod config;
mod context;
mod daemon;
mod engine;
mod goals;
mod proto;
mod provider;
mod service;
mod setup;
mod store;
mod tools;
mod tui;
mod types;
mod util;
mod voice;

use std::io::{IsTerminal, Read, Write};

use anyhow::{Context, Result, anyhow, bail};
use clap::{Args, Parser, Subcommand};

use crate::client::Conn;
use crate::config::{Config, Paths};
use crate::proto::{Command, Event};
use crate::store::{Session, Store};
use crate::types::{Block, MsgKind, Role};
use crate::util::{first_line, fmt_age, fmt_tokens};

#[derive(Parser)]
#[command(name = "narsil", version, about = "A persistent agent harness: TUI, daemon, unified memory, autonomous goals")]
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
    /// Show the daemon log
    Logs {
        #[arg(short, long)]
        follow: bool,
        #[arg(short = 'n', long, default_value_t = 50)]
        lines: usize,
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
    Rm { id: i64 },
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
        Some(Cmd::Daemon) => daemon::run(paths).await,
        Some(Cmd::Run(a)) => cmd_run(&paths, a).await,
        Some(Cmd::Sessions { cmd }) => cmd_sessions(&paths, cmd.unwrap_or(SessionsCmd::List { all: false, limit: 30 })).await,
        Some(Cmd::Goal { cmd }) => cmd_goal(&paths, cmd).await,
        Some(Cmd::Memory { cmd }) => cmd_memory(&paths, cmd),
        Some(Cmd::Config { cmd }) => cmd_config(&paths, cmd.unwrap_or(ConfigCmd::Path)),
        Some(Cmd::Doctor) => cmd_doctor(&paths).await,
        Some(Cmd::Logs { follow, lines }) => cmd_logs(&paths, follow, lines),
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
    loop {
        let Some(env) = events.recv().await else { bail!("lost the connection to the daemon") };
        if env.root.is_empty() {
            continue;
        }
        let sub = env.session != sid;
        let who = env.agent.as_deref().map(|a| format!("[{a}] ")).unwrap_or_default();
        match env.event {
            Event::TextDelta { text } if !sub => {
                write!(out, "{text}")?;
                out.flush()?;
                mid_line = !text.ends_with('\n');
                streamed = true;
            }
            Event::StreamReset if !sub => {
                if mid_line {
                    writeln!(out)?;
                    mid_line = false;
                }
                streamed = false;
            }
            Event::Message { msg } if !sub && msg.role == Role::Assistant => {
                if !streamed {
                    let text = types::blocks_text(&msg.blocks);
                    if !text.is_empty() {
                        writeln!(out, "{text}")?;
                    }
                } else if mid_line {
                    writeln!(out)?;
                }
                mid_line = false;
                streamed = false;
            }
            Event::ToolStart { name, input, .. } => {
                eprintln!("{who}> {name} {}", tools::summarize_call(&name, &input));
            }
            Event::ToolEnd { is_error, output, .. } => {
                let lines = output.lines().count();
                let more = if lines > 1 { format!(" (+{} lines)", lines - 1) } else { String::new() };
                eprintln!("{who}  {} {}{more}", if is_error { "!" } else { "=" }, first_line(&output, 160));
            }
            Event::Approval { id, tool, summary } => {
                let decision = if std::io::stdin().is_terminal() {
                    eprintln!("\n{who}wants to run {tool}:\n{summary}");
                    eprint!("allow? [y]es / [a]lways / [N]o: ");
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
                eprintln!("{who}[{level}] {text}");
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
    eprintln!("(session {sid})");
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
                    first_line(&g.objective, 70)
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
    let project = context::project_scope(&cwd()?);
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
            let id = store.memory_save(&scope, &kind, &title, body.trim(), None)?;
            println!("saved as #{id}; new sessions will see it");
        }
        MemoryCmd::Search { query } => {
            let items = store.memory_search(&query.join(" "), &[], 20)?;
            if items.is_empty() {
                println!("no matches");
            }
            items.iter().for_each(show);
        }
        MemoryCmd::Rm { id } => {
            if store.memory_delete(id)? {
                println!("deleted #{id}");
            } else {
                bail!("there is no memory #{id}");
            }
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
    println!(
        "voice       {}",
        match voice::configured(&cfg.voice) {
            Ok(()) => "configured (F2 in the TUI)".to_string(),
            Err(e) => e.to_string(),
        }
    );
    Ok(())
}

fn cmd_logs(paths: &Paths, follow: bool, lines: usize) -> Result<()> {
    let file = paths.log_file();
    if !file.exists() {
        bail!("no log yet at {}", file.display());
    }
    if follow {
        use std::os::unix::process::CommandExt;
        let err = std::process::Command::new("tail").arg("-n").arg(lines.to_string()).arg("-f").arg(&file).exec();
        bail!("could not run tail: {err}");
    }
    let text = std::fs::read_to_string(&file)?;
    let all: Vec<&str> = text.lines().collect();
    for line in &all[all.len().saturating_sub(lines)..] {
        println!("{line}");
    }
    Ok(())
}
