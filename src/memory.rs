//! Durable memory: a folder of Markdown notes that link to each other.
//!
//! The notes are the source of truth. One note is one entry: its file name is
//! the title, its text is the body, and `[[Title]]` in the text is a relation
//! to another entry. A few properties at the top say where it applies:
//!
//!   ---
//!   scope: global            (or project:/path/to/project)
//!   kind: user               (user | feedback | project | reference | note)
//!   updated: 2026-11-26
//!   ---
//!
//! Anything can edit the folder: narsil's memory tools, a text editor, Obsidian.
//! `refresh` reads the folder into the `memories` table of the store, which is
//! only an index (full-text search, the link graph, and what gets put into a
//! session's prompt). Delete the index and the next refresh rebuilds it.
//!
//! The folder may be somewhere that is not always there, such as an Obsidian
//! vault on a drive that is not mounted. Then the index keeps answering reads,
//! and entries saved meanwhile are held in it and written out when the folder
//! returns. A marker file tells a folder that is really there from an empty
//! mount point with the same name.
//!
//! Task notes (the scratchpad shared by the agents on one task) are not part
//! of this: they are short-lived working state and stay in the store.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::{Context, Result, bail};

use crate::config::{Config, Paths};
use crate::obsidian::{Note, file_name, links, markdown_files, read_note, rel_string};
use crate::store::{IndexEntry, Memory, Store};
use crate::util::{expand_tilde, new_id, now, today};

const MARKER: &str = ".narsil-memory";
const META_ROOT: &str = "memory_root";
pub const KINDS: [&str; 7] = ["user", "feedback", "project", "reference", "note", "lesson", "dead_end"];

/// Writers and the scanner in this process take turns.
static LOCK: Mutex<()> = Mutex::new(());

pub struct Root {
    pub dir: PathBuf,
    /// The Obsidian vault the folder is inside, if it is.
    pub vault: Option<PathBuf>,
}

/// Where the memory notes live under this configuration.
pub fn root(cfg: &Config, paths: &Paths) -> Root {
    let vault = cfg.obsidian.vault.trim();
    if !vault.is_empty() && cfg.obsidian.memory_in_vault {
        let vault = expand_tilde(vault);
        let folder = cfg.obsidian.folder.trim().trim_matches('/');
        let folder = if folder.is_empty() { "narsil" } else { folder };
        return Root { dir: vault.join(folder).join("Memory"), vault: Some(vault) };
    }
    let custom = cfg.memory.dir.trim();
    Root { dir: if custom.is_empty() { paths.data_dir.join("memory") } else { expand_tilde(custom) }, vault: None }
}

/// Whether the memory folder can be used right now, found out without
/// touching anything: `Some(true)` it is there, `Some(false)` it will be made
/// when the daemon next looks, `None` it cannot be reached (a vault whose
/// drive is not mounted).
pub fn folder_state(root: &Root) -> Option<bool> {
    if root.dir.join(MARKER).is_file() {
        return Some(true);
    }
    root.vault.as_ref().is_none_or(|v| crate::obsidian::Vault::present(v)).then_some(false)
}

/// What a pass over the folder did.
#[derive(Default, Debug)]
pub struct Refresh {
    /// False when the folder cannot be used right now; `why` says why.
    pub available: bool,
    pub why: String,
    /// Notes read because they were new or had changed.
    pub indexed: usize,
    /// Entries dropped because their note was deleted.
    pub removed: usize,
    /// Held entries written out to the folder.
    pub flushed: usize,
    pub problems: Vec<String>,
}

impl Refresh {
    pub fn changed(&self) -> bool {
        self.indexed + self.removed + self.flushed > 0
    }

    pub fn summary(&self) -> String {
        if !self.available {
            return format!("memory folder unavailable: {}", self.why);
        }
        let mut parts = Vec::new();
        for (n, what) in [
            (self.indexed, "notes read"),
            (self.removed, "entries dropped (note deleted)"),
            (self.flushed, "held entries written"),
        ] {
            if n > 0 {
                parts.push(format!("{n} {what}"));
            }
        }
        if parts.is_empty() { "index and notes already agree".to_string() } else { parts.join(", ") }
    }
}

fn mtime_ns(path: &Path) -> i64 {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos().min(i64::MAX as u128) as i64)
        .unwrap_or(0)
}

