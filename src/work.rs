//! The work graph: what there is to do, what depends on what, who has taken
//! what, and what came of it. This is how a long task keeps its shape across
//! compactions, restarts, subagents and separate sessions.
//!
//! One item is one Markdown note, kept beside the memory notes:
//!
//!   Work/<project>/<Title>.md
//!
//!   ---
//!   id: w-7c2e1
//!   status: open                  (open | in_progress | done | dropped | waiting)
//!   parent: "[[Personal website live]]"
//!   blocked_by: ["[[Employment history gathered]]", "[[Site scaffold builds locally]]"]
//!   project: /home/me/Code/personal-site
//!   ---
//!   What has to be true when this is done, and how to tell.
//!
//!   ## Result
//!   What was done, where it is, and the evidence.
//!
//!   ## Log
//!   - 2026-11-27 01:12 · created (session k3f9m2xa)
//!
//! As with memory, the notes are the source of truth and can be edited by
//! hand: set `status: done` on a question to answer it, `dropped` to cancel an
//! item, add a link to `blocked_by`, or write a new note to add work. The
//! tables in the store are an index (which items are ready, who holds what)
//! rebuilt from the notes, plus `work_events`, the record of which session did
//! what to an item and where in its transcript.
//!
//! An item is **ready** when it is open, has no parts of its own, and nothing
//! it is blocked by is still open. Taking an item (`claim`) is atomic within
//! the daemon, so two agents never hold the same one.
//!
//! The folder can be away (a vault on an unmounted drive). Then the index
//! carries on, changes are held, and they are written out when it returns.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Instant;

use anyhow::{Context, Result, anyhow, bail};
use rusqlite::params;
use serde::{Deserialize, Serialize};

use crate::config::{Config, Paths};
use crate::obsidian::{Note, Vault, file_name, markdown_files, read_note, rel_string};
use crate::store::Store;
use crate::util::{expand_tilde, first_line, new_id, now, today};

const MARKER: &str = ".narsil-work";
const META_ROOT: &str = "work_root";
pub const STATUSES: [&str; 5] = ["open", "in_progress", "done", "dropped", "waiting"];

static LOCK: Mutex<()> = Mutex::new(());
/// When the folder was last walked, so a burst of calls does not walk it each time.
static LAST_SCAN: Mutex<Option<Instant>> = Mutex::new(None);

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct Item {
    pub id: String,
    /// The directory the work belongs to.
    pub project: String,
    pub title: String,
    pub status: String,
    /// `task`, or `question` for something only the user can settle.
    pub kind: String,
    pub parent: Option<String>,
    pub discovered_from: Option<String>,
    /// The goal this item is the root of, if it is one.
    pub goal: String,
    pub body: String,
    pub result: String,
    pub log: String,
    /// The session that has taken it.
    pub claimed_by: String,
    pub claimed_agent: String,
    pub created_at: i64,
    pub updated_at: i64,
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub mtime: i64,
    #[serde(default)]
    pub pending: i64,
    /// Ids of the items that must be finished first.
    pub blocked_by: Vec<String>,
}

impl Item {
    pub fn closed(&self) -> bool {
        matches!(self.status.as_str(), "done" | "dropped")
    }
}

/// Who is acting, for the record.
#[derive(Clone, Copy, Default)]
pub struct By<'a> {
    pub session: &'a str,
    pub agent: &'a str,
    pub message_id: i64,
}

pub struct Root {
    pub dir: PathBuf,
    pub vault: Option<PathBuf>,
}

/// Where the work notes live: beside the memory notes.
pub fn root(cfg: &Config, paths: &Paths) -> Root {
    let vault = cfg.obsidian.vault.trim();
    if !vault.is_empty() && cfg.obsidian.memory_in_vault {
        let vault = expand_tilde(vault);
        let folder = cfg.obsidian.folder.trim().trim_matches('/');
        let folder = if folder.is_empty() { "narsil" } else { folder };
        return Root { dir: vault.join(folder).join("Work"), vault: Some(vault) };
    }
    Root { dir: paths.data_dir.join("work"), vault: None }
}

// ---- the index ---------------------------------------------------------------

const COLS: &str = "id, project, title, status, kind, parent, discovered_from, goal, body, result, log, claimed_by, \
     claimed_agent, created_at, updated_at, path, mtime, pending";

fn item_from(row: &rusqlite::Row) -> rusqlite::Result<Item> {
    Ok(Item {
        id: row.get(0)?,
        project: row.get(1)?,
        title: row.get(2)?,
        status: row.get(3)?,
        kind: row.get(4)?,
        parent: row.get(5)?,
        discovered_from: row.get(6)?,
        goal: row.get(7)?,
        body: row.get(8)?,
        result: row.get(9)?,
        log: row.get(10)?,
        claimed_by: row.get(11)?,
        claimed_agent: row.get(12)?,
        created_at: row.get(13)?,
        updated_at: row.get(14)?,
        path: row.get(15)?,
        mtime: row.get(16)?,
        pending: row.get(17)?,
        blocked_by: Vec::new(),
    })
}

/// Every item (of one project, or of all), with what blocks it. Items waiting
/// to be removed are left out.
pub fn load(store: &Store, project: Option<&str>) -> Result<Vec<Item>> {
    let db = store.conn();
    let mut items: Vec<Item> = {
        let sql = format!("SELECT {COLS} FROM work_items WHERE (?1 IS NULL OR project = ?1) AND pending != 2 ORDER BY created_at, rowid");
        let mut stmt = db.prepare(&sql)?;
        let rows = stmt.query_map(params![project], item_from)?;
        rows.collect::<rusqlite::Result<_>>()?
    };
    let mut stmt = db.prepare("SELECT item, blocked_by FROM work_edges")?;
    let edges: Vec<(String, String)> = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<_>>()?;
    let mut by_item: HashMap<String, Vec<String>> = HashMap::new();
    for (item, blocker) in edges {
        by_item.entry(item).or_default().push(blocker);
    }
    for item in &mut items {
        item.blocked_by = by_item.remove(&item.id).unwrap_or_default();
    }
    Ok(items)
}

fn put(store: &Store, item: &Item) -> Result<()> {
    let mut db = store.conn();
    let tx = db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    tx.execute(
        "INSERT INTO work_items (id, project, title, status, kind, parent, discovered_from, goal, body, result, log, \
         claimed_by, claimed_agent, created_at, updated_at, path, mtime, pending) \
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18) \
         ON CONFLICT(id) DO UPDATE SET project = excluded.project, title = excluded.title, status = excluded.status, \
         kind = excluded.kind, parent = excluded.parent, discovered_from = excluded.discovered_from, goal = excluded.goal, \
         body = excluded.body, result = excluded.result, log = excluded.log, claimed_by = excluded.claimed_by, \
         claimed_agent = excluded.claimed_agent, updated_at = excluded.updated_at, path = excluded.path, \
         mtime = excluded.mtime, pending = excluded.pending",
        params![
            item.id,
            item.project,
            item.title,
            item.status,
            item.kind,
            item.parent,
            item.discovered_from,
            item.goal,
            item.body,
            item.result,
            item.log,
            item.claimed_by,
            item.claimed_agent,
            item.created_at,
            item.updated_at,
            item.path,
            item.mtime,
            item.pending
        ],
    )?;
    tx.execute("DELETE FROM work_edges WHERE item = ?1", params![item.id])?;
    for blocker in &item.blocked_by {
        tx.execute("INSERT OR IGNORE INTO work_edges (item, blocked_by) VALUES (?1,?2)", params![item.id, blocker])?;
    }
    tx.commit()?;
    Ok(())
}

