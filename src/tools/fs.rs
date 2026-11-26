//! File tools: read, write, edit, list, glob, grep.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};

use super::{ToolCtx, ToolOutput, guard, opt_bool, opt_str, opt_u64, resolve, str_arg, vault_near};
use crate::obsidian::Vault;
use crate::util::floor_boundary;

const MAX_READ_BYTES: u64 = 50 * 1024 * 1024;
const MAX_LINE_CHARS: usize = 2000;

fn looks_binary(bytes: &[u8]) -> bool {
    bytes[..bytes.len().min(8192)].contains(&0)
}

pub async fn read_file(ctx: &ToolCtx<'_>) -> Result<ToolOutput> {
    let input = &ctx.call.input;
    let path = resolve(&ctx.session.cwd, str_arg(input, "path")?);
    guard(ctx.cfg, &path)?;
    let meta = std::fs::metadata(&path).with_context(|| format!("{}", path.display()))?;
    if meta.is_dir() {
        return Ok(ToolOutput::err(format!("{} is a directory; use list_dir", path.display())));
    }
    if meta.len() > MAX_READ_BYTES {
        return Ok(ToolOutput::err(format!(
            "{} is {} bytes, too large to read whole; use grep, or bash with head/tail/sed",
            path.display(),
            meta.len()
        )));
    }
    let bytes = std::fs::read(&path).with_context(|| format!("{}", path.display()))?;
    if bytes.is_empty() {
        return Ok(ToolOutput::ok("[empty file]"));
    }
    if looks_binary(&bytes) {
        return Ok(ToolOutput::ok(format!("[binary file, {} bytes]", bytes.len())));
    }
    let text = String::from_utf8_lossy(&bytes);
    let offset = opt_u64(input, "offset").unwrap_or(1).max(1) as usize;
    let limit = opt_u64(input, "limit").unwrap_or(2000).max(1) as usize;
    let total = text.lines().count();
    let mut out = String::new();
    let mut shown = 0;
    for (i, line) in text.lines().enumerate().skip(offset - 1).take(limit) {
        let line = if line.len() > MAX_LINE_CHARS {
            format!("{}… [line truncated]", &line[..floor_boundary(line, MAX_LINE_CHARS)])
        } else {
            line.to_string()
        };
        out.push_str(&format!("{:>6}\t{}\n", i + 1, line));
        shown += 1;
    }
    if shown == 0 {
        return Ok(ToolOutput::err(format!("offset {offset} is past the end of the file ({total} lines)")));
    }
    if offset - 1 + shown < total {
        out.push_str(&format!(
            "[lines {}-{} of {}; pass offset={} to continue]\n",
            offset,
            offset - 1 + shown,
            total,
            offset + shown
        ));
    }
    Ok(ToolOutput::ok(out))
}

pub async fn write_file(ctx: &ToolCtx<'_>) -> Result<ToolOutput> {
    let input = &ctx.call.input;
    let path = resolve(&ctx.session.cwd, str_arg(input, "path")?);
    guard(ctx.cfg, &path)?;
    let content = str_arg(input, "content")?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    let existed = path.exists();
    std::fs::write(&path, content).with_context(|| format!("writing {}", path.display()))?;
    Ok(ToolOutput::ok(format!(
        "{} {} ({} lines, {} bytes)",
        if existed { "Overwrote" } else { "Created" },
        path.display(),
        content.lines().count(),
        content.len()
    )))
}