/// The folder a scope's notes go in. A note can be moved anywhere inside the
/// memory folder afterwards; its `scope` property is what counts.
fn scope_folder(scope: &str) -> PathBuf {
    match scope.strip_prefix("project:") {
        Some(dir) => {
            let name = Path::new(dir).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            Path::new("Projects").join(file_name(&name))
        }
        None => PathBuf::from("Global"),
    }
}

/// The scope of a note that does not state one, from where it was put:
/// under `Projects/<name>` it belongs to the known project of that name,
/// and anywhere else it is global.
fn scope_from_folder(rel: &Path, known: &[String]) -> Option<String> {
    let mut parts = rel.components().map(|c| c.as_os_str().to_string_lossy().into_owned());
    if parts.next().as_deref() != Some("Projects") {
        return Some("global".to_string());
    }
    let name = parts.next()?;
    parts.next()?; // a file directly in Projects/ belongs to no project
    known.iter().find(|s| s.starts_with("project:") && scope_folder(s).ends_with(&name)).cloned()
}

fn default_kind(scope: &str) -> &'static str {
    if scope == "global" { "user" } else { "project" }
}

/// Write an entry's note, keeping whatever other properties the file has.
fn write_note(path: &Path, scope: &str, kind: &str, title: &str, body: &str) -> Result<()> {
    let dir = path.parent().unwrap_or(Path::new("."));
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let mut note = read_note(path).map(|t| Note::parse(&t)).unwrap_or_default();
    note.set("scope", scope);
    note.set("kind", kind);
    note.set("updated", &today());
    let stem = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    if stem != title {
        // The title has characters a file name cannot hold; keep it exactly.
        note.set("title", title);
    } else {
        note.front.retain(|l| !l.starts_with("title:"));
    }
    note.body = body.trim().to_string();
    // Written beside the note and moved into place, so a reader never sees half a note.
    let tmp = dir.join(format!(".narsil-tmp-{}", new_id()));
    std::fs::write(&tmp, note.render()).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

/// A path for a new note that does not collide with a different one.
fn new_path(dir: &Path, scope: &str, title: &str) -> PathBuf {
    let folder = dir.join(scope_folder(scope));
    let base = file_name(title);
    let mut path = folder.join(format!("{base}.md"));
    let mut n = 2;
    while path.exists() {
        path = folder.join(format!("{base} ({n}).md"));
        n += 1;
    }
    path
}

/// Whether the memory folder can be set up at `root` now.
fn can_start(root: &Root) -> bool {
    match &root.vault {
        None => true,
        Some(vault) => crate::obsidian::Vault::present(vault),
    }
}

/// Remove a note. In a vault it goes to the vault's trash instead, so an entry
/// the agent forgot can still be put back by hand.
fn discard(root: &Root, note: &Path) {
    let kept = root.vault.as_ref().is_some_and(|v| crate::obsidian::trash(v, note).is_ok());
    if !kept {
        let _ = std::fs::remove_file(note);
    }
}

fn unavailable(why: String) -> Refresh {
    Refresh { available: false, why, ..Default::default() }
}

/// Bring the index into line with the notes. Cheap when nothing changed: one
/// walk of the folder comparing modification times.
pub fn refresh(store: &Store, cfg: &Config, paths: &Paths) -> Refresh {
    let _turn = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    refresh_locked(store, &root(cfg, paths)).unwrap_or_else(|e| unavailable(format!("{e:#}")))
}

fn refresh_locked(store: &Store, root: &Root) -> Result<Refresh> {
    let key = root.dir.to_string_lossy().into_owned();
    if store.meta_get(META_ROOT)?.as_deref() != Some(key.as_str()) {
        // First use, or memory was pointed somewhere else (a vault switched on or off).
        if !can_start(root) {
            return Ok(unavailable(format!(
                "{} is not there yet (is the vault's drive mounted?). Memory is served from the index, and \
                 new entries are held until the folder appears",
                root.dir.display()
            )));
        }
        std::fs::create_dir_all(&root.dir).with_context(|| format!("creating {}", root.dir.display()))?;
        std::fs::write(root.dir.join(MARKER), "narsil keeps its memory notes in this folder.\n")?;
        // Whatever the index holds is written into the new folder; notes already there are read below.
        store.memory_mark_all_pending()?;
        store.meta_set(META_ROOT, &key)?;
    }
    if !root.dir.join(MARKER).is_file() {
        return Ok(unavailable(format!(
            "{} is missing or has been emptied (a drive that is not mounted, or the folder was removed). Memory \
             is served from the index and new entries are held. If the folder is gone for good, \
             `narsil memory restore` writes the entries out again",
            root.dir.display()
        )));
    }

    let mut report = Refresh { available: true, ..Default::default() };

    // 1. Entries saved or removed while the folder was away.
    for m in store.memory_durable()?.into_iter().filter(|m| m.pending != 0) {
        let existing = m.path.as_ref().map(|p| root.dir.join(p));
        let edited_meanwhile = existing.as_ref().is_some_and(|p| p.exists() && mtime_ns(p) != m.mtime);
        if m.pending == 2 {
            // A note edited since then is kept: the edit is newer than the wish to forget it.
            if let Some(p) = existing.filter(|p| !edited_meanwhile && p.exists()) {
                discard(root, &p);
            }
            store.memory_delete(m.id)?;
            continue;
        }
        let target = match existing {
            Some(p) if !edited_meanwhile => p,
            // Edited in the meantime, or never written: ours goes in a file of its own.
            _ => new_path(&root.dir, &m.scope, &m.title),
        };
        match write_note(&target, &m.scope, &m.kind, &m.title, &m.body) {
            Ok(()) => {
                let rel = rel_string(target.strip_prefix(&root.dir).unwrap_or(&target));
                store.memory_set_file(m.id, &rel, mtime_ns(&target))?;
                report.flushed += 1;
            }
            Err(e) => report.problems.push(format!("{}: {e:#}", m.title)),
        }
    }

    // 2. Read notes that are new or have changed.
    let before = store.memory_durable()?;
    let scopes: Vec<String> = {
        let mut s: Vec<String> = before.iter().map(|m| m.scope.clone()).collect();
        s.sort();
        s.dedup();
        s
    };
    let mut seen_paths: HashSet<String> = HashSet::new();
    for file in markdown_files(&root.dir) {
        let inside = file.strip_prefix(&root.dir).unwrap_or(&file).to_path_buf();
        let rel = rel_string(&inside);
        let mtime = mtime_ns(&file);
        if let Some(m) = before.iter().find(|m| m.path.as_deref() == Some(rel.as_str())) {
            if m.mtime == mtime {
                seen_paths.insert(rel);
                continue;
            }
        }
        let Some(text) = read_note(&file) else { continue };
        let note = Note::parse(&text);
        if note.body.trim().is_empty() {
            continue; // an empty note says nothing yet
        }
        let stem = file.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        // The file name is the title, unless the note records a title that the name was made from.
        let title = match note.get("title") {
            Some(t) if file_name(&t) == stem => t,
            _ => stem,
        };
        let scope = note
            .get("scope")
            .filter(|s| s == "global" || s.starts_with("project:/"))
            .or_else(|| scope_from_folder(&inside, &scopes));
        let Some(scope) = scope else {
            report.problems.push(format!(
                "{rel}: not used, because nothing says which project it belongs to. Add a property \
                 `scope: project:/path/to/project`, or move it out of Projects/ to make it global"
            ));
            continue;
        };
        let kind = note.get("kind").filter(|k| KINDS.contains(&k.as_str())).unwrap_or_else(|| default_kind(&scope).to_string());
        // Two notes cannot be the same entry. (A note that was moved is not a second one: its old file is gone.)
        let twin = store
            .memory_find(std::slice::from_ref(&scope), &title)?
            .and_then(|m| m.path)
            .filter(|p| *p != rel && root.dir.join(p).exists());
        if let Some(other) = twin {
            report.problems.push(format!("{rel}: ignored, because {other} is already the note called {title:?} in that scope"));
            continue;
        }
        store.memory_index_put(&IndexEntry {
            scope: &scope,
            kind: &kind,
            title: &title,
            body: note.body.trim(),
            path: Some(&rel),
            mtime,
            pending: 0,
            updated_at: mtime / 1_000_000_000,
            links: &links(&note.body),
        })?;
        seen_paths.insert(rel);
        report.indexed += 1;
    }

    // 3. An entry whose note is gone was deleted by hand: forget it.
    for m in store.memory_durable()? {
        if m.pending == 0 && m.path.as_ref().is_some_and(|p| !seen_paths.contains(p)) {
            store.memory_delete(m.id)?;
            report.removed += 1;
        }
    }
    Ok(report)
}

/// An entry to save.
pub struct Entry<'a> {
    pub scope: &'a str,
    pub kind: &'a str,
    pub title: &'a str,
    pub body: &'a str,
    /// The session that is saving it, if one is.
    pub source: Option<&'a str>,
}