fn remove(store: &Store, id: &str) -> Result<()> {
    let db = store.conn();
    db.execute("DELETE FROM work_edges WHERE item = ?1 OR blocked_by = ?1", params![id])?;
    db.execute("DELETE FROM work_items WHERE id = ?1", params![id])?;
    Ok(())
}

/// Put a line in the item's log and in the record of who did what.
fn record(store: &Store, item: &mut Item, by: By<'_>, what: &str, text: &str) -> Result<()> {
    let who = match (by.agent.is_empty(), by.session.is_empty()) {
        (false, _) => format!(" by subagent {} (session {})", by.agent, by.session),
        (true, false) => format!(" (session {})", by.session),
        (true, true) => String::new(),
    };
    let detail = if text.trim().is_empty() { String::new() } else { format!(": {}", first_line(text, 200)) };
    let line = format!("- {} · {what}{who}{detail}", chrono::Local::now().format("%Y-%m-%d %H:%M"));
    item.log = if item.log.trim().is_empty() { line } else { format!("{}\n{line}", item.log.trim_end()) };
    store.conn().execute(
        "INSERT INTO work_events (item, ts, session, message_id, agent, what, text) VALUES (?1,?2,?3,?4,?5,?6,?7)",
        params![item.id, now(), by.session, by.message_id, by.agent, what, text],
    )?;
    Ok(())
}

/// Finished top-level work in other projects, newest first: (title, project).
/// So an agent starting something can see it has done the like before.
pub fn finished_elsewhere(store: &Store, project: &str, limit: usize) -> Result<Vec<(String, String)>> {
    let db = store.conn();
    let mut stmt = db.prepare(
        "SELECT title, project FROM work_items WHERE parent IS NULL AND status = 'done' AND project != ?1 AND pending != 2 \
         ORDER BY updated_at DESC LIMIT ?2",
    )?;
    let rows = stmt.query_map(params![project, limit as i64], |r| Ok((r.get(0)?, r.get(1)?)))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// For each item, the session that last did something to it.
pub fn last_sessions(store: &Store, project: &str) -> Result<HashMap<String, String>> {
    let db = store.conn();
    let mut stmt = db.prepare(
        "SELECT e.item, e.session FROM work_events e JOIN work_items w ON w.id = e.item \
         WHERE w.project = ?1 AND e.session != '' ORDER BY e.id",
    )?;
    let rows = stmt.query_map(params![project], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
    Ok(rows.collect::<rusqlite::Result<HashMap<_, _>>>()?)
}

/// For each item a session touched, the latest place in its transcript where it did.
pub fn anchors(store: &Store, session: &str) -> Result<HashMap<String, i64>> {
    let db = store.conn();
    let mut stmt = db.prepare("SELECT item, MAX(message_id) FROM work_events WHERE session = ?1 AND message_id > 0 GROUP BY item")?;
    let rows = stmt.query_map(params![session], |r| Ok((r.get(0)?, r.get(1)?)))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

// ---- notes -------------------------------------------------------------------

fn mtime_ns(path: &Path) -> i64 {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos().min(i64::MAX as u128) as i64)
        .unwrap_or(0)
}

fn project_folder(project: &str) -> String {
    file_name(&Path::new(project).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "work".into()))
}

fn title_of<'a>(items: &'a [Item], id: &str) -> Option<&'a str> {
    items.iter().find(|i| i.id == id).map(|i| i.title.as_str())
}

/// Split a note's text into the description and the two sections narsil keeps.
fn sections(body: &str) -> (String, String, String) {
    let mut parts: [Vec<&str>; 3] = [Vec::new(), Vec::new(), Vec::new()];
    let mut at = 0;
    for line in body.lines() {
        match line.trim_end() {
            "## Result" => at = 1,
            "## Log" => at = 2,
            _ => parts[at].push(line),
        }
    }
    let join = |lines: &[&str]| lines.join("\n").trim().to_string();
    (join(&parts[0]), join(&parts[1]), join(&parts[2]))
}

/// Write the item's note, keeping any other properties the file has.
fn write_note(root: &Root, item: &Item, all: &[Item]) -> Result<(String, i64)> {
    let path = match &item.path {
        Some(rel) if root.dir.join(rel).exists() => root.dir.join(rel),
        _ => {
            let folder = root.dir.join(project_folder(&item.project));
            let base = file_name(&item.title);
            let mut path = folder.join(format!("{base}.md"));
            let mut n = 2;
            while path.exists() {
                path = folder.join(format!("{base} ({n}).md"));
                n += 1;
            }
            path
        }
    };
    let dir = path.parent().unwrap_or(Path::new("."));
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let mut note = read_note(&path).map(|t| Note::parse(&t)).unwrap_or_default();
    note.set("id", &item.id);
    note.set("status", &item.status);
    if item.kind == "question" {
        note.set("kind", "question");
    } else {
        note.remove("kind");
    }
    let link = |id: &Option<String>| id.as_deref().and_then(|id| title_of(all, id)).map(str::to_string);
    note.set_links("parent", &link(&item.parent).into_iter().collect::<Vec<_>>());
    let blockers: Vec<String> = item.blocked_by.iter().filter_map(|b| title_of(all, b)).map(str::to_string).collect();
    note.set_links("blocked_by", &blockers);
    note.set_links("discovered_from", &link(&item.discovered_from).into_iter().collect::<Vec<_>>());
    note.set("project", &item.project);
    if item.goal.is_empty() {
        note.remove("goal");
    } else {
        note.set("goal", &item.goal);
    }
    if item.claimed_by.is_empty() {
        note.remove("claimed_by");
    } else if item.claimed_agent.is_empty() {
        note.set("claimed_by", &item.claimed_by);
    } else {
        note.set("claimed_by", &format!("{} ({})", item.claimed_agent, item.claimed_by));
    }
    let stem = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    if stem == item.title {
        note.remove("title");
    } else {
        note.set("title", &item.title);
    }
    note.set("updated", &today());
    let mut body = item.body.trim().to_string();
    if !item.result.trim().is_empty() {
        body.push_str(&format!("\n\n## Result\n\n{}", item.result.trim()));
    }
    if !item.log.trim().is_empty() {
        body.push_str(&format!("\n\n## Log\n\n{}", item.log.trim()));
    }
    note.body = body;
    let tmp = dir.join(format!(".narsil-tmp-{}", new_id()));
    std::fs::write(&tmp, note.render()).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, &path).with_context(|| format!("writing {}", path.display()))?;
    Ok((rel_string(path.strip_prefix(&root.dir).unwrap_or(&path)), mtime_ns(&path)))
}

/// Save a changed item: to its note if the folder is there, otherwise held in the index.
fn save(store: &Store, root: &Root, available: bool, item: &mut Item, all: &[Item]) -> Result<()> {
    item.updated_at = now();
    if available {
        let (rel, mtime) = write_note(root, item, all)?;
        item.path = Some(rel);
        item.mtime = mtime;
        item.pending = 0;
    } else {
        item.pending = 1;
    }
    put(store, item)
}

// ---- keeping the index and the notes in step ---------------------------------

#[derive(Default, Debug)]
pub struct Refresh {
    pub available: bool,
    pub why: String,
    /// Notes read because they were new or edited by hand.
    pub indexed: usize,
    pub removed: usize,
    pub flushed: usize,
    pub problems: Vec<String>,
    /// Projects whose items changed, for telling whoever is watching.
    pub projects: Vec<String>,
}

