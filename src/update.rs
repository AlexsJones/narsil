//! `narsil update`: bring the installed narsil up to date by whatever put it
//! there, then restart the daemon, so that the new version is the one that is
//! running and not only the one on disk.
//!
//!   - installed with Homebrew: `brew upgrade`;
//!   - installed with cargo: the same `cargo install` again, from the checkout
//!     or the repository it came from;
//!   - a binary downloaded from a release: the latest release's binary for
//!     this machine, checked against the published checksums, swapped in.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, anyhow, bail};
use sha2::{Digest, Sha256};

use crate::config::Paths;

/// How this narsil came to be installed.
#[derive(Debug, PartialEq)]
pub enum Install {
    /// By Homebrew, under this prefix.
    Brew { prefix: PathBuf },
    /// By `cargo install`, into this root (`~/.cargo` unless told otherwise).
    Cargo { root: PathBuf, source: Source },
    /// A binary put somewhere by hand.
    Standalone { exe: PathBuf },
}

/// What `cargo install` built from.
#[derive(Debug, PartialEq)]
pub enum Source {
    /// A checkout on this machine.
    Path(PathBuf),
    /// A git repository.
    Git(String),
}

/// The repository releases come from, as `owner/name`.
fn repository() -> &'static str {
    env!("CARGO_PKG_REPOSITORY").trim_start_matches("https://github.com/").trim_end_matches('/')
}

/// Work out how narsil was installed from where its program file is.
pub fn detect(exe: &Path) -> Install {
    let text = exe.to_string_lossy();
    for marker in ["/Caskroom/narsil/", "/Cellar/narsil/"] {
        if let Some(at) = text.find(marker) {
            return Install::Brew { prefix: PathBuf::from(&text[..at]) };
        }
    }
    // cargo keeps a record of what it installed, beside the `bin` folder.
    if let Some(root) = exe.parent().filter(|bin| bin.file_name().is_some_and(|n| n == "bin")).and_then(Path::parent) {
        if let Some(source) = std::fs::read_to_string(root.join(".crates2.json")).ok().and_then(|record| cargo_source(&record)) {
            return Install::Cargo { root: root.to_path_buf(), source };
        }
    }
    Install::Standalone { exe: exe.to_path_buf() }
}

/// Where cargo says narsil was built from, out of its `.crates2.json`.
fn cargo_source(record: &str) -> Option<Source> {
    let record: serde_json::Value = serde_json::from_str(record).ok()?;
    // Keys read `narsil 0.1.0 (path+file:///home/me/narsil)`.
    let key = record.get("installs")?.as_object()?.keys().find(|k| k.starts_with("narsil "))?;
    let source = key.split_once('(')?.1.trim_end_matches(')');
    if let Some(dir) = source.strip_prefix("path+file://") {
        return Some(Source::Path(PathBuf::from(dir)));
    }
    let url = source.strip_prefix("git+").map(|u| u.split(['#', '?']).next().unwrap_or(u).to_string());
    Some(Source::Git(url.unwrap_or_else(|| env!("CARGO_PKG_REPOSITORY").to_string())))
}

/// The numbers of a version: `v0.2.1-dev+abc` is (0, 2, 1), and not a finished release.
fn numbers(version: &str) -> ((u64, u64, u64), bool) {
    let v = version.trim().trim_start_matches('v');
    let (core, rest) = v.split_at(v.find(['-', '+']).unwrap_or(v.len()));
    let mut parts = core.split('.').map(|p| p.parse::<u64>().unwrap_or(0));
    ((parts.next().unwrap_or(0), parts.next().unwrap_or(0), parts.next().unwrap_or(0)), rest.is_empty())
}

/// Whether `latest` is a version to move to from `current`. A build made
/// between releases gives way to the release of the same number.
pub fn newer(latest: &str, current: &str) -> bool {
    let ((l, _), (c, finished)) = (numbers(latest), numbers(current));
    l > c || (l == c && !finished)
}

