//! Context assembly: the one place that decides what an agent knows when it
//! starts. Main agents, subagents, resumed sessions and sessions recovered
//! after a crash all get their context from here, from the same store.
//!
//! The system prompt is built once per context epoch (session start, and again
//! at each compaction) and then frozen. Anything that changes mid-epoch is
//! appended to the transcript instead. That keeps the prompt prefix stable,
//! which providers need for prompt caching and for replaying signed reasoning.

use std::path::{Path, PathBuf};

use crate::config::{Config, Paths};
use crate::store::{Session, Store};
use crate::util;

const PREAMBLE: &str = "\
You are an AI agent running inside narsil, a persistent agent harness on the user's own machine. \
You act through tools: a shell, file tools, memory, and (where offered) subagents.

How this harness works:
- Everything you do is journaled. A session can be stopped, resumed, or recovered after a crash, \
and continues from its transcript. If a tool result says a call was interrupted, its effects may be \
partial: inspect the current state before repeating it.
- When the conversation grows long it is compacted into a summary and you continue from that. \
Anything that must survive should be in memory, in task notes, or in files, not only in conversation.
- Relative paths resolve against the working directory given below. Shell commands start there \
each time; a `cd` does not persist between calls.

Memory:
- Durable memory holds facts that matter beyond this task, in `global` scope (about the user, \
everywhere) or `project` scope (about this working directory). Save with memory_save when you learn \
a lasting preference, receive a correction, or discover something about a project that its files do \
not already say. Relevant entries are shown below; memory_search finds the rest.
- `task` scope is a shared notebook for this task: every agent working on it, including parallel \
subagents, reads and writes the same notes.

Working style:
- Use tools to find things out instead of guessing. Check your work by running it.
- For work with more than a couple of steps, lay out objectives with the objectives tool and keep \
their status current. The user follows your progress there.
- Say what you did and what you found plainly, including failures.";

const SUBAGENT_ROLE: &str = "\
You are a subagent. A parent agent gave you the task in the first message and is waiting for your \
result; other subagents may be working in parallel on sibling tasks. Do the task completely, then end \
with a final message containing everything the parent needs: findings, file paths, and anything you \
could not do. That final message is all the parent sees, so make it self-contained. You cannot ask the \
user questions. Put intermediate findings that siblings could use in task notes \
(memory_save with scope \"task\").";

const GOAL_ROLE: &str = "\
You are working autonomously on a goal. Nobody is watching and no reply will come, so do not ask \
questions or wait for confirmation: decide, act, verify, and continue. Keep going until the objective \
is achieved and you have verified it. Begin by breaking the goal into objectives with the objectives \
tool, and keep their status current as you go. Record milestones with goal_update; those notes survive \
compaction and restarts and are how you will know where you were. When the objective is complete and \
verified, call goal_complete. If you are blocked on something only the user can provide, call \
goal_blocked with the specific question, and stop.";

/// The memory scopes visible to a session: global, its project, its task.
pub fn scopes_for(session: &Session) -> Vec<String> {
    vec!["global".to_string(), project_scope(&session.cwd), task_scope(session)]
}

pub fn project_scope(cwd: &str) -> String {
    format!("project:{}", project_root(Path::new(cwd)).display())
}

pub fn task_scope(session: &Session) -> String {
    format!("task:{}", session.root_id)
}

/// The enclosing git repository, or the directory itself.
pub fn project_root(cwd: &Path) -> PathBuf {
    let home = util::home();
    let mut dir = Some(cwd);
    while let Some(d) = dir {
        if d == home {
            break;
        }
        if d.join(".git").exists() {
            return d.to_path_buf();
        }
        dir = d.parent();
    }
    cwd.to_path_buf()
}

/// Project instruction files from the filesystem root down to `cwd`, so that
/// more specific files come later and take precedence.
fn project_files(cfg: &Config, cwd: &Path) -> Vec<(PathBuf, String)> {
    let mut dirs: Vec<&Path> = cwd.ancestors().collect();
    dirs.reverse();
    let mut out = Vec::new();
    for dir in dirs {
        for name in &cfg.context.project_files {
            let file = dir.join(name);
            if let Ok(text) = std::fs::read_to_string(&file) {
                if !text.trim().is_empty() {
                    out.push((file, text));
                }
            }
        }
    }
    out
}

fn read_trimmed(path: &Path) -> String {
    std::fs::read_to_string(path).map(|s| s.trim().to_string()).unwrap_or_default()
}

fn os_name() -> String {
    std::fs::read_to_string("/etc/os-release")
        .ok()
        .and_then(|t| {
            t.lines()
                .find_map(|l| l.strip_prefix("PRETTY_NAME=").map(|v| v.trim_matches('"').to_string()))
        })
        .unwrap_or_else(|| std::env::consts::OS.to_string())
}

