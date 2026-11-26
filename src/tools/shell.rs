//! The shell tool. Commands run in their own session and process group with
//! no terminal, so they can be killed as a unit and can never sit waiting on a
//! password or confirmation prompt.

use std::path::Path;
use std::process::Stdio;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use regex::Regex;
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio_util::sync::CancellationToken;

use super::{ToolCtx, ToolOutput, opt_str, opt_u64, resolve, str_arg};
use crate::config::ShellConfig;

const CAPTURE_LIMIT: usize = 4 * 1024 * 1024;

static PRIVILEGE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(^|[\s;&|(`])(sudo|su|doas|pkexec|run0)(\s|$)").unwrap());

pub fn uses_privilege(command: &str) -> bool {
    PRIVILEGE.is_match(command)
}

pub async fn bash(ctx: &ToolCtx<'_>) -> Result<ToolOutput> {
    let input = &ctx.call.input;
    let command = str_arg(input, "command")?;
    let cfg = ctx.cfg;
    for rule in cfg.permissions.deny.iter().filter(|r| !r.trim().is_empty()) {
        if command.contains(rule.as_str()) {
            return Ok(ToolOutput::err(format!(
                "Refused: this command matches the deny rule {rule:?} in narsil's config."
            )));
        }
    }
    if !cfg.privilege.sudo && uses_privilege(command) {
        return Ok(ToolOutput::err(
            "Refused: privilege escalation is disabled ([privilege] sudo = false in narsil's config).",
        ));
    }
    let timeout = opt_u64(input, "timeout_secs")
        .unwrap_or(cfg.shell.default_timeout_secs)
        .clamp(1, cfg.shell.max_timeout_secs.max(1));
    let cwd = match opt_str(input, "cwd") {
        Some(d) => resolve(&ctx.session.cwd, d),
        None => std::path::PathBuf::from(&ctx.session.cwd),
    };
    // Journal the process group, tagged so it can be recognized later: if the
    // harness dies mid-command, recovery stops the orphan instead of leaving
    // it running behind the agent's back.
    let marker = format!("{}:{}:{}", ctx.session.id, ctx.call.message_id, ctx.call.seq);
    let journal = |pgid: i32| {
        let state = serde_json::json!({ "pgid": pgid, "marker": marker });
        let _ = ctx.engine.store.tool_call_set_state(&ctx.session.id, ctx.call, &state);
    };
    run_command(&cfg.shell, command, &cwd, Duration::from_secs(timeout), ctx.cancel, &marker, &journal).await
}

/// Stop a command left running by a crashed harness. The process is only
/// killed if it still carries the marker it was started with, so a recycled
/// process id is never touched. Returns whether something was killed.
pub fn kill_orphan(state: &serde_json::Value) -> bool {
    let (Some(pgid), Some(marker)) = (state["pgid"].as_i64(), state["marker"].as_str()) else {
        return false;
    };
    let Ok(environ) = std::fs::read(format!("/proc/{pgid}/environ")) else {
        return false;
    };
    let needle = format!("NARSIL_CALL={marker}");
    if !environ.split(|b| *b == 0).any(|kv| kv == needle.as_bytes()) {
        return false;
    }
    unsafe { libc::killpg(pgid as i32, libc::SIGKILL) };
    true
}

fn capture<R: AsyncRead + Unpin + Send + 'static>(mut reader: R, buf: Arc<Mutex<Vec<u8>>>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut chunk = [0u8; 8192];
        loop {
            match reader.read(&mut chunk).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    // Keep draining past the limit so the child never blocks on a full pipe.
                    let mut b = buf.lock().unwrap_or_else(|e| e.into_inner());
                    if b.len() < CAPTURE_LIMIT {
                        b.extend_from_slice(&chunk[..n]);
                    }
                }
            }
        }
    })
}

enum Ended {
    Exited(std::process::ExitStatus),
    TimedOut,
    Cancelled,
}

pub async fn run_command(
    shell: &ShellConfig,
    command: &str,
    cwd: &Path,
    timeout: Duration,
    cancel: &CancellationToken,
    marker: &str,
    on_spawn: &(dyn Fn(i32) + Send + Sync),
) -> Result<ToolOutput> {
    if !cwd.is_dir() {
        return Ok(ToolOutput::err(format!("working directory does not exist: {}", cwd.display())));
    }
    let mut cmd = tokio::process::Command::new(&shell.program);
    cmd.arg("-c")
        .arg(command)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env("TERM", "dumb")
        .env("NARSIL", "1")
        .env("NARSIL_CALL", marker)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("PAGER", "cat")
        .envs(&shell.env)
        .kill_on_drop(true);
    // A new session: its own process group (so the whole tree can be killed)
    // and no controlling terminal (so nothing can prompt).
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = cmd.spawn().with_context(|| format!("starting {}", shell.program))?;
    let pgid = child.id().map(|p| p as i32);
    if let Some(pgid) = pgid {
        on_spawn(pgid);
    }
    let out = Arc::new(Mutex::new(Vec::new()));
    let err = Arc::new(Mutex::new(Vec::new()));
    let readers = [
        capture(child.stdout.take().expect("piped stdout"), out.clone()),
        capture(child.stderr.take().expect("piped stderr"), err.clone()),
    ];

    let ended = tokio::select! {
        status = child.wait() => Ended::Exited(status?),
        _ = tokio::time::sleep(timeout) => Ended::TimedOut,
        _ = cancel.cancelled() => Ended::Cancelled,
    };
    if !matches!(ended, Ended::Exited(_)) {
        if let Some(pgid) = pgid {
            unsafe { libc::killpg(pgid, libc::SIGTERM) };
            if tokio::time::timeout(Duration::from_secs(2), child.wait()).await.is_err() {
                unsafe { libc::killpg(pgid, libc::SIGKILL) };
            }
        }
        let _ = child.wait().await;
    }
    // Readers end when the pipes close. A background grandchild may hold them
    // open, so do not wait for long.
    for r in readers {
        let abort = r.abort_handle();
        if tokio::time::timeout(Duration::from_millis(400), r).await.is_err() {
            abort.abort();
        }
    }

    let take = |b: &Arc<Mutex<Vec<u8>>>| {
        let bytes = b.lock().unwrap_or_else(|e| e.into_inner());
        String::from_utf8_lossy(&bytes).trim_end().to_string()
    };
    let (stdout, stderr) = (take(&out), take(&err));
    let mut text = stdout;
    if !stderr.is_empty() {
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str("[stderr]\n");
        text.push_str(&stderr);
    }
    let note = |text: &mut String, n: String| {
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str(&n);
    };
    let is_error = match ended {
        Ended::Exited(status) if status.success() => false,
        Ended::Exited(status) => {
            match status.code() {
                Some(code) => note(&mut text, format!("[exit code {code}]")),
                None => note(&mut text, "[killed by a signal]".to_string()),
            }
            true
        }
        Ended::TimedOut => {
            note(&mut text, format!("[timed out after {}s and was killed; output above is partial]", timeout.as_secs()));
            true
        }
        Ended::Cancelled => {
            note(&mut text, "[interrupted before it finished and was killed; output above is partial]".to_string());
            true
        }
    };
    Ok(ToolOutput { content: text, is_error })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shell() -> ShellConfig {
        ShellConfig::default()
    }

    #[test]
    fn detects_privilege_escalation() {
        for c in ["sudo dnf install x", "ls && sudo -n true", "echo hi | sudo tee /etc/x", "su - root", "(sudo id)"] {
            assert!(uses_privilege(c), "{c}");
        }
        for c in ["ls /usr/share/sudoku", "echo pseudo", "cat sudoers.md", "git log --author=su"] {
            assert!(!uses_privilege(c), "{c}");
        }
    }

    #[tokio::test]
    async fn captures_output_and_exit_status() {
        let cwd = std::env::temp_dir();
        let cancel = CancellationToken::new();
        let ok = run_command(&shell(), "echo out; echo err >&2", &cwd, Duration::from_secs(10), &cancel, "", &|_| {}).await.unwrap();
        assert!(!ok.is_error);
        assert_eq!(ok.content, "out\n[stderr]\nerr");
        let bad = run_command(&shell(), "echo nope; exit 3", &cwd, Duration::from_secs(10), &cancel, "", &|_| {}).await.unwrap();
        assert!(bad.is_error);
        assert_eq!(bad.content, "nope\n[exit code 3]");
    }

    #[tokio::test]
    async fn timeout_and_cancel_kill_the_whole_process_group() {
        let cwd = std::env::temp_dir();
        let started = std::time::Instant::now();
        let cancel = CancellationToken::new();
        // The inner sleep is a grandchild; killing only the shell would leave it holding the pipe.
        let out = run_command(&shell(), "echo begun; (sleep 30; echo late) ; echo never", &cwd, Duration::from_millis(300), &cancel, "", &|_| {})
            .await
            .unwrap();
        assert!(out.is_error && out.content.contains("timed out") && out.content.starts_with("begun"));
        assert!(!out.content.contains("never"));

        let cancel2 = CancellationToken::new();
        let c = cancel2.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(200)).await;
            c.cancel();
        });
        let out = run_command(&shell(), "sleep 30", &cwd, Duration::from_secs(60), &cancel2, "", &|_| {}).await.unwrap();
        assert!(out.is_error && out.content.contains("interrupted"));
        assert!(started.elapsed() < Duration::from_secs(8), "neither case waited for the sleep");
    }

    #[tokio::test]
    async fn an_orphaned_command_is_recognized_by_its_marker_and_stopped() {
        let cwd = std::env::temp_dir();
        let seen = std::sync::Arc::new(std::sync::Mutex::new(None));
        let record = seen.clone();
        let on_spawn = move |pgid: i32| *record.lock().unwrap() = Some(pgid);
        let cancel = CancellationToken::new();
        let sh = shell();
        let task = tokio::spawn(async move {
            run_command(&sh, "sleep 30", &cwd, Duration::from_secs(60), &cancel, "sess:7:0", &on_spawn).await
        });
        let mut pgid = None;
        for _ in 0..100 {
            pgid = *seen.lock().unwrap();
            if pgid.is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let pgid = pgid.expect("the command started");
        tokio::time::sleep(Duration::from_millis(100)).await;
        // A marker from some other call does not match: nothing is touched.
        assert!(!kill_orphan(&serde_json::json!({ "pgid": pgid, "marker": "other:1:0" })));
        assert!(kill_orphan(&serde_json::json!({ "pgid": pgid, "marker": "sess:7:0" })));
        let out = tokio::time::timeout(Duration::from_secs(5), task).await.expect("ended promptly").unwrap().unwrap();
        assert!(out.is_error && out.content.contains("killed by a signal"), "{}", out.content);
    }

    #[tokio::test]
    async fn a_command_that_wants_a_terminal_fails_instead_of_hanging() {
        let cwd = std::env::temp_dir();
        let cancel = CancellationToken::new();
        let out = run_command(&shell(), "read -p 'password: ' x </dev/tty; echo got=$x", &cwd, Duration::from_secs(5), &cancel, "", &|_| {})
            .await
            .unwrap();
        assert!(out.content.contains("got="), "ran to completion: {}", out.content);
        assert!(!out.content.contains("timed out"));
    }
}