/// The release archive for the machine this is running on.
pub fn asset_name(version: &str) -> Result<String> {
    let os = match std::env::consts::OS {
        "macos" => "darwin",
        "linux" => "linux",
        other => bail!("there are no released binaries for {other}"),
    };
    let arch = match std::env::consts::ARCH {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        other => bail!("there are no released binaries for {other}"),
    };
    Ok(format!("narsil_{version}_{os}_{arch}.tar.gz"))
}

fn http() -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder().user_agent("narsil").timeout(std::time::Duration::from_secs(120)).build()?)
}

/// The version of the latest release, without its `v`.
pub async fn latest_release() -> Result<String> {
    let url = format!("https://api.github.com/repos/{}/releases/latest", repository());
    let reply = http()?.get(&url).send().await.context("asking GitHub for the latest release")?;
    if !reply.status().is_success() {
        bail!("GitHub answered {} when asked for the latest release", reply.status());
    }
    let release: serde_json::Value = reply.json().await?;
    let tag = release["tag_name"].as_str().ok_or_else(|| anyhow!("the latest release has no tag"))?;
    Ok(tag.trim_start_matches('v').to_string())
}

/// The checksum a release publishes for one of its files.
fn published_sum(checksums: &str, name: &str) -> Option<String> {
    checksums.lines().find_map(|line| {
        let mut parts = line.split_whitespace();
        let (sum, file) = (parts.next()?, parts.next()?);
        (file.trim_start_matches('*') == name).then(|| sum.to_lowercase())
    })
}

