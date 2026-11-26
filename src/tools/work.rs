//! Tools over the work graph: lay work out, see what can be done now and take
//! it, close it with evidence, and adjust it along the way.

use anyhow::{Result, bail};
use serde_json::Value;

use super::{ToolCtx, ToolOutput, opt_str, str_arg};
use crate::context::project_dir;
use crate::work::{By, Graph, Overview, Spec};

fn by<'a>(ctx: &'a ToolCtx<'_>) -> By<'a> {
    By { session: &ctx.session.id, agent: ctx.session.agent_name.as_deref().unwrap_or(""), message_id: ctx.call.message_id }
}

fn strings(value: Option<&Value>) -> Vec<String> {
    match value {
        Some(Value::Array(list)) => list.iter().filter_map(|v| v.as_str()).map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect(),
        Some(Value::String(one)) if !one.trim().is_empty() => vec![one.trim().to_string()],
        _ => Vec::new(),
    }
}

fn spec(entry: &Value) -> Result<Spec> {
    let title = entry.get("title").and_then(Value::as_str).unwrap_or("").trim();
    if title.is_empty() {
        bail!("every item needs a `title`");
    }
    Ok(Spec {
        title: title.to_string(),
        description: entry.get("description").and_then(Value::as_str).unwrap_or("").to_string(),
        done_when: strings(entry.get("done_when")),
        blocked_by: strings(entry.get("blocked_by")),
        parent: entry.get("parent").and_then(Value::as_str).map(str::to_string).filter(|p| !p.trim().is_empty()),
        question: entry.get("question").and_then(Value::as_bool).unwrap_or(false),
        discovered_from: None,
    })
}

/// Where the project's work stands, as text for the model.
fn state(ctx: &ToolCtx<'_>, project: &str) -> Result<String> {
    let running = |s: &str| ctx.engine.is_running(s);
    Ok(Overview::of(&ctx.engine.store, project)?.text(&ctx.session.id, &running))
}

pub fn work_plan(ctx: &ToolCtx<'_>) -> Result<ToolOutput> {
    let input = &ctx.call.input;
    let Some(list) = input.get("items").and_then(Value::as_array) else {
        bail!("missing required array parameter `items`");
    };
    if list.len() > 60 {
        bail!("at most 60 items in one call; lay the work out in stages");
    }
    let specs: Vec<Spec> = list.iter().map(spec).collect::<Result<_>>()?;
    let engine = ctx.engine;
    // Work is filed by directory. A plan for somewhere else moves the session
    // there first, so the plan, the project's memory and the commands sit with
    // the work and a later session started there finds them.
    let mut cwd = ctx.session.cwd.clone();
    let mut moved = String::new();
    if let Some(dir) = opt_str(input, "directory").map(str::trim).filter(|d| !d.is_empty() && *d != ".") {
        let now = engine.change_directory(&ctx.session.id, dir, true, false)?;
        if now != cwd {
            moved = format!(
                "The working directory is now {now} (it was {cwd}): commands start there, relative paths resolve there, and \
                 this plan and the project's memory are filed under it.\n\n"
            );
            cwd = now;
        }
    }
    let project = project_dir(&cwd);
    let goal = ctx.session.goal_id.clone().unwrap_or_default();
    // A goal's work hangs under one item, so it is plain when the whole is finished.
    let goal_title = (!goal.is_empty())
        .then(|| ctx.engine.store.goal_get(&goal).ok().flatten())
        .flatten()
        .map(|g| crate::util::first_line(&g.objective, 70));
    let root = opt_str(input, "root").map(str::to_string).or(goal_title);
    let touched = {
        let graph = Graph::open(&engine.store, ctx.cfg, &engine.paths);
        graph.plan(by(ctx), &project, &goal, root.as_deref(), &specs)?
    };
    engine.emit_work(&project);
    // A plan with no order in it is usually one where the order was left unsaid.
    let unordered = specs.len() >= 3 && specs.iter().all(|s| s.blocked_by.is_empty());
    let order = if unordered {
        "\n\nNone of these items waits on another, so all are ready at once. If some must come first, call work_plan again \
         with `blocked_by` on the ones that wait; if they really are independent, they can go to subagents in parallel."
    } else {
        ""
    };
    Ok(ToolOutput::ok(format!(
        "{moved}Laid out {} item{}.\n\n{}{order}\n\nNow take the item you are starting on with work_next (claim), and close each one with \
         work_done when it is finished and checked, as you go. That is what shows progress and what survives a restart; \
         closing everything at the end records nothing until then.",
        touched.len(),
        if touched.len() == 1 { "" } else { "s" },
        state(ctx, &project)?
    )))
}