pub async fn edit_file(ctx: &ToolCtx<'_>) -> Result<ToolOutput> {
    let input = &ctx.call.input;
    let path = resolve(&ctx.session.cwd, str_arg(input, "path")?);
    guard(ctx.cfg, &path)?;
    let old = str_arg(input, "old_string")?;
    let new = str_arg(input, "new_string")?;
    if old.is_empty() {
        bail!("old_string is empty; to create a file use write_file");
    }
    if old == new {
        bail!("old_string and new_string are identical");
    }
    let text = std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let count = text.matches(old).count();
    let replace_all = opt_bool(input, "replace_all");
    match count {
        0 => {
            return Ok(ToolOutput::err(format!(
                "old_string was not found in {}. It must match exactly, including indentation; read the file again.",
                path.display()
            )));
        }
        n if n > 1 && !replace_all => {
            return Ok(ToolOutput::err(format!(
                "old_string occurs {n} times in {}. Add surrounding lines to make it unique, or set replace_all.",
                path.display()
            )));
        }
        _ => {}
    }
    let updated = if replace_all { text.replace(old, new) } else { text.replacen(old, new, 1) };
    std::fs::write(&path, updated).with_context(|| format!("writing {}", path.display()))?;
    Ok(ToolOutput::ok(format!(
        "Edited {} ({} replacement{})",
        path.display(),
        if replace_all { count } else { 1 },
        if replace_all && count != 1 { "s" } else { "" }
    )))
}

pub async fn list_dir(ctx: &ToolCtx<'_>) -> Result<ToolOutput> {
    let path = resolve(&ctx.session.cwd, opt_str(&ctx.call.input, "path").unwrap_or("."));
    guard(ctx.cfg, &path)?;
    let vault = vault_near(ctx.cfg, &path);
    let mut entries: Vec<(String, bool, u64)> = Vec::new();
    for e in std::fs::read_dir(&path).with_context(|| format!("{}", path.display()))? {
        let e = e?;
        // What is closed in the vault is not listed either.
        if vault.as_ref().is_some_and(|v| v.closes(&e.path())) {
            continue;
        }
        let meta = e.metadata().ok();
        let is_dir = meta.as_ref().is_some_and(|m| m.is_dir());
        entries.push((e.file_name().to_string_lossy().into_owned(), is_dir, meta.map(|m| m.len()).unwrap_or(0)));
    }
    entries.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    let total = entries.len();
    let mut out = format!("{} ({} entries)\n", path.display(), total);
    for (name, is_dir, size) in entries.iter().take(1000) {
        if *is_dir {
            out.push_str(&format!("  {name}/\n"));
        } else {
            out.push_str(&format!("  {name}  ({size} bytes)\n"));
        }
    }
    if total > 1000 {
        out.push_str(&format!("  [... {} more]\n", total - 1000));
    }
    Ok(ToolOutput::ok(out))
}

/// Walk a directory, leaving out `.git` and, when the walk reaches into the
/// Obsidian vault, the parts of it that are closed.
fn walker(base: &Path, vault: Option<Vault>) -> ignore::Walk {
    ignore::WalkBuilder::new(base)
        .hidden(false)
        .filter_entry(move |e| e.file_name() != ".git" && !vault.as_ref().is_some_and(|v| v.closes_as_written(e.path())))
        .build()
}

/// Where a walk starts, and the vault if the walk can reach it. Inside the
/// vault the start is given with links resolved, so that every entry under it
/// can be checked by its path.
fn walk_from(ctx: &ToolCtx<'_>, base: PathBuf) -> Result<(PathBuf, Option<Vault>)> {
    guard(ctx.cfg, &base)?;
    let vault = vault_near(ctx.cfg, &base);
    let base = vault.as_ref().map(|v| v.settle(&base)).unwrap_or(base);
    Ok((base, vault))
}

