//! Memory tools, goal controls, and web fetch.

use std::sync::LazyLock;
use std::time::Duration;

use anyhow::{Result, anyhow, bail};
use regex::Regex;
use serde_json::Value;

use super::{ToolCtx, ToolOutput, opt_str, opt_u64, str_arg};
use crate::context::{project_scope, scopes_for, task_scope};
use crate::memory;

fn scope_label(scope: &str) -> &'static str {
    if scope == "global" {
        "global"
    } else if scope.starts_with("project:") {
        "project"
    } else {
        "task"
    }
}

fn link_list(links: &[String]) -> String {
    links.iter().map(|l| format!("[[{l}]]")).collect::<Vec<_>>().join(", ")
}

pub fn memory_save(ctx: &ToolCtx<'_>) -> Result<ToolOutput> {
    let input = &ctx.call.input;
    let title = str_arg(input, "title")?.trim();
    let body = str_arg(input, "body")?.trim();
    if title.is_empty() || body.is_empty() {
        bail!("title and body must not be empty");
    }
    let which = opt_str(input, "scope").unwrap_or("project");
    let scope = match which {
        "global" => "global".to_string(),
        "project" => project_scope(&ctx.session.cwd),
        "task" => task_scope(ctx.session),
        other => bail!("unknown scope {other:?}; use global, project or task"),
    };
    let default_kind = match which {
        "global" => "user",
        "task" => "note",
        _ => "project",
    };
    let kind = opt_str(input, "kind").filter(|k| memory::KINDS.contains(k)).unwrap_or(default_kind);
    let engine = ctx.engine;
    let entry = memory::Entry { scope: &scope, kind, title, body, source: Some(&ctx.session.id) };
    let saved = memory::save(&engine.store, ctx.cfg, &engine.paths, &entry)?;
    let mut out = format!("Saved to {which} memory: {title}.{}", crate::secrets::masked_note(&format!("{title}\n{body}")));
    if !saved.links.is_empty() {
        out.push_str(&format!("\nLinks to: {}", link_list(&saved.links)));
    }
    if saved.held {
        out.push_str(
            "\nThe memory folder is not reachable right now, so this is held and will be written there when it \
             returns. It is already used in new sessions.",
        );
    }
    Ok(ToolOutput::ok(out))
}

pub fn memory_search(ctx: &ToolCtx<'_>) -> Result<ToolOutput> {
    let input = &ctx.call.input;
    let query = str_arg(input, "query")?;
    let limit = opt_u64(input, "limit").unwrap_or(8).clamp(1, 50) as i64;
    let engine = ctx.engine;
    let scopes = scopes_for(ctx.session);
    memory::refresh(&engine.store, ctx.cfg, &engine.paths);
    let hits = engine.store.memory_search(query, &scopes, limit)?;
    if hits.is_empty() {
        return Ok(ToolOutput::ok("Nothing in memory matches."));
    }
    let mut out = String::new();
    for m in hits {
        out.push_str(&format!("## {} [{} · {}]\n{}\n", m.title, scope_label(&m.scope), m.kind, m.body.trim()));
        let back = engine.store.memory_backlinks(&m.title, &scopes)?;
        if !back.is_empty() {
            out.push_str(&format!("Linked from: {}\n", link_list(&back)));
        }
        out.push('\n');
    }
    out.push_str("Follow a [[link]] with memory_read.");
    Ok(ToolOutput::ok(out))
}

