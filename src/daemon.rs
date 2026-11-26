//! The daemon: owns the engine, serves clients over a Unix socket, supervises
//! goals, and on start recovers whatever the previous instance left unfinished.

use std::os::unix::fs::PermissionsExt;
use std::os::unix::io::AsRawFd;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{broadcast, mpsc};
use tokio_util::sync::CancellationToken;

use crate::config::{Config, Paths};
use crate::engine::{Decision, Engine};
use crate::goals;
use crate::logln;
use crate::proto::{Command, Envelope, Event, Request, ServerMsg};
use crate::store::Store;
use crate::util::first_line;

/// Take the single-instance lock. The file stays locked for the life of the process.
fn lock_pid_file(paths: &Paths) -> Result<std::fs::File> {
    use std::io::Write;
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(paths.pid_file())
        .with_context(|| format!("opening {}", paths.pid_file().display()))?;
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        bail!("another narsil daemon is already running");
    }
    file.set_len(0)?;
    (&file).write_all(format!("{}\n", std::process::id()).as_bytes())?;
    Ok(file)
}

/// Decide what to do with sessions the previous instance left mid-run.
fn recover(engine: &Arc<Engine>) -> Result<()> {
    let crashed = engine.store.mark_running_as_interrupted()?;
    if !crashed.is_empty() {
        logln!("recovery: {} session(s) were running when the previous daemon died", crashed.len());
    }
    let interrupted = engine.store.interrupted_sessions()?;
    let resume_chats = engine.cfg().daemon.resume_interrupted_chats;
    for s in interrupted {
        match s.kind.as_str() {
            // Goals are restarted by the supervisor; subagents by their parent's tool call.
            "goal" | "subagent" => {}
            _ if resume_chats => {
                logln!("recovery: resuming chat session {} ({})", s.id, first_line(&s.title, 60));
                engine.start_run(&s.id);
            }
            _ => logln!("recovery: chat session {} is waiting for /continue ({})", s.id, first_line(&s.title, 60)),
        }
    }
    Ok(())
}

pub async fn run(paths: Paths) -> Result<()> {
    paths.ensure()?;
    let _lock = lock_pid_file(&paths)?;
    let cfg = Config::load(&paths)?;
    let store = Store::open(&paths.db_file())?;
    let engine = Engine::new(paths.clone(), cfg, store);
    logln!("narsil daemon {} starting (pid {})", env!("CARGO_PKG_VERSION"), std::process::id());
    if !engine.cfg().is_configured() {
        logln!("no model configured yet; run `narsil setup`");
    }

    let socket = paths.socket();
    let _ = std::fs::remove_file(&socket);
    let listener = UnixListener::bind(&socket).with_context(|| format!("binding {}", socket.display()))?;
    let _ = std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600));

    recover(&engine)?;

    let shutdown = CancellationToken::new();
    let supervisor = tokio::spawn(goals::supervise(engine.clone(), shutdown.clone()));
    let started = Instant::now();

    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let mut sigint = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
    logln!("listening on {}", socket.display());
    loop {
        tokio::select! {
            accepted = listener.accept() => match accepted {
                Ok((stream, _)) => {
                    tokio::spawn(serve_client(engine.clone(), stream, shutdown.clone(), started));
                }
                Err(e) => logln!("accept failed: {e}"),
            },
            _ = sigterm.recv() => { logln!("SIGTERM"); break; }
            _ = sigint.recv() => { logln!("SIGINT"); break; }
            _ = shutdown.cancelled() => break,
        }
    }

    logln!("shutting down: suspending active work");
    shutdown.cancel();
    engine.shutdown().await;
    let _ = supervisor.await;
    let _ = std::fs::remove_file(&socket);
    let _ = std::fs::remove_file(paths.pid_file());
    logln!("stopped");
    Ok(())
}