pub struct Saved {
    pub id: i64,
    /// True when the folder was not reachable and the entry is being held.
    pub held: bool,
    pub links: Vec<String>,
}

/// Save an entry, replacing the one with the same title in the same scope.
pub fn save(store: &Store, cfg: &Config, paths: &Paths, entry: &Entry<'_>) -> Result<Saved> {
    let Entry { scope, kind, source, .. } = *entry;
    let (title, body) = (entry.title.trim(), entry.body.trim());
    if title.is_empty() || body.is_empty() {
        bail!("a memory entry needs a title and a body");
    }
    let found = links(body);
    if scope.starts_with("task:") {
        let id = store.memory_save(scope, kind, title, body, source)?;
        return Ok(Saved { id, held: false, links: found });
    }
    let _turn = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = root(cfg, paths);
    let state = refresh_locked(store, &root).unwrap_or_else(|e| unavailable(format!("{e:#}")));
    // "editor" updates an existing "Editor" rather than making a second note.
    let existing = store.memory_find(&[scope.to_string()], title)?;
    let title = existing.as_ref().map(|m| m.title.clone()).unwrap_or_else(|| title.to_string());
    let mut entry = IndexEntry {
        scope,
        kind,
        title: &title,
        body,
        path: None,
        mtime: existing.as_ref().map(|m| m.mtime).unwrap_or(0),
        pending: 1,
        updated_at: now(),
        links: &found,
    };
    let old_path = existing.as_ref().and_then(|m| m.path.clone());
    if !state.available {
        entry.path = old_path.as_deref();
        let id = store.memory_index_put(&entry)?;
        return Ok(Saved { id, held: true, links: found });
    }
    let file = match &old_path {
        Some(p) if root.dir.join(p).exists() => root.dir.join(p),
        _ => new_path(&root.dir, scope, &title),
    };
    write_note(&file, scope, kind, &title, body)?;
    let rel = rel_string(file.strip_prefix(&root.dir).unwrap_or(&file));
    entry.path = Some(&rel);
    entry.mtime = mtime_ns(&file);
    entry.pending = 0;
    let id = store.memory_index_put(&entry)?;
    Ok(Saved { id, held: false, links: found })
}