impl Refresh {
    pub fn changed(&self) -> bool {
        self.indexed + self.removed + self.flushed > 0
    }
}

fn unavailable(why: String) -> Refresh {
    Refresh { available: false, why, ..Default::default() }
}

/// Read by-hand changes to the work notes into the index, and write out
/// anything held while the folder was away.
pub fn refresh(store: &Store, cfg: &Config, paths: &Paths) -> Refresh {
    let _turn = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    refresh_locked(store, &root(cfg, paths), false).unwrap_or_else(|e| unavailable(format!("{e:#}")))
}

fn refresh_locked(store: &Store, root: &Root, force: bool) -> Result<Refresh> {
    let key = root.dir.to_string_lossy().into_owned();
    if store.meta_get(META_ROOT)?.as_deref() != Some(key.as_str()) {
        let can_start = root.vault.as_ref().is_none_or(|v| Vault::present(v));
        if !can_start {
            return Ok(unavailable(format!("{} is not there yet (is the vault's drive mounted?); work is tracked in the index until it appears", root.dir.display())));
        }
        std::fs::create_dir_all(&root.dir).with_context(|| format!("creating {}", root.dir.display()))?;
        std::fs::write(root.dir.join(MARKER), "narsil keeps its work notes in this folder.\n")?;
        // Everything the index holds is written into the new folder.
        store.conn().execute("UPDATE work_items SET path = NULL, mtime = 0, pending = 1 WHERE pending != 2", [])?;
        store.meta_set(META_ROOT, &key)?;
    }
    if !root.dir.join(MARKER).is_file() {
        return Ok(unavailable(format!("{} is missing or has been emptied; work is tracked in the index and written out when it returns", root.dir.display())));
    }
    let mut report = Refresh { available: true, ..Default::default() };
    let mut touched: HashSet<String> = HashSet::new();

    // 1. Changes held while the folder was away.
    let held: Vec<Item> = {
        let db = store.conn();
        let mut stmt = db.prepare(&format!("SELECT {COLS} FROM work_items WHERE pending != 0"))?;
        let rows = stmt.query_map([], item_from)?;
        rows.collect::<rusqlite::Result<_>>()?
    };
    if !held.is_empty() {
        let all = load(store, None)?;
        for h in held {
            if h.pending == 2 {
                if let Some(p) = h.path.as_ref().map(|p| root.dir.join(p)).filter(|p| p.exists()) {
                    let _ = std::fs::remove_file(p);
                }
                remove(store, &h.id)?;
                continue;
            }
            let Some(mut item) = all.iter().find(|i| i.id == h.id).cloned() else { continue };
            // The note changed too while we could not see it. Ours carries the state of
            // the work, so it is the one kept; theirs goes to the vault's trash, not away.
            if let Some(p) = item.path.as_ref().map(|p| root.dir.join(p)).filter(|p| p.exists() && mtime_ns(p) != item.mtime) {
                if let Some(vault) = &root.vault {
                    let _ = crate::obsidian::trash(vault, &p);
                }
                report.problems.push(format!("{}: changed in the note and by narsil while the folder was away; narsil's version was kept", item.title));
            }
            let (rel, mtime) = write_note(root, &item, &all)?;
            item.path = Some(rel);
            item.mtime = mtime;
            item.pending = 0;
            put(store, &item)?;
            touched.insert(item.project.clone());
            report.flushed += 1;
        }
    }

    // 2. Notes that are new or were edited by hand.
    let recently = LAST_SCAN.lock().unwrap_or_else(|e| e.into_inner()).is_some_and(|t| t.elapsed().as_millis() < 1500);
    if recently && !force && report.flushed == 0 {
        return Ok(report);
    }
    let before = load(store, None)?;
    let known_projects: Vec<String> = {
        let mut p: Vec<String> = before.iter().map(|i| i.project.clone()).collect();
        p.sort();
        p.dedup();
        p
    };
    struct Read {
        item: Item,
        parent: Option<String>,
        blockers: Vec<String>,
        from: Option<String>,
        stamp: bool,
    }
    let mut seen_paths: HashSet<String> = HashSet::new();
    let mut read: Vec<Read> = Vec::new();
    for file in markdown_files(&root.dir) {
        let inside = file.strip_prefix(&root.dir).unwrap_or(&file).to_path_buf();
        let rel = rel_string(&inside);
        let mtime = mtime_ns(&file);
        if before.iter().any(|i| i.path.as_deref() == Some(rel.as_str()) && i.mtime == mtime) {
            seen_paths.insert(rel);
            continue;
        }
        let Some(text) = read_note(&file) else { continue };
        let note = Note::parse(&text);
        let stem = file.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        let old = note.get("id").and_then(|id| before.iter().find(|i| i.id == id));
        let project = note.get("project").or_else(|| old.map(|o| o.project.clone())).or_else(|| {
            // Put by hand in a project's folder: it belongs to that project.
            let folder = inside.components().next().map(|c| c.as_os_str().to_string_lossy().into_owned())?;
            known_projects.iter().find(|p| project_folder(p) == folder).cloned()
        });
        let Some(project) = project else {
            report.problems.push(format!("{rel}: not tracked, because nothing says which project it belongs to. Add a property `project: /path/to/project`"));
            continue;
        };
        let (body, result, log) = sections(&note.body);
        let status = note.get("status").unwrap_or_else(|| "open".into());
        if !STATUSES.contains(&status.as_str()) {
            report.problems.push(format!("{rel}: status {status:?} is not one of {}; the note was left out until that is fixed", STATUSES.join(", ")));
            seen_paths.insert(rel);
            continue;
        }
        let title = match note.get("title") {
            Some(t) if file_name(&t) == stem => t,
            _ => stem,
        };
        let mut item = old.cloned().unwrap_or_else(|| Item { id: format!("w-{}", &new_id()[..5]), created_at: now(), ..Default::default() });
        let claimed = note.get("claimed_by").unwrap_or_default();
        if status != "in_progress" || claimed.is_empty() {
            item.claimed_by.clear();
            item.claimed_agent.clear();
        }
        item.project = project;
        item.title = title;
        item.status = status;
        item.kind = if note.get("kind").as_deref() == Some("question") { "question".into() } else { "task".into() };
        item.goal = note.get("goal").unwrap_or_default();
        item.body = body;
        item.result = result;
        item.log = log;
        item.path = Some(rel.clone());
        item.mtime = mtime;
        item.pending = 0;
        item.updated_at = mtime / 1_000_000_000;
        read.push(Read {
            item,
            parent: note.get_links("parent").into_iter().next(),
            blockers: note.get_links("blocked_by"),
            from: note.get_links("discovered_from").into_iter().next(),
            stamp: note.get("id").is_none(),
        });
        seen_paths.insert(rel);
    }
    *LAST_SCAN.lock().unwrap_or_else(|e| e.into_inner()) = Some(Instant::now());

    // Links are by title; turn them into ids now that every note has been read.
    let mut names: HashMap<(String, String), String> =
        before.iter().map(|i| ((i.project.clone(), i.title.to_lowercase()), i.id.clone())).collect();
    for r in &read {
        names.insert((r.item.project.clone(), r.item.title.to_lowercase()), r.item.id.clone());
    }
    let mut stamps: Vec<String> = Vec::new();
    for mut r in read {
        let project = r.item.project.clone();
        let mut resolve = |title: &str| {
            let found = names.get(&(project.clone(), title.to_lowercase())).cloned();
            if found.is_none() {
                report.problems.push(format!("{}: links to {title:?}, which is not a work item in this project; the link is ignored", r.item.title));
            }
            found
        };
        r.item.parent = r.parent.as_deref().and_then(&mut resolve);
        r.item.discovered_from = r.from.as_deref().and_then(&mut resolve);
        r.item.blocked_by = r.blockers.iter().filter_map(|b| resolve(b)).filter(|b| *b != r.item.id).collect();
        let old = before.iter().find(|i| i.id == r.item.id);
        if let Some(old) = old.filter(|o| o.status != r.item.status) {
            // A status changed by hand is a decision by the user; put it on record.
            let what = format!("{} (was {})", r.item.status.replace('_', " "), old.status.replace('_', " "));
            record(store, &mut r.item, By::default(), &what, "edited in the note")?;
            r.stamp = true;
        }
        put(store, &r.item)?;
        if r.stamp {
            stamps.push(r.item.id.clone());
        }
        touched.insert(project);
        report.indexed += 1;
    }

    // 3. A note that is gone was deleted by hand: the item goes too.
    for i in load(store, None)? {
        if i.pending == 0 && i.path.as_ref().is_some_and(|p| !seen_paths.contains(p)) {
            remove(store, &i.id)?;
            touched.insert(i.project);
            report.removed += 1;
        }
    }
    // Notes that needed an id, or a line in their log, are written back.
    if !stamps.is_empty() {
        let all = load(store, None)?;
        for id in stamps {
            if let Some(mut item) = all.iter().find(|i| i.id == id).cloned() {
                let (rel, mtime) = write_note(root, &item, &all)?;
                item.path = Some(rel);
                item.mtime = mtime;
                put(store, &item)?;
            }
        }
    }
    report.projects = touched.into_iter().collect();
    Ok(report)
}