/// One entry with its neighbours in the graph: what it links to and what links to it.
pub fn memory_read(ctx: &ToolCtx<'_>) -> Result<ToolOutput> {
    let title = str_arg(&ctx.call.input, "title")?;
    let engine = ctx.engine;
    let scopes = scopes_for(ctx.session);
    memory::refresh(&engine.store, ctx.cfg, &engine.paths);
    let Some(found) = memory::read(&engine.store, &scopes, title)? else {
        let near = engine.store.memory_search(title, &scopes, 5)?;
        let hint = if near.is_empty() {
            String::new()
        } else {
            format!(" Closest entries: {}.", link_list(&near.into_iter().map(|m| m.title).collect::<Vec<_>>()))
        };
        return Ok(ToolOutput::err(format!(
            "There is no memory entry called {title:?}.{hint} A link to a name with no entry is a note not written yet."
        )));
    };
    let m = &found.entry;
    let mut out = format!("## {} [{} · {}]\n{}\n", m.title, scope_label(&m.scope), m.kind, m.body.trim());
    if !found.links.is_empty() {
        // A link may also point out of memory, at one of the user's own notes.
        let vault = crate::obsidian::Vault::open(ctx.cfg);
        let shown: Vec<String> = found
            .links
            .iter()
            .map(|(l, exists)| match (exists, vault.as_ref().is_some_and(|v| v.resolve(l).is_ok())) {
                (true, _) => format!("[[{l}]]"),
                (false, true) => format!("[[{l}]] (a note in the vault: vault_read)"),
                (false, false) => format!("[[{l}]] (no entry yet)"),
            })
            .collect();
        out.push_str(&format!("\nLinks to: {}", shown.join(", ")));
    }
    if !found.backlinks.is_empty() {
        out.push_str(&format!("\nLinked from: {}", link_list(&found.backlinks)));
    }
    Ok(ToolOutput::ok(out))
}

pub fn memory_forget(ctx: &ToolCtx<'_>) -> Result<ToolOutput> {
    let input = &ctx.call.input;
    let engine = ctx.engine;
    let scopes = scopes_for(ctx.session);
    memory::refresh(&engine.store, ctx.cfg, &engine.paths);
    let by_id = input.get("id").and_then(|v| v.as_i64().or_else(|| v.as_str().and_then(|s| s.parse().ok())));
    let entry = match (opt_str(input, "title"), by_id) {
        (Some(title), _) => engine.store.memory_find(&scopes, title)?,
        (None, Some(id)) => engine.store.memory_get(id)?.filter(|m| scopes.contains(&m.scope)),
        (None, None) => return Err(anyhow!("missing required string parameter `title`")),
    };
    let Some(entry) = entry else {
        return Ok(ToolOutput::err("There is no such memory entry in this session's scopes."));
    };
    memory::delete(&engine.store, ctx.cfg, &engine.paths, entry.id)?;
    Ok(ToolOutput::ok(format!("Forgot the {} memory entry {:?}.", scope_label(&entry.scope), entry.title)))
}

pub fn change_directory(ctx: &ToolCtx<'_>) -> Result<ToolOutput> {
    let path = str_arg(&ctx.call.input, "path")?;
    let create = super::opt_bool(&ctx.call.input, "create");
    let now = ctx.engine.change_directory(&ctx.session.id, path, create, false)?;
    ctx.engine.emit_work(&crate::context::project_dir(&now));
    Ok(ToolOutput::ok(format!(
        "The working directory is now {now}. Commands start here and relative paths resolve here; project memory and \
         the work plan are this directory's. Call work_next to see whether work is already laid out for it."
    )))
}