/// Forget an entry and remove its note.
pub fn delete(store: &Store, cfg: &Config, paths: &Paths, id: i64) -> Result<bool> {
    let Some(m) = store.memory_get(id)? else { return Ok(false) };
    if m.scope.starts_with("task:") {
        return store.memory_delete(id);
    }
    let _turn = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = root(cfg, paths);
    let state = refresh_locked(store, &root).unwrap_or_else(|e| unavailable(format!("{e:#}")));
    // The pass above may already have dropped it, if its note was gone.
    let Some(m) = store.memory_get(id)? else { return Ok(true) };
    match (&m.path, state.available) {
        (Some(p), true) => {
            let note = root.dir.join(p);
            if note.exists() {
                discard(&root, &note);
            }
            store.memory_delete(id)
        }
        // Remembered so the note is removed when the folder is back.
        (Some(_), false) => store.memory_set_pending(id, 2).map(|_| true),
        (None, _) => store.memory_delete(id),
    }
}

/// Write every entry the index holds into the memory folder again, for when
/// the folder was lost. Notes still there are kept.
pub fn restore(store: &Store, cfg: &Config, paths: &Paths) -> Result<Refresh> {
    let _turn = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    store.meta_set(META_ROOT, "")?;
    refresh_locked(store, &root(cfg, paths))
}

/// Save what an agent says it learned while doing a piece of work: one memory
/// entry per finding, each noting where and when it was learned. `entries` is
/// the array the work tools take: `[{title, body, dead_end?, applies?}]`.
/// Returns the titles saved. Malformed entries are skipped, not refused, so a
/// slip here never blocks closing the work.
pub fn save_learned(store: &Store, cfg: &Config, paths: &Paths, project: &str, from: &str, source: &str, entries: Option<&serde_json::Value>) -> Vec<String> {
    let Some(list) = entries.and_then(|v| v.as_array()) else { return Vec::new() };
    let mut saved = Vec::new();
    for e in list.iter().take(12) {
        let text = |k: &str| e.get(k).and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
        let (title, body) = (text("title"), text("body"));
        if title.is_empty() || body.is_empty() {
            continue;
        }
        let dead_end = e.get("dead_end").and_then(|v| v.as_bool()).unwrap_or(false);
        let scope = if text("applies") == "this project" { format!("project:{project}") } else { "global".to_string() };
        // Where it came from: a link to the work item, so the two notes sit together in the graph.
        let body = format!("{body}\n\nRecorded {} while working on [[{from}]] in {project}.", today());
        let entry = Entry { scope: &scope, kind: if dead_end { "dead_end" } else { "lesson" }, title: &title, body: &body, source: Some(source) };
        if save(store, cfg, paths, &entry).is_ok() {
            saved.push(title);
        }
    }
    saved
}