async fn serve_client(engine: Arc<Engine>, stream: UnixStream, shutdown: CancellationToken, started: Instant) {
    let (read, mut write) = stream.into_split();
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<String>();
    let attached: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));

    let writer = tokio::spawn(async move {
        while let Some(line) = out_rx.recv().await {
            if write.write_all(line.as_bytes()).await.is_err() || write.write_all(b"\n").await.is_err() {
                break;
            }
        }
    });

    // Forward events for the attached session tree, plus global ones.
    let pusher = {
        let mut events = engine.events.subscribe();
        let attached = attached.clone();
        let out_tx = out_tx.clone();
        tokio::spawn(async move {
            loop {
                let envelope = match events.recv().await {
                    Ok(e) => e,
                    Err(broadcast::error::RecvError::Lagged(n)) => Envelope {
                        root: String::new(),
                        session: String::new(),
                        agent: None,
                        event: Event::Notice {
                            level: "warn".into(),
                            text: format!("display fell behind and skipped {n} updates; reopen the session to refresh"),
                        },
                    },
                    Err(broadcast::error::RecvError::Closed) => break,
                };
                let wanted = envelope.root.is_empty()
                    || attached.lock().unwrap_or_else(|e| e.into_inner()).as_deref() == Some(envelope.root.as_str());
                if !wanted {
                    continue;
                }
                let Ok(line) = serde_json::to_string(&ServerMsg::Push(envelope)) else { continue };
                if out_tx.send(line).is_err() {
                    break;
                }
            }
        })
    };

    let mut lines = BufReader::new(read).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        if line.trim().is_empty() {
            continue;
        }
        let reply = match serde_json::from_str::<Request>(&line) {
            Ok(req) => {
                let id = req.id;
                match handle(&engine, req.cmd, &attached, &shutdown, started).await {
                    Ok(data) => ServerMsg::Reply { id, ok: true, data, error: String::new() },
                    Err(e) => ServerMsg::Reply { id, ok: false, data: Value::Null, error: format!("{e:#}") },
                }
            }
            Err(e) => ServerMsg::Reply { id: 0, ok: false, data: Value::Null, error: format!("bad request: {e}") },
        };
        let Ok(text) = serde_json::to_string(&reply) else { continue };
        if out_tx.send(text).is_err() {
            break;
        }
    }
    pusher.abort();
    drop(out_tx);
    let _ = writer.await;
}