// ---- reading the graph -------------------------------------------------------

/// Where a project's work stands.
pub struct Overview {
    pub items: Vec<Item>,
}

impl Overview {
    pub fn of(store: &Store, project: &str) -> Result<Overview> {
        Ok(Overview { items: load(store, Some(project))? })
    }

    pub fn get(&self, id: &str) -> Option<&Item> {
        self.items.iter().find(|i| i.id == id)
    }

    /// Find an item by its title (any capitalisation, with or without `[[ ]]`) or its id.
    pub fn find(&self, name: &str) -> Option<&Item> {
        let name = name.trim().trim_start_matches("[[").trim_end_matches("]]").trim();
        let lower = name.to_lowercase();
        self.items.iter().find(|i| i.id == name).or_else(|| self.items.iter().find(|i| i.title.to_lowercase() == lower))
    }

    pub fn children(&self, id: &str) -> Vec<&Item> {
        self.items.iter().filter(|i| i.parent.as_deref() == Some(id)).collect()
    }

    /// What still stands in an item's way.
    pub fn open_blockers(&self, item: &Item) -> Vec<&Item> {
        item.blocked_by.iter().filter_map(|b| self.get(b)).filter(|b| !b.closed()).collect()
    }

    /// Open, with nothing in the way, and not a container for other items.
    pub fn is_ready(&self, item: &Item) -> bool {
        item.status == "open"
            && item.kind != "question"
            && self.open_blockers(item).is_empty()
            && self.children(&item.id).iter().all(|c| c.status == "dropped")
            && item.parent.as_deref().and_then(|p| self.get(p)).is_none_or(|p| !p.closed())
    }

    pub fn ready(&self) -> Vec<&Item> {
        self.items.iter().filter(|i| self.is_ready(i)).collect()
    }

    /// The top-level item an item belongs to.
    pub fn root_of<'a>(&'a self, item: &'a Item) -> &'a Item {
        let mut at = item;
        let mut hops = 0;
        while let Some(up) = at.parent.as_deref().and_then(|p| self.get(p)).filter(|_| hops < 32) {
            at = up;
            hops += 1;
        }
        at
    }

    /// Everything under an item, at any depth.
    pub fn descendants(&self, id: &str) -> Vec<&Item> {
        let mut out = Vec::new();
        let mut queue = vec![id.to_string()];
        while let Some(next) = queue.pop() {
            for c in self.children(&next) {
                if out.len() < 10_000 && !out.iter().any(|o: &&Item| o.id == c.id) {
                    out.push(c);
                    queue.push(c.id.clone());
                }
            }
        }
        out
    }

    /// The state of play as the agent reads it. `running` says whether a
    /// session is alive, to tell work in hand from work that was abandoned.
    pub fn text(&self, me: &str, running: &dyn Fn(&str) -> bool) -> String {
        if self.items.is_empty() {
            return "No work is laid out for this project yet. Use work_plan to lay it out.".to_string();
        }
        let mut out = String::new();
        let roots: Vec<&Item> = self.items.iter().filter(|i| i.parent.is_none() && !self.children(&i.id).is_empty()).collect();
        for r in &roots {
            let parts = self.descendants(&r.id);
            let done = parts.iter().filter(|p| p.status == "done").count();
            let live = parts.iter().filter(|p| p.status != "dropped").count();
            out.push_str(&format!("{}: {done} of {live} done{}\n", r.title, if r.closed() { format!(" ({})", r.status) } else { String::new() }));
        }
        let open: Vec<&Item> = self.items.iter().filter(|i| !self.root_of(i).closed() || !i.closed()).collect();
        let mut section = |title: &str, items: Vec<String>| {
            if !items.is_empty() {
                out.push_str(&format!("\n{title} ({}):\n", items.len()));
                for line in items {
                    out.push_str(&format!("- {line}\n"));
                }
            }
        };
        section("Ready now", open.iter().filter(|i| self.is_ready(i)).map(|i| i.title.clone()).collect());
        section(
            "In progress",
            open.iter()
                .filter(|i| i.status == "in_progress")
                .map(|i| {
                    let who = match (i.claimed_by.as_str(), i.claimed_agent.as_str()) {
                        (s, _) if s == me => "yours".to_string(),
                        (s, _) if !s.is_empty() && !running(s) => "taken by a session that is no longer running; work_next can take it over".to_string(),
                        (_, agent) if !agent.is_empty() => format!("subagent {agent}"),
                        (s, _) => format!("session {s}"),
                    };
                    format!("{} ({who})", i.title)
                })
                .collect(),
        );
        section(
            "Waiting on the user",
            open.iter().filter(|i| i.status == "waiting" || (i.kind == "question" && i.status == "open")).map(|i| i.title.clone()).collect(),
        );
        section(
            "Blocked",
            open.iter()
                .filter(|i| i.status == "open" && i.kind != "question" && !self.open_blockers(i).is_empty())
                .map(|i| format!("{}: after {}", i.title, self.open_blockers(i).iter().map(|b| b.title.as_str()).collect::<Vec<_>>().join(", ")))
                .collect(),
        );
        out.trim_end().to_string()
    }

    /// An item in full, whatever its state: for reading work that is finished.
    pub fn show(&self, item: &Item) -> String {
        let mut out = format!("[{}] {}", item.status.replace('_', " "), self.bundle(item));
        if !item.result.trim().is_empty() {
            out.push_str(&format!("\nResult:\n{}\n", item.result.trim()));
        }
        let parts = self.children(&item.id);
        if !parts.is_empty() {
            out.push_str("\nIts parts (show any of them the same way):\n");
            for p in parts {
                let result = if p.result.trim().is_empty() { String::new() } else { format!(": {}", first_line(&p.result, 160)) };
                out.push_str(&format!("- [{}] {}{result}\n", p.status.replace('_', " "), p.title));
            }
        }
        out
    }