/// Words too common to say what a request is about.
const COMMON: [&str; 48] = [
    "about", "after", "again", "also", "because", "before", "being", "build", "could", "create", "does", "done", "each", "from",
    "have", "here", "into", "just", "know", "like", "local", "make", "more", "much", "need", "only", "other", "over", "please",
    "should", "show", "some", "such", "than", "that", "their", "them", "then", "there", "these", "they", "this", "what", "when",
    "where", "which", "with", "would",
];

/// The words of a piece of text that say what it is about: lower case, four
/// letters or more, the commonest left out.
pub fn subject_terms(text: &str) -> Vec<String> {
    let mut terms: Vec<String> = text
        .to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.len() >= 4 && !COMMON.contains(w))
        .map(str::to_string)
        .collect();
    terms.sort();
    terms.dedup();
    terms.truncate(24);
    terms
}

/// Memory entries that bear on a piece of text (a request, a work item):
/// what an agent would otherwise have to rediscover, or worse, try again and
/// fail at again. Dead ends and lessons come first. Entries whose title is
/// in `have` (already in front of the agent) are left out.
pub fn recall(store: &Store, scopes: &[String], text: &str, have: &dyn Fn(&str) -> bool, limit: usize) -> Vec<Memory> {
    let terms = subject_terms(text);
    if terms.is_empty() {
        return Vec::new();
    }
    let found = store.memory_search(&terms.join(" "), scopes, 40).unwrap_or_default();
    let mut scored: Vec<(usize, Memory)> = found
        .into_iter()
        // A lesson or a dead end is worth repeating at the moment it applies, even if it
        // is somewhere in the prompt already; anything else already shown is left out.
        .filter(|m| !m.scope.starts_with("task:") && (matches!(m.kind.as_str(), "dead_end" | "lesson") || !have(&m.title)))
        .filter_map(|m| {
            let title = m.title.to_lowercase();
            let body = m.body.to_lowercase();
            let in_title = terms.iter().filter(|t| title.contains(t.as_str())).count();
            let anywhere = terms.iter().filter(|t| title.contains(t.as_str()) || body.contains(t.as_str())).count();
            // One shared word is chance; a word in the title, or two anywhere, is a subject in common.
            (in_title >= 1 || anywhere >= 2).then(|| {
                let weight = anywhere + in_title * 2 + if matches!(m.kind.as_str(), "dead_end" | "lesson") { 3 } else { 0 };
                (weight, m)
            })
        })
        .collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0).then(b.1.updated_at.cmp(&a.1.updated_at)));
    scored.into_iter().take(limit).map(|(_, m)| m).collect()
}

/// Recalled entries as text for the agent. A dead end is marked as one, with
/// its date, since what failed then may not fail now.
pub fn recalled_text(found: &[Memory]) -> String {
    let mut out = String::new();
    for m in found {
        let scope = if m.scope == "global" { "global" } else { "project" };
        let day = chrono::DateTime::from_timestamp(m.updated_at, 0).map(|t| t.format("%Y-%m-%d").to_string()).unwrap_or_default();
        let kind = match m.kind.as_str() {
            "dead_end" => format!("TRIED BEFORE AND FAILED, {day}"),
            "lesson" => format!("learned {day}"),
            other => other.to_string(),
        };
        out.push_str(&format!("\n## {} [{scope} · {kind}]\n{}\n", m.title, crate::util::truncate_middle(m.body.trim(), 900)));
    }
    out
}

/// An entry with the entries it links to and the ones that link to it.
pub struct Linked {
    pub entry: Memory,
    /// (title as written, whether an entry by that name is visible)
    pub links: Vec<(String, bool)>,
    pub backlinks: Vec<String>,
}

