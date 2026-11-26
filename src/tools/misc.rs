//! Memory tools, goal controls, and web fetch.

use std::sync::LazyLock;
use std::time::Duration;

use anyhow::{Result, anyhow, bail};
use regex::Regex;
use serde_json::Value;

use super::{ToolCtx, ToolOutput, opt_str, opt_u64, str_arg};
use crate::context::{project_scope, scopes_for, task_scope};
use crate::proto::Event;
use crate::store::Objective;

fn scope_label(scope: &str) -> &'static str {
    if scope == "global" {
        "global"
    } else if scope.starts_with("project:") {
        "project"
    } else {
        "task"
    }
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
    let kind = opt_str(input, "kind").unwrap_or(default_kind);
    let id = ctx.engine.store.memory_save(&scope, kind, title, body, Some(&ctx.session.id))?;
    Ok(ToolOutput::ok(format!("Saved to {which} memory as entry {id}: {title}")))
}

pub fn memory_search(ctx: &ToolCtx<'_>) -> Result<ToolOutput> {
    let input = &ctx.call.input;
    let query = str_arg(input, "query")?;
    let limit = opt_u64(input, "limit").unwrap_or(8).clamp(1, 50) as i64;
    let hits = ctx.engine.store.memory_search(query, &scopes_for(ctx.session), limit)?;
    if hits.is_empty() {
        return Ok(ToolOutput::ok("Nothing in memory matches."));
    }
    let mut out = String::new();
    for m in hits {
        out.push_str(&format!(
            "## {} [{} · {} · id {}]\n{}\n\n",
            m.title,
            scope_label(&m.scope),
            m.kind,
            m.id,
            m.body.trim()
        ));
    }
    Ok(ToolOutput::ok(out))
}

pub fn memory_forget(ctx: &ToolCtx<'_>) -> Result<ToolOutput> {
    let id = ctx
        .call
        .input
        .get("id")
        .and_then(|v| v.as_i64().or_else(|| v.as_str().and_then(|s| s.parse().ok())))
        .ok_or_else(|| anyhow!("missing required integer parameter `id`"))?;
    if ctx.engine.store.memory_delete(id)? {
        Ok(ToolOutput::ok(format!("Deleted memory entry {id}.")))
    } else {
        Ok(ToolOutput::err(format!("There is no memory entry {id}.")))
    }
}

/// Replace the session's objectives. An entry keeps its anchor (the message it
/// was last changed in) unless it is new or its status changed in this call.
pub fn objectives(ctx: &ToolCtx<'_>) -> Result<ToolOutput> {
    let Some(list) = ctx.call.input.get("items").and_then(Value::as_array) else {
        bail!("missing required array parameter `items`");
    };
    if list.len() > 40 {
        bail!("at most 40 objectives; merge the smaller ones");
    }
    let store = &ctx.engine.store;
    let previous = store.objectives_get(&ctx.session.id)?;
    let mut items: Vec<Objective> = Vec::new();
    for entry in list {
        let text = entry.get("text").and_then(Value::as_str).unwrap_or("").trim();
        if text.is_empty() {
            bail!("every objective needs a `text`");
        }
        let status = match entry.get("status").and_then(Value::as_str).unwrap_or("pending") {
            "pending" | "todo" => "pending",
            "active" | "in_progress" | "doing" => "active",
            "done" | "completed" | "complete" => "done",
            "dropped" | "cancelled" | "skipped" => "dropped",
            other => bail!("unknown status {other:?}; use pending, active, done or dropped"),
        };
        if items.iter().any(|i| i.text == text) {
            continue;
        }
        let anchor = match previous.iter().find(|p| p.text == text) {
            Some(p) if p.status == status => p.anchor,
            _ => ctx.call.message_id,
        };
        items.push(Objective { text: text.to_string(), status: status.to_string(), anchor });
    }
    store.objectives_set(&ctx.session.id, &items)?;
    ctx.engine.emit(ctx.session, Event::Objectives { items: items.clone() });
    let count = |s: &str| items.iter().filter(|i| i.status == s).count();
    Ok(ToolOutput::ok(format!(
        "Objectives recorded: {} done, {} active, {} pending{}.",
        count("done"),
        count("active"),
        count("pending"),
        if count("dropped") > 0 { format!(", {} dropped", count("dropped")) } else { String::new() }
    )))
}

pub fn goal_tool(ctx: &ToolCtx<'_>) -> Result<ToolOutput> {
    let Some(goal_id) = ctx.session.goal_id.as_deref() else {
        return Ok(ToolOutput::err("This session is not working on a goal."));
    };
    let input = &ctx.call.input;
    match ctx.call.name.as_str() {
        "goal_update" => {
            let note = str_arg(input, "note")?.trim();
            ctx.engine.store.goal_log(goal_id, "progress", note)?;
            Ok(ToolOutput::ok("Recorded in the goal's progress journal."))
        }
        "goal_complete" => {
            let summary = str_arg(input, "summary")?.trim();
            ctx.engine.set_goal_status(goal_id, "completed", summary)?;
            Ok(ToolOutput::ok("The goal is marked complete. End your turn with a brief closing message; no further work is expected."))
        }
        "goal_blocked" => {
            let reason = str_arg(input, "reason")?.trim();
            ctx.engine.set_goal_status(goal_id, "blocked", reason)?;
            Ok(ToolOutput::ok("The goal is marked blocked and the user has been notified. End your turn now; work resumes when they reply."))
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