    /// One line for prompts and continuation notes.
    pub fn brief(&self) -> String {
        let live: Vec<&Item> = self.items.iter().filter(|i| !self.root_of(i).closed()).collect();
        if live.is_empty() {
            return String::new();
        }
        let count = |f: &dyn Fn(&Item) -> bool| live.iter().filter(|i| f(i)).count();
        format!(
            "{} ready, {} in progress, {} waiting on the user, {} blocked, {} done",
            count(&|i| self.is_ready(i)),
            count(&|i| i.status == "in_progress"),
            count(&|i| i.status == "waiting" || (i.kind == "question" && i.status == "open")),
            count(&|i| i.status == "open" && i.kind != "question" && !self.open_blockers(i).is_empty()),
            count(&|i| i.status == "done" && self.children(&i.id).is_empty())
        )
    }

    /// What an agent needs to start on an item without reading how the rest
    /// was done: the item, why it exists, and what the work it rests on produced.
    pub fn bundle(&self, item: &Item) -> String {
        let mut out = format!("# {}\n", item.title);
        if !item.body.trim().is_empty() {
            out.push_str(&format!("\n{}\n", item.body.trim()));
        }
        if let Some(parent) = item.parent.as_deref().and_then(|p| self.get(p)) {
            out.push_str(&format!("\nPart of: {}", parent.title));
            if !parent.body.trim().is_empty() {
                out.push_str(&format!(" ({})", first_line(&parent.body, 200)));
            }
            out.push('\n');
        }
        if let Some(from) = item.discovered_from.as_deref().and_then(|p| self.get(p)) {
            out.push_str(&format!("Found while working on: {}\n", from.title));
        }
        let inputs: Vec<&Item> = item.blocked_by.iter().filter_map(|b| self.get(b)).collect();
        if !inputs.is_empty() {
            out.push_str("\nIt rests on:\n");
            for b in inputs {
                let result = if b.result.trim().is_empty() { format!("({})", b.status.replace('_', " ")) } else { b.result.trim().to_string() };
                // Enough to know what was done; the item's own note has the whole of it.
                out.push_str(&format!("- {}: {}\n", b.title, crate::util::truncate_middle(&result, 500)));
            }
        }
        if !item.log.trim().is_empty() {
            let recent: Vec<&str> = item.log.lines().rev().take(6).collect();
            out.push_str(&format!("\nIts log so far:\n{}\n", recent.into_iter().rev().collect::<Vec<_>>().join("\n")));
        }
        out
    }
}

// ---- changing the graph ------------------------------------------------------

/// An item to lay out.
#[derive(Default, Clone)]
pub struct Spec {
    pub title: String,
    pub description: String,
    pub done_when: Vec<String>,
    pub blocked_by: Vec<String>,
    pub parent: Option<String>,
    pub question: bool,
    pub discovered_from: Option<String>,
}

/// The pieces every change needs: the store, where the notes are, and whether they can be written.
pub struct Graph<'a> {
    store: &'a Store,
    root: Root,
    available: bool,
    _turn: std::sync::MutexGuard<'static, ()>,
}

