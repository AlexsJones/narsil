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
use tracing::Instrument;

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
                    tracing::info!(goal = %goal.id, session = goal.session_id.as_deref().unwrap_or(""), "goal starting ({})", first_line(&goal.objective, 80));
                    let cancel = CancellationToken::new();
                    let handle = tokio::spawn(run_goal(engine.clone(), goal.id.clone(), cancel.clone()));
                    running.insert(goal.id.clone(), (cancel, handle));
                }
                // Paused or cancelled from outside: stop driving it, leaving in-flight
                // work resumable. A goal the agent itself just completed or blocked is
                // different: its turn is allowed to end, so its closing words are kept
                // and its session is not left looking interrupted.
                Some((cancel, _)) if matches!(goal.status.as_str(), "paused" | "cancelled") => {
                    if let Some(sid) = &goal.session_id {
                        engine.interrupt(sid, true);
                    }
                    cancel.cancel();
                }
                _ => {}
            }
        }
        // Goals no longer listed while running: cancelled or deleted ones are
        // stopped. One the agent just completed is left to finish its turn.
        for (id, (cancel, _)) in &running {
            if !goals.iter().any(|g| &g.id == id) {
                let completed = engine.store.goal_get(id).ok().flatten().is_some_and(|g| g.status == "completed");
                if !completed {
                    cancel.cancel();
                }
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

/// The session a goal works in, made if it has none: briefed with the objective and
/// with what memory holds on it.
pub fn ensure_session(engine: &Arc<Engine>, goal: &Goal) -> Result<String> {
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
    engine.recall_note(&session, &goal.objective);
    engine.store.goal_set_session(&goal.id, &session.id)?;
    Ok(session.id)
}

async fn run_goal(engine: Arc<Engine>, goal_id: String, cancel: CancellationToken) {
    if let Err(e) = drive(&engine, &goal_id, &cancel).await {
        tracing::warn!(goal = %goal_id, "goal stopped by an error: {e:#}");
        let _ = engine.set_goal_status(&goal_id, "paused", &format!("stopped by an error: {e:#}"));
    }
}

/// What `drive` writes when it stops a goal because only questions are left.
pub const WAITING_ON_YOU: &str = "waiting on you: ";

/// Start again the goals that stopped to wait for the user, once they have
/// answered: a question under the goal was closed after it stopped (in its
/// note, or by the agent on their say-so) and something can move again. Whether
/// the supervisor stopped the goal or the agent did makes no difference. A goal
/// the agent blocks again afterwards stays blocked until the next answer.
pub fn resume_answered(engine: &Arc<Engine>) {
    for goal in engine.store.goal_list(false).unwrap_or_default() {
        if goal.status != "blocked" {
            continue;
        }
        let Ok(view) = crate::work::Overview::of(&engine.store, &context::project_dir(&goal.cwd)) else { continue };
        let Some(root) = view.items.iter().find(|i| i.goal == goal.id) else { continue };
        let under = view.descendants(&root.id);
        let answered: Vec<&str> = under
            .iter()
            .filter(|i| i.kind == "question" && i.status == "done" && i.updated_at >= goal.updated_at)
            .map(|i| i.title.as_str())
            .collect();
        let ready = under.iter().any(|i| view.is_ready(i));
        if !answered.is_empty() && ready {
            let _ = engine.set_goal_status(&goal.id, "active", &format!("resumed: you answered {}", answered.join("; ")));
        }
    }
}

/// If every item still open under the goal is waiting on the user (or blocked
/// behind one that is), the questions that are open. None if anything can move.
fn waiting_only(engine: &Arc<Engine>, goal: &Goal) -> Option<String> {
    let view = crate::work::Overview::of(&engine.store, &context::project_dir(&goal.cwd)).ok()?;
    let root = view.items.iter().find(|i| i.goal == goal.id)?;
    let open: Vec<&crate::work::Item> = view.descendants(&root.id).into_iter().filter(|i| !i.closed()).collect();
    let can_move = open.iter().any(|i| view.is_ready(i) || i.status == "in_progress");
    let questions: Vec<&str> = open.iter().filter(|i| i.status == "waiting" || i.kind == "question").map(|i| i.title.as_str()).collect();
    (!can_move && !questions.is_empty()).then(|| questions.join("; "))
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
            let work = crate::work::Overview::of(&engine.store, &context::project_dir(&goal.cwd)).map(|v| v.brief()).unwrap_or_default();
            let note = [Block::Text { text: context::goal_continuation(turn, &work) }];
            let msg = engine.store.append_message(&sid, Role::User, MsgKind::Note, &note)?;
            if let Some(session) = engine.store.get_session(&sid)? {
                engine.emit(&session, Event::Message { msg });
            }
        }

        let turn_span = tracing::info_span!(
            "goal_turn",
            goal = %goal_id,
            session = %sid,
            root = %sid,
            what = %first_line(&goal.objective, 60),
            turn,
            outcome = tracing::field::Empty,
            ok = tracing::field::Empty,
        );
        let outcome = engine.run_session(sid.clone(), cancel.child_token()).instrument(turn_span.clone()).await;
        turn_span.record("outcome", match &outcome {
            Ok(Outcome::Completed { .. }) => "turn ended",
            Ok(Outcome::Interrupted) => "interrupted",
            Ok(Outcome::Refused(_)) => "refused",
            Err(_) => "failed",
        });
        turn_span.record("ok", matches!(outcome, Ok(Outcome::Completed { .. } | Outcome::Interrupted)));
        drop(turn_span);
        match outcome? {
            Outcome::Completed { tool_calls, .. } => {
                let Some(after) = engine.store.goal_get(goal_id)? else { return Ok(()) };
                if after.status != "active" {
                    return Ok(()); // the agent marked it complete or blocked
                }
                // Nothing left that can move without the user: say so, instead of turning over idly.
                if let Some(questions) = waiting_only(engine, &after) {
                    engine.set_goal_status(goal_id, "blocked", &format!("{WAITING_ON_YOU}{questions}"))?;
                    return Ok(());
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
