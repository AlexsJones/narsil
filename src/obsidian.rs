//! Optional integration with an Obsidian vault: a folder of Markdown notes
//! joined by `[[wikilinks]]`.
//!
//! Three things happen when `[obsidian] vault` is set:
//!   - the agent gets tools to search, read and write notes in the vault;
//!   - the memory notes live in the vault, under `<folder>/Memory`, so they sit
//!     in the same graph as the user's own notes (see `memory.rs`);
//!   - each goal gets a note in `<folder>/Goals` that follows its progress.
//!
//! This module also holds what any folder of Markdown notes needs, vault or
//! not: frontmatter, wikilinks, file names.

use std::collections::{HashMap, HashSet};
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex};

use anyhow::{Context, Result, bail};
use regex::Regex;

use crate::config::Config;
use crate::store::{Goal, Store};
use crate::util::{expand_tilde, first_line, floor_boundary, home};

const MAX_NOTE_BYTES: u64 = 2 * 1024 * 1024;

static LINK: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\[\[([^\]\|#\n]+)(?:#[^\]\|\n]*)?(?:\|[^\]\n]*)?\]\]").unwrap());

pub struct Vault {
    pub root: PathBuf,
    /// Folder inside the vault for narsil's own notes.
    pub folder: String,
    /// `root` with links resolved, for telling whether a path on disk is in the vault.
    real_root: PathBuf,
    /// The parts of the vault the user has closed to narsil.
    pub closed: Closed,
}

/// Folders that are housekeeping, not notes: a NAS's recycle bin and its thumbnails.
const NOT_NOTES: [&str; 2] = ["#recycle", "@eaDir"];

const CLOSED: &str = "is in a part of the Obsidian vault the user has closed to narsil";

/// What a tool says when it is pointed at a closed part of the vault.
pub fn closed_message(what: &str) -> String {
    format!("{what} {CLOSED}. Do not look for another way to read it; if the task needs it, tell the user.")
}

/// The parts of a vault narsil must not read or write: what the user lists
/// under `[obsidian] exclude`, and folders that are not notes at all.
#[derive(Clone, Default)]
pub struct Closed {
    set: globset::GlobSet,
    /// How many of the patterns are the user's own.
    pub by_user: usize,
    /// Patterns that could not be read as wildcards and were taken as plain names.
    pub problems: Vec<String>,
}

impl Closed {
    pub fn new(patterns: &[String]) -> Closed {
        let mut builder = globset::GlobSetBuilder::new();
        let mut problems = Vec::new();
        let mut by_user = 0;
        let built_in = NOT_NOTES.iter().map(|s| (s.to_string(), false));
        for (raw, users) in built_in.chain(patterns.iter().map(|p| (p.clone(), true))) {
            let p = raw.trim().trim_start_matches("./").trim_matches('/');
            if p.is_empty() {
                continue;
            }
            by_user += users as usize;
            // A bare name closes a folder or note of that name wherever it is;
            // a path is taken from the top of the vault.
            let anywhere = !p.contains('/');
            let mut forms = vec![p.to_string(), format!("{p}/**")];
            if !p.to_lowercase().ends_with(".md") && !p.ends_with('*') {
                forms.push(format!("{p}.md"));
            }
            for form in forms {
                let build = |pattern: &str| {
                    let pattern = if anywhere { format!("**/{pattern}") } else { pattern.to_string() };
                    globset::GlobBuilder::new(&pattern).literal_separator(true).case_insensitive(true).build()
                };
                // Only * and ? are wildcards: brackets and braces are common in note names.
                let wild = form.replace("**", "\u{1}").replace('*', "\u{2}").replace('?', "\u{3}");
                let wild = globset::escape(&wild).replace('\u{1}', "**").replace('\u{2}', "*").replace('\u{3}', "?");
                match build(&wild).or_else(|_| build(&globset::escape(&form))) {
                    Ok(glob) => {
                        builder.add(glob);
                    }
                    Err(e) => problems.push(format!("{raw:?}: {e}")),
                }
            }
        }
        match builder.build() {
            Ok(set) => Closed { set, by_user, problems },
            Err(e) => Closed { set: globset::GlobSet::empty(), by_user, problems: vec![format!("the exclude list could not be used: {e}")] },
        }
    }

    /// Whether a path, relative to the vault, is closed.
    pub fn covers(&self, rel: &Path) -> bool {
        self.set.is_match(rel)
    }
}

/// A path with `.` and `..` worked out as written, without touching the disk.
pub fn lexical(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for part in path.components() {
        match part {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other),
        }
    }
    out
}

/// A path with links resolved, as far as it exists: the part that is not
/// there yet (a file about to be written) is kept as written.
fn real(path: &Path) -> PathBuf {
    let path = lexical(path);
    let mut rest = Vec::new();
    let mut at = path.as_path();
    loop {
        if let Ok(found) = std::fs::canonicalize(at) {
            return rest.iter().rev().fold(found, |p, part| p.join(part));
        }
        match (at.parent(), at.file_name()) {
            (Some(up), Some(name)) => {
                rest.push(name.to_os_string());
                at = up;
            }
            _ => return path,
        }
    }
}

/// The text of notes already read, so that searching the vault again only
/// has to ask which files changed. A vault on a network drive takes seconds
/// to read through cold and milliseconds to check.
struct Kept {
    modified: std::time::SystemTime,
    len: u64,
    text: Arc<String>,
    lower: Arc<String>,
}

static TEXTS: LazyLock<Mutex<HashMap<PathBuf, Kept>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

/// A note's text and its lower-case form, from the cache when the file has not changed.
fn note_text(path: &Path) -> Option<(Arc<String>, Arc<String>, std::time::SystemTime)> {
    let meta = std::fs::metadata(path).ok()?;
    if meta.len() > MAX_NOTE_BYTES {
        return None;
    }
    let modified = meta.modified().unwrap_or(std::time::UNIX_EPOCH);
    {
        let texts = TEXTS.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(k) = texts.get(path).filter(|k| k.modified == modified && k.len == meta.len()) {
            return Some((k.text.clone(), k.lower.clone(), modified));
        }
    }
    let text = Arc::new(std::fs::read_to_string(path).ok()?);
    let lower = Arc::new(text.to_lowercase());
    let mut texts = TEXTS.lock().unwrap_or_else(|e| e.into_inner());
    // A vault that large is not what this is for; start again and keep memory bounded.
    if texts.len() > 20_000 {
        texts.clear();
    }
    texts.insert(path.to_path_buf(), Kept { modified, len: meta.len(), text: text.clone(), lower: lower.clone() });
    Some((text, lower, modified))
}

/// A note split into its frontmatter lines (kept verbatim, so properties
/// narsil does not know about survive a rewrite) and its body.
#[derive(Default, Clone, Debug)]
pub struct Note {
    pub front: Vec<String>,
    pub body: String,
}

impl Note {
    pub fn parse(text: &str) -> Note {
        let text = text.strip_prefix('\u{feff}').unwrap_or(text);
        let mut lines = text.lines();
        if lines.next().map(str::trim_end) != Some("---") {
            return Note { front: Vec::new(), body: text.to_string() };
        }
        let mut front = Vec::new();
        let mut consumed = text.lines().next().map(|l| l.len() + 1).unwrap_or(0);
        for line in lines {
            consumed += line.len() + 1;
            if line.trim_end() == "---" {
                let body = text.get(consumed.min(text.len())..).unwrap_or("");
                return Note { front, body: body.trim_start_matches(['\n', '\r']).to_string() };
            }
            front.push(line.trim_end_matches('\r').to_string());
        }
        // No closing fence: it was not frontmatter after all.
        Note { front: Vec::new(), body: text.to_string() }
    }

    /// A top-level `key: value` property, without surrounding quotes.
    pub fn get(&self, key: &str) -> Option<String> {
        self.front.iter().find_map(|l| {
            let value = l.strip_prefix(key)?.strip_prefix(':')?.trim();
            let value = value.trim_matches('"').trim_matches('\'');
            (!value.is_empty()).then(|| value.to_string())
        })
    }

    /// The notes linked from a property, whether written on one line
    /// (`key: ["[[A]]", "[[B]]"]`) or as the indented list Obsidian produces.
    pub fn get_links(&self, key: &str) -> Vec<String> {
        let Some(at) = self.front.iter().position(|l| l.strip_prefix(key).is_some_and(|r| r.starts_with(':'))) else {
            return Vec::new();
        };
        let mut text = self.front[at].clone();
        for line in &self.front[at + 1..] {
            if !(line.starts_with(' ') || line.starts_with('\t') || line.starts_with('-')) {
                break;
            }
            text.push('\n');
            text.push_str(line);
        }
        links(&text)
    }

    /// Set a property to a list of links, on one line. Removes it when empty.
    pub fn set_links(&mut self, key: &str, titles: &[String]) {
        self.remove(key);
        if !titles.is_empty() {
            let items: Vec<String> = titles.iter().map(|t| format!("\"[[{t}]]\"")).collect();
            self.front.push(format!("{key}: [{}]", items.join(", ")));
        }
    }

    /// Remove a property, with the indented lines that belong to it.
    pub fn remove(&mut self, key: &str) {
        while let Some(at) = self.front.iter().position(|l| l.strip_prefix(key).is_some_and(|r| r.starts_with(':'))) {
            self.front.remove(at);
            while at < self.front.len() && (self.front[at].starts_with(' ') || self.front[at].starts_with('\t') || self.front[at].starts_with('-')) {
                self.front.remove(at);
            }
        }
    }

    pub fn set(&mut self, key: &str, value: &str) {
        // Quote anything YAML could read as something other than a plain string.
        let plain = !value.is_empty()
            && value.chars().all(|c| c.is_alphanumeric() || matches!(c, '-' | '_' | '.' | '/' | ' ' | '~'))
            && !value.starts_with(' ');
        let line = if plain { format!("{key}: {value}") } else { format!("{key}: \"{}\"", value.replace('\\', "\\\\").replace('"', "\\\"")) };
        match self.front.iter_mut().find(|l| l.strip_prefix(key).is_some_and(|r| r.starts_with(':'))) {
            Some(existing) => *existing = line,
            None => self.front.push(line),
        }
    }

    pub fn render(&self) -> String {
        let body = self.body.trim();
        if self.front.is_empty() {
            return format!("{body}\n");
        }
        format!("---\n{}\n---\n\n{body}\n", self.front.join("\n"))
    }
}

/// A title as a file name Obsidian accepts on every platform.
pub fn file_name(title: &str) -> String {
    let mut out = String::new();
    for c in title.chars() {
        let c = if c.is_control() || r#"\/:*?"<>|#^[]"#.contains(c) { '-' } else { c };
        if c.is_whitespace() {
            if !out.ends_with(' ') {
                out.push(' ');
            }
        } else {
            out.push(c);
        }
    }
    let mut out = out.trim().trim_matches('.').trim().to_string();
    if out.len() > 120 {
        out.truncate(floor_boundary(&out, 120));
        out = out.trim().to_string();
    }
    if out.is_empty() { "untitled".to_string() } else { out }
}

/// What a wikilink or a loosely written note name points at:
/// `[[Folder/Note#Heading|shown]]` -> `Folder/Note`.
fn link_target(name: &str) -> String {
    let n = name.trim().trim_start_matches("[[").trim_end_matches("]]");
    let n = n.split('|').next().unwrap_or(n);
    let n = n.split('#').next().unwrap_or(n);
    n.trim().trim_start_matches("./").to_string()
}

/// The `#heading` part of a note reference, if it has one.
pub fn link_heading(name: &str) -> Option<String> {
    let n = name.trim().trim_start_matches("[[").trim_end_matches("]]");
    let n = n.split('|').next().unwrap_or(n);
    n.split_once('#').map(|(_, h)| h.trim().to_string()).filter(|h| !h.is_empty())
}

/// The notes a piece of text links to, in order, without repeats.
pub fn links(text: &str) -> Vec<String> {
    let mut seen = HashSet::new();
    LINK.captures_iter(text)
        .map(|c| c[1].trim().to_string())
        .filter(|t| !t.is_empty() && seen.insert(t.to_lowercase()))
        .collect()
}

/// One section of a note: from the heading that matches to the next heading
/// of the same or a higher level.
pub fn section(body: &str, heading: &str) -> Option<String> {
    let want = heading.trim().to_lowercase();
    let mut out: Option<(usize, Vec<&str>)> = None;
    for line in body.lines() {
        let level = line.chars().take_while(|c| *c == '#').count();
        let is_heading = level > 0 && line[level..].starts_with(' ');
        match &mut out {
            Some((start, lines)) => {
                if is_heading && level <= *start {
                    break;
                }
                lines.push(line);
            }
            None if is_heading && line[level..].trim().to_lowercase() == want => out = Some((level, vec![line])),
            None => {}
        }
    }
    out.map(|(_, lines)| lines.join("\n"))
}

pub struct Hit {
    pub path: String,
    pub lines: Vec<(usize, String)>,
    score: usize,
    modified: std::time::SystemTime,
}

pub fn rel_string(rel: &Path) -> String {
    rel.components().map(|c| c.as_os_str().to_string_lossy()).collect::<Vec<_>>().join("/")
}

pub fn markdown_files(dir: &Path) -> Vec<PathBuf> {
    // Not gitignore-aware (a vault's ignore file is about sync, not about what
    // counts as a note), but hidden folders such as .obsidian and .trash are skipped.
    let mut out: Vec<PathBuf> = ignore::WalkBuilder::new(dir)
        .standard_filters(false)
        .hidden(true)
        .build()
        .flatten()
        .filter(|e| e.file_type().is_some_and(|t| t.is_file()) && e.path().extension().is_some_and(|x| x == "md"))
        .map(|e| e.into_path())
        .collect();
    out.sort();
    out
}

pub fn read_note(path: &Path) -> Option<String> {
    let small = std::fs::metadata(path).is_ok_and(|m| m.len() <= MAX_NOTE_BYTES);
    if small { std::fs::read_to_string(path).ok() } else { None }
}

impl Vault {
    /// Whether a vault is really at `root`. A drive that is not mounted leaves
    /// an empty folder with the right name, which must not be mistaken for a
    /// vault with nothing in it.
    pub fn present(root: &Path) -> bool {
        root.is_dir() && (root.join(".obsidian").is_dir() || std::fs::read_dir(root).is_ok_and(|mut d| d.next().is_some()))
    }

    /// The configured vault, if the integration is on and the vault is there.
    pub fn open(cfg: &Config) -> Option<Vault> {
        let raw = cfg.obsidian.vault.trim();
        if raw.is_empty() {
            return None;
        }
        let root = expand_tilde(raw);
        if !Vault::present(&root) {
            return None;
        }
        Some(Vault::at(root, &cfg.obsidian.folder, &cfg.obsidian.exclude))
    }

    /// The vault at `root`, with narsil's notes under `folder` and the parts
    /// matching `exclude` closed.
    pub fn at(root: PathBuf, folder: &str, exclude: &[String]) -> Vault {
        let folder = folder.trim().trim_matches('/');
        Vault {
            real_root: std::fs::canonicalize(&root).unwrap_or_else(|_| root.clone()),
            root,
            folder: if folder.is_empty() { "narsil".to_string() } else { folder.to_string() },
            closed: Closed::new(exclude),
        }
    }

    /// Whether a note path, relative to the vault, is in a part closed to
    /// narsil. Its own folder never is.
    pub fn closed_rel(&self, rel: &Path) -> bool {
        !rel.starts_with(&self.folder) && self.closed.covers(rel)
    }

    /// Where a path on disk sits inside the vault, as written and with links
    /// resolved (the two differ when a link inside the vault points elsewhere in it).
    fn places(&self, path: &Path, follow: bool) -> Vec<PathBuf> {
        let mut out = Vec::new();
        let written = lexical(path);
        for root in [&self.root, &self.real_root] {
            if let Ok(rel) = written.strip_prefix(root) {
                out.push(rel.to_path_buf());
            }
        }
        if follow {
            if let Ok(rel) = real(path).strip_prefix(&self.real_root) {
                out.push(rel.to_path_buf());
            }
        }
        out
    }

    /// Whether the vault lies under a directory, so that a walk from there reaches it.
    pub fn is_under(&self, dir: &Path) -> bool {
        self.root.starts_with(lexical(dir)) || self.real_root.starts_with(real(dir))
    }

    /// A path inside the vault with links resolved, so that everything found
    /// under it can be checked against what is closed by its path alone.
    pub fn settle(&self, path: &Path) -> PathBuf {
        if self.holds(path) { real(path) } else { path.to_path_buf() }
    }

    /// The vault inside this folder, when this folder is not itself one: a
    /// sign that `[obsidian] vault` names the folder above the vault.
    pub fn vault_inside(&self) -> Option<PathBuf> {
        if self.root.join(".obsidian").is_dir() {
            return None;
        }
        std::fs::read_dir(&self.root).ok()?.flatten().map(|e| e.path()).find(|p| p.join(".obsidian").is_dir())
    }

    /// Whether a path on disk is inside the vault at all.
    pub fn holds(&self, path: &Path) -> bool {
        !self.places(path, true).is_empty()
    }

    /// Whether a path on disk is in a closed part of the vault. False for
    /// everything outside the vault.
    pub fn closes(&self, path: &Path) -> bool {
        self.places(path, true).iter().any(|rel| self.closed_rel(rel))
    }

    /// The same, going only by the path as written: for the entries of a
    /// directory walk, which does not follow links and visits a great many files.
    pub fn closes_as_written(&self, path: &Path) -> bool {
        self.places(path, false).iter().any(|rel| self.closed_rel(rel))
    }

    /// Why a shell command may not run, if it reaches for the vault while
    /// parts of it are closed: a command cannot be held to what is closed.
    pub fn refuses_shell(&self, command: &str, cwd: &Path) -> Option<String> {
        if self.closed.by_user == 0 {
            return None;
        }
        let mut names = Vec::new();
        for root in [&self.root, &self.real_root] {
            names.push(root.to_string_lossy().into_owned());
            if let Ok(rel) = root.strip_prefix(home()) {
                let rel = rel.to_string_lossy();
                names.extend([format!("~/{rel}"), format!("$HOME/{rel}"), format!("${{HOME}}/{rel}")]);
            }
            // `cd ~ && grep -r x Vaults/Mine`: the last two folders of the path are name enough.
            let parts: Vec<String> = root.components().rev().take(2).map(|c| c.as_os_str().to_string_lossy().into_owned()).collect();
            if parts.len() == 2 && !parts[1].contains('/') {
                names.push(format!("{}/{}", parts[1], parts[0]));
            }
        }
        (self.holds(cwd) || names.iter().any(|n| n.len() > 3 && command.contains(n.as_str()))).then(|| {
            "Refused: parts of the Obsidian vault are closed to you, and a shell command cannot be held to that. Use \
             vault_search and vault_read for notes, or read_file, grep, glob and list_dir, which leave out what is closed."
                .to_string()
        })
    }

    /// Every note in the vault that is open to narsil, relative to its root.
    pub fn notes(&self) -> Vec<PathBuf> {
        markdown_files(&self.root)
            .into_iter()
            .filter_map(|p| p.strip_prefix(&self.root).ok().map(Path::to_path_buf))
            .filter(|rel| !self.closed_rel(rel))
            .collect()
    }

    /// Read every note once, so the first search of a session does not wait on a cold drive.
    pub fn warm(&self) -> usize {
        self.notes().iter().filter(|rel| note_text(&self.root.join(rel)).is_some()).count()
    }

    /// The user's notes that bear on a piece of text, best first, by path: for
    /// naming to the agent when a request arrives. A word counts for more the
    /// fewer notes have it, and for more in a note's name than in its text;
    /// a note is named when it shares something with the request that most
    /// of the vault does not.
    pub fn lookup(&self, text: &str, limit: usize) -> Vec<String> {
        let terms = crate::memory::subject_terms(text);
        if terms.is_empty() {
            return Vec::new();
        }
        // (path, modified, for each term: in the name, in the text)
        let mut seen: Vec<(String, std::time::SystemTime, Vec<(bool, bool)>)> = Vec::new();
        for rel in self.notes() {
            // narsil's own notes come back through its memory, not through here.
            if rel.starts_with(&self.folder) {
                continue;
            }
            let Some((_, lower, modified)) = note_text(&self.root.join(&rel)) else { continue };
            let name = rel_string(&rel.with_extension("")).to_lowercase();
            let has: Vec<(bool, bool)> = terms.iter().map(|t| (name.contains(t.as_str()), lower.contains(t.as_str()))).collect();
            seen.push((rel_string(&rel), modified, has));
        }
        let total = seen.len() as f64 + 1.0;
        let rarity: Vec<f64> = (0..terms.len())
            .map(|i| {
                let with = seen.iter().filter(|(_, _, has)| has[i].0 || has[i].1).count();
                if with == 0 { 0.0 } else { (total / with as f64).ln() }
            })
            .collect();
        let enough = 0.85 * total.ln();
        let mut found: Vec<(f64, std::time::SystemTime, String)> = seen
            .into_iter()
            .filter_map(|(path, modified, has)| {
                let score: f64 = has.iter().zip(&rarity).map(|((name, body), r)| if *name { 3.0 * r } else if *body { *r } else { 0.0 }).sum();
                (score >= enough).then_some((score, modified, path))
            })
            .collect();
        found.sort_by(|a, b| b.0.total_cmp(&a.0).then(b.1.cmp(&a.1)));
        found.into_iter().take(limit).map(|(_, _, path)| path).collect()
    }

    /// A vault-relative note name as an absolute path, refusing anything that
    /// would land outside the vault or in its hidden folders.
    pub fn confine(&self, note: &str) -> Result<PathBuf> {
        let target = link_target(note);
        if target.is_empty() {
            bail!("no note name given");
        }
        let mut rel = PathBuf::from(&target);
        if rel.is_absolute() {
            rel = match rel.strip_prefix(&self.root) {
                Ok(inner) => inner.to_path_buf(),
                Err(_) => bail!("{target} is outside the vault; give a path relative to the vault"),
            };
        }
        let mut clean = PathBuf::new();
        for part in rel.components() {
            match part {
                Component::Normal(name) if name.to_string_lossy().starts_with('.') => {
                    bail!("{target}: hidden folders such as .obsidian are off limits")
                }
                Component::Normal(name) => clean.push(name),
                Component::CurDir => {}
                _ => bail!("{target}: note paths must stay inside the vault"),
            }
        }
        if clean.as_os_str().is_empty() {
            bail!("no note name given");
        }
        if clean.extension().is_none_or(|x| x != "md") {
            let name = format!("{}.md", clean.file_name().unwrap_or_default().to_string_lossy());
            clean.set_file_name(name);
        }
        if self.closed_rel(&clean) || self.closes(&self.root.join(&clean)) {
            bail!(closed_message(&target));
        }
        Ok(self.root.join(clean))
    }

    /// Find a note the way Obsidian resolves a link: an exact path, else the
    /// note with that name anywhere in the vault (the shallowest, if several).
    pub fn resolve(&self, name: &str) -> Result<PathBuf> {
        let target = link_target(name);
        match self.confine(&target) {
            Ok(path) if path.is_file() => {
                // As the note is named on disk: where capitals do not matter to the
                // filesystem (macOS), `house/budget` opens `House/Budget.md`.
                let named = std::fs::canonicalize(&path).ok().and_then(|p| p.strip_prefix(&self.real_root).ok().map(Path::to_path_buf));
                return Ok(named.unwrap_or_else(|| path.strip_prefix(&self.root).unwrap_or(&path).to_path_buf()));
            }
            // Named outright and closed: say so, so the agent stops there.
            Err(e) if e.to_string().contains(CLOSED) => return Err(e),
            _ => {}
        }
        let want = target.trim_end_matches(".md").to_lowercase();
        let notes = self.notes();
        let mut found: Vec<&PathBuf> = notes
            .iter()
            .filter(|p| {
                let s = rel_string(&p.with_extension("")).to_lowercase();
                s == want || s.ends_with(&format!("/{want}"))
            })
            .collect();
        found.sort_by_key(|p| p.components().count());
        if let Some(p) = found.first() {
            return Ok((*p).clone());
        }
        let words: Vec<&str> = want.split_whitespace().collect();
        let near: Vec<String> = notes
            .iter()
            .map(|p| rel_string(p))
            .filter(|s| !words.is_empty() && words.iter().any(|w| s.to_lowercase().contains(w)))
            .take(6)
            .collect();
        if near.is_empty() {
            bail!("there is no note called {target:?} in the vault; vault_search finds notes by content")
        }
        bail!("there is no note called {target:?}. Similar names: {}", near.join(", "))
    }

    /// Notes ranked by how well they match the words of `query`. A word in
    /// the note's name counts for more than one in its text.
    pub fn search(&self, query: &str, limit: usize) -> Vec<Hit> {
        let terms: Vec<String> =
            query.to_lowercase().split(|c: char| !c.is_alphanumeric()).filter(|t| t.len() > 1).map(str::to_string).collect();
        if terms.is_empty() {
            return Vec::new();
        }
        let mut hits = Vec::new();
        for rel in self.notes() {
            let path = self.root.join(&rel);
            let Some((text, lower, modified)) = note_text(&path) else { continue };
            let name = rel_string(&rel).to_lowercase();
            let mut score = 0;
            let mut matched = 0;
            for t in &terms {
                let in_name = name.contains(t.as_str());
                let count = lower.matches(t.as_str()).count();
                if in_name || count > 0 {
                    matched += 1;
                }
                score += if in_name { 8 } else { 0 } + count.min(10);
            }
            // Any of the words finds a note; the more of them it has, the higher it comes.
            // Someone searching tries several words for one thing ("work job career").
            if matched == 0 {
                continue;
            }
            let lines: Vec<(usize, String)> = text
                .lines()
                .enumerate()
                .filter(|(_, l)| {
                    let l = l.to_lowercase();
                    terms.iter().any(|t| l.contains(t.as_str()))
                })
                .take(3)
                .map(|(i, l)| (i + 1, first_line(l, 200)))
                .collect();
            hits.push(Hit { path: rel_string(&rel), lines, score: score + matched * 1000, modified });
        }
        hits.sort_by(|a, b| b.score.cmp(&a.score).then(b.modified.cmp(&a.modified)));
        hits.truncate(limit);
        hits
    }

    /// The notes that link to `rel`.
    pub fn backlinks(&self, rel: &Path) -> Vec<String> {
        let full = rel_string(&rel.with_extension("")).to_lowercase();
        let stem = rel.file_stem().map(|s| s.to_string_lossy().to_lowercase()).unwrap_or_default();
        let mut out = Vec::new();
        for other in self.notes() {
            if other == rel {
                continue;
            }
            let Some((text, _, _)) = note_text(&self.root.join(&other)) else { continue };
            let points_here = links(&text).iter().any(|l| {
                let l = l.trim_end_matches(".md").to_lowercase();
                l == stem || l == full || full.ends_with(&format!("/{l}"))
            });
            if points_here {
                out.push(rel_string(&other));
            }
        }
        out
    }

    /// A short description of the vault for the system prompt.
    pub fn describe(&self) -> String {
        let notes = self.notes();
        let mut folders: Vec<String> = notes
            .iter()
            .filter_map(|p| p.components().next().filter(|_| p.components().count() > 1))
            .map(|c| c.as_os_str().to_string_lossy().into_owned())
            .collect();
        folders.sort();
        folders.dedup();
        let shown = if folders.is_empty() {
            "no folders".to_string()
        } else {
            let more = folders.len().saturating_sub(24);
            format!("{}{}", folders.iter().take(24).cloned().collect::<Vec<_>>().join(", "), if more > 0 { format!(", and {more} more") } else { String::new() })
        };
        let closed = if self.closed.by_user > 0 {
            " The user has closed parts of the vault to you. A tool that reaches one says so: stop there, do not \
             look for another way to read it, and tell the user if the task needs it."
        } else {
            ""
        };
        format!(
            "The user keeps notes in an Obsidian vault at {} ({} notes; top-level folders: {shown}). Notes are \
             Markdown, joined by [[wikilinks]]. The vault is the first place to look for anything about the user \
             themself (who they are, their work and its history, the people, places and projects in their life, \
             their plans, decisions and preferences) and for anything they may have written down or collected: \
             search it with vault_search and read with vault_read before answering from general knowledge, before \
             searching the web, and before asking them, and say which note an answer came from.{closed} Write with \
             vault_write only when asked to, or to record a result the user wants kept; link new notes to the \
             ones they relate to. narsil's own notes live under `{}/`: your memory entries (change those with \
             the memory tools, not by writing the files) and a note per goal. A [[link]] in a memory entry \
             can name one of the user's notes as well as another entry, which ties what you remember to what \
             they wrote: link to their note by its name instead of mentioning its path.",
            self.root.display(),
            notes.len(),
            self.folder
        )
    }

}

/// Take a note out of the way without destroying it: into the vault's
/// `.trash` folder, where Obsidian's own deleted notes go and from where it
/// can be put back. Returns where it went.
pub fn trash(vault_root: &Path, note: &Path) -> Result<PathBuf> {
    let bin = vault_root.join(".trash");
    std::fs::create_dir_all(&bin).with_context(|| format!("creating {}", bin.display()))?;
    let stem = note.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let stamp = chrono::Local::now().format("%Y-%m-%d %H%M%S");
    let target = bin.join(format!("{stem} ({stamp}).md"));
    std::fs::rename(note, &target).with_context(|| format!("moving {} to the vault's trash", note.display()))?;
    Ok(target)
}

// ---- goal notes ------------------------------------------------------------

/// Write (or refresh) the note that follows a goal.
pub fn write_goal_note(store: &Store, vault: &Vault, goal: &Goal) -> Result<PathBuf> {
    let dir = vault.root.join(&vault.folder).join("Goals");
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let day = |ts: i64, fmt: &str| {
        chrono::DateTime::from_timestamp(ts, 0)
            .map(|t| t.with_timezone(&chrono::Local).format(fmt).to_string())
            .unwrap_or_default()
    };
    let headline = first_line(&goal.objective, 70);
    let base = format!("{} {}", day(goal.created_at, "%Y-%m-%d"), file_name(&headline));
    let mut path = dir.join(format!("{base}.md"));
    // Two goals with the same wording on the same day each keep their own note.
    let taken_by_another =
        read_note(&path).is_some_and(|t| Note::parse(&t).get("id").is_some_and(|id| id != goal.id));
    if taken_by_another {
        path = dir.join(format!("{base} ({}).md", goal.id));
    }
    let mut note = read_note(&path).map(|t| Note::parse(&t)).unwrap_or_default();
    note.set("narsil", "goal");
    note.set("id", &goal.id);
    note.set("status", &goal.status);
    if let Some(session) = &goal.session_id {
        note.set("session", session);
    }
    note.set("directory", &goal.cwd);
    note.set("created", &day(goal.created_at, "%Y-%m-%d"));
    note.set("updated", &day(goal.updated_at, "%Y-%m-%d %H:%M"));

    let mut body = format!("# {headline}\n\n**Status:** {}", goal.status);
    if !goal.note.trim().is_empty() {
        body.push_str(&format!("\n\n{}", goal.note.trim()));
    }
    body.push_str(&format!("\n\n## Objective\n\n{}\n", goal.objective.trim()));
    // The items link to their own notes, so the goal sits in the graph with its work.
    let view = crate::work::Overview::of(store, &crate::context::project_dir(&goal.cwd))?;
    if let Some(root) = view.items.iter().find(|i| i.goal == goal.id) {
        let parts = view.descendants(&root.id);
        if !parts.is_empty() {
            body.push_str(&format!("\n## Work\n\nUnder [[{}]]:\n\n", root.title));
            for i in parts {
                body.push_str(&match i.status.as_str() {
                    "done" => format!("- [x] [[{}]]\n", i.title),
                    "in_progress" => format!("- [ ] [[{}]] *(in progress)*\n", i.title),
                    "waiting" => format!("- [ ] [[{}]] *(waiting on you)*\n", i.title),
                    "dropped" => format!("- [ ] ~~{}~~ *(dropped)*\n", i.title),
                    _ => format!("- [ ] [[{}]]\n", i.title),
                });
            }
        }
    }
    let progress: Vec<_> = store.goal_logs(&goal.id, 200)?.into_iter().filter(|l| l.kind == "progress").collect();
    if !progress.is_empty() {
        body.push_str("\n## Progress\n\n");
        for l in progress {
            body.push_str(&format!("- {} — {}\n", day(l.ts, "%Y-%m-%d %H:%M"), l.text.trim().replace('\n', " ")));
        }
    }
    if let Some(session) = &goal.session_id {
        body.push_str(&format!("\nOpen the session with `narsil resume {session}`.\n"));
    }
    note.body = body;
    std::fs::write(&path, note.render()).with_context(|| format!("writing {}", path.display()))?;
    Ok(path)
}

// ---- finding vaults --------------------------------------------------------

/// Vaults that the Obsidian app on this machine knows about, whether or not
/// their folders are reachable right now.
pub fn known_vaults() -> Vec<PathBuf> {
    let h = home();
    let candidates = [
        std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from).unwrap_or_else(|| h.join(".config")).join("obsidian"),
        h.join(".var/app/md.obsidian.Obsidian/config/obsidian"),
        h.join("snap/obsidian/current/.config/obsidian"),
        h.join("Library/Application Support/obsidian"),
    ];
    let mut out: Vec<PathBuf> = Vec::new();
    for dir in candidates {
        let Ok(text) = std::fs::read_to_string(dir.join("obsidian.json")) else { continue };
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) else { continue };
        let Some(vaults) = v.get("vaults").and_then(|v| v.as_object()) else { continue };
        for entry in vaults.values() {
            if let Some(path) = entry.get("path").and_then(|p| p.as_str()) {
                let path = PathBuf::from(path);
                if !out.contains(&path) {
                    out.push(path);
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::NewSession;

    /// A vault in a fresh temporary folder, removed when dropped.
    struct TempVault(Vault);

    impl TempVault {
        fn new(tag: &str) -> TempVault {
            let root = std::env::temp_dir().join(format!("narsil-vault-{tag}-{}-{}", std::process::id(), crate::util::new_id()));
            std::fs::create_dir_all(&root).unwrap();
            TempVault(Vault::at(root, "narsil", &[]))
        }
        /// The same vault with parts of it closed.
        fn closing(self, patterns: &[&str]) -> TempVault {
            let patterns: Vec<String> = patterns.iter().map(|p| p.to_string()).collect();
            let vault = Vault::at(self.0.root.clone(), "narsil", &patterns);
            std::mem::forget(self);
            TempVault(vault)
        }
        fn write(&self, rel: &str, text: &str) {
            let path = self.0.root.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }
    }

    impl Drop for TempVault {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0.root);
        }
    }

    fn strings(found: Vec<PathBuf>) -> Vec<String> {
        found.iter().map(|p| rel_string(p)).collect()
    }

    #[test]
    fn closed_parts_are_named_by_folder_path_or_pattern() {
        let closed = Closed::new(&["Journal".to_string(), "Areas/Health/".to_string(), "*.private.md".to_string(), "People/[Family]".to_string(), " ".to_string()]);
        assert_eq!(closed.by_user, 4, "a blank entry closes nothing");
        assert!(closed.problems.is_empty(), "{:?}", closed.problems);
        for path in [
            "Journal/2026-11-26.md",
            "Journal.md",
            "Archive/Journal/old.md",
            "journal/lower-case.md",
            "Areas/Health/GP.md",
            "Areas/Health/Tests/bloods.md",
            "Money/tax.private.md",
            "People/[Family]/Mum.md",
            "#recycle/deleted.md",
            "creation/#recycle/deleted.md",
            "@eaDir/thumb.md",
        ] {
            assert!(closed.covers(Path::new(path)), "{path} should be closed");
        }
        for path in [
            "Journaling ideas.md",
            "Areas/Healthy eating.md",
            "Other/Areas/Health/x.md",
            "Money/tax.md",
            "People/Family/Mum.md",
            "People/F/x.md",
            "Projects/House.md",
        ] {
            assert!(!closed.covers(Path::new(path)), "{path} should be open");
        }
        // With nothing listed, only folders that are not notes at all are closed.
        let nothing = Closed::new(&[]);
        assert_eq!(nothing.by_user, 0);
        assert!(nothing.covers(Path::new("#recycle/x.md")) && !nothing.covers(Path::new("Journal/x.md")));
    }

    #[test]
    fn what_is_closed_cannot_be_listed_searched_read_or_written() {
        let v = TempVault::new("closed");
        v.write("Projects/House.md", "Survey booked. See [[Diary]] and [[Heating]].\n");
        v.write("Projects/Heating.md", "The heat pump quote came to 9000.\n");
        v.write("Journal/2026-11-26.md", "Private thoughts about the heat pump, and [[House]].\n");
        v.write("Journal/Diary.md", "Dear diary.\n");
        v.write("Areas/Health/GP.md", "Appointment about the heat rash.\n");
        v.write("narsil/Memory/Global/Journal.md", "A memory entry that happens to share a closed name.\n");
        v.write("#recycle/Old.md", "A deleted note about the heat pump.\n");
        let v = v.closing(&["Journal", "Areas/Health"]);
        let vault = &v.0;

        let notes = strings(vault.notes());
        assert_eq!(notes, vec!["Projects/Heating.md", "Projects/House.md", "narsil/Memory/Global/Journal.md"], "narsil's own folder is never closed to it");

        // Searching finds only what is open, however well the closed notes match.
        let found: Vec<String> = vault.search("pump", 10).into_iter().map(|h| h.path).collect();
        assert_eq!(found, vec!["Projects/Heating.md"]);
        assert!(vault.search("diary", 10).iter().all(|h| h.path == "Projects/House.md"), "only the open note that mentions it");
        assert!(vault.lookup("what did we decide about the heat pump", 5).iter().all(|p| p.starts_with("Projects/")));

        // Named outright, a closed note is refused as closed, by path or through a link.
        for name in ["Journal/Diary", "Journal/2026-11-26.md", "[[Journal/Diary]]", "Areas/Health/GP", "journal/diary"] {
            let refused = vault.resolve(name).unwrap_err().to_string();
            assert!(refused.contains("closed to narsil"), "{name}: {refused}");
            assert!(vault.confine(name).is_err(), "{name} must not be writable either");
        }
        // By name alone it is simply not there, and is not offered as a near match.
        let missing = vault.resolve("Diary").unwrap_err().to_string();
        assert!(missing.contains("no note called") && !missing.contains("Journal"), "{missing}");
        // A closed note that links to an open one does not show among its backlinks.
        assert!(vault.backlinks(Path::new("Projects/House.md")).is_empty());
        // The prompt says parts are closed, without naming them.
        let told = vault.describe();
        assert!(told.contains("closed parts of the vault") && !told.contains("Journal,") && !told.contains("Health"), "{told}");
        assert!(told.contains("top-level folders: Projects, narsil"), "{told}");
    }

    #[test]
    fn a_path_on_disk_is_checked_however_it_is_written() {
        let v = TempVault::new("paths");
        v.write("Open/Note.md", "open\n");
        v.write("Journal/Diary.md", "closed\n");
        std::os::unix::fs::symlink(v.0.root.join("Journal"), v.0.root.join("Open/shortcut")).unwrap();
        let outside = v.0.root.with_extension("link");
        std::os::unix::fs::symlink(&v.0.root, &outside).unwrap();
        let v = v.closing(&["Journal"]);
        let vault = &v.0;
        let root = vault.root.clone();

        assert!(vault.closes(&root.join("Journal/Diary.md")));
        assert!(vault.closes(&root.join("Journal")), "the folder itself");
        assert!(vault.closes(&root.join("Open/../Journal/Diary.md")), "through ..");
        assert!(vault.closes(&root.join("Open/shortcut/Diary.md")), "through a link inside the vault");
        assert!(vault.closes(&outside.join("Journal/Diary.md")), "through a link to the vault from outside it");
        assert!(vault.closes(&root.join("Journal/not-written-yet.md")), "a file that does not exist yet");
        assert!(!vault.closes(&root.join("Open/Note.md")));
        assert!(!vault.closes(&root.with_extension("other").join("Journal/Diary.md")), "another folder is not the vault's business");
        assert!(vault.holds(&outside.join("Open")) && !vault.holds(Path::new("/etc")));
        assert!(vault.is_under(root.parent().unwrap()) && !vault.is_under(&root.join("Open")));
        // A walk started through the outside link is moved onto the real path, where each entry can be checked as written.
        let settled = vault.settle(&outside.join("Journal"));
        assert!(vault.closes_as_written(&settled.join("Diary.md")));
        assert!(!vault.closes_as_written(&vault.settle(&outside.join("Open")).join("Note.md")));
        let _ = std::fs::remove_file(&outside);
    }

    #[test]
    fn shell_commands_are_kept_off_a_vault_with_closed_parts() {
        let v = TempVault::new("shell");
        v.write("Open/Note.md", "open\n");
        let elsewhere = std::env::temp_dir();
        let open = &v.0;
        let root = open.root.to_string_lossy().into_owned();
        assert!(open.refuses_shell(&format!("cat {root}/Open/Note.md"), &elsewhere).is_none(), "nothing is closed, so nothing is refused");
        let v = v.closing(&["Journal"]);
        let vault = &v.0;
        let two = {
            let parts: Vec<String> = vault.root.components().rev().take(2).map(|c| c.as_os_str().to_string_lossy().into_owned()).collect();
            format!("{}/{}", parts[1], parts[0])
        };
        for command in [format!("cat {root}/Open/Note.md"), format!("grep -r secret '{root}'"), format!("cd / && ls {two}")] {
            let why = vault.refuses_shell(&command, &elsewhere).unwrap_or_else(|| panic!("{command} should be refused"));
            assert!(why.contains("vault_search") && why.contains("grep"), "{why}");
        }
        assert!(vault.refuses_shell("ls", &vault.root.join("Open")).is_some(), "run from inside the vault");
        assert!(vault.refuses_shell("cargo test", &elsewhere).is_none(), "work elsewhere is untouched");
    }

    #[test]
    fn a_request_names_the_notes_that_bear_on_it() {
        let v = TempVault::new("lookup");
        v.write("People/About me.md", "I am a lighthouse inspector. I live in Wexcombe. Reviewed in the autumn.\n");
        v.write("House/Harbour Cottage.md", "The cottage in Wexcombe. The survey found damp in the cellar. Reviewed in the autumn.\n");
        v.write("House/Heating.md", "Quotes for a heat pump. The cottage has an old boiler. Reviewed in the autumn.\n");
        v.write("Work/Kubernetes notes.md", "Cluster upgrade steps. Cilium. Testing the upgrade. Reviewed in the autumn.\n");
        v.write("Work/Rust notes.md", "Lifetimes. Testing with cargo. Reviewed in the autumn.\n");
        v.write("Recipes/Bread.md", "Flour, water, salt, yeast. Reviewed in the autumn.\n");
        v.write("Recipes/Soup.md", "Leeks and potatoes. Reviewed in the autumn.\n");
        v.write("Reading/Books 2026.md", "A list of novels. Reviewed in the autumn.\n");
        v.write("narsil/Memory/Global/Cottage survey.md", "narsil's own note about the cottage survey. Reviewed in the autumn.\n");
        let vault = &v.0;

        // A word in a note's name, when few notes have it, names the note.
        assert_eq!(vault.lookup("what did the survey say about the cottage?", 5), vec!["House/Harbour Cottage.md"]);
        // Two notes share the subject: the one named for it comes first.
        assert_eq!(vault.lookup("heat pump quotes for the cottage", 5), vec!["House/Heating.md", "House/Harbour Cottage.md"]);
        assert_eq!(vault.lookup("how do I upgrade the kubernetes cluster", 5), vec!["Work/Kubernetes notes.md"]);
        // A word found in one note's text and nowhere else names it too.
        assert_eq!(vault.lookup("is the yeast still good", 5), vec!["Recipes/Bread.md"]);
        // Words that are everywhere, or nowhere, name nothing.
        assert!(vault.lookup("fix the failing integration suite", 5).is_empty());
        assert!(vault.lookup("what was reviewed in the autumn", 5).is_empty(), "words every note has say nothing");
        assert!(vault.lookup("who is it", 5).is_empty(), "no words to go on");
        // narsil's own notes are its memory's to recall, not the vault's.
        assert!(vault.lookup("cottage survey", 5).iter().all(|p| !p.starts_with("narsil/")));
        assert_eq!(vault.lookup("cottage survey", 1).len(), 1, "no more than asked for");

        // Changing a note changes what is found, without waiting for anything.
        std::thread::sleep(std::time::Duration::from_millis(20));
        v.write("Recipes/Soup.md", "Leeks, potatoes and a pinch of saffron.\n");
        assert_eq!(vault.lookup("where did I use saffron", 5), vec!["Recipes/Soup.md"]);
    }

    #[test]
    fn a_folder_above_the_vault_is_noticed() {
        let v = TempVault::new("above");
        v.write("creation/.obsidian/app.json", "{}");
        v.write("creation/Note.md", "x\n");
        assert_eq!(v.0.vault_inside(), Some(v.0.root.join("creation")));
        let inner = Vault::at(v.0.root.join("creation"), "narsil", &[]);
        assert_eq!(inner.vault_inside(), None, "a real vault is fine as it is");
    }

    #[test]
    fn frontmatter_round_trips_and_keeps_unknown_properties() {
        let text = "---\ntags:\n  - house\naliases: [Home]\nid: 7\n---\n\nBody line\n\n---\nnot frontmatter\n";
        let mut note = Note::parse(text);
        assert_eq!(note.get("id").as_deref(), Some("7"));
        assert_eq!(note.get("tags"), None, "a list is not a plain value");
        assert!(note.body.starts_with("Body line"));
        note.set("id", "8");
        note.set("scope", "project:/home/me/my proj");
        note.set("title", "a: tricky \"one\"");
        let again = Note::parse(&note.render());
        assert_eq!(again.get("id").as_deref(), Some("8"));
        assert_eq!(again.get("scope").as_deref(), Some("project:/home/me/my proj"));
        assert!(again.front.contains(&"  - house".to_string()) && again.front.contains(&"aliases: [Home]".to_string()));
        assert!(again.body.contains("not frontmatter"));
        // Links in properties, in either YAML form.
        let listed = Note::parse("---\nblocked_by:\n  - \"[[A one]]\"\n  - \"[[B]]\"\nstatus: open\nparent: \"[[Root]]\"\n---\nx");
        assert_eq!(listed.get_links("blocked_by"), vec!["A one", "B"]);
        assert_eq!(listed.get_links("parent"), vec!["Root"]);
        let mut inline = listed.clone();
        inline.set_links("blocked_by", &["C".to_string()]);
        assert_eq!(inline.get_links("blocked_by"), vec!["C"]);
        assert_eq!(inline.get("status").as_deref(), Some("open"), "the lines of the old list are gone, the rest is kept");
        inline.set_links("blocked_by", &[]);
        assert!(inline.get_links("blocked_by").is_empty() && !inline.render().contains("blocked_by"));
        // No frontmatter, and an unclosed fence, are both just a body.
        assert!(Note::parse("plain").front.is_empty());
        assert_eq!(Note::parse("---\nkey: v\nno end").body, "---\nkey: v\nno end");
    }

    #[test]
    fn names_links_and_sections() {
        assert_eq!(file_name("  What/Why: a \"plan\"?  "), "What-Why- a -plan--");
        assert_eq!(file_name("..."), "untitled");
        assert_eq!(links("See [[Plan]] and [[House/Budget#Costs|the budget]], then [[plan]] again."), vec!["Plan", "House/Budget"]);
        let body = "# Top\nintro\n## Costs\n- survey 900\n### Detail\nmore\n## Dates\nlater";
        assert_eq!(section(body, "costs").unwrap(), "## Costs\n- survey 900\n### Detail\nmore");
        assert!(section(body, "missing").is_none());
    }

    #[test]
    fn notes_resolve_like_obsidian_and_writes_stay_inside() {
        let v = TempVault::new("resolve");
        v.write("House/Budget.md", "Costs so far. See [[Survey]].");
        v.write("Survey.md", "Booked.");
        v.write("Archive/Old/Survey.md", "Old one.");
        v.write(".obsidian/app.md", "settings");
        let vault = &v.0;
        assert_eq!(vault.notes().len(), 3, "hidden folders are not notes");
        assert_eq!(rel_string(&vault.resolve("[[Budget]]").unwrap()), "House/Budget.md");
        assert_eq!(rel_string(&vault.resolve("house/budget#Costs").unwrap()), "House/Budget.md");
        assert_eq!(rel_string(&vault.resolve("survey").unwrap()), "Survey.md", "the shallowest match wins");
        assert!(vault.resolve("Nothing").unwrap_err().to_string().contains("no note"));
        assert_eq!(vault.backlinks(Path::new("Survey.md")), vec!["House/Budget.md"]);

        assert!(vault.confine("New/Idea").unwrap().ends_with("New/Idea.md"));
        assert!(vault.confine("Notes.v2").unwrap().ends_with("Notes.v2.md"));
        for bad in ["../outside", "/etc/passwd", ".obsidian/workspace", "a/../../b", ""] {
            assert!(vault.confine(bad).is_err(), "{bad:?} must be refused");
        }
        let inside = format!("{}/Deep/Note.md", vault.root.display());
        assert!(vault.confine(&inside).unwrap().ends_with("Deep/Note.md"));
    }

    #[test]
    fn search_ranks_names_above_passing_mentions() {
        let v = TempVault::new("search");
        v.write("Boiler service.md", "Annual check booked for March.");
        v.write("Journal/2026-03-01.md", "Called about the boiler. Also the boiler made a noise.\nUnrelated line.");
        v.write("Garden.md", "Roses.");
        // Any of the words finds a note; the one with all of them comes first.
        let hits = v.0.search("boiler service", 10);
        assert_eq!(hits.iter().map(|h| h.path.as_str()).collect::<Vec<_>>(), vec!["Boiler service.md", "Journal/2026-03-01.md"]);
        // Several words for one thing, as someone searching tries them: one hit is enough.
        let hits = v.0.search("roses flowers planting shrubs", 10);
        assert_eq!(hits.iter().map(|h| h.path.as_str()).collect::<Vec<_>>(), vec!["Garden.md"]);
        let hits = v.0.search("boiler", 10);
        assert_eq!(hits[0].path, "Boiler service.md");
        assert_eq!(hits[1].path, "Journal/2026-03-01.md");
        assert_eq!(hits[1].lines[0].0, 1);
        assert!(v.0.search("zzz", 10).is_empty() && v.0.search("a", 10).is_empty());
    }

    #[test]
    fn a_goal_gets_a_note_that_follows_it() {
        let v = TempVault::new("goal");
        let store = Store::open_memory().unwrap();
        let goal = store.goal_create("Get the test suite green\nand keep it that way", "/work", "mock/m").unwrap();
        let session = store
            .create_session(NewSession { goal_id: Some(goal.id.clone()), kind: "goal".into(), cwd: "/work".into(), model: "mock/m".into(), perm_mode: "auto".into(), ..Default::default() })
            .unwrap();
        store.goal_set_session(&goal.id, &session.id).unwrap();
        // Two work items under the goal, one finished.
        {
            let cfg = Config::default();
            let paths = crate::config::Paths {
                config_dir: v.0.root.join("c"),
                data_dir: v.0.root.join("d"),
                state_dir: v.0.root.join("s"),
                runtime_dir: v.0.root.join("r"),
            };
            let graph = crate::work::Graph::open(&store, &cfg, &paths);
            let by = crate::work::By { session: &session.id, agent: "", message_id: 1 };
            let spec = |t: &str| crate::work::Spec { title: t.into(), ..Default::default() };
            graph.plan(by, "/work", &goal.id, Some("Suite green"), &[spec("Failing tests listed"), spec("Flaky test fixed")]).unwrap();
            graph.claim(by, "/work", "Failing tests listed", &|_| true).unwrap();
            graph.done(by, "/work", "Failing tests listed", "3 fail", "ran the suite", &|_| true).unwrap();
            graph.claim(by, "/work", "Flaky test fixed", &|_| true).unwrap();
        }
        store.goal_log(&goal.id, "progress", "Found 3 failures").unwrap();
        let goal = store.goal_get(&goal.id).unwrap().unwrap();
        let path = write_goal_note(&store, &v.0, &goal).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(path.file_name().unwrap().to_string_lossy().ends_with("Get the test suite green.md"));
        for expected in ["narsil: goal", "status: active", "# Get the test suite green", "Under [[Suite green]]:", "- [x] [[Failing tests listed]]", "- [ ] [[Flaky test fixed]] *(in progress)*", "Found 3 failures", &format!("narsil resume {}", session.id)] {
            assert!(text.contains(expected), "missing {expected:?} in:\n{text}");
        }
        store.goal_set_status(&goal.id, "completed", "All green").unwrap();
        let again = write_goal_note(&store, &v.0, &store.goal_get(&goal.id).unwrap().unwrap()).unwrap();
        assert_eq!(again, path, "the same note is refreshed");
        assert!(std::fs::read_to_string(&path).unwrap().contains("**Status:** completed\n\nAll green"));
    }
}
