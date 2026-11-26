//! Starting and stopping the daemon, and running it under systemd.

use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

use crate::client::Conn;
use crate::config::Paths;
use crate::proto::Command;
use crate::util;

const UNIT: &str = "narsil.service";

pub fn unit_path() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| util::home().join(".config"))
        .join("systemd/user")
        .join(UNIT)
}

/// The systemd unit only manages the default instance, not a NARSIL_HOME one.
fn managed_by_systemd() -> bool {
    std::env::var_os("NARSIL_HOME").is_none() && unit_path().exists()
}

fn systemctl(args: &[&str]) -> Result<bool> {
    let status = std::process::Command::new("systemctl").arg("--user").args(args).status().context("running systemctl")?;
    Ok(status.success())
}

pub async fn ping(paths: &Paths) -> Option<serde_json::Value> {
    let conn = Conn::connect(paths).await.ok()?;
    tokio::time::timeout(Duration::from_secs(3), conn.call(Command::Status)).await.ok()?.ok()
}

/// The narsil program to run in order to restart the daemon: the one this was
/// started as, found afresh, so that after an update it is the new one. (The
/// file this process itself runs from may have been replaced and be gone.)
pub fn program() -> std::path::PathBuf {
    let started_as = std::env::args_os().next().map(std::path::PathBuf::from).unwrap_or_else(|| "narsil".into());
    // With a folder in it, it names a file; a bare name is looked up on the PATH when it is run.
    if started_as.components().count() > 1 && started_as.exists() { started_as } else { "narsil".into() }
}

/// What to say if the running daemon was started before the narsil program
/// on disk was last replaced (an upgrade, a rebuild): it is still the old
/// version, and none of what is new is in force until it is restarted.
/// `status` is what the daemon answered to a status request.
pub fn outdated(status: &serde_json::Value) -> Option<String> {
    let up = Duration::from_secs(status["uptime_secs"].as_u64()?);
    let started = std::time::SystemTime::now().checked_sub(up)?;
    let replaced = std::env::current_exe().ok()?.metadata().ok()?.modified().ok()?;
    // A daemon started by this very program begins a moment after the file was written.
    (replaced > started + Duration::from_secs(2)).then(|| {
        "narsil has been updated since its daemon started, so the daemon is still the older version. \
         Run `narsil restart` (or /daemonrestart in the TUI) to use the new one: sessions and work in flight are kept."
            .to_string()
    })
}

fn read_pid(paths: &Paths) -> Option<i32> {
    std::fs::read_to_string(paths.pid_file()).ok()?.trim().parse().ok()
}

/// Whether a process is still running. One that has exited but has not been
/// collected by whoever started it (a TUI that started the daemon and is still
/// open) still has its number, and is not running.
fn pid_alive(pid: i32) -> bool {
    if unsafe { libc::kill(pid, 0) } != 0 {
        return false;
    }
    // Linux says what state it is in; elsewhere `ps` does.
    let state = match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        // "1234 (name) S ...": the state follows the name, which may itself hold spaces and brackets.
        Ok(stat) => stat.rsplit_once(')').and_then(|(_, rest)| rest.trim_start().chars().next()),
        Err(_) => std::process::Command::new("ps")
            .args(["-o", "stat=", "-p", &pid.to_string()])
            .output()
            .ok()
            .and_then(|o| String::from_utf8_lossy(&o.stdout).trim().chars().next()),
    };
    state != Some('Z')
}