impl<'a> Graph<'a> {
    /// Take the graph for a change. Reads by-hand edits first, so the change is made to the current state.
    pub fn open(store: &'a Store, cfg: &Config, paths: &Paths) -> Graph<'a> {
        let turn = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let root = root(cfg, paths);
        let available = refresh_locked(store, &root, false).map(|r| r.available).unwrap_or(false);
        Graph { store, root, available, _turn: turn }
    }

    fn write(&self, item: &mut Item) -> Result<()> {
        // A work note is kept, and read by whoever picks the work up: no credentials in it.
        for text in [&mut item.body, &mut item.result, &mut item.log] {
            if !crate::secrets::kinds(text).is_empty() {
                *text = crate::secrets::kept(text);
            }
        }
        let all = load(self.store, Some(&item.project))?;
        save(self.store, &self.root, self.available, item, &all)
    }

    /// Create or update items, with what blocks what. Titles identify items
    /// within a project, so laying out the same plan twice changes nothing.
    pub fn plan(&self, by: By<'_>, project: &str, goal: &str, root_title: Option<&str>, specs: &[Spec]) -> Result<Vec<String>> {
        let mut view = Overview::of(self.store, project)?;
        let mut created: Vec<String> = Vec::new();
        let ensure = |view: &mut Overview, title: &str, spec: Option<&Spec>, created: &mut Vec<String>| -> Result<String> {
            let title = title.trim().trim_start_matches("[[").trim_end_matches("]]").trim();
            if title.is_empty() {
                bail!("a work item needs a title");
            }
            if let Some(found) = view.find(title) {
                return Ok(found.id.clone());
            }
            let question = spec.is_some_and(|s| s.question);
            let item = Item {
                id: format!("w-{}", &new_id()[..5]),
                project: project.to_string(),
                title: title.to_string(),
                status: if question { "waiting".into() } else { "open".into() },
                kind: if question { "question".into() } else { "task".into() },
                created_at: now(),
                updated_at: now(),
                ..Default::default()
            };
            created.push(item.id.clone());
            let id = item.id.clone();
            view.items.push(item);
            Ok(id)
        };
        let root_id = match root_title {
            Some(title) => Some(ensure(&mut view, title, None, &mut created)?),
            None => None,
        };
        let mut touched: Vec<String> = root_id.iter().cloned().collect();
        for spec in specs {
            // A parent that is not an item yet is a grouping: it becomes one.
            if let Some(parent) = spec.parent.as_deref().filter(|p| !specs.iter().any(|s| s.title.eq_ignore_ascii_case(p.trim()))) {
                let existed = view.find(parent).is_some();
                let id = ensure(&mut view, parent, None, &mut created)?;
                if !existed {
                    if let Some(group) = view.items.iter_mut().find(|i| i.id == id) {
                        group.parent = root_id.clone().filter(|r| *r != id);
                    }
                    touched.push(id);
                }
            }
            let id = ensure(&mut view, &spec.title, Some(spec), &mut created)?;
            touched.push(id);
        }
        // Second pass, now that every title in the plan exists.
        for spec in specs {
            let id = view.find(&spec.title).map(|i| i.id.clone()).ok_or_else(|| anyhow!("lost track of {:?}", spec.title))?;
            let mut blockers = Vec::new();
            for b in &spec.blocked_by {
                let found = view.find(b).ok_or_else(|| anyhow!("{:?} is blocked by {b:?}, which is not an item in this plan or this project", spec.title))?;
                if found.id == id {
                    bail!("{:?} cannot be blocked by itself", spec.title);
                }
                blockers.push(found.id.clone());
            }
            let parent = match &spec.parent {
                Some(p) => Some(view.find(p).ok_or_else(|| anyhow!("{:?} names the parent {p:?}, which is not an item here", spec.title))?.id.clone()),
                None => root_id.clone().filter(|r| *r != id),
            };
            let from = match &spec.discovered_from {
                Some(f) => view.find(f).map(|i| i.id.clone()),
                None => None,
            };
            let item = view.items.iter_mut().find(|i| i.id == id).expect("just found");
            if !spec.description.trim().is_empty() || !spec.done_when.is_empty() {
                let mut body = spec.description.trim().to_string();
                if !spec.done_when.is_empty() {
                    body.push_str(&format!("\n\n## Done when\n{}", spec.done_when.iter().map(|d| format!("- {}", d.trim())).collect::<Vec<_>>().join("\n")));
                }
                item.body = body.trim().to_string();
            }
            for b in blockers {
                if !item.blocked_by.contains(&b) {
                    item.blocked_by.push(b);
                }
            }
            if item.parent.is_none() || spec.parent.is_some() {
                item.parent = parent.filter(|p| *p != item.id);
            }
            if from.is_some() {
                item.discovered_from = from;
            }
        }
        if let Some(root) = root_id.as_ref().and_then(|r| view.items.iter_mut().find(|i| i.id == *r)) {
            if !goal.is_empty() {
                root.goal = goal.to_string();
            }
        }
        if let Some(cycle) = find_cycle(&view.items) {
            bail!("that would make a circle of items waiting on each other: {}", cycle.join(" -> "));
        }
        // Save every touched item, twice over for links: a note names its blockers by title, so those must exist first.
        // In the order given, which is the order they are shown in.
        let mut seen = HashSet::new();
        touched.retain(|id| seen.insert(id.clone()));
        for id in &touched {
            let Some(mut item) = view.items.iter().find(|i| i.id == *id).cloned() else { continue };
            if created.contains(id) {
                let what = if item.kind == "question" { "asked" } else { "created" };
                record(self.store, &mut item, by, what, "")?;
            }
            put(self.store, &item)?;
            if let Some(slot) = view.items.iter_mut().find(|i| i.id == *id) {
                *slot = item;
            }
        }
        for id in &touched {
            if let Some(mut item) = view.items.iter().find(|i| i.id == *id).cloned() {
                save(self.store, &self.root, self.available, &mut item, &view.items)?;
            }
        }
        Ok(touched.iter().filter_map(|id| view.get(id)).map(|i| i.title.clone()).collect())
    }

    /// Take an item. Fails if someone else has it and is still at work.
    pub fn claim(&self, by: By<'_>, project: &str, name: &str, running: &dyn Fn(&str) -> bool) -> Result<Item> {
        let view = Overview::of(self.store, project)?;
        let found = view.find(name).ok_or_else(|| anyhow!("there is no work item called {name:?} in this project"))?;
        let mut item = found.clone();
        match item.status.as_str() {
            "in_progress" if item.claimed_by == by.session => return Ok(item),
            "in_progress" if !item.claimed_by.is_empty() && running(&item.claimed_by) => {
                let who = if item.claimed_agent.is_empty() { format!("session {}", item.claimed_by) } else { format!("subagent {}", item.claimed_agent) };
                bail!("{:?} is already being worked on by {who}; pick another item", item.title)
            }
            "in_progress" => {}
            "open" if !view.open_blockers(&item).is_empty() => bail!(
                "{:?} cannot be started yet: it waits on {}",
                item.title,
                view.open_blockers(&item).iter().map(|b| format!("{:?}", b.title)).collect::<Vec<_>>().join(", ")
            ),
            "open" if !view.children(&item.id).iter().all(|c| c.status == "dropped") => {
                bail!("{:?} is made of other items; take one of those instead", item.title)
            }
            "open" => {}
            "waiting" => bail!("{:?} is waiting on the user", item.title),
            other => bail!("{:?} is already {other}", item.title),
        }
        let taken_over = item.status == "in_progress";
        item.status = "in_progress".into();
        item.claimed_by = by.session.to_string();
        item.claimed_agent = by.agent.to_string();
        record(self.store, &mut item, by, if taken_over { "taken over" } else { "started" }, "")?;
        self.write(&mut item)?;
        Ok(item)
    }

    /// Close an item with what was done and how that was checked. Returns the
    /// titles of the items this made ready.
    pub fn done(&self, by: By<'_>, project: &str, name: &str, result: &str, evidence: &str, running: &dyn Fn(&str) -> bool) -> Result<Vec<String>> {
        let before = Overview::of(self.store, project)?;
        let found = before.find(name).ok_or_else(|| anyhow!("there is no work item called {name:?} in this project"))?;
        if found.status == "done" {
            bail!("{:?} is already done", found.title);
        }
        if found.status == "in_progress" && !found.claimed_by.is_empty() && found.claimed_by != by.session && running(&found.claimed_by) {
            bail!("{:?} is held by another session that is still at work; it is theirs to close", found.title);
        }
        let open_parts: Vec<&str> = before.children(&found.id).iter().filter(|c| !c.closed()).map(|c| c.title.as_str()).collect();
        if !open_parts.is_empty() {
            bail!("{:?} still has open parts: {}", found.title, open_parts.join(", "));
        }
        let was_ready: HashSet<String> = before.ready().iter().map(|i| i.id.clone()).collect();
        let mut item = found.clone();
        item.status = "done".into();
        item.claimed_by.clear();
        item.claimed_agent.clear();
        item.result = match (result.trim(), evidence.trim()) {
            (r, "") => r.to_string(),
            (r, e) => format!("{r}\n\nEvidence: {e}"),
        };
        let what = if item.kind == "question" { "answered" } else { "done" };
        record(self.store, &mut item, by, what, result)?;
        self.write(&mut item)?;
        // An item whose every part is finished is finished.
        let mut at = item.parent.clone();
        while let Some(pid) = at {
            let view = Overview::of(self.store, project)?;
            let Some(parent) = view.get(&pid).filter(|p| !p.closed()) else { break };
            let parts = view.children(&pid);
            if parts.is_empty() || !parts.iter().all(|c| c.closed()) || !parts.iter().any(|c| c.status == "done") {
                break;
            }
            let mut parent = parent.clone();
            parent.status = "done".into();
            parent.result = format!("All {} parts are done.", parts.iter().filter(|c| c.status == "done").count());
            record(self.store, &mut parent, by, "done", "every part is done")?;
            at = parent.parent.clone();
            self.write(&mut parent)?;
        }
        let after = Overview::of(self.store, project)?;
        Ok(after.ready().iter().filter(|i| !was_ready.contains(&i.id)).map(|i| i.title.clone()).collect())
    }

    /// Change an item's status (`open`, `waiting`, `dropped`), add a note to its log, or add blockers.
    pub fn update(&self, by: By<'_>, project: &str, name: &str, status: Option<&str>, note: Option<&str>, blockers: &[String]) -> Result<Item> {
        let view = Overview::of(self.store, project)?;
        let mut item = view.find(name).ok_or_else(|| anyhow!("there is no work item called {name:?} in this project"))?.clone();
        for b in blockers {
            let found = view.find(b).ok_or_else(|| anyhow!("{b:?} is not a work item in this project"))?;
            if found.id != item.id && !item.blocked_by.contains(&found.id) {
                item.blocked_by.push(found.id.clone());
            }
        }
        let mut probe = view.items.clone();
        if let Some(slot) = probe.iter_mut().find(|i| i.id == item.id) {
            slot.blocked_by = item.blocked_by.clone();
        }
        if let Some(cycle) = find_cycle(&probe) {
            bail!("that would make a circle of items waiting on each other: {}", cycle.join(" -> "));
        }
        if let Some(status) = status {
            match status {
                "open" | "waiting" | "dropped" => {}
                "done" => bail!("close an item with work_done, giving its result and evidence"),
                "in_progress" => bail!("take an item with work_next"),
                other => bail!("unknown status {other:?}; use open, waiting or dropped"),
            }
            if item.status != status {
                let what = match (status, item.status.as_str()) {
                    ("open", "done" | "dropped") => "reopened",
                    ("open", _) => "released",
                    ("waiting", _) => "waiting on the user",
                    _ => "dropped",
                };
                item.status = status.to_string();
                item.claimed_by.clear();
                item.claimed_agent.clear();
                record(self.store, &mut item, by, what, note.unwrap_or(""))?;
            } else if let Some(text) = note.filter(|t| !t.trim().is_empty()) {
                record(self.store, &mut item, by, "note", text)?;
            }
        } else if let Some(text) = note.filter(|t| !t.trim().is_empty()) {
            record(self.store, &mut item, by, "note", text)?;
        }
        self.write(&mut item)?;
        Ok(item)
    }
}

/// A chain of items each blocked by the next and ending where it began, if there is one.
fn find_cycle(items: &[Item]) -> Option<Vec<String>> {
    let by_id: HashMap<&str, &Item> = items.iter().map(|i| (i.id.as_str(), i)).collect();
    fn visit<'a>(id: &'a str, by_id: &HashMap<&'a str, &'a Item>, path: &mut Vec<&'a str>, clear: &mut HashSet<&'a str>) -> Option<Vec<String>> {
        if clear.contains(id) {
            return None;
        }
        if let Some(at) = path.iter().position(|p| *p == id) {
            let mut names: Vec<String> = path[at..].iter().map(|p| by_id.get(p).map(|i| i.title.clone()).unwrap_or_default()).collect();
            names.push(names[0].clone());
            return Some(names);
        }
        path.push(id);
        if let Some(item) = by_id.get(id) {
            for b in &item.blocked_by {
                if let Some(found) = visit(b.as_str(), by_id, path, clear) {
                    return Some(found);
                }
            }
        }
        path.pop();
        clear.insert(id);
        None
    }
    let mut clear = HashSet::new();
    items.iter().find_map(|i| visit(i.id.as_str(), &by_id, &mut Vec::new(), &mut clear))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Sandbox {
        dir: PathBuf,
        cfg: Config,
        paths: Paths,
        store: Store,
    }

