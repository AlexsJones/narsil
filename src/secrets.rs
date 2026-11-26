//! Handling credentials with care.
//!
//! An agent has to work with secrets: a token to push with, a key to call an
//! API. Seeing one is not the problem. What matters is that it is used only
//! where the task needs it, and that it does not spread: into memory that is
//! read into every later session, into notes that sync to other machines,
//! onto the screen, into a trace someone pastes into an issue.
//!
//! So nothing here stops the agent reading or using a credential. Instead:
//!
//!   - when a tool result holds one, or a credential store was read, a line
//!     is added to that result saying how to handle it (`reminder`);
//!   - what narsil itself writes down to keep (memory, work notes, a goal's
//!     journal) has anything shaped like a secret masked (`mask`);
//!   - so does what it shows a person: the TUI, `narsil trace`.
//!
//! The model is still sent the real text of what a tool returned, because it
//! may need it. This is care, not a sandbox.

use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use regex::Regex;

use crate::util::home;

/// Files under the home directory that hold credentials and nothing else.
const FILES: [&str; 14] = [
    ".git-credentials",
    ".netrc",
    ".pgpass",
    ".pypirc",
    ".aws/credentials",
    ".docker/config.json",
    ".kube/config",
    ".config/gh/hosts.yml",
    ".cargo/credentials.toml",
    ".cargo/credentials",
    ".config/gcloud/application_default_credentials.json",
    ".config/gcloud/credentials.db",
    ".config/gcloud/access_tokens.db",
    ".config/hub",
];

/// Folders under the home directory where everything is a secret.
const FOLDERS: [&str; 4] = [".gnupg/private-keys-v1.d", ".password-store", ".local/share/keyrings", ".config/gcloud/legacy_credentials"];

/// What in `~/.ssh` is not a private key.
const SSH_OPEN: [&str; 5] = ["config", "known_hosts", "known_hosts.old", "authorized_keys", "environment"];

/// Where narsil keeps its own API keys.
fn own_credentials() -> PathBuf {
    crate::config::Paths::resolve().credentials_file()
}

/// Whether a path, as written, is a credential store.
fn is_store_as_written(path: &Path) -> bool {
    if path == own_credentials() {
        return true;
    }
    let Ok(rel) = path.strip_prefix(home()) else { return false };
    if FILES.iter().any(|f| rel == Path::new(f)) || FOLDERS.iter().any(|f| rel.starts_with(f)) {
        return true;
    }
    match rel.strip_prefix(".ssh") {
        Ok(inside) if !inside.as_os_str().is_empty() => {
            let name = inside.to_string_lossy();
            !(SSH_OPEN.contains(&name.as_ref()) || name.ends_with(".pub") || inside.components().count() > 1 && name.starts_with("config.d"))
        }
        _ => false,
    }
}

/// Whether a path on disk is a credential store, however it is written
/// (`..`, a link to it).
pub fn is_store(path: &Path) -> bool {
    let written = crate::obsidian::lexical(path);
    is_store_as_written(&written) || std::fs::canonicalize(path).is_ok_and(|real| real != written && is_store_as_written(&real))
}

/// The credential store a shell command names, if it names one: by its path
/// from the home directory in any of the usual spellings, or as a word that
/// resolves to one from the directory the command runs in.
pub fn named_in(command: &str, cwd: &Path) -> Option<String> {
    let home_dir = home();
    for word in command.split(|c: char| c.is_whitespace() || matches!(c, ';' | '|' | '&' | '<' | '>' | '(' | ')' | '=' | '`')) {
        let word = word.trim_matches(|c| matches!(c, '"' | '\'' | ',' | ':'));
        if word.is_empty() || word.starts_with('-') {
            continue;
        }
        let spelled = word
            .strip_prefix("~/")
            .or_else(|| word.strip_prefix("$HOME/"))
            .or_else(|| word.strip_prefix("${HOME}/"))
            .map(|rest| home_dir.join(rest))
            .unwrap_or_else(|| if Path::new(word).is_absolute() { PathBuf::from(word) } else { cwd.join(word) });
        if is_store(&spelled) {
            return Some(word.to_string());
        }
    }
    None
}

