//! Parallel subagents. Each subagent is a real session in the same store: it
//! has its own transcript and context, shares the parent's memory, project
//! instructions and task notes, and is journaled like any other session.
//!
//! The call records which child sessions it created. If the harness stops or
//! crashes mid-flight, re-running the call picks the same children back up:
//! finished ones return their report at once, unfinished ones continue from
//! their own transcripts.
//!
//! Each child also has a `subagents` record saying what it was spawned for
//! (its task, and the work item it took) and how it ended. Every change to
//! that record is pushed to clients, which is what lets a UI show who is
//! working on what while it happens.
//!
//! A child given a work item takes it before it starts, so no two agents hold
//! the same one, and is handed what the item rests on. If it ends without
//! closing the item, the item is released for someone else.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, bail};
use serde_json::{Map, Value, json};
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;
use tracing::Instrument;

use super::{ToolCtx, ToolOutput};
use crate::engine::{Engine, Outcome};
use crate::store::{NewSession, Store};
use crate::work::{By, Graph, Overview};
use crate::types::{Block, Role, blocks_text};
use crate::util::{first_line, floor_boundary};

struct Planned {
    name: String,
    task: String,
    /// The title of the work item it takes, or whatever was written if that is not an item.
    work: String,
}

/// Match what the model wrote against the project's work items: an exact
/// title or id, or a fragment that fits exactly one item that is not finished.
/// Text that matches nothing is kept as given, so it is still shown.
pub fn resolve_work(given: Option<&Value>, view: &Overview) -> String {
    let text = match given {
        Some(Value::String(s)) => s.trim().to_string(),
        _ => return String::new(),
    };
    if text.is_empty() {
        return text;
    }
    if let Some(item) = view.find(&text) {
        return item.title.clone();
    }
    let wanted = text.to_lowercase();
    let partial: Vec<&crate::work::Item> = view
        .items
        .iter()
        .filter(|i| !i.closed())
        .filter(|i| {
            let have = i.title.to_lowercase();
            have.contains(&wanted) || wanted.contains(&have)
        })
        .collect();
    match partial[..] {
        [only] => only.title.clone(),
        _ => first_line(&text, 200),
    }
}

/// The opening of a report, on one line, for showing next to the agent's name.
fn snippet(text: &str, max: usize) -> String {
    let mut out = String::new();
    for line in text.lines() {
        let line = line.trim().trim_start_matches(['#', '*', '-', '>', ' ']).trim_end_matches(['*', ':']).trim();
        let line = line.replace("**", "").replace('`', "");
        if line.is_empty() {
            continue;
        }
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(&line);
        if out.len() >= max {
            break;
        }
    }
    if out.len() > max {
        out.truncate(floor_boundary(&out, max));
        out.push('…');
    }
    out
}

/// The final message of a session whose turn has ended.
fn final_text(store: &Store, id: &str) -> Result<Option<String>> {
    Ok(store.last_message(id)?.and_then(|m| {
        let ended = m.role == Role::Assistant && !m.blocks.iter().any(|b| matches!(b, Block::ToolUse { .. }));
        ended.then(|| blocks_text(&m.blocks))
    }))
}