    const PROJECT: &str = "/home/me/Code/personal-site";

    impl Sandbox {
        fn new(tag: &str) -> Sandbox {
            let dir = std::env::temp_dir().join(format!("narsil-work-{tag}-{}-{}", std::process::id(), new_id()));
            std::fs::create_dir_all(&dir).unwrap();
            let paths = Paths { config_dir: dir.join("config"), data_dir: dir.join("data"), state_dir: dir.join("state"), runtime_dir: dir.join("run") };
            Sandbox { dir, cfg: Config::default(), paths, store: Store::open_memory().unwrap() }
        }
        fn graph(&self) -> Graph<'_> {
            Graph::open(&self.store, &self.cfg, &self.paths)
        }
        fn view(&self) -> Overview {
            Overview::of(&self.store, PROJECT).unwrap()
        }
        fn ready(&self) -> Vec<String> {
            let mut r: Vec<String> = self.view().ready().iter().map(|i| i.title.clone()).collect();
            r.sort();
            r
        }
        fn note(&self, title: &str) -> PathBuf {
            root(&self.cfg, &self.paths).dir.join("personal-site").join(format!("{title}.md"))
        }
        fn rescan(&self) -> Refresh {
            let _turn = LOCK.lock().unwrap_or_else(|e| e.into_inner());
            refresh_locked(&self.store, &root(&self.cfg, &self.paths), true).unwrap()
        }
        fn website(&self) {
            let spec = |title: &str, blocked_by: &[&str]| Spec { title: title.into(), blocked_by: blocked_by.iter().map(|b| b.to_string()).collect(), ..Default::default() };
            let mut plan = vec![
                spec("Employment history gathered", &[]),
                spec("Site scaffold builds locally", &[]),
                spec("Work page lists every employer", &["Employment history gathered", "Site scaffold builds locally"]),
                spec("Site checked", &["Work page lists every employer"]),
                spec("Deployed", &["Site checked", "Which domain and host?"]),
                spec("Which domain and host?", &[]),
            ];
            plan[2].description = "Every employer appears on /work.".into();
            plan[2].done_when = vec!["The build passes".into()];
            plan[5].question = true;
            self.graph().plan(By { session: "s1", agent: "", message_id: 3 }, PROJECT, "g1", Some("Personal website live"), &plan).unwrap();
        }
    }

    impl Drop for Sandbox {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    const ALIVE: &dyn Fn(&str) -> bool = &|_| true;
    const DEAD: &dyn Fn(&str) -> bool = &|_| false;

    #[test]
    fn a_plan_becomes_notes_and_ready_follows_the_dependencies() {
        let s = Sandbox::new("plan");
        s.website();
        assert_eq!(s.ready(), vec!["Employment history gathered", "Site scaffold builds locally"]);
        let note = std::fs::read_to_string(s.note("Work page lists every employer")).unwrap();
        for expected in [
            "status: open",
            "parent: [\"[[Personal website live]]\"]",
            "blocked_by: [\"[[Employment history gathered]]\", \"[[Site scaffold builds locally]]\"]",
            "project: /home/me/Code/personal-site",
            "Every employer appears on /work.",
            "## Done when\n- The build passes",
            "## Log\n\n- ",
            "· created (session s1)",
        ] {
            assert!(note.contains(expected), "missing {expected:?} in:\n{note}");
        }
        let root_note = std::fs::read_to_string(s.note("Personal website live")).unwrap();
        assert!(root_note.contains("goal: g1"));
        assert!(std::fs::read_to_string(s.note("Which domain and host-")).unwrap().contains("status: waiting"));
        // Laying the same plan out again changes nothing.
        let before = s.view().items.len();
        s.website();
        assert_eq!(s.view().items.len(), before);
        // Naming a parent that is not an item makes it one, under the root.
        let grouped = Spec { title: "Blog index page".into(), parent: Some("Blog".into()), ..Default::default() };
        s.graph().plan(By::default(), PROJECT, "", Some("Personal website live"), &[grouped]).unwrap();
        let view = s.view();
        let blog = view.find("Blog").unwrap();
        assert_eq!(view.get(blog.parent.as_deref().unwrap()).unwrap().title, "Personal website live");
        assert_eq!(view.find("Blog index page").unwrap().parent.as_deref(), Some(blog.id.as_str()));
        let g = s.graph();
        g.update(By::default(), PROJECT, "Blog index page", Some("dropped"), None, &[]).unwrap();
        g.update(By::default(), PROJECT, "Blog", Some("dropped"), None, &[]).unwrap();
        drop(g);
        let text = s.view().text("s1", ALIVE);
        assert!(text.starts_with("Personal website live: 0 of 6 done"), "{text}");
        assert!(text.contains("Ready now (2):") && text.contains("Waiting on the user (1):\n- Which domain and host?"), "{text}");
        assert!(text.contains("- Deployed: after Site checked, Which domain and host?"), "{text}");
        assert_eq!(s.view().brief(), "2 ready, 0 in progress, 1 waiting on the user, 3 blocked, 0 done");
    }

    #[test]
    fn items_are_taken_closed_with_evidence_and_unblock_what_waited() {
        let s = Sandbox::new("flow");
        s.website();
        let g = s.graph();
        let a = By { session: "sa", agent: "history", message_id: 7 };
        let b = By { session: "sb", agent: "scaffold", message_id: 9 };
        g.claim(a, PROJECT, "employment history gathered", ALIVE).unwrap();
        // Nobody else can take it while its holder is at work, and what is blocked cannot be started.
        assert!(g.claim(b, PROJECT, "Employment history gathered", ALIVE).unwrap_err().to_string().contains("subagent history"));
        assert!(g.claim(b, PROJECT, "Deployed", ALIVE).unwrap_err().to_string().contains("waits on"));
        assert!(g.claim(b, PROJECT, "Personal website live", ALIVE).unwrap_err().to_string().contains("made of other items"));
        g.claim(b, PROJECT, "Site scaffold builds locally", ALIVE).unwrap();
        assert!(g.done(b, PROJECT, "Employment history gathered", "x", "y", ALIVE).unwrap_err().to_string().contains("theirs to close"));
        assert_eq!(g.done(a, PROJECT, "Employment history gathered", "7 employers in data/work.yaml", "counted the entries", ALIVE).unwrap(), Vec::<String>::new());
        let opened = g.done(b, PROJECT, "Site scaffold builds locally", "Astro scaffold", "npm run build passes", ALIVE).unwrap();
        assert_eq!(opened, vec!["Work page lists every employer"], "closing the last blocker makes the next item ready");
        drop(g);
        let note = std::fs::read_to_string(s.note("Employment history gathered")).unwrap();
        assert!(note.contains("status: done") && note.contains("## Result\n\n7 employers in data/work.yaml\n\nEvidence: counted the entries"), "{note}");
        assert!(note.contains("· started by subagent history (session sa)") && !note.contains("claimed_by"), "{note}");

        // The next item's bundle carries what it rests on, so nobody has to re-read how that was done.
        let view = s.view();
        let bundle = view.bundle(view.find("Work page lists every employer").unwrap());
        assert!(bundle.contains("Part of: Personal website live"), "{bundle}");
        assert!(bundle.contains("- Employment history gathered: 7 employers in data/work.yaml"), "{bundle}");
        assert_eq!(anchors(&s.store, "sa").unwrap().get(&view.find("Employment history gathered").unwrap().id), Some(&7));

        // A holder that is gone can be taken over; a note in the log; dropping; and the root closes with its last part.
        let g = s.graph();
        g.claim(a, PROJECT, "Work page lists every employer", ALIVE).unwrap();
        let taken = g.claim(b, PROJECT, "Work page lists every employer", DEAD).unwrap();
        assert_eq!(taken.claimed_by, "sb");
        g.update(b, PROJECT, "Work page lists every employer", None, Some("dates for one employer look wrong"), &[]).unwrap();
        g.done(b, PROJECT, "Work page lists every employer", "page built", "7 entries render", ALIVE).unwrap();
        g.done(b, PROJECT, "Site checked", "links fine", "ran the checker", ALIVE).unwrap();
        assert!(g.update(b, PROJECT, "Deployed", Some("done"), None, &[]).is_err(), "closing needs a result");
        assert!(g.update(b, PROJECT, "Site checked", None, None, &["Deployed".into()]).unwrap_err().to_string().contains("circle"));
        g.done(b, PROJECT, "Which domain and host?", "example.dev on Netlify", "", ALIVE).unwrap();
        assert_eq!(s.ready(), vec!["Deployed"]);
        g.update(b, PROJECT, "Deployed", Some("dropped"), Some("the user will deploy by hand"), &[]).unwrap();
        drop(g);
        let view = s.view();
        assert_eq!(view.find("Personal website live").unwrap().status, "open", "dropping the last part does not finish the whole");
        let g = s.graph();
        g.update(b, PROJECT, "Deployed", Some("open"), None, &[]).unwrap();
        g.claim(b, PROJECT, "Deployed", ALIVE).unwrap();
        g.done(b, PROJECT, "Deployed", "live", "curl returns 200", ALIVE).unwrap();
        drop(g);
        let root_item = s.view().find("Personal website live").unwrap().clone();
        assert_eq!((root_item.status.as_str(), root_item.result.as_str()), ("done", "All 6 parts are done."));
    }

    #[test]
    fn the_notes_can_be_edited_by_hand() {
        let s = Sandbox::new("hand");
        s.website();
        // Answer the question by editing its note.
        let q = s.note("Which domain and host-");
        let text = std::fs::read_to_string(&q).unwrap().replace("status: waiting", "status: done").replace("## Log", "## Result\n\nexample.dev, on Netlify\n\n## Log");
        std::fs::write(&q, text).unwrap();
        // Add an item by writing a note, in the block form Obsidian uses for lists.
        std::fs::write(
            s.note("Favicon added"),
            "---\nblocked_by:\n  - \"[[Site scaffold builds locally]]\"\nparent: \"[[Personal website live]]\"\n---\nA small one.\n",
        )
        .unwrap();
        // Break a status, and delete an item.
        let broken = s.note("Site checked");
        std::fs::write(&broken, std::fs::read_to_string(&broken).unwrap().replace("status: open", "status: finished")).unwrap();
        std::fs::remove_file(s.note("Employment history gathered")).unwrap();
        let r = s.rescan();
        assert_eq!((r.indexed, r.removed), (2, 1), "{:?}", r.problems);
        assert!(r.problems.iter().any(|p| p.contains("status \"finished\"")), "{:?}", r.problems);
        let view = s.view();
        let question = view.find("Which domain and host?").unwrap();
        assert_eq!((question.status.as_str(), question.result.as_str()), ("done", "example.dev, on Netlify"));
        assert!(question.log.contains("· done (was waiting): edited in the note"), "{}", question.log);
        let favicon = view.find("Favicon added").unwrap();
        assert_eq!(favicon.project, PROJECT, "taken from the folder it was put in");
        assert_eq!(view.open_blockers(favicon).len(), 1);
        assert!(std::fs::read_to_string(s.note("Favicon added")).unwrap().contains(&format!("id: {}", favicon.id)), "stamped with its id");
        assert!(view.find("Employment history gathered").is_none());
        assert_eq!(view.find("Site checked").unwrap().status, "open", "the last good state is kept until the note is fixed");
        assert!(!s.rescan().changed(), "nothing more to do");
    }

    #[test]
    fn work_carries_on_while_the_vault_is_away() {
        let mut s = Sandbox::new("away");
        let vault = s.dir.join("vault");
        std::fs::create_dir_all(vault.join(".obsidian")).unwrap();
        s.cfg.obsidian.vault = vault.to_string_lossy().into_owned();
        s.website();
        let note = vault.join("narsil/Work/personal-site/Site scaffold builds locally.md");
        assert!(note.exists());
        // The drive goes away, leaving an empty mount point.
        let hidden = s.dir.join("vault-unmounted");
        std::fs::rename(&vault, &hidden).unwrap();
        std::fs::create_dir_all(&vault).unwrap();
        *LAST_SCAN.lock().unwrap() = None;
        let g = s.graph();
        let by = By { session: "s1", agent: "", message_id: 5 };
        g.claim(by, PROJECT, "Site scaffold builds locally", ALIVE).unwrap();
        g.done(by, PROJECT, "Site scaffold builds locally", "scaffolded", "build passes", ALIVE).unwrap();
        drop(g);
        assert!(std::fs::read_dir(&vault).unwrap().next().is_none(), "nothing is written into the mount point");
        assert_eq!(s.view().find("Site scaffold builds locally").unwrap().status, "done");
        // It returns: what was held is written out.
        std::fs::remove_dir(&vault).unwrap();
        std::fs::rename(&hidden, &vault).unwrap();
        let r = s.rescan();
        assert!(r.available && r.flushed >= 1, "{r:?}");
        assert!(std::fs::read_to_string(&note).unwrap().contains("status: done"));
    }
}