/// Shapes that are a secret wherever they appear, and what to call each.
static SHAPES: LazyLock<Vec<(Regex, &'static str)>> = LazyLock::new(|| {
    [
        (r"-----BEGIN [A-Z ]*PRIVATE KEY-----[\s\S]*?(?:-----END [A-Z ]*PRIVATE KEY-----|\z)", "a private key"),
        (r"\bgithub_pat_[A-Za-z0-9_]{30,}", "a GitHub token"),
        (r"\bgh[pousr]_[A-Za-z0-9]{30,}", "a GitHub token"),
        (r"\bglpat-[A-Za-z0-9_\-]{20,}", "a GitLab token"),
        (r"\bsk-ant-[A-Za-z0-9_\-]{20,}", "an Anthropic API key"),
        (r"\bsk-[A-Za-z0-9_\-]{24,}", "an API key"),
        (r"\bhf_[A-Za-z0-9]{30,}", "a Hugging Face token"),
        (r"\bnpm_[A-Za-z0-9]{30,}", "an npm token"),
        (r"\bxox[baprs]-[A-Za-z0-9\-]{10,}", "a Slack token"),
        (r"\bAIza[0-9A-Za-z_\-]{35}", "a Google API key"),
        (r"\bAKIA[0-9A-Z]{16}\b", "an AWS access key"),
        (r"(?i)\b(aws_secret_access_key\s*[=:]\s*)[A-Za-z0-9/+=]{30,}", "an AWS secret key"),
        (r"(?i)\b(authorization:\s*(?:bearer|token|basic)\s+)[A-Za-z0-9._~+/=\-]{16,}", "a credential"),
    ]
    .into_iter()
    .map(|(pattern, what)| (Regex::new(pattern).expect("a valid pattern"), what))
    .collect()
});

/// `https://user:password@host`: the password is the secret.
static IN_URL: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\b([a-z][a-z0-9+.\-]*://[^/\s:@]+:)[^/\s@]{3,}@").expect("a valid pattern"));

/// Whether to mask in what is kept and shown: `[permissions] mask_secrets`.
static MASKING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);

pub fn set_masking(on: bool) {
    MASKING.store(on, std::sync::atomic::Ordering::Relaxed);
}

/// A text as narsil keeps it (a memory entry, a work note, a goal's journal)
/// or shows it (the TUI, a trace): with anything shaped like a secret masked.
pub fn kept(text: &str) -> String {
    if MASKING.load(std::sync::atomic::Ordering::Relaxed) { mask(text).0 } else { text.to_string() }
}

/// What to tell the agent when something it asked to have kept had a secret
/// in it, which was masked.
pub fn masked_note(text: &str) -> &'static str {
    if MASKING.load(std::sync::atomic::Ordering::Relaxed) && !kinds(text).is_empty() {
        " A credential in it was masked: this is kept as plain text and read into later sessions. Say where the \
         credential lives (a file, a variable, a password manager), not what it is."
    } else {
        ""
    }
}

/// The kinds of secret a text holds, each named once.
pub fn kinds(text: &str) -> Vec<&'static str> {
    let mut found: Vec<&'static str> = SHAPES.iter().filter(|(shape, _)| shape.is_match(text)).map(|(_, what)| *what).collect();
    if IN_URL.is_match(text) {
        found.push("a password");
    }
    found.dedup();
    found
}

/// What to add to a tool result that holds a credential, or that read a
/// credential store: how to handle it. Said at the moment it applies.
pub fn reminder(holds: &[&str], store: Option<&str>) -> String {
    let what = match (holds.is_empty(), store) {
        (false, _) => format!("This output holds {}.", holds.join(" and ")),
        (true, Some(store)) => format!("{store} is a credential store."),
        (true, None) => return String::new(),
    };
    format!(
        "\n\n[narsil] {what} Use it only if the task in hand needs it, and only with the service it belongs to. Do not \
         repeat it in your replies, and do not put it in memory, notes, work items, files, commit messages or any \
         command that would send it somewhere else. If the task did not call for it, leave it alone and move on."
    )
}