/// Run one child to the end of its turn, keeping its record and the clients
/// watching it up to date.
async fn run_child(engine: &Arc<Engine>, id: &str, cancel: CancellationToken) -> Result<Outcome> {
    let store = &engine.store;
    let child = store.require_session(id)?;
    // Finished in an earlier attempt at this same call: its report stands.
    let finished = store.subagent_get(id)?.is_some_and(|a| a.state == "done");
    if finished && store.pending_count(id)? == 0 {
        if let Some(text) = final_text(store, id)? {
            return Ok(Outcome::Completed { text, tool_calls: 0 });
        }
    }
    // The user may be talking to this agent directly; wait for that turn to end.
    while engine.is_running(id) && !cancel.is_cancelled() {
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    if cancel.is_cancelled() {
        store.subagent_finished(id, "interrupted", "stopped before it started")?;
        engine.emit_agent(&child, None);
        return Ok(Outcome::Interrupted);
    }
    store.subagent_started(id)?;
    engine.emit_agent(&child, Some("running"));
    let result = engine.run_session(id.to_string(), cancel).await;
    let (outcome, summary) = match &result {
        Ok(Outcome::Completed { text, .. }) if !text.trim().is_empty() => ("done", snippet(text, 240)),
        Ok(Outcome::Completed { .. }) => ("done", "finished without a final message".to_string()),
        Ok(Outcome::Interrupted) => ("interrupted", "interrupted before it finished".to_string()),
        Ok(Outcome::Refused(why)) => ("failed", format!("the model declined this task: {why}")),
        Err(e) => ("failed", first_line(&format!("{e:#}"), 240)),
    };
    store.subagent_finished(id, outcome, &summary)?;
    engine.emit_agent(&child, None);
    result
}

pub async fn spawn_agents(ctx: &ToolCtx<'_>) -> Result<ToolOutput> {
    let cfg = ctx.cfg;
    if ctx.session.depth >= cfg.agents.max_depth {
        return Ok(ToolOutput::err("Subagents cannot be spawned from this depth; do the work directly."));
    }
    let Some(list) = ctx.call.input.get("agents").and_then(Value::as_array) else {
        bail!("missing required array parameter `agents`");
    };
    if list.is_empty() || list.len() > 16 {
        bail!("`agents` must contain between 1 and 16 entries");
    }
    let store = &ctx.engine.store;
    let project = crate::context::project_dir(&ctx.session.cwd);
    crate::work::refresh(store, cfg, &ctx.engine.paths);
    let view = Overview::of(store, &project)?;
    let mut plan: Vec<Planned> = Vec::new();
    for (i, a) in list.iter().enumerate() {
        let task = a.get("task").and_then(Value::as_str).unwrap_or("").trim();
        if task.is_empty() {
            bail!("agent {} has no task", i + 1);
        }
        let base = a.get("name").and_then(Value::as_str).unwrap_or("").trim();
        let base = if base.is_empty() { format!("agent-{}", i + 1) } else { base.to_string() };
        let mut name = base.clone();
        let mut n = 2;
        while plan.iter().any(|p| p.name == name) {
            name = format!("{base}-{n}");
            n += 1;
        }
        let work = resolve_work(a.get("work").or_else(|| a.get("objective")), &view);
        plan.push(Planned { name, task: task.to_string(), work });
    }

    // Children from an earlier, interrupted attempt at this same call.
    let mut children: Map<String, Value> = ctx
        .call
        .state
        .as_ref()
        .and_then(|s| s.get("children"))
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let model = if cfg.model.subagent.is_empty() { ctx.session.model.clone() } else { cfg.model.subagent.clone() };
    for p in &plan {
        if !children.contains_key(&p.name) {
            let child = store.create_session(NewSession {
                parent: Some(ctx.session.clone()),
                goal_id: ctx.session.goal_id.clone(),
                kind: "subagent".into(),
                agent_name: Some(p.name.clone()),
                title: first_line(&p.task, 60),
                cwd: ctx.session.cwd.clone(),
                model: model.clone(),
                perm_mode: ctx.session.perm_mode.clone(),
            })?;
            // Take the item for the child before anything else can, and hand it what the item rests on.
            let mut brief = p.task.clone();
            if view.find(&p.work).is_some() {
                let engine = ctx.engine;
                let by = By { session: &child.id, agent: &p.name, message_id: ctx.call.message_id };
                let taken = Graph::open(store, cfg, &engine.paths).claim(by, &project, &p.work, &|s: &str| engine.is_running(s));
                brief = match taken {
                    Ok(item) => format!(
                        "[narsil] You hold the work item {:?}. Do it, check it, and close it with work_done (result and \
                         evidence). File anything else you find as a follow-up.\n\n{}\nYour instructions:\n\n{}",
                        item.title,
                        Overview::of(store, &project)?.bundle(&item),
                        p.task
                    ),
                    Err(e) => format!("[narsil] The work item {:?} could not be taken for you ({e:#}); do the task without it.\n\n{}", p.work, p.task),
                };
            }
            store.pending_add(&child.id, &brief)?;
            children.insert(p.name.clone(), json!(child.id));
            // Journal after every child so a crash never orphans one.
            store.tool_call_set_state(&ctx.session.id, ctx.call, &json!({ "children": children }))?;
        }
        // Idempotent, so a child journaled just before a crash still gets its record.
        let child_id = children[&p.name].as_str().unwrap_or_default();
        store.subagent_add(child_id, &ctx.call.id, &p.task, &p.work)?;
        if let Some(child) = store.get_session(child_id)? {
            ctx.engine.emit_agent(&child, None);
        }
    }

    // Start them in the order given, `max_parallel` at a time. Taking the slot
    // here rather than inside each task is what keeps that order.
    let slots = Arc::new(Semaphore::new(cfg.agents.max_parallel.max(1)));
    let mut running = Vec::new();
    for p in &plan {
        let child_id = children[&p.name].as_str().unwrap_or_default().to_string();
        let slot = tokio::select! {
            slot = slots.clone().acquire_owned() => slot.ok(),
            _ = ctx.cancel.cancelled() => None,
        };
        let engine = ctx.engine.clone();
        let cancel = ctx.cancel.child_token();
        let id = child_id.clone();
        // Carries the spawning call's span into the task, so the child's run is recorded beneath it.
        let handle = tokio::spawn(
            async move {
                let _slot = slot;
                run_child(&engine, &id, cancel).await
            }
            .in_current_span(),
        );
        running.push((p.name.clone(), child_id, handle));
    }
    ctx.engine.emit_work(&project);

    let mut out = String::new();
    let mut failed = 0;
    let total = running.len();
    for (name, child_id, handle) in running {
        out.push_str(&format!("## {name} (session {child_id})\n"));
        match handle.await {
            Ok(Ok(Outcome::Completed { text, .. })) if !text.trim().is_empty() => out.push_str(text.trim()),
            Ok(Ok(Outcome::Completed { .. })) => out.push_str("[finished without a final message]"),
            Ok(Ok(Outcome::Interrupted)) => {
                failed += 1;
                out.push_str("[interrupted before it finished]");
            }
            Ok(Ok(Outcome::Refused(why))) => {
                failed += 1;
                out.push_str(&format!("[the model declined this task: {why}]"));
            }
            Ok(Err(e)) => {
                failed += 1;
                out.push_str(&format!("[failed: {e:#}]"));
            }
            Err(e) => {
                failed += 1;
                out.push_str(&format!("[crashed: {e}]"));
            }
        }
        out.push_str("\n\n");
        // An item its agent did not close goes back, so it is not left looking busy.
        let held = Overview::of(store, &project)?.items.into_iter().find(|i| i.status == "in_progress" && i.claimed_by == child_id);
        if let Some(item) = held {
            let by = By { session: &ctx.session.id, agent: "", message_id: ctx.call.message_id };
            let note = format!("subagent {name} ended without closing it");
            let _ = Graph::open(store, cfg, &ctx.engine.paths).update(by, &project, &item.title, Some("open"), Some(&note), &[]);
            out.push_str(&format!("[The work item {:?} was not closed by {name} and is open again.]\n\n", item.title));
        }
    }
    ctx.engine.emit_work(&project);
    Ok(ToolOutput { content: out, is_error: failed == total })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn view() -> Overview {
        let item = |id: &str, title: &str, status: &str| crate::work::Item {
            id: id.into(),
            title: title.into(),
            status: status.into(),
            ..Default::default()
        };
        Overview {
            items: vec![
                item("w-aaaaa", "Tokenizer passes the spec tests", "open"),
                item("w-bbbbb", "README documents the CLI", "open"),
                item("w-ccccc", "README drafted", "done"),
            ],
        }
    }

    #[test]
    fn a_work_link_resolves_to_the_items_own_title() {
        let v = view();
        let r = |x: Value| resolve_work(Some(&x), &v);
        assert_eq!(r(json!("readme documents the cli")), "README documents the CLI");
        assert_eq!(r(json!("[[Tokenizer passes the spec tests]]")), "Tokenizer passes the spec tests");
        assert_eq!(r(json!("w-bbbbb")), "README documents the CLI");
        assert_eq!(r(json!("tokenizer")), "Tokenizer passes the spec tests");
        assert_eq!(r(json!("README")), "README documents the CLI", "a finished item is not what a fragment means");
        // Unknown or ambiguous text is kept, not guessed at.
        assert_eq!(r(json!("the")), "the");
        assert_eq!(r(json!("Deploy to production")), "Deploy to production");
        assert_eq!(resolve_work(None, &v), "");
        assert_eq!(r(json!(3)), "");
    }

    #[test]
    fn report_snippets_skip_markup_and_stay_short() {
        assert_eq!(snippet("## Findings\n\n- Lexer done.\n- 14 tests pass.", 80), "Findings Lexer done. 14 tests pass.");
        assert_eq!(snippet("Measured `east.txt`.\n1. **Line count:** 64", 80), "Measured east.txt. 1. Line count: 64");
        let long = snippet(&"word ".repeat(100), 40);
        assert!(long.chars().count() <= 41 && long.ends_with('…'));
        assert_eq!(snippet("\n\n", 40), "");
    }
}