/// Download a release's binary for this machine, check it against the
/// release's checksums, and put it in place of `exe`.
async fn replace_from_release(version: &str, exe: &Path) -> Result<()> {
    let name = asset_name(version)?;
    let base = format!("https://github.com/{}/releases/download/v{version}", repository());
    let client = http()?;
    let fetch = |what: String| {
        let client = client.clone();
        async move {
            let reply = client.get(&what).send().await.with_context(|| format!("downloading {what}"))?;
            if !reply.status().is_success() {
                bail!("{what}: {}", reply.status());
            }
            Ok::<_, anyhow::Error>(reply.bytes().await?)
        }
    };
    println!("downloading {name}");
    let archive = fetch(format!("{base}/{name}")).await?;
    let checksums = String::from_utf8_lossy(&fetch(format!("{base}/checksums.txt")).await?).into_owned();
    let wanted = published_sum(&checksums, &name).ok_or_else(|| anyhow!("the release's checksums do not list {name}"))?;
    let got: String = Sha256::digest(&archive).iter().map(|byte| format!("{byte:02x}")).collect();
    if got != wanted {
        bail!("{name} does not match its published checksum (expected {wanted}, got {got}); nothing was changed");
    }

    // Unpack beside the program, so that swapping it in is a rename on one filesystem.
    let dir = exe.parent().ok_or_else(|| anyhow!("{} has no folder", exe.display()))?;
    let work = dir.join(format!(".narsil-update-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&work);
    std::fs::create_dir_all(&work).with_context(|| format!("{} cannot be written to; update narsil the way it was installed", dir.display()))?;
    let outcome = (|| -> Result<()> {
        let packed = work.join(&name);
        std::fs::write(&packed, &archive)?;
        let unpacked = Command::new("tar").arg("-xzf").arg(&packed).arg("-C").arg(&work).arg("narsil").status().context("running tar")?;
        if !unpacked.success() {
            bail!("could not unpack {name}");
        }
        let fresh = work.join("narsil");
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&fresh, std::fs::Permissions::from_mode(0o755))?;
        // The running program keeps the file it was started from; the name now leads to the new one.
        std::fs::rename(&fresh, exe).with_context(|| format!("replacing {}", exe.display()))
    })();
    let _ = std::fs::remove_dir_all(&work);
    outcome
}

fn run(command: &mut Command) -> Result<()> {
    let shown = format!("{} {}", command.get_program().to_string_lossy(), command.get_args().map(|a| a.to_string_lossy()).collect::<Vec<_>>().join(" "));
    println!("$ {shown}");
    let status = command.status().with_context(|| format!("running {shown}"))?;
    if !status.success() {
        bail!("`{shown}` failed; narsil was not updated");
    }
    Ok(())
}

pub struct Opts {
    /// Say whether there is something newer, and change nothing.
    pub check: bool,
    /// Update the program but leave the running daemon as it is.
    pub no_restart: bool,
}

pub async fn update(paths: &Paths, opts: Opts) -> Result<()> {
    let exe = std::env::current_exe().context("finding the narsil program")?;
    let install = detect(&exe);
    let current = crate::VERSION;
    // Where the program will be once updated: Homebrew moves it to a folder named for the new version.
    let (how, launcher) = match &install {
        Install::Brew { prefix } => (format!("Homebrew ({})", prefix.display()), prefix.join("bin/narsil")),
        Install::Cargo { root, source: Source::Path(dir) } => (format!("cargo, from the checkout at {}", dir.display()), root.join("bin/narsil")),
        Install::Cargo { root, source: Source::Git(url) } => (format!("cargo, from {url}"), root.join("bin/narsil")),
        Install::Standalone { exe } => (format!("a downloaded binary at {}", exe.display()), exe.clone()),
    };
    println!("narsil {current}, installed with {how}");

    // A checkout is updated to whatever it holds; everything else to the latest release.
    let from_checkout = matches!(install, Install::Cargo { source: Source::Path(_), .. });
    let latest = if from_checkout { None } else { Some(latest_release().await?) };
    if let Some(latest) = &latest {
        if !newer(latest, current) {
            println!("this is the latest release ({latest})");
            if opts.check {
                return Ok(());
            }
        } else {
            println!("the latest release is {latest}");
        }
    }
    if opts.check {
        if from_checkout {
            println!("`narsil update` rebuilds from the checkout as it stands");
        }
        return Ok(());
    }

    let mut changed = true;
    match &install {
        Install::Brew { prefix } => {
            let brew = prefix.join("bin/brew");
            let brew = if brew.exists() { brew } else { PathBuf::from("brew") };
            // The tap first, so that Homebrew knows of the release at all.
            run(Command::new(&brew).args(["update", "--quiet"]))?;
            run(Command::new(&brew).args(["upgrade", "--cask", "narsil"]))?;
        }
        Install::Cargo { root, source } => {
            let mut cargo = Command::new("cargo");
            cargo.args(["install", "--locked", "--root"]).arg(root);
            match source {
                Source::Path(dir) if dir.join("Cargo.toml").is_file() => cargo.arg("--path").arg(dir),
                Source::Path(dir) => bail!("narsil was installed from {}, which is no longer there. Install it again from a checkout, or with Homebrew.", dir.display()),
                Source::Git(url) => cargo.args(["--git", url, "narsil"]),
            };
            run(&mut cargo)?;
        }
        Install::Standalone { exe } => match &latest {
            Some(latest) if newer(latest, current) => replace_from_release(latest, exe).await?,
            _ => changed = false,
        },
    }

    let now = Command::new(&launcher).arg("--version").output().ok().map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string()).unwrap_or_default();
    if !now.is_empty() {
        println!("{}{now}", if changed { "now " } else { "" });
    }

    // The daemon is the old program until it is started again.
    let Some(status) = crate::service::ping(paths).await else {
        println!("the daemon is not running; it starts with the next `narsil`");
        return Ok(());
    };
    if opts.no_restart {
        println!("the daemon was left running as it was: `narsil restart` puts the new version to work");
        return Ok(());
    }
    if !changed && crate::service::outdated(&status).is_none() {
        println!("the daemon is already this version; nothing to restart");
        return Ok(());
    }
    let working = status["running"].as_array().map(|a| a.len()).unwrap_or(0);
    if working > 0 {
        println!("{working} session(s) are at work: they are suspended and picked up again by the new daemon");
    }
    println!("restarting the daemon");
    let restarted = Command::new(&launcher).arg("restart").status().with_context(|| format!("running {} restart", launcher.display()))?;
    if !restarted.success() {
        bail!("narsil was updated, but the daemon did not restart: run `narsil restart`");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn how_it_was_installed_is_read_from_where_it_is() {
        assert_eq!(
            detect(Path::new("/home/linuxbrew/.linuxbrew/Caskroom/narsil/0.1.0/narsil")),
            Install::Brew { prefix: PathBuf::from("/home/linuxbrew/.linuxbrew") }
        );
        assert_eq!(detect(Path::new("/opt/homebrew/Caskroom/narsil/0.2.0/narsil")), Install::Brew { prefix: PathBuf::from("/opt/homebrew") });
        assert_eq!(detect(Path::new("/usr/local/Cellar/narsil/0.2.0/bin/narsil")), Install::Brew { prefix: PathBuf::from("/usr/local") });

        // cargo's own record says what it built from.
        let root = std::env::temp_dir().join(format!("narsil-update-test-{}", std::process::id()));
        std::fs::create_dir_all(root.join("bin")).unwrap();
        let exe = root.join("bin/narsil");
        assert_eq!(detect(&exe), Install::Standalone { exe: exe.clone() }, "a bin folder with no record beside it is just a folder");
        std::fs::write(root.join(".crates2.json"), r#"{"installs":{"ripgrep 14.0.0 (registry+https://github.com/rust-lang/crates.io-index)":{},"narsil 0.1.0 (path+file:///home/me/code/narsil)":{"bins":["narsil"]}}}"#).unwrap();
        assert_eq!(detect(&exe), Install::Cargo { root: root.clone(), source: Source::Path(PathBuf::from("/home/me/code/narsil")) });
        std::fs::write(root.join(".crates2.json"), r#"{"installs":{"narsil 0.1.0 (git+https://github.com/AlexsJones/narsil?tag=v0.1.0#abc123)":{}}}"#).unwrap();
        assert_eq!(detect(&exe), Install::Cargo { root: root.clone(), source: Source::Git("https://github.com/AlexsJones/narsil".into()) });
        std::fs::write(root.join(".crates2.json"), r#"{"installs":{"ripgrep 14.0.0 (registry+x)":{}}}"#).unwrap();
        assert_eq!(detect(&exe), Install::Standalone { exe: exe.clone() }, "cargo put other things here, but not narsil");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_release_is_newer_by_its_numbers() {
        for (latest, current, expected) in [
            ("0.2.0", "0.1.0", true),
            ("0.1.1", "0.1.0", true),
            ("1.0.0", "0.9.9", true),
            ("0.1.10", "0.1.9", true),
            ("0.1.0", "0.1.0", false),
            ("0.1.0", "0.2.0", false),
            ("v0.1.1", "0.1.0", true),
            // A build made between releases gives way to the release it was leading up to.
            ("0.1.1", "0.1.1-dev+abc123", true),
            ("0.1.0", "0.1.1-dev+abc123", false),
        ] {
            assert_eq!(newer(latest, current), expected, "{latest} against {current}");
        }
    }

    #[test]
    fn the_archive_and_its_checksum_are_found_by_name() {
        let name = asset_name("0.3.0").unwrap();
        assert!(name.starts_with("narsil_0.3.0_") && name.ends_with(".tar.gz"), "{name}");
        assert!(["_linux_amd64", "_linux_arm64", "_darwin_amd64", "_darwin_arm64"].iter().any(|m| name.contains(m)), "{name}");
        let sums = "aaa111  narsil_0.3.0_linux_amd64.tar.gz\nBBB222 *narsil_0.3.0_darwin_arm64.tar.gz\n";
        assert_eq!(published_sum(sums, "narsil_0.3.0_linux_amd64.tar.gz").as_deref(), Some("aaa111"));
        assert_eq!(published_sum(sums, "narsil_0.3.0_darwin_arm64.tar.gz").as_deref(), Some("bbb222"));
        assert_eq!(published_sum(sums, "narsil_0.3.0_linux_arm64.tar.gz"), None);
        assert_eq!(repository(), "AlexsJones/narsil");
    }
}