pub fn goal_tool(ctx: &ToolCtx<'_>) -> Result<ToolOutput> {
    let Some(goal_id) = ctx.session.goal_id.as_deref() else {
        return Ok(ToolOutput::err("This session is not working on a goal."));
    };
    let input = &ctx.call.input;
    match ctx.call.name.as_str() {
        "goal_define" => {
            let checks: Vec<String> = input["done_when"]
                .as_array()
                .map(|a| a.iter().filter_map(|v| v.as_str()).map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect())
                .unwrap_or_default();
            let open = input["open_ended"] == true || checks.is_empty();
            ctx.engine.store.goal_set_done_when(goal_id, &if open { "open".to_string() } else { checks.join("\n") })?;
            ctx.engine.store.goal_log(goal_id, "progress", &if open { "open-ended: no finish line".to_string() } else { format!("done when: {}", checks.join("; ")) })?;
            ctx.engine.goal_defined(goal_id);
            let hours = ctx.cfg.goals.open_ended_hours;
            Ok(ToolOutput::ok(match open {
                true if hours > 0.0 => format!(
                    "Recorded as open-ended: the user is shown that this runs until stopped, and it will be paused after {hours} h \
                     for them to decide whether it goes on. Work on what matters most first, and record each thing finished with goal_update."
                ),
                true => "Recorded as open-ended: the user is shown that this runs until stopped. Work on what matters most first, \
                         and record each thing finished with goal_update."
                    .to_string(),
                false => format!("Recorded. The goal is done when: {}. Call goal_complete once every one of them holds.", checks.join("; ")),
            }))
        }
        "goal_update" => {
            let note = str_arg(input, "note")?.trim();
            ctx.engine.store.goal_log(goal_id, "progress", note)?;
            ctx.engine.write_goal_note(goal_id);
            Ok(ToolOutput::ok("Recorded in the goal's progress journal."))
        }
        "goal_complete" => {
            let summary = str_arg(input, "summary")?.trim();
            let project = crate::context::project_dir(&ctx.session.cwd);
            let goal_title = ctx.engine.store.goal_get(goal_id)?.map(|g| crate::util::first_line(&g.objective, 70)).unwrap_or_default();
            let save_lessons = |from: &str| {
                let saved = memory::save_learned(&ctx.engine.store, ctx.cfg, &ctx.engine.paths, &project, from, &ctx.session.id, input.get("lessons"));
                if saved.is_empty() { String::new() } else { format!(" Saved to memory for next time: {}.", saved.join("; ")) }
            };
            crate::work::refresh(&ctx.engine.store, ctx.cfg, &ctx.engine.paths);
            let view = crate::work::Overview::of(&ctx.engine.store, &project)?;
            let Some(root) = view.items.iter().find(|i| i.goal == goal_id) else {
                // Done without a plan. Leave a record all the same, so a later session
                // can see that this was done here, what came of it and how it was checked.
                let engine = ctx.engine;
                let title = ctx.engine.store.goal_get(goal_id)?.map(|g| crate::util::first_line(&g.objective, 70)).unwrap_or_else(|| goal_id.to_string());
                let by = crate::work::By { session: &ctx.session.id, agent: "", message_id: ctx.call.message_id };
                {
                    let graph = crate::work::Graph::open(&engine.store, ctx.cfg, &engine.paths);
                    graph.plan(by, &project, goal_id, Some(&title), &[])?;
                    let _ = graph.done(by, &project, &title, summary, "", &|s: &str| engine.is_running(s));
                }
                engine.emit_work(&project);
                let lessons = save_lessons(&title);
                engine.set_goal_status(goal_id, "completed", summary)?;
                return Ok(ToolOutput::ok(format!(
                    "The goal is marked complete, and recorded as a finished work item with your summary as its result.{lessons} \
                     End your turn with a brief closing message; no further work is expected."
                )));
            };
            {
                let open: Vec<&str> = view.descendants(&root.id).into_iter().filter(|i| !i.closed()).map(|i| i.title.as_str()).collect();
                if !open.is_empty() {
                    return Ok(ToolOutput::err(format!(
                        "The goal is not complete: {} item{} still open ({}). Finish them and close each with work_done, \
                         or drop what will not be done with work_update, giving the reason. If they wait on the user, \
                         call goal_blocked.",
                        open.len(),
                        if open.len() == 1 { " is" } else { "s are" },
                        open.join("; ")
                    )));
                }
            }
            let from = view.items.iter().find(|i| i.goal == goal_id).map(|i| i.title.clone()).unwrap_or(goal_title);
            let lessons = save_lessons(&from);
            ctx.engine.set_goal_status(goal_id, "completed", summary)?;
            Ok(ToolOutput::ok(format!(
                "The goal is marked complete.{lessons} End your turn with a brief closing message; no further work is expected."
            )))
        }
        "goal_blocked" => {
            let reason = str_arg(input, "reason")?.trim();
            ctx.engine.set_goal_status(goal_id, "blocked", reason)?;
            // What it found to be a dead end on the way here is kept, marked as one.
            let dead_ends = input.get("dead_ends").and_then(Value::as_array).map(|list| {
                Value::Array(
                    list.iter()
                        .cloned()
                        .map(|mut e| {
                            if let Some(entry) = e.as_object_mut() {
                                entry.insert("dead_end".into(), Value::Bool(true));
                            }
                            e
                        })
                        .collect(),
                )
            });
            let project = crate::context::project_dir(&ctx.session.cwd);
            let from = ctx.engine.store.goal_get(goal_id)?.map(|g| crate::util::first_line(&g.objective, 70)).unwrap_or_default();
            let saved = memory::save_learned(&ctx.engine.store, ctx.cfg, &ctx.engine.paths, &project, &from, &ctx.session.id, dead_ends.as_ref());
            let kept = if saved.is_empty() { String::new() } else { format!(" Saved to memory for next time: {}.", saved.join("; ")) };
            Ok(ToolOutput::ok(format!(
                "The goal is marked blocked and the user has been notified.{kept} End your turn now; work resumes when they reply."
            )))
        }
        other => Ok(ToolOutput::err(format!("unknown goal tool: {other}"))),
    }
}

