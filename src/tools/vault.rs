//! Tools for the user's Obsidian vault: search it, read a note with its links,
//! write a note. Offered only when a vault is configured.

use anyhow::{Context, Result, bail};

use super::{ToolCtx, ToolOutput, opt_str, opt_u64, str_arg};
use crate::obsidian::{Note, Vault, link_heading, links, read_note, rel_string, section};

const AWAY: &str = "The Obsidian vault is not reachable right now (its folder is missing; a drive may not be \
     mounted). Tell the user rather than working around it.";

pub async fn vault_search(ctx: &ToolCtx<'_>) -> Result<ToolOutput> {
    let query = str_arg(&ctx.call.input, "query")?.to_string();
    let limit = opt_u64(&ctx.call.input, "limit").unwrap_or(10).clamp(1, 50) as usize;
    let Some(vault) = Vault::open(ctx.cfg) else { return Ok(ToolOutput::err(AWAY)) };
    let hits = tokio::task::spawn_blocking(move || vault.search(&query, limit)).await?;
    if hits.is_empty() {
        return Ok(ToolOutput::ok("No notes match. Try fewer or different words."));
    }
    let mut out = String::new();
    for h in hits {
        out.push_str(&format!("{}\n", h.path));
        for (n, line) in h.lines {
            out.push_str(&format!("  {n}: {}\n", line.trim()));
        }
    }
    out.push_str("\nRead one with vault_read.");
    Ok(ToolOutput::ok(out))
}

pub async fn vault_read(ctx: &ToolCtx<'_>) -> Result<ToolOutput> {
    let name = str_arg(&ctx.call.input, "note")?.to_string();
    let Some(vault) = Vault::open(ctx.cfg) else { return Ok(ToolOutput::err(AWAY)) };
    let found = tokio::task::spawn_blocking(move || -> Result<(String, String, Vec<String>, Vec<String>)> {
        let rel = vault.resolve(&name)?;
        let path = vault.root.join(&rel);
        let Some(text) = read_note(&path) else { bail!("{} is too large to read whole; use read_file with offset and limit", rel_string(&rel)) };
        let note = Note::parse(&text);
        let shown = match link_heading(&name) {
            Some(h) => section(&note.body, &h).with_context(|| format!("{} has no heading {h:?}", rel_string(&rel)))?,
            None => text.clone(),
        };
        Ok((rel_string(&rel), shown, links(&note.body), vault.backlinks(&rel)))
    })
    .await?;
    let (rel, text, outgoing, incoming) = match found {
        Ok(f) => f,
        Err(e) => return Ok(ToolOutput::err(format!("{e:#}"))),
    };
    let mut out = format!("# {rel}\n\n{}\n", text.trim_end());
    let list = |names: &[String], wrap: bool| {
        names.iter().take(40).map(|n| if wrap { format!("[[{n}]]") } else { n.clone() }).collect::<Vec<_>>().join(", ")
    };
    if !outgoing.is_empty() {
        out.push_str(&format!("\nLinks to: {}", list(&outgoing, true)));
    }
    if !incoming.is_empty() {
        out.push_str(&format!("\nLinked from: {}", list(&incoming, false)));
    }
    Ok(ToolOutput::ok(out))
}

pub async fn vault_write(ctx: &ToolCtx<'_>) -> Result<ToolOutput> {
    let input = &ctx.call.input;
    let name = str_arg(input, "note")?;
    let content = str_arg(input, "content")?;
    let mode = opt_str(input, "mode").unwrap_or("create");
    let Some(vault) = Vault::open(ctx.cfg) else { return Ok(ToolOutput::err(AWAY)) };
    let path = vault.confine(name)?;
    let rel = rel_string(path.strip_prefix(&vault.root).unwrap_or(&path));
    let exists = path.exists();
    let text = match (mode, exists) {
        ("create", true) => {
            return Ok(ToolOutput::err(format!(
                "{rel} already exists. Read it first, then use mode `append` to add to it or `replace` to rewrite it."
            )));
        }
        ("replace", false) => return Ok(ToolOutput::err(format!("{rel} does not exist; use mode `create`."))),
        ("create", false) | ("replace", true) | ("append", false) => content.to_string(),
        ("append", true) => {
            let old = std::fs::read_to_string(&path).with_context(|| format!("reading {rel}"))?;
            format!("{}\n\n{}", old.trim_end(), content.trim_start())
        }
        (other, _) => bail!("unknown mode {other:?}; use create, append or replace"),
    };
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    let text = if text.ends_with('\n') { text } else { format!("{text}\n") };
    // A rewritten note is the user's writing being replaced: keep what was there.
    let mut kept = String::new();
    if mode == "replace" {
        let old = crate::obsidian::trash(&vault.root, &path)?;
        kept = format!(" The previous version is in the vault's trash as {:?}.", old.file_name().unwrap_or_default().to_string_lossy());
    }
    std::fs::write(&path, &text).with_context(|| format!("writing {rel}"))?;
    let verb = match (mode, exists) {
        ("append", true) => "Appended to",
        ("replace", _) => "Replaced",
        _ => "Created",
    };
    Ok(ToolOutput::ok(format!("{verb} {rel} ({} lines).{kept}", text.lines().count())))
}
