//! Voice input and output through commands you configure, so any recorder,
//! speech-to-text engine or TTS can be plugged in.
//!
//!   record_command      records until interrupted; `{file}` is the WAV path
//!   transcribe_command  prints the transcript of `{file}` on stdout
//!   speak_command       reads text on stdin and speaks it

use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use tokio::io::AsyncWriteExt;

use crate::config::{Paths, VoiceConfig};
use crate::util::{new_id, shell_quote};

pub struct Recorder {
    child: tokio::process::Child,
    file: PathBuf,
}

pub fn configured(cfg: &VoiceConfig) -> Result<()> {
    if !cfg.enabled {
        bail!("voice is off: set enabled = true under [voice] in config.toml");
    }
    if cfg.record_command.trim().is_empty() || cfg.transcribe_command.trim().is_empty() {
        bail!("voice needs record_command and transcribe_command under [voice] in config.toml");
    }
    Ok(())
}

fn shell(command: &str) -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new("sh");
    cmd.arg("-c").arg(command).stdin(Stdio::null()).kill_on_drop(true);
    cmd
}

impl Recorder {
    pub fn start(cfg: &VoiceConfig, paths: &Paths) -> Result<Recorder> {
        configured(cfg)?;
        let file = paths.runtime_dir.join(format!("voice-{}.wav", new_id()));
        let command = cfg.record_command.replace("{file}", &shell_quote(&file.to_string_lossy()));
        // `exec` so the signal that ends the recording reaches the recorder itself.
        let child = shell(&format!("exec {command}"))
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .context("starting the recorder")?;
        Ok(Recorder { child, file })
    }

    /// Stop recording and return the transcript.
    pub async fn finish(mut self, cfg: &VoiceConfig) -> Result<String> {
        if let Some(pid) = self.child.id() {
            unsafe { libc::kill(pid as i32, libc::SIGINT) };
        }
        if tokio::time::timeout(Duration::from_secs(5), self.child.wait()).await.is_err() {
            let _ = self.child.kill().await;
        }
        let file = self.file.clone();
        let result = async {
            if !file.exists() {
                bail!("the recorder produced no audio file");
            }
            let command = cfg.transcribe_command.replace("{file}", &shell_quote(&file.to_string_lossy()));
            let out = tokio::time::timeout(
                Duration::from_secs(180),
                shell(&command).stdout(Stdio::piped()).stderr(Stdio::piped()).output(),
            )
            .await
            .context("transcription timed out")??;
            if !out.status.success() {
                bail!("transcription failed: {}", String::from_utf8_lossy(&out.stderr).trim());
            }
            let text = String::from_utf8_lossy(&out.stdout).split_whitespace().collect::<Vec<_>>().join(" ");
            Ok(text)
        }
        .await;
        let _ = std::fs::remove_file(&self.file);
        result
    }
}

/// Speak text with the configured command, without waiting for it to finish.
pub fn speak(cfg: &VoiceConfig, text: String) {
    if cfg.speak_command.trim().is_empty() || text.trim().is_empty() {
        return;
    }
    let command = cfg.speak_command.clone();
    tokio::spawn(async move {
        let mut cmd = tokio::process::Command::new("sh");
        cmd.arg("-c").arg(command).stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::null());
        if let Ok(mut child) = cmd.spawn() {
            if let Some(mut stdin) = child.stdin.take() {
                let _ = stdin.write_all(text.as_bytes()).await;
            }
            let _ = child.wait().await;
        }
    });
}