static DROP_BLOCKS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?is)<script\b.*?</script>|<style\b.*?</style>|<noscript\b.*?</noscript>|<svg\b.*?</svg>|<head\b.*?</head>|<!--.*?-->",
    )
    .unwrap()
});
static BREAK_TAGS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)</?(p|div|br|li|tr|h[1-6]|section|article|pre|ul|ol|table|blockquote|hr)\b[^>]*>").unwrap()
});
static ANY_TAG: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?s)<[^>]*>").unwrap());

pub fn html_to_text(html: &str) -> String {
    let s = DROP_BLOCKS.replace_all(html, " ");
    let s = BREAK_TAGS.replace_all(&s, "\n");
    let s = ANY_TAG.replace_all(&s, "");
    let s = s
        .replace("&nbsp;", " ")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&apos;", "'")
        .replace("&amp;", "&");
    let mut out = String::new();
    let mut blank = false;
    for line in s.lines() {
        let line = line.split_whitespace().collect::<Vec<_>>().join(" ");
        if line.is_empty() {
            blank = !out.is_empty();
        } else {
            if blank {
                out.push('\n');
            }
            blank = false;
            out.push_str(&line);
            out.push('\n');
        }
    }
    out
}

pub async fn web_fetch(ctx: &ToolCtx<'_>) -> Result<ToolOutput> {
    let url = str_arg(&ctx.call.input, "url")?.trim();
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        bail!("only http and https URLs can be fetched");
    }
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(45))
        .user_agent(concat!("narsil/", env!("CARGO_PKG_VERSION")))
        .build()?;
    let fetch = async {
        let resp = client.get(url).send().await?;
        let status = resp.status();
        let ctype = resp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_lowercase();
        let bytes = resp.bytes().await?;
        Ok::<_, reqwest::Error>((status, ctype, bytes))
    };
    let (status, ctype, bytes) = tokio::select! {
        r = fetch => r.map_err(|e| anyhow!("fetch failed: {e}"))?,
        _ = ctx.cancel.cancelled() => return Ok(ToolOutput::err("[interrupted]")),
    };
    let body = String::from_utf8_lossy(&bytes[..bytes.len().min(3 * 1024 * 1024)]);
    let text = if ctype.contains("html") || body.trim_start().starts_with("<!") || body.trim_start().starts_with("<html") {
        html_to_text(&body)
    } else if ctype.contains("json") {
        serde_json::from_str::<Value>(&body)
            .ok()
            .and_then(|v| serde_json::to_string_pretty(&v).ok())
            .unwrap_or_else(|| body.to_string())
    } else {
        body.to_string()
    };
    let out = format!("[{} {}]\n{}", status.as_u16(), url, text.trim());
    Ok(ToolOutput { content: out, is_error: !status.is_success() })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn html_reduces_to_readable_text() {
        let html = "<html><head><title>x</title><style>p{}</style></head><body>\
                    <script>alert(1)</script><h1>Title</h1><p>One &amp; <b>two</b></p>\n\n\n<ul><li>a</li><li>b</li></ul></body></html>";
        assert_eq!(html_to_text(html), "Title\n\nOne & two\n\na\n\nb\n");
    }
}