/// A text with anything shaped like a secret masked, and how many were.
pub fn mask(text: &str) -> (String, usize) {
    let mut out = std::borrow::Cow::Borrowed(text);
    let mut found = 0;
    for (shape, what) in SHAPES.iter() {
        if shape.is_match(&out) {
            found += shape.find_iter(&out).count();
            out = std::borrow::Cow::Owned(
                shape
                    .replace_all(&out, |c: &regex::Captures| {
                        // A label before the secret (`Authorization: Bearer `) stays, so the line still reads.
                        format!("{}[masked by narsil: {what}]", c.get(1).map(|m| m.as_str()).unwrap_or(""))
                    })
                    .into_owned(),
            );
        }
    }
    if IN_URL.is_match(&out) {
        found += IN_URL.find_iter(&out).count();
        out = std::borrow::Cow::Owned(IN_URL.replace_all(&out, "${1}[masked by narsil: a password]@").into_owned());
    }
    (out.into_owned(), found)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credential_stores_are_known_by_where_they_are() {
        let h = home();
        for path in [".git-credentials", ".netrc", ".aws/credentials", ".ssh/id_ed25519", ".ssh/id_rsa", ".ssh/work_key", ".kube/config", ".config/gh/hosts.yml", ".gnupg/private-keys-v1.d/ABC.key", ".password-store/web/site.gpg"] {
            assert!(is_store(&h.join(path)), "{path} is a credential store");
        }
        assert!(is_store(&own_credentials()), "narsil's own keys");
        assert!(is_store(&h.join("Code/../.git-credentials")), "however the path is written");
        for path in [".gitconfig", ".ssh/config", ".ssh/known_hosts", ".ssh/id_ed25519.pub", ".ssh", ".aws/config", ".zshrc", "Code/project/.git-credentials-notes.md", "notes/credentials.md"] {
            assert!(!is_store(&h.join(path)), "{path} is not");
        }
        assert!(!is_store(Path::new("/etc/hosts")));
    }

    #[test]
    fn a_command_that_names_a_credential_store_is_recognised() {
        let h = home();
        let elsewhere = Path::new("/tmp");
        for command in [
            "cat ~/.git-credentials",
            "cat /etc/hosts; cat $HOME/.netrc",
            "head -3 \"${HOME}/.aws/credentials\"",
            &format!("cat {}/.ssh/id_ed25519", h.display()),
            "cp ~/.kube/config /tmp/k",
            "base64 <~/.git-credentials",
        ] {
            assert!(named_in(command, elsewhere).is_some(), "{command} names a credential store");
        }
        // Named from where the command runs.
        assert!(named_in("cat .git-credentials", &h).is_some());
        assert!(named_in("cat ../.git-credentials", &h.join("Code")).is_some());
        for command in ["cat ~/.gitconfig", "ls -la ~/.ssh", "cat ~/.ssh/id_ed25519.pub", "git push origin main", "grep -r credentials src/", "aws s3 ls"] {
            assert_eq!(named_in(command, elsewhere), None, "{command} is fine");
        }
        assert_eq!(named_in("cat .git-credentials", elsewhere), None, "a file of the same name somewhere else is not the store");
    }

    #[test]
    fn anything_shaped_like_a_secret_is_masked() {
        let token = format!("ghp_{}", "a1B2c3D4e5".repeat(4));
        let (masked, n) = mask(&format!("https://someone:{token}@github.com\nnext line"));
        assert_eq!(n, 1);
        assert!(!masked.contains(&token) && masked.contains("[masked by narsil: a GitHub token]") && masked.ends_with("next line"), "{masked}");

        let all = [
            format!("export ANTHROPIC_API_KEY=sk-ant-api03-{}", "x".repeat(40)),
            format!("OPENAI_API_KEY=sk-proj-{}", "Y9".repeat(20)),
            format!("github_pat_{}", "Z".repeat(60)),
            "AKIAABCDEFGHIJKLMNOP".to_string(),
            format!("aws_secret_access_key = {}", "k".repeat(40)),
            format!("Authorization: Bearer {}", "t0k3n".repeat(8)),
            "postgres://app:hunter2secret@db.internal:5432/app".to_string(),
            "-----BEGIN OPENSSH PRIVATE KEY-----\nb3BlbnNzaC1rZXktdjEAAAAA\nmorelines\n-----END OPENSSH PRIVATE KEY-----".to_string(),
        ];
        for secret in &all {
            let (masked, n) = mask(&format!("before\n{secret}\nafter"));
            assert!(n >= 1, "{secret} was not seen");
            assert!(masked.starts_with("before\n") && masked.ends_with("\nafter") && masked.contains("[masked by narsil: "), "{masked}");
            // The secret part is gone: for a URL that is the password, for the rest it runs to the end.
            let part: String = if secret.contains("://") { "hunter2secret".into() } else { secret.chars().rev().take(12).collect::<String>().chars().rev().collect() };
            assert!(!masked.contains(&part), "{part} is still there: {masked}");
        }
        // What stays readable: the label before a secret, and the host a password was for.
        assert!(mask("Authorization: Bearer abcdefghijklmnopqrstuvwxyz").0.starts_with("Authorization: Bearer [masked"));
        assert!(mask("https://u:supersecretpw@example.org/x").0.contains("https://u:[masked by narsil: a password]@example.org/x"));

        // Which kinds, for saying so; and what is said.
        assert_eq!(kinds(&format!("x {token} y https://u:supersecretpw@h/")), vec!["a GitHub token", "a password"]);
        assert!(kinds("nothing here").is_empty());
        let said = reminder(&["a GitHub token"], None);
        assert!(said.contains("holds a GitHub token") && said.contains("only with the service it belongs to") && said.contains("do not put it in memory"), "{said}");
        assert!(reminder(&[], Some("~/.git-credentials")).contains("~/.git-credentials is a credential store."));
        assert!(reminder(&[], None).is_empty());

        // Ordinary text is left exactly as it was.
        for plain in [
            "commit 3f2a9c1e8b7d6f5a4c3b2a1908f7e6d5c4b3a291",
            "sha256:9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08",
            "https://github.com/AlexsJones/narsil.git",
            "git@github.com:AlexsJones/narsil.git",
            "the task-id is sk-1 and the ask is small",
            "user@example.com: 3 files changed",
            "http://localhost:8080/path?x=1",
        ] {
            assert_eq!(mask(plain), (plain.to_string(), 0), "{plain}");
        }
    }
}