/// Build the system prompt for a session's next context epoch.
pub fn build_system_prompt(cfg: &Config, paths: &Paths, store: &Store, session: &Session) -> String {
    let mut s = String::from(PREAMBLE);
    let cwd = Path::new(&session.cwd);

    match session.kind.as_str() {
        "subagent" => {
            s.push_str("\n\n# Your role\n");
            s.push_str(SUBAGENT_ROLE);
        }
        "goal" => {
            s.push_str("\n\n# Your role\n");
            s.push_str(GOAL_ROLE);
        }
        _ => {}
    }

    let soul = read_trimmed(&paths.soul_file());
    if !soul.is_empty() {
        s.push_str("\n\n# Soul (SOUL.md, written by the user)\n");
        s.push_str(&soul);
    }
    let agents = read_trimmed(&paths.agents_file());
    if !agents.is_empty() {
        s.push_str("\n\n# Operating instructions (AGENTS.md, written by the user)\n");
        s.push_str(&agents);
    }
    for (file, text) in project_files(cfg, cwd) {
        s.push_str(&format!("\n\n# Project instructions ({})\n", file.display()));
        s.push_str(text.trim());
    }

    s.push_str("\n\n# Environment\n");
    s.push_str(&format!("- Working directory: {}\n", session.cwd));
    let root = project_root(cwd);
    if root.join(".git").exists() {
        s.push_str(&format!("- Git repository root: {}\n", root.display()));
    }
    s.push_str(&format!("- System: {} ({})\n", os_name(), std::env::consts::ARCH));
    s.push_str(&format!("- Shell for commands: {}\n", cfg.shell.program));
    s.push_str(&format!("- Date when this context began: {}\n", util::today()));
    s.push_str(&format!("- Model: {}\n", session.model));
    if cfg.privilege.sudo {
        s.push_str(
            "- sudo: allowed. Commands run without a terminal, so use `sudo -n` (it fails at once \
             instead of waiting for a password). If it reports that a password is required, say so \
             rather than retrying.\n",
        );
    } else {
        s.push_str("- sudo: not allowed. Do not attempt to gain root.\n");
    }
    let mode = if session.perm_mode == "ask" {
        "the user approves shell commands and file writes before they run"
    } else {
        "tools run without confirmation, so be deliberate with anything destructive"
    };
    s.push_str(&format!("- Permissions: {mode}.\n"));

    // Durable memory: newest first, bodies until the budget is spent, then titles.
    let durable = vec!["global".to_string(), project_scope(&session.cwd)];
    let memories = store.memory_list(&durable, 200).unwrap_or_default();
    if !memories.is_empty() {
        s.push_str("\n# Memory\n");
        let mut budget = cfg.context.memory_budget_chars as i64;
        let mut titles_only = Vec::new();
        for m in &memories {
            let scope = if m.scope == "global" { "global" } else { "project" };
            if budget > 0 {
                budget -= (m.title.len() + m.body.len()) as i64;
                s.push_str(&format!("\n## {} [{} · {} · id {}]\n{}\n", m.title, scope, m.kind, m.id, m.body.trim()));
            } else {
                titles_only.push(format!("- {} [{} · id {}]", m.title, scope, m.id));
            }
        }
        if !titles_only.is_empty() {
            s.push_str("\nMore entries (use memory_search to read them):\n");
            s.push_str(&titles_only.join("\n"));
            s.push('\n');
        }
    }

    let notes = store.memory_list(&[task_scope(session)], 50).unwrap_or_default();
    if !notes.is_empty() {
        s.push_str("\n# Task notes (shared by every agent on this task)\n");
        for m in notes.iter().rev() {
            s.push_str(&format!("\n## {}\n{}\n", m.title, util::truncate_middle(m.body.trim(), 4000)));
        }
    }

    let objectives = store.objectives_get(&session.id).unwrap_or_default();
    if !objectives.is_empty() {
        s.push_str("\n# Objectives (as you last set them)\n");
        for o in &objectives {
            s.push_str(&format!("- [{}] {}\n", o.status, o.text));
        }
    }

    if let Some(goal) = session.goal_id.as_ref().and_then(|g| store.goal_get(g).ok().flatten()) {
        s.push_str(&format!("\n# Goal {}\n{}\n", goal.id, goal.objective.trim()));
        let progress: Vec<_> = store
            .goal_logs(&goal.id, 40)
            .unwrap_or_default()
            .into_iter()
            .filter(|l| l.kind == "progress")
            .collect();
        if !progress.is_empty() {
            s.push_str("\nProgress recorded so far (oldest first):\n");
            for l in progress {
                s.push_str(&format!("- {}\n", l.text.trim()));
            }
        }
    }
    s
}

pub const COMPACTION_REQUEST: &str = "\
[narsil] This conversation is about to be compacted: everything above will be replaced by your next \
reply, and you will continue working from it alone. Write a complete handoff for yourself. Include:
1. What the user asked for, and any instructions or preferences they gave, in their words where the \
wording matters.
2. What has been done, concretely: files created or changed (with paths), commands run, results, and \
decisions together with the reasons for them.
3. Where things stand right now: what you were in the middle of, and the exact next steps.
4. What failed or was ruled out, so it is not tried again.
5. Facts about this environment or project that you had to discover.
Do not call any tools. Reply with the handoff only.";

pub fn compaction_message(summary: &str, last_user_text: Option<&str>) -> String {
    let mut s = String::from(
        "[narsil] The earlier part of this conversation was compacted. This is the handoff you wrote:\n\n",
    );
    s.push_str(summary.trim());
    if let Some(text) = last_user_text.filter(|t| !t.trim().is_empty()) {
        s.push_str("\n\nThe user's most recent message, verbatim:\n\n");
        s.push_str(text.trim());
    }
    s.push_str("\n\nContinue from where you left off. Do not repeat work that is already done.");
    s
}

pub fn goal_brief(goal_id: &str, objective: &str) -> String {
    format!(
        "[narsil goal {goal_id}] Work on this objective autonomously until it is achieved and verified.\n\n\
         ## Objective\n{}\n\n\
         Start now. Record milestones with goal_update, finish with goal_complete, and use goal_blocked \
         only for something the user alone can resolve.",
        objective.trim()
    )
}

pub fn goal_continuation(iteration: i64) -> String {
    format!(
        "[narsil] Continue working toward the goal (turn {iteration}). If the objective is fully achieved \
         and verified, call goal_complete with a summary. If you are blocked on something only the user \
         can resolve, call goal_blocked. Otherwise take the next concrete step now."
    )
}