pub fn read(store: &Store, scopes: &[String], title: &str) -> Result<Option<Linked>> {
    let title = title.trim().trim_start_matches("[[").trim_end_matches("]]");
    let title = title.split(['|', '#']).next().unwrap_or(title).trim();
    let Some(entry) = store.memory_find(scopes, title)? else { return Ok(None) };
    let mut out = Vec::new();
    for l in links(&entry.body) {
        let exists = store.memory_find(scopes, &l)?.is_some();
        out.push((l, exists));
    }
    let backlinks = store.memory_backlinks(&entry.title, scopes)?;
    Ok(Some(Linked { entry, links: out, backlinks }))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A config and paths rooted in a fresh temporary folder, removed when dropped.
    struct Sandbox {
        dir: PathBuf,
        cfg: Config,
        paths: Paths,
        store: Store,
    }

    impl Sandbox {
        fn new(tag: &str) -> Sandbox {
            let dir = std::env::temp_dir().join(format!("narsil-mem-{tag}-{}-{}", std::process::id(), new_id()));
            std::fs::create_dir_all(&dir).unwrap();
            let paths = Paths {
                config_dir: dir.join("config"),
                data_dir: dir.join("data"),
                state_dir: dir.join("state"),
                runtime_dir: dir.join("run"),
            };
            Sandbox { dir, cfg: Config::default(), paths, store: Store::open_memory().unwrap() }
        }
        fn notes(&self) -> PathBuf {
            root(&self.cfg, &self.paths).dir
        }
        fn write(&self, rel: &str, text: &str) {
            let path = self.notes().join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }
        fn read(&self, rel: &str) -> String {
            std::fs::read_to_string(self.notes().join(rel)).unwrap()
        }
        fn refresh(&self) -> Refresh {
            refresh(&self.store, &self.cfg, &self.paths)
        }
        fn save(&self, scope: &str, title: &str, body: &str) -> Saved {
            let entry = Entry { scope, kind: default_kind(scope), title, body, source: None };
            save(&self.store, &self.cfg, &self.paths, &entry).unwrap()
        }
        fn titles(&self) -> Vec<String> {
            let mut t: Vec<String> = self.store.memory_list(&[], 100).unwrap().into_iter().map(|m| m.title).collect();
            t.sort();
            t
        }
    }

    impl Drop for Sandbox {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    #[test]
    fn the_notes_are_the_memory() {
        let s = Sandbox::new("truth");
        let project = "project:/home/me/acme";
        s.save("global", "Spelling", "British spelling in prose. See [[Tone]].");
        s.save(project, "Build: how?", "Use just, not make.");
        let note = s.read("Global/Spelling.md");
        assert!(note.contains("scope: global") && note.contains("kind: user") && note.contains("British spelling"));
        let built = s.read("Projects/acme/Build- how-.md");
        assert!(built.contains("title: \"Build: how?\""), "a title a file name cannot hold is kept exactly:\n{built}");
        assert!(!s.refresh().changed(), "nothing to do when nothing changed");

        // Edited by hand: the entry follows.
        s.write("Global/Spelling.md", &note.replace("British spelling in prose.", "British spelling, always."));
        assert_eq!(s.refresh().indexed, 1);
        let m = s.store.memory_find(&["global".into()], "spelling").unwrap().unwrap();
        assert!(m.body.starts_with("British spelling, always."));

        // Written by hand, with no properties at all: remembered as global.
        s.write("Global/Tone.md", "Plain and direct. No filler.");
        s.write("People/Sam.md", "Sam reviews the designs. Likes [[Tone]] kept dry.");
        assert_eq!(s.refresh().indexed, 2);
        assert_eq!(s.titles(), vec!["Build: how?", "Sam", "Spelling", "Tone"]);
        assert_eq!(s.store.memory_find(&["global".into()], "Sam").unwrap().unwrap().scope, "global");

        // Renamed by hand: the entry is renamed. Deleted by hand: the entry is gone.
        std::fs::rename(s.notes().join("Global/Tone.md"), s.notes().join("Global/Voice.md")).unwrap();
        std::fs::remove_file(s.notes().join("People/Sam.md")).unwrap();
        let r = s.refresh();
        assert_eq!((r.indexed, r.removed), (1, 2));
        assert_eq!(s.titles(), vec!["Build: how?", "Spelling", "Voice"]);

        // Saving again under any capitalisation updates the same note and keeps a property added by hand.
        let tagged = s.read("Global/Spelling.md").replace("kind: user", "kind: user\ntags: [style]");
        s.write("Global/Spelling.md", &tagged);
        s.save("global", "spelling", "British spelling. No exceptions.");
        let after = s.read("Global/Spelling.md");
        assert!(after.contains("No exceptions.") && after.contains("tags: [style]"));
        assert_eq!(s.titles().len(), 3);

        // Forgetting removes the note.
        let id = s.store.memory_find(&[project.into()], "Build: how?").unwrap().unwrap().id;
        assert!(delete(&s.store, &s.cfg, &s.paths, id).unwrap());
        assert!(!s.notes().join("Projects/acme/Build- how-.md").exists());
        assert!(!delete(&s.store, &s.cfg, &s.paths, id).unwrap());
    }

    #[test]
    fn what_was_learned_is_saved_and_comes_back_for_a_similar_request() {
        let s = Sandbox::new("recall");
        let learned = serde_json::json!([
            { "title": "No public data about Eldermoor-on-Wyre", "body": "Searched Wikipedia and OpenStreetMap: no such town. It appears to be fictional.", "dead_end": true },
            { "title": "Town history map site: what worked", "body": "Vendor Leaflet locally, geocode landmarks, check the page in headless Chrome." },
            { "title": "Site is served on port 8123", "body": "Run serve.sh in the folder.", "applies": "this project" },
            { "title": "", "body": "skipped: no title" }
        ]);
        let saved = save_learned(&s.store, &s.cfg, &s.paths, "/home/me/site", "Research the town", "sess1", Some(&learned));
        assert_eq!(saved.len(), 3);
        let note = s.read("Global/No public data about Eldermoor-on-Wyre.md");
        assert!(note.contains("kind: dead_end") && note.contains("while working on [[Research the town]] in /home/me/site."), "{note}");
        assert!(s.read("Projects/site/Site is served on port 8123.md").contains("kind: lesson"));
        s.save("global", "Spelling", "British spelling in prose.");

        let global = vec!["global".to_string()];
        let nothing = |_: &str| false;
        // The same request again: the dead end comes back first.
        let again = recall(&s.store, &global, "Build me a website about the history of Eldermoor-on-Wyre", &nothing, 5);
        assert_eq!(again[0].title, "No public data about Eldermoor-on-Wyre");
        assert!(recalled_text(&again).contains("TRIED BEFORE AND FAILED"));
        // A similar request about another town: the approach comes back, the unrelated entry does not.
        let similar = recall(&s.store, &global, "I want a site with a map showing the history of the town of Dorking", &nothing, 5);
        let titles: Vec<&str> = similar.iter().map(|m| m.title.as_str()).collect();
        assert!(titles.contains(&"Town history map site: what worked") && !titles.contains(&"Spelling"), "{titles:?}");
        assert!(!titles.contains(&"Site is served on port 8123"), "another project's entry is out of scope");
        // Nothing in common, and what the agent already has in front of it, are left out.
        assert!(recall(&s.store, &global, "Fix the flaky login test", &nothing, 5).is_empty());
        let have = |t: &str| t == "Spelling";
        assert_eq!(recall(&s.store, &global, "Which spelling do I use in prose?", &nothing, 5).len(), 1);
        assert!(recall(&s.store, &global, "Which spelling do I use in prose?", &have, 5).is_empty());
        // A dead end is repeated at the moment it applies even if it is already shown.
        let shown = |t: &str| t.starts_with("No public data");
        assert_eq!(recall(&s.store, &global, "history of Eldermoor-on-Wyre", &shown, 5)[0].kind, "dead_end");
    }

    #[test]
    fn links_make_a_graph_that_can_be_walked() {
        let s = Sandbox::new("graph");
        let scopes = vec!["global".to_string()];
        s.save("global", "House purchase", "Hub. Open items: [[Survey]] and [[Mortgage offer]].");
        s.save("global", "Survey", "Booked for March. Part of [[House purchase]].");
        let hub = read(&s.store, &scopes, "[[house purchase]]").unwrap().unwrap();
        assert_eq!(hub.links, vec![("Survey".to_string(), true), ("Mortgage offer".to_string(), false)]);
        assert_eq!(hub.backlinks, vec!["Survey"]);
        let survey = read(&s.store, &scopes, "Survey").unwrap().unwrap();
        assert_eq!(survey.backlinks, vec!["House purchase"]);
        // A link made by hand in the file counts too.
        s.write("Global/Mortgage offer.md", "Expires in June. See [[Survey]].");
        s.refresh();
        assert_eq!(read(&s.store, &scopes, "Survey").unwrap().unwrap().backlinks, vec!["House purchase", "Mortgage offer"]);
        // Another scope's entries are not visible through links.
        assert!(read(&s.store, &["project:/x".to_string()], "Survey").unwrap().is_none());
    }

    #[test]
    fn project_notes_need_a_known_project_and_duplicates_are_reported() {
        let s = Sandbox::new("scope");
        s.save("project:/home/me/acme", "Deploys", "Fridays are frozen.");
        s.write("Projects/acme/Testing.md", "Run the slow suite before merging.");
        s.write("Projects/unknown/Idea.md", "Something.");
        s.write("Archive/deploys.md", "---\nscope: project:/home/me/acme\n---\nOlder copy.");
        let r = s.refresh();
        assert_eq!(r.indexed, 1, "{:?}", r.problems);
        assert_eq!(r.problems.len(), 2, "{:?}", r.problems);
        assert_eq!(s.store.memory_find(&["project:/home/me/acme".into()], "Testing").unwrap().unwrap().kind, "project");
        assert!(s.store.memory_find(&[], "Idea").unwrap().is_none());
    }

    #[test]
    fn a_vault_that_is_away_holds_entries_until_it_returns() {
        let mut s = Sandbox::new("away");
        s.save("global", "Before", "Saved while memory was local.");
        let local = s.notes();

        // Memory is pointed at a vault whose drive is not mounted: only an empty mount point.
        let vault = s.dir.join("mount/vault");
        std::fs::create_dir_all(&vault).unwrap();
        s.cfg.obsidian.vault = vault.to_string_lossy().into_owned();
        let r = s.refresh();
        assert!(!r.available && r.why.contains("not there yet"), "{}", r.why);
        assert_eq!(s.titles(), vec!["Before"], "the index keeps answering");
        let held = s.save("global", "During", "Saved while the vault was away.");
        assert!(held.held);
        assert!(std::fs::read_dir(&vault).unwrap().next().is_none(), "nothing is written into the mount point");
        assert_eq!(s.titles(), vec!["Before", "During"]);

        // The drive is mounted: both entries are written into the vault.
        std::fs::create_dir_all(vault.join(".obsidian")).unwrap();
        let r = s.refresh();
        assert!(r.available);
        assert_eq!(r.flushed, 2);
        assert!(s.read("Global/During.md").contains("while the vault was away"));
        assert!(s.read("Global/Before.md").contains("while memory was local"));
        assert!(local.join("Global/Before.md").exists(), "the old folder is left as it was");

        // It goes away again (the marker is not there): held, then removed on return.
        let hidden = s.dir.join("mount/vault-unmounted");
        std::fs::rename(&vault, &hidden).unwrap();
        std::fs::create_dir_all(&vault).unwrap();
        assert!(!s.refresh().available);
        let before = s.store.memory_find(&["global".into()], "Before").unwrap().unwrap();
        assert!(delete(&s.store, &s.cfg, &s.paths, before.id).unwrap());
        assert_eq!(s.titles(), vec!["During"], "a held removal is hidden at once");
        std::fs::remove_dir(&vault).unwrap();
        std::fs::rename(&hidden, &vault).unwrap();
        assert!(s.refresh().available);
        assert!(!s.notes().join("Global/Before.md").exists());
        assert_eq!(s.titles(), vec!["During"]);
        // In a vault a forgotten note is not destroyed: it is in the vault's trash.
        let binned: Vec<String> = std::fs::read_dir(vault.join(".trash")).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
        assert!(binned.len() == 1 && binned[0].starts_with("Before ("), "{binned:?}");
    }

    #[test]
    fn a_lost_folder_can_be_written_out_again() {
        let s = Sandbox::new("restore");
        s.save("global", "One", "First.");
        s.save("global", "Two", "Second.");
        s.store.memory_save("task:abc", "note", "Scratch", "Task notes stay in the store.", None).unwrap();
        std::fs::remove_dir_all(s.notes()).unwrap();
        let r = s.refresh();
        assert!(!r.available && r.why.contains("narsil memory restore"));
        assert_eq!(s.titles(), vec!["One", "Scratch", "Two"]);
        let r = restore(&s.store, &s.cfg, &s.paths).unwrap();
        assert_eq!(r.flushed, 2);
        assert!(s.read("Global/Two.md").contains("Second."));
        assert!(!s.notes().join("Global/Scratch.md").exists());
    }
}
