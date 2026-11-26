//! Continuous operation. A goal is an objective the agent works on without
//! waiting for the user: when the model ends a turn, the supervisor starts the
//! next one, until the agent declares the goal complete or blocked, a budget
//! runs out, or the agent stalls.
//!
//! The database is the control plane. `narsil goal add/pause/resume/cancel`
//! just edit the goals table; the supervisor polls it. Goals left `active`
//! when the daemon stops are picked up again when it starts, so a crash or a
//! reboot costs at most the step that was in flight.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::context;
use crate::engine::{Engine, Outcome};
use crate::logln;
use crate::proto::Event;
use crate::store::{Goal, NewSession};
use crate::types::{Block, MsgKind, Role};
use crate::util::{first_line, now};

pub async fn supervise(engine: Arc<Engine>, shutdown: CancellationToken) {
    let mut running: HashMap<String, (CancellationToken, JoinHandle<()>)> = HashMap::new();
    loop {
        tokio::select! {
            _ = shutdown.cancelled() => break,
            _ = tokio::time::sleep(Duration::from_millis(700)) => {}
        }
        running.retain(|_, (_, handle)| !handle.is_finished());
        let goals = match engine.store.goal_list(false) {
            Ok(g) => g,
            Err(e) => {
                logln!("goal supervisor: {e:#}");
                continue;
            }
        };
        for goal in &goals {
            let active = goal.status == "active";
            match running.get(&goal.id) {
                None if active => {
                    logln!("goal {}: starting ({})", goal.id, first_line(&goal.objective, 80));
                    let cancel = CancellationToken::new();
                    let handle = tokio::spawn(run_goal(engine.clone(), goal.id.clone(), cancel.clone()));
                    running.insert(goal.id.clone(), (cancel, handle));
                }
                Some((cancel, _)) if !active => {
                    // Paused, cancelled or blocked from outside: stop driving it,
                    // leaving in-flight work resumable.
                    if let Some(sid) = &goal.session_id {
                        engine.interrupt(sid, true);
                    }
                    cancel.cancel();
                }
                _ => {}
            }
        }
        // Goals that were finished (and so are no longer listed) while running.
        for (id, (cancel, _)) in &running {
            if !goals.iter().any(|g| &g.id == id) {
                cancel.cancel();
            }
        }
    }
    for (cancel, _) in running.values() {
        cancel.cancel();
    }
    for (_, (_, handle)) in running {
        let _ = tokio::time::timeout(Duration::from_secs(20), handle).await;
    }
}

fn ensure_session(engine: &Arc<Engine>, goal: &Goal) -> Result<String> {
    if let Some(sid) = &goal.session_id {
        if engine.store.get_session(sid)?.is_some() {
            return Ok(sid.clone());
        }
    }
    let cfg = engine.cfg();
    let model = engine.default_model(Some(goal.model.clone()))?;
    let session = engine.store.create_session(NewSession {
        goal_id: Some(goal.id.clone()),
        kind: "goal".into(),
        title: format!("goal: {}", first_line(&goal.objective, 60)),
        cwd: goal.cwd.clone(),
        model,
        perm_mode: cfg.permissions.goal_mode.clone(),
        ..Default::default()
    })?;
    let brief = [Block::Text { text: context::goal_brief(&goal.id, &goal.objective) }];
    engine.store.append_message(&session.id, Role::User, MsgKind::Message, &brief)?;
    engine.store.goal_set_session(&goal.id, &session.id)?;
    Ok(session.id)
}

async fn run_goal(engine: Arc<Engine>, goal_id: String, cancel: CancellationToken) {
    if let Err(e) = drive(&engine, &goal_id, &cancel).await {
        logln!("goal {goal_id}: {e:#}");
        let _ = engine.set_goal_status(&goal_id, "paused", &format!("stopped by an error: {e:#}"));
    }
}

async fn drive(engine: &Arc<Engine>, goal_id: &str, cancel: &CancellationToken) -> Result<()> {
    let mut idle_turns = 0i64;
    loop {
        let Some(goal) = engine.store.goal_get(goal_id)? else { return Ok(()) };
        if goal.status != "active" || cancel.is_cancelled() || engine.is_shutting_down() {
            return Ok(());
        }
        let cfg = engine.cfg();
        if cfg.goals.max_iterations > 0 && goal.iterations >= cfg.goals.max_iterations {
            engine.set_goal_status(goal_id, "paused", &format!("turn budget reached ({} turns)", goal.iterations))?;
            return Ok(());
        }
        if cfg.goals.max_hours > 0.0 && (now() - goal.created_at) as f64 > cfg.goals.max_hours * 3600.0 {
            engine.set_goal_status(goal_id, "paused", &format!("time budget reached ({} h)", cfg.goals.max_hours))?;
            return Ok(());
        }
        let sid = ensure_session(engine, &goal)?;
        if engine.is_running(&sid) {
            // Someone else is driving the session right now (a manual /continue).
            tokio::time::sleep(Duration::from_millis(500)).await;
            continue;
        }
        let turn = goal.iterations + 1;
        engine.store.goal_bump_iteration(goal_id)?;
        // If the last turn ended and nothing new is queued, tell the agent to keep going.
        let ended = engine
            .store
            .last_message(&sid)?
            .is_some_and(|m| m.role == Role::Assistant && !m.blocks.iter().any(|b| matches!(b, Block::ToolUse { .. })));
        if ended && engine.store.pending_count(&sid)? == 0 {
            let note = [Block::Text { text: context::goal_continuation(turn) }];
            let msg = engine.store.append_message(&sid, Role::User, MsgKind::Note, &note)?;
            if let Some(session) = engine.store.get_session(&sid)? {
                engine.emit(&session, Event::Message { msg });
            }
        }

        match engine.run_session(sid.clone(), cancel.child_token()).await? {
            Outcome::Completed { tool_calls, .. } => {
                let Some(after) = engine.store.goal_get(goal_id)? else { return Ok(()) };
                if after.status != "active" {
                    return Ok(()); // the agent marked it complete or blocked
                }
                idle_turns = if tool_calls == 0 { idle_turns + 1 } else { 0 };
                if cfg.goals.stall_limit > 0 && idle_turns >= cfg.goals.stall_limit {
                    engine.set_goal_status(
                        goal_id,
                        "paused",
                        &format!(
                            "stalled: {idle_turns} turns in a row without using a tool. Read the session, then \
                             resume the goal or send it a message"
                        ),
                    )?;
                    return Ok(());
                }
            }
            Outcome::Interrupted => return Ok(()),
            Outcome::Refused(why) => {
                engine.set_goal_status(goal_id, "blocked", &format!("the model declined to continue: {why}"))?;
                return Ok(());
            }
        }
    }
}