pub async fn glob(ctx: &ToolCtx<'_>) -> Result<ToolOutput> {
    let input = &ctx.call.input;
    let pattern = str_arg(input, "pattern")?.to_string();
    let (base, vault) = walk_from(ctx, resolve(&ctx.session.cwd, opt_str(input, "path").unwrap_or(".")))?;
    let found = tokio::task::spawn_blocking(move || -> Result<(Vec<PathBuf>, usize)> {
        let matcher = globset::GlobBuilder::new(pattern.trim_start_matches("./"))
            .literal_separator(true)
            .build()
            .map_err(|e| anyhow!("bad glob pattern: {e}"))?
            .compile_matcher();
        let mut hits: Vec<(std::time::SystemTime, PathBuf)> = Vec::new();
        for entry in walker(&base, vault).flatten() {
            if !entry.file_type().is_some_and(|t| t.is_file()) {
                continue;
            }
            let rel = entry.path().strip_prefix(&base).unwrap_or(entry.path());
            if matcher.is_match(rel) {
                let mtime = entry.metadata().ok().and_then(|m| m.modified().ok()).unwrap_or(std::time::UNIX_EPOCH);
                hits.push((mtime, entry.path().to_path_buf()));
            }
        }
        hits.sort_by_key(|hit| std::cmp::Reverse(hit.0));
        let total = hits.len();
        Ok((hits.into_iter().take(200).map(|h| h.1).collect(), total))
    })
    .await??;
    let (paths, total) = found;
    if paths.is_empty() {
        return Ok(ToolOutput::ok("No files match."));
    }
    let mut out: String = paths.iter().map(|p| format!("{}\n", p.display())).collect();
    if total > paths.len() {
        out.push_str(&format!("[{} matches; showing the {} most recently modified]\n", total, paths.len()));
    }
    Ok(ToolOutput::ok(out))
}

pub async fn grep(ctx: &ToolCtx<'_>) -> Result<ToolOutput> {
    let input = &ctx.call.input;
    let pattern = str_arg(input, "pattern")?;
    let re = regex::RegexBuilder::new(pattern)
        .case_insensitive(opt_bool(input, "ignore_case"))
        .build()
        .map_err(|e| anyhow!("bad regular expression: {e}"))?;
    let (base, vault) = walk_from(ctx, resolve(&ctx.session.cwd, opt_str(input, "path").unwrap_or(".")))?;
    let filter = match opt_str(input, "glob") {
        Some(g) => Some(
            globset::GlobBuilder::new(g).build().map_err(|e| anyhow!("bad glob filter: {e}"))?.compile_matcher(),
        ),
        None => None,
    };
    let max = opt_u64(input, "max_results").unwrap_or(100).clamp(1, 2000) as usize;
    if !base.exists() {
        bail!("{} does not exist", base.display());
    }
    let (lines, truncated) = tokio::task::spawn_blocking(move || {
        let mut lines = Vec::new();
        let mut truncated = false;
        let files: Box<dyn Iterator<Item = PathBuf>> = if base.is_file() {
            Box::new(std::iter::once(base.clone()))
        } else {
            Box::new(
                walker(&base, vault)
                    .flatten()
                    .filter(|e| e.file_type().is_some_and(|t| t.is_file()))
                    .map(|e| e.into_path()),
            )
        };
        'files: for file in files {
            if let Some(f) = &filter {
                let rel = file.strip_prefix(&base).unwrap_or(&file);
                if !f.is_match(rel) && !file.file_name().is_some_and(|n| f.is_match(n)) {
                    continue;
                }
            }
            if std::fs::metadata(&file).map(|m| m.len() > 5 * 1024 * 1024).unwrap_or(true) {
                continue;
            }
            let Ok(bytes) = std::fs::read(&file) else { continue };
            if looks_binary(&bytes) {
                continue;
            }
            let text = String::from_utf8_lossy(&bytes);
            for (i, line) in text.lines().enumerate() {
                if re.is_match(line) {
                    if lines.len() >= max {
                        truncated = true;
                        break 'files;
                    }
                    let shown = if line.len() > 300 { &line[..floor_boundary(line, 300)] } else { line };
                    lines.push(format!("{}:{}: {}", file.display(), i + 1, shown.trim_end()));
                }
            }
        }
        (lines, truncated)
    })
    .await?;
    if lines.is_empty() {
        return Ok(ToolOutput::ok("No matches."));
    }
    let mut out = lines.join("\n");
    if truncated {
        out.push_str(&format!("\n[stopped at {max} matches; narrow the pattern or raise max_results]"));
    }
    Ok(ToolOutput::ok(out))
}