/// Start the daemon in the background and wait until it answers.
pub async fn start(paths: &Paths, quiet: bool) -> Result<()> {
    paths.ensure()?;
    if ping(paths).await.is_some() {
        if !quiet {
            println!("narsil is already running");
        }
        return Ok(());
    }
    if managed_by_systemd() {
        if !systemctl(&["start", UNIT])? {
            bail!("systemctl --user start {UNIT} failed; see `journalctl --user -u narsil`");
        }
    } else {
        let exe = std::env::current_exe().context("locating the narsil binary")?;
        let log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(paths.log_file())
            .with_context(|| format!("opening {}", paths.log_file().display()))?;
        let mut cmd = std::process::Command::new(exe);
        cmd.arg("daemon").stdin(Stdio::null()).stdout(log.try_clone()?).stderr(log);
        // Detach from this terminal and session so the daemon outlives it.
        unsafe {
            cmd.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
        let mut daemon = cmd.spawn().context("starting the daemon")?;
        // If this process outlives the daemon (a TUI that started it), collect it when it
        // exits, so that it does not linger as a process that has ended but still has a number.
        std::thread::spawn(move || {
            let _ = daemon.wait();
        });
    }
    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline {
        if ping(paths).await.is_some() {
            if !quiet {
                println!("narsil started (log: {})", paths.log_file().display());
            }
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    bail!("the daemon did not come up; check {}", paths.log_file().display())
}

/// Ask the daemon to stop. Work in flight is suspended and resumes on the next start.
pub async fn stop(paths: &Paths) -> Result<()> {
    let pid = read_pid(paths);
    let running = ping(paths).await.is_some();
    if !running && !pid.is_some_and(pid_alive) {
        println!("narsil is not running");
        return Ok(());
    }
    if managed_by_systemd() {
        systemctl(&["stop", UNIT])?;
    } else if let Ok(conn) = Conn::connect(paths).await {
        let _ = conn.call(Command::Shutdown).await;
    } else if let Some(pid) = pid {
        unsafe { libc::kill(pid, libc::SIGTERM) };
    }
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline {
        let alive = pid.is_some_and(pid_alive) || ping(paths).await.is_some();
        if !alive {
            println!("narsil stopped; unfinished work resumes on the next start");
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
    if let Some(pid) = pid.filter(|p| pid_alive(*p)) {
        unsafe { libc::kill(pid, libc::SIGKILL) };
        println!("narsil did not stop in time and was killed; unfinished work resumes on the next start");
        return Ok(());
    }
    bail!("could not stop the daemon")
}

pub async fn status(paths: &Paths) -> Result<()> {
    let Some(st) = ping(paths).await else {
        println!("narsil is not running");
        return Ok(());
    };
    let up = st["uptime_secs"].as_u64().unwrap_or(0);
    println!(
        "narsil {} is running (pid {}, up {}h{:02}m){}",
        st["version"].as_str().unwrap_or("?"),
        st["pid"],
        up / 3600,
        (up % 3600) / 60,
        if managed_by_systemd() { ", managed by systemd" } else { "" }
    );
    if let Some(warning) = outdated(&st) {
        println!("  ! {warning}");
    }
    let model = st["model"].as_str().unwrap_or("");
    println!("  model:    {}", if model.is_empty() { "(not configured; run `narsil setup`)" } else { model });
    let running: Vec<&str> = st["running"].as_array().map(|a| a.iter().filter_map(|v| v.as_str()).collect()).unwrap_or_default();
    println!("  running:  {} session(s){}", running.len(), if running.is_empty() { String::new() } else { format!(": {}", running.join(", ")) });
    // What is in flight this moment, innermost steps only: a model call, a tool, a wait.
    let doing: Vec<crate::store::SpanRow> = serde_json::from_value(st["doing"].clone()).unwrap_or_default();
    let now = crate::telemetry::now_ms();
    for s in doing.iter().filter(|s| !doing.iter().any(|other| other.parent == Some(s.id))) {
        // Named as in the log, so the same id finds it there: session, or session>subagent name, and the goal.
        let who = crate::telemetry::Who::of(s);
        let what = util::first_line(&format!("{} {} {}", s.kind.replace('_', " "), s.what, s.text("summary")), 70);
        println!("  now:      [{who}] {} for {}", what.trim(), crate::telemetry::fmt_ms(now - s.start_ms));
    }
    let goals = st["goals"].as_array().cloned().unwrap_or_default();
    if goals.is_empty() {
        println!("  goals:    none open");
    }
    for g in goals {
        println!(
            "  goal {}  {:<9} turn {:<4} {}",
            g["id"].as_str().unwrap_or(""),
            g["status"].as_str().unwrap_or(""),
            g["iterations"],
            util::first_line(g["objective"].as_str().unwrap_or(""), 70)
        );
    }
    Ok(())
}

/// Install a systemd user unit so the daemon restarts after a crash and
/// starts at login.
pub fn install(paths: &Paths) -> Result<()> {
    let exe = std::env::current_exe().context("locating the narsil binary")?;
    let unit = format!(
        "[Unit]\nDescription=narsil agent harness\nAfter=network-online.target\n\n\
         [Service]\nExecStart={} daemon\nRestart=on-failure\nRestartSec=3\n\
         StandardOutput=append:{log}\nStandardError=append:{log}\n\n\
         [Install]\nWantedBy=default.target\n",
        exe.display(),
        log = paths.log_file().display()
    );
    let path = unit_path();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    paths.ensure()?;
    std::fs::write(&path, unit)?;
    println!("wrote {}", path.display());
    if !systemctl(&["daemon-reload"])? || !systemctl(&["enable", UNIT])? {
        bail!("systemctl could not enable the unit");
    }
    println!("enabled: narsil now starts at login and restarts if it crashes");
    println!();
    println!("Two things to know:");
    println!("  - A service does not see your shell's environment. API keys must be in");
    println!("    {} (narsil setup offers to store them).", paths.credentials_file().display());
    println!("  - User services stop when you log out. For work that should continue after");
    println!("    logout or across reboots, run:  loginctl enable-linger {}", util::current_user());
    println!();
    println!("Start it now with: narsil start");
    Ok(())
}

pub fn uninstall() -> Result<()> {
    let path = unit_path();
    if !path.exists() {
        println!("no systemd unit is installed");
        return Ok(());
    }
    let _ = systemctl(&["disable", "--now", UNIT]);
    std::fs::remove_file(&path)?;
    let _ = systemctl(&["daemon-reload"]);
    println!("removed {}", path.display());
    Ok(())
}