async fn handle(
    engine: &Arc<Engine>,
    cmd: Command,
    attached: &Arc<Mutex<Option<String>>>,
    shutdown: &CancellationToken,
    started: Instant,
) -> Result<Value> {
    let store = &engine.store;
    match cmd {
        Command::Ping => Ok(json!({ "version": env!("CARGO_PKG_VERSION"), "pid": std::process::id() })),
        Command::Status => {
            let cfg = engine.cfg();
            Ok(json!({
                "version": env!("CARGO_PKG_VERSION"),
                "pid": std::process::id(),
                "uptime_secs": started.elapsed().as_secs(),
                "model": cfg.model.default,
                "running": engine.running_sessions(),
                "goals": store.goal_list(false)?,
            }))
        }
        Command::Shutdown => {
            shutdown.cancel();
            Ok(Value::Null)
        }
        Command::Reload => {
            engine.reload_config()?;
            Ok(json!({ "model": engine.cfg().model.default }))
        }
        Command::SessionCreate { cwd, title, model } => {
            let session = engine.create_chat_session(&cwd, title, model)?;
            Ok(json!({ "session": session }))
        }
        Command::SessionGet { id } => {
            let id = store.resolve_session(&id)?;
            let session = store.require_session(&id)?;
            let goal = match &session.goal_id {
                Some(g) => store.goal_get(g)?,
                None => None,
            };
            Ok(json!({
                "messages": store.messages_from(&id, 0)?,
                "approvals": engine.pending_approvals(&session.root_id),
                "running": engine.is_running(&id),
                "queued": store.pending_count(&id)?,
                "objectives": store.objectives_get(&id)?,
                "agents": engine.agents(&session.root_id),
                "goal": goal,
                "session": session,
            }))
        }
        Command::Attach { id } => {
            let id = store.resolve_session(&id)?;
            let session = store.require_session(&id)?;
            *attached.lock().unwrap_or_else(|e| e.into_inner()) = Some(session.root_id.clone());
            Ok(json!({ "root": session.root_id }))
        }
        Command::Send { session, text } => {
            if text.trim().is_empty() {
                bail!("empty message");
            }
            let id = store.resolve_session(&session)?;
            engine.send_user_message(&id, &text)?;
            Ok(Value::Null)
        }
        Command::Interrupt { session } => {
            let id = store.resolve_session(&session)?;
            let s = store.require_session(&id)?;
            let mut paused_goal = false;
            if let Some(goal) = s.goal_id.as_ref().and_then(|g| store.goal_get(g).ok().flatten()) {
                if s.kind == "goal" && goal.status == "active" {
                    // Otherwise the supervisor would simply start the next turn.
                    engine.set_goal_status(&goal.id, "paused", "paused by the user")?;
                    paused_goal = true;
                }
            }
            let stopped = engine.interrupt(&id, false);
            Ok(json!({ "stopped": stopped, "paused_goal": paused_goal }))
        }
        Command::Resume { session } => {
            let id = store.resolve_session(&session)?;
            let s = store.require_session(&id)?;
            if let Some(goal) = s.goal_id.as_ref().and_then(|g| store.goal_get(g).ok().flatten()) {
                if s.kind == "goal" && matches!(goal.status.as_str(), "paused" | "blocked") {
                    engine.set_goal_status(&goal.id, "active", "resumed by the user")?;
                    return Ok(json!({ "resumed": "goal" }));
                }
            }
            if engine.is_running(&id) {
                bail!("the session is already running");
            }
            engine.start_run(&id);
            Ok(json!({ "resumed": "session" }))
        }
        Command::Approve { id, decision } => {
            let decision = match decision.as_str() {
                "allow" => Decision::Allow,
                "always" => Decision::Always,
                "deny" => Decision::Deny,
                other => bail!("unknown decision {other:?}"),
            };
            if !engine.resolve_approval(&id, decision) {
                bail!("that approval request is no longer pending");
            }
            Ok(Value::Null)
        }
        Command::SetMode { session, mode } => {
            if !matches!(mode.as_str(), "ask" | "auto") {
                bail!("mode must be ask or auto");
            }
            let id = store.resolve_session(&session)?;
            store.set_perm_mode(&id, &mode)?;
            engine.emit(&store.require_session(&id)?, Event::SessionChanged);
            Ok(Value::Null)
        }
        Command::SetModel { session, model } => {
            engine.validate_model(&model)?;
            let id = store.resolve_session(&session)?;
            if engine.is_running(&id) {
                bail!("the session is busy; change the model between turns");
            }
            store.set_model(&id, &model)?;
            engine.emit(&store.require_session(&id)?, Event::SessionChanged);
            Ok(Value::Null)
        }
        Command::Compact { session } => {
            let id = store.resolve_session(&session)?;
            if store.message_count(&id)? == 0 {
                bail!("nothing to compact yet");
            }
            engine.compact_now(&id).await?;
            Ok(Value::Null)
        }
        Command::GoalAdd { objective, cwd, model } => {
            if objective.trim().is_empty() {
                bail!("the goal needs an objective");
            }
            let model = engine.default_model(model)?;
            let goal = store.goal_create(objective.trim(), &cwd, &model)?;
            engine.emit_global(Event::Goal { goal: goal.clone() });
            // Create the session right away so a client can attach to it.
            let cfg = engine.cfg();
            let session = store.create_session(crate::store::NewSession {
                goal_id: Some(goal.id.clone()),
                kind: "goal".into(),
                title: format!("goal: {}", first_line(&goal.objective, 60)),
                cwd: goal.cwd.clone(),
                model,
                perm_mode: cfg.permissions.goal_mode.clone(),
                ..Default::default()
            })?;
            let brief = [crate::types::Block::Text { text: crate::context::goal_brief(&goal.id, &goal.objective) }];
            store.append_message(&session.id, crate::types::Role::User, crate::types::MsgKind::Message, &brief)?;
            store.goal_set_session(&goal.id, &session.id)?;
            let goal = store.goal_get(&goal.id)?.ok_or_else(|| anyhow!("goal vanished"))?;
            Ok(json!({ "goal": goal, "session": session }))
        }
        Command::GoalAction { id, action } => {
            let id = store.resolve_goal(&id)?;
            let goal = store.goal_get(&id)?.ok_or_else(|| anyhow!("no such goal"))?;
            let (status, note) = match action.as_str() {
                "pause" => ("paused", "paused by the user"),
                "resume" => ("active", "resumed by the user"),
                "cancel" => ("cancelled", "cancelled by the user"),
                other => bail!("unknown goal action {other:?}"),
            };
            if goal.status == status {
                bail!("goal {id} is already {status}");
            }
            if action == "pause" && goal.status != "active" {
                bail!("goal {id} is {}, not active", goal.status);
            }
            engine.set_goal_status(&id, status, note)?;
            if status != "active" {
                if let Some(sid) = &goal.session_id {
                    engine.interrupt(sid, true);
                }
            }
            Ok(json!({ "goal": store.goal_get(&id)? }))
        }
    }
}