pub fn work_next(ctx: &ToolCtx<'_>) -> Result<ToolOutput> {
    let project = project_dir(&ctx.session.cwd);
    let engine = ctx.engine;
    let running = |s: &str| engine.is_running(s);
    if let Some(name) = opt_str(&ctx.call.input, "show") {
        // Any item, finished ones included, here or in another project.
        crate::work::refresh(&engine.store, ctx.cfg, &engine.paths);
        let elsewhere = opt_str(&ctx.call.input, "project").map(|p| project_dir(&super::resolve(&ctx.session.cwd, p).to_string_lossy()));
        let view = Overview::of(&engine.store, elsewhere.as_deref().unwrap_or(&project))?;
        return Ok(match view.find(name) {
            Some(item) => ToolOutput::ok(view.show(item)),
            None => ToolOutput::err(format!("There is no work item called {name:?} there.\n\n{}", view.text(&ctx.session.id, &running))),
        });
    }
    let Some(name) = opt_str(&ctx.call.input, "claim") else {
        // Look only: read any by-hand edits first, so the answer is current.
        crate::work::refresh(&engine.store, ctx.cfg, &engine.paths);
        return Ok(ToolOutput::ok(state(ctx, &project)?));
    };
    let taken = {
        let graph = Graph::open(&engine.store, ctx.cfg, &engine.paths);
        graph.claim(by(ctx), &project, name, &running)
    };
    let item = match taken {
        Ok(item) => item,
        Err(e) => return Ok(ToolOutput::err(format!("{e:#}\n\n{}", state(ctx, &project)?))),
    };
    engine.emit_work(&project);
    let view = Overview::of(&engine.store, &project)?;
    // What memory holds on this item's subject, before any of it is tried again.
    let recalled = engine.recall(ctx.session, &format!("{} {}", item.title, item.body));
    let remembered = if recalled.is_empty() {
        String::new()
    } else {
        format!("\nFrom memory, on this subject (do not retry what failed unless something has changed):\n{}\n", crate::memory::recalled_text(&recalled))
    };
    Ok(ToolOutput::ok(format!(
        "You have taken this item.\n\n{}{remembered}\nWhen it is finished, close it with work_done, giving the result, the evidence and what you \
         learned. Anything you find that needs doing but is not part of it goes in `follow_ups`, not into this item. If a path you try \
         turns out to be a dead end, record it at once with memory_save (kind dead_end).",
        view.bundle(&item)
    )))
}

pub fn work_done(ctx: &ToolCtx<'_>) -> Result<ToolOutput> {
    let input = &ctx.call.input;
    let name = str_arg(input, "item")?;
    let result = str_arg(input, "result")?.trim();
    let evidence = opt_str(input, "evidence").unwrap_or("").trim();
    if result.is_empty() {
        bail!("`result` must say what was done and where it is");
    }
    let project = project_dir(&ctx.session.cwd);
    let engine = ctx.engine;
    let running = |s: &str| engine.is_running(s);
    let follow_ups: Vec<Spec> = match input.get("follow_ups").and_then(Value::as_array) {
        Some(list) => list.iter().map(spec).collect::<Result<_>>()?,
        None => Vec::new(),
    };
    let (opened, found) = {
        let graph = Graph::open(&engine.store, ctx.cfg, &engine.paths);
        let view = Overview::of(&engine.store, &project)?;
        let Some(item) = view.find(name).cloned() else {
            return Ok(ToolOutput::err(format!("There is no work item called {name:?}.\n\n{}", state(ctx, &project)?)));
        };
        if item.kind != "question" && evidence.is_empty() {
            return Ok(ToolOutput::err(
                "Closing an item needs `evidence`: the command you ran and what it showed, the file, the commit. If it has \
                 not been checked yet, check it first.",
            ));
        }
        // Work found along the way is filed first, beside the item it came from.
        let mut found = Vec::new();
        if !follow_ups.is_empty() {
            let parent = item.parent.as_deref().and_then(|p| view.get(p)).map(|p| p.title.clone());
            let filed: Vec<Spec> = follow_ups
                .into_iter()
                .map(|mut s| {
                    s.discovered_from = Some(item.title.clone());
                    s.parent = s.parent.or_else(|| parent.clone());
                    s
                })
                .collect();
            found = graph.plan(by(ctx), &project, "", None, &filed)?;
        }
        (graph.done(by(ctx), &project, name, result, evidence, &running), found)
    };
    let opened = match opened {
        Ok(opened) => opened,
        Err(e) => return Ok(ToolOutput::err(format!("{e:#}"))),
    };
    engine.emit_work(&project);
    if let Some(goal) = &ctx.session.goal_id {
        engine.write_goal_note(goal);
    }
    let learned = crate::memory::save_learned(&engine.store, ctx.cfg, &engine.paths, &project, name, &ctx.session.id, input.get("learned"));
    let mut out = format!("Closed {name:?}.{}", crate::secrets::masked_note(&format!("{result}\n{evidence}")));
    if !learned.is_empty() {
        out.push_str(&format!(" Saved to memory for next time: {}.", learned.join("; ")));
    }
    if !found.is_empty() {
        out.push_str(&format!(" Filed: {}.", found.join(", ")));
    }
    if !opened.is_empty() {
        out.push_str(&format!(" That makes ready: {}.", opened.join(", ")));
    }
    Ok(ToolOutput::ok(format!("{out}\n\n{}", state(ctx, &project)?)))
}

pub fn work_update(ctx: &ToolCtx<'_>) -> Result<ToolOutput> {
    let input = &ctx.call.input;
    let name = str_arg(input, "item")?;
    let status = opt_str(input, "status");
    let note = opt_str(input, "note");
    let blockers = strings(input.get("blocked_by"));
    if status.is_none() && note.is_none() && blockers.is_empty() {
        bail!("give a `status`, a `note`, or items for `blocked_by`");
    }
    let project = project_dir(&ctx.session.cwd);
    let engine = ctx.engine;
    let changed = {
        let graph = Graph::open(&engine.store, ctx.cfg, &engine.paths);
        graph.update(by(ctx), &project, name, status, note, &blockers)
    };
    let item = match changed {
        Ok(item) => item,
        Err(e) => return Ok(ToolOutput::err(format!("{e:#}"))),
    };
    engine.emit_work(&project);
    Ok(ToolOutput::ok(format!("{:?} is {}.\n\n{}", item.title, item.status.replace('_', " "), state(ctx, &project)?)))
}
