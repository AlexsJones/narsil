//! `narsil setup`: choose a backend, store its key, pick a model, check that it
//! answers, and configure permissions and sudo. Safe to run again at any time;
//! existing settings are kept unless you change them.

use std::time::Duration;

use anyhow::{Context, Result, bail};
use dialoguer::theme::ColorfulTheme;
use dialoguer::{Confirm, Input, Password, Select};

use crate::config::{Config, Credentials, Paths, ProviderConfig, write_default_guidance};
use crate::provider::{self, ChatRequest, StreamEvent};
use crate::types::Message;
use crate::util;

#[derive(Default)]
pub struct SetupOpts {
    pub provider: Option<String>,
    pub name: Option<String>,
    pub base_url: Option<String>,
    pub api_key_env: Option<String>,
    pub model: Option<String>,
    pub yes: bool,
    pub no_test: bool,
    /// A vault folder, or `none`.
    pub obsidian: Option<String>,
}

struct Preset {
    key: &'static str,
    label: &'static str,
    kind: &'static str,
    base_url: &'static str,
    env: &'static str,
    default_model: &'static str,
}

const PRESETS: &[Preset] = &[
    Preset { key: "anthropic", label: "Anthropic (Claude)", kind: "anthropic", base_url: "https://api.anthropic.com", env: "ANTHROPIC_API_KEY", default_model: "claude-opus-5-5" },
    Preset { key: "openai", label: "OpenAI", kind: "openai", base_url: "https://api.openai.com/v1", env: "OPENAI_API_KEY", default_model: "" },
    Preset { key: "gemini", label: "Google Gemini", kind: "openai", base_url: "https://generativelanguage.googleapis.com/v1beta/openai", env: "GEMINI_API_KEY", default_model: "" },
    Preset { key: "openrouter", label: "OpenRouter (many models, one key)", kind: "openai", base_url: "https://openrouter.ai/api/v1", env: "OPENROUTER_API_KEY", default_model: "" },
    Preset { key: "deepseek", label: "DeepSeek", kind: "openai", base_url: "https://api.deepseek.com/v1", env: "DEEPSEEK_API_KEY", default_model: "" },
    Preset { key: "mistral", label: "Mistral", kind: "openai", base_url: "https://api.mistral.ai/v1", env: "MISTRAL_API_KEY", default_model: "" },
    Preset { key: "groq", label: "Groq", kind: "openai", base_url: "https://api.groq.com/openai/v1", env: "GROQ_API_KEY", default_model: "" },
    Preset { key: "xai", label: "xAI (Grok)", kind: "openai", base_url: "https://api.x.ai/v1", env: "XAI_API_KEY", default_model: "" },
    Preset { key: "local", label: "Local server (llama.cpp, Ollama, LM Studio, vLLM)", kind: "openai", base_url: "http://127.0.0.1:8080/v1", env: "", default_model: "" },
    Preset { key: "custom", label: "Another OpenAI-compatible endpoint", kind: "openai", base_url: "", env: "", default_model: "" },
    Preset { key: "mock", label: "Mock (scripted, for testing narsil itself)", kind: "mock", base_url: "", env: "", default_model: "mock" },
];

fn theme() -> ColorfulTheme {
    ColorfulTheme::default()
}

/// Context size reported by a llama.cpp server, if this is one.
async fn llama_context(base_url: &str) -> Option<i64> {
    let root = base_url.trim_end_matches('/').trim_end_matches("/v1");
    let client = reqwest::Client::builder().timeout(Duration::from_secs(4)).build().ok()?;
    let v: serde_json::Value = client.get(format!("{root}/props")).send().await.ok()?.json().await.ok()?;
    v.pointer("/default_generation_settings/n_ctx").and_then(|n| n.as_i64()).filter(|n| *n > 0)
}

pub async fn run(paths: &Paths, opts: SetupOpts) -> Result<()> {
    paths.ensure()?;
    let mut cfg = Config::load(paths).unwrap_or_default();
    let interactive = !opts.yes;
    println!("narsil setup");
    println!("  config: {}\n", paths.config_file().display());

    // 1. Backend
    let preset = match &opts.provider {
        Some(key) => PRESETS
            .iter()
            .find(|p| p.key == key)
            .with_context(|| format!("unknown provider {key:?}; choose from: {}", PRESETS.iter().map(|p| p.key).collect::<Vec<_>>().join(", ")))?,
        None if interactive => {
            let shown: Vec<&Preset> = PRESETS.iter().filter(|p| p.key != "mock").collect();
            let labels: Vec<&str> = shown.iter().map(|p| p.label).collect();
            let current = crate::config::split_model_ref(&cfg.model.default).0.to_string();
            let default = shown.iter().position(|p| p.key == current).unwrap_or(0);
            let i = Select::with_theme(&theme()).with_prompt("Which backend should narsil use?").items(&labels).default(default).interact()?;
            shown[i]
        }
        None => bail!("--yes needs --provider"),
    };
    let name = opts.name.clone().unwrap_or_else(|| preset.key.to_string());
    let mut p: ProviderConfig = cfg.providers.get(&name).cloned().unwrap_or_default();
    p.kind = preset.kind.to_string();

    let known_url = if p.base_url.is_empty() { preset.base_url.to_string() } else { p.base_url.clone() };
    p.base_url = match &opts.base_url {
        Some(u) => u.clone(),
        None if interactive && matches!(preset.key, "local" | "custom") => {
            Input::<String>::with_theme(&theme()).with_prompt("Base URL (ending in /v1)").with_initial_text(known_url).interact_text()?
        }
        None => known_url,
    };
    if preset.kind == "openai" && p.base_url.trim().is_empty() {
        bail!("a base URL is required for this backend (--base-url)");
    }
    p.base_url = p.base_url.trim().trim_end_matches('/').to_string();

    // 2. Credentials
    p.api_key_env = opts.api_key_env.clone().unwrap_or_else(|| {
        if p.api_key_env.is_empty() { preset.env.to_string() } else { p.api_key_env.clone() }
    });
    let mut creds = Credentials::load(paths);
    let needs_key = !matches!(preset.key, "local" | "mock");
    if interactive && (needs_key || preset.key == "custom") {
        let from_env = if p.api_key_env.is_empty() { None } else { std::env::var(&p.api_key_env).ok().filter(|v| !v.is_empty()) };
        if let Some(value) = from_env {
            println!("Found {} in your environment.", p.api_key_env);
            let store = Confirm::with_theme(&theme())
                .with_prompt("Also keep a copy in credentials.toml (mode 0600)? The daemon needs it when started by systemd rather than from your shell")
                .default(true)
                .interact()?;
            if store {
                creds.keys.insert(name.clone(), value);
            }
        } else {
            let have = creds.keys.contains_key(&name);
            let prompt = if have {
                format!("API key for {} (Enter keeps the stored key)", preset.label)
            } else if preset.key == "custom" {
                "API key (Enter for none)".to_string()
            } else {
                format!("API key for {} (stored in credentials.toml, mode 0600)", preset.label)
            };
            let key = Password::with_theme(&theme()).with_prompt(prompt).allow_empty_password(true).interact()?;
            if !key.trim().is_empty() {
                creds.keys.insert(name.clone(), key.trim().to_string());
            } else if !have && needs_key {
                println!("No key entered. Set {} or re-run setup before using this backend.", if p.api_key_env.is_empty() { "one" } else { &p.api_key_env });
            }
        }
        creds.save(paths)?;
    }
    cfg.providers.insert(name.clone(), p.clone());

    // 3. Model
    let client = provider::build(&cfg, paths, &name)?;
    let models = match tokio::time::timeout(Duration::from_secs(20), client.list_models()).await {
        Ok(Ok(mut m)) => {
            m.sort();
            m
        }
        Ok(Err(e)) => {
            println!("Could not list models: {e:#}");
            Vec::new()
        }
        Err(_) => {
            println!("Could not list models: the backend did not answer in time.");
            Vec::new()
        }
    };
    let (current_provider, current_model) = crate::config::split_model_ref(&cfg.model.default);
    let remembered = if current_provider == name { current_model.to_string() } else { String::new() };
    let suggestion = if !remembered.is_empty() { remembered } else { preset.default_model.to_string() };
    let model_id = match &opts.model {
        Some(m) => m.clone(),
        None if !interactive => {
            if !suggestion.is_empty() {
                suggestion
            } else if models.len() == 1 {
                models[0].clone()
            } else {
                bail!("--yes needs --model (the backend lists {} models)", models.len())
            }
        }
        None if !models.is_empty() && models.len() <= 40 => {
            let mut items: Vec<String> = models.clone();
            items.push("Other (type a model id)".to_string());
            let default = models.iter().position(|m| *m == suggestion).unwrap_or(0);
            let i = Select::with_theme(&theme()).with_prompt("Model").items(&items).default(default).interact()?;
            if i < models.len() {
                models[i].clone()
            } else {
                Input::<String>::with_theme(&theme()).with_prompt("Model id").interact_text()?
            }
        }
        None => {
            if !models.is_empty() {
                println!("{} models available, for example: {}", models.len(), models.iter().take(8).cloned().collect::<Vec<_>>().join(", "));
            }
            Input::<String>::with_theme(&theme()).with_prompt("Model id").with_initial_text(suggestion).interact_text()?
        }
    };
    let model_id = model_id.trim().to_string();
    if model_id.is_empty() {
        bail!("no model chosen");
    }
    cfg.model.default = format!("{name}/{model_id}");

    // Local models have small context windows: compact well before the limit.
    if preset.key == "local" || preset.key == "custom" {
        let detected = llama_context(&p.base_url).await;
        let window = match detected {
            Some(n) => {
                println!("The server reports a context window of {n} tokens.");
                Some(n)
            }
            None if interactive && preset.key == "local" => Some(
                Input::<i64>::with_theme(&theme())
                    .with_prompt("Context window of the model, in tokens")
                    .default(32768)
                    .interact_text()?,
            ),
            None => None,
        };
        if let (Some(n), Some(pc)) = (window, cfg.providers.get_mut(&name)) {
            pc.compact_at_tokens = (n * 7 / 10).max(2000);
        }
    }

    // 4. Check that it answers
    if !opts.no_test {
        print!("Testing {} ... ", cfg.model.default);
        use std::io::Write;
        let _ = std::io::stdout().flush();
        let client = provider::build(&cfg, paths, &name)?;
        let messages = [Message::user_text("Reply with the single word: ready")];
        let req = ChatRequest {
            model: &model_id,
            system: "",
            messages: &messages,
            tools: &[],
            max_tokens: 2000,
            effort: "low",
            thinking: "auto",
            strip_thinking: false,
        };
        let sink = |_e: StreamEvent| {};
        match tokio::time::timeout(Duration::from_secs(120), client.chat(&req, &sink)).await {
            Ok(Ok(resp)) => println!("ok: {}", util::first_line(&crate::types::blocks_text(&resp.blocks), 60)),
            Ok(Err(e)) => {
                println!("failed");
                println!("  {e}");
                if interactive && !Confirm::with_theme(&theme()).with_prompt("Save this configuration anyway?").default(false).interact()? {
                    bail!("setup cancelled; nothing was changed");
                }
            }
            Err(_) => println!("no answer within 120s; saving anyway"),
        }
    }

    // 5. Permissions and privilege
    if interactive {
        let modes = [
            "ask: confirm shell commands and file writes in chats (goals still run unattended)",
            "auto: never ask",
        ];
        let default = if cfg.permissions.mode == "auto" { 1 } else { 0 };
        let i = Select::with_theme(&theme()).with_prompt("Permissions for interactive sessions").items(&modes).default(default).interact()?;
        cfg.permissions.mode = if i == 1 { "auto".into() } else { "ask".into() };
        setup_privilege(&mut cfg, paths)?;
    }

    // 6. Obsidian
    match opts.obsidian.as_deref().map(str::trim) {
        Some("none") | Some("off") | Some("") => cfg.obsidian.vault.clear(),
        Some(path) => {
            let dir = crate::util::expand_tilde(path);
            if !dir.is_dir() {
                println!("note: {} is not there right now; saved anyway, and used once it appears", dir.display());
            }
            cfg.obsidian.vault = path.to_string();
        }
        None if interactive => setup_obsidian(&mut cfg)?,
        None => {}
    }

    cfg.save(paths)?;
    let written = write_default_guidance(paths)?;
    println!("\nSaved {}", paths.config_file().display());
    for f in written {
        println!("Created {}", f.display());
    }
    println!("\nFiles you can edit:");
    println!("  {}   backends, permissions, goals, voice", paths.config_file().display());
    println!("  {}       voice, style, how the agent gives feedback", paths.soul_file().display());
    println!("  {}     standing operating instructions", paths.agents_file().display());
    println!("  ./AGENTS.md in a project           project-specific instructions");
    println!("\nNext:");
    println!("  narsil                     open the TUI in the current directory");
    println!("  narsil goal add \"...\"      give it something to work on unattended");
    println!("  narsil service install     restart after crashes, start at login");
    if cfg.voice.transcribe_command.is_empty() {
        println!("  voice input: set [voice] transcribe_command in config.toml (e.g. whisper.cpp), then F2 in the TUI");
    }
    apply_to_daemon(paths, interactive).await
}

/// A daemon that is running still has the old settings, and the environment it
/// was started in, which may lack a key exported since. Start it again from
/// here, so that what was just set up is what narsil uses.
async fn apply_to_daemon(paths: &Paths, interactive: bool) -> Result<()> {
    let Some(status) = crate::service::ping(paths).await else {
        return Ok(());
    };
    let working = status["running"].as_array().map(|a| a.len()).unwrap_or(0);
    println!();
    let restart = !interactive || {
        let prompt = match working {
            0 => "The daemon is running with the old settings. Restart it now, so the new ones are in force?".to_string(),
            n => format!(
                "The daemon is running with the old settings, and {n} session{} at work (suspended, then picked up again). Restart it now?",
                if n == 1 { " is" } else { "s are" }
            ),
        };
        Confirm::with_theme(&theme()).with_prompt(prompt).default(true).interact()?
    };
    if !restart {
        println!("The daemon keeps the old settings until `narsil restart` (or /daemonrestart in the TUI).");
        return Ok(());
    }
    let launcher = crate::service::program();
    let restarted = std::process::Command::new(&launcher).arg("restart").status().with_context(|| format!("running {} restart", launcher.display()))?;
    if !restarted.success() {
        bail!("the settings were saved, but the daemon did not restart: run `narsil restart`");
    }
    println!("The new settings are in force. A TUI that was open has to be opened again (or /daemonrestart in it).");
    Ok(())
}

fn in_group(group: &str) -> bool {
    std::process::Command::new("id")
        .arg("-nG")
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).split_whitespace().any(|g| g == group))
        .unwrap_or(false)
}

fn passwordless_sudo() -> bool {
    std::process::Command::new("sudo")
        .args(["-n", "true"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Sudo for the agent. Being in `wheel` lets the account use sudo at all; a
/// passwordless rule is what lets an unattended run use it, since the agent
/// has no terminal and nobody is there to type a password.
/// Offer the vaults Obsidian knows about. With one chosen, the agent can use
/// the user's notes and narsil's memory notes live inside the vault.
fn setup_obsidian(cfg: &mut Config) -> Result<()> {
    let mut vaults = crate::obsidian::known_vaults();
    let current = cfg.obsidian.vault.trim().to_string();
    if !current.is_empty() && !vaults.iter().any(|v| v.to_string_lossy() == current) {
        vaults.insert(0, std::path::PathBuf::from(&current));
    }
    let mut items = vec!["No Obsidian integration (memory notes stay in narsil's own folder)".to_string()];
    items.extend(vaults.iter().map(|v| {
        let away = if v.is_dir() { "" } else { "  (not reachable right now)" };
        format!("{}{away}", v.display())
    }));
    items.push("Another folder…".to_string());
    let default = vaults.iter().position(|v| v.to_string_lossy() == current).map(|i| i + 1).unwrap_or(0);
    println!(
        "\nObsidian: with a vault chosen, the agent can search, read and write your notes, and narsil keeps \
         its memory notes and a note per goal inside the vault, in a `{}` folder.",
        cfg.obsidian.folder
    );
    let i = Select::with_theme(&theme()).with_prompt("Obsidian vault").items(&items).default(default).interact()?;
    cfg.obsidian.vault = if i == 0 {
        String::new()
    } else if i <= vaults.len() {
        vaults[i - 1].to_string_lossy().into_owned()
    } else {
        Input::<String>::with_theme(&theme()).with_prompt("Vault folder").interact_text()?.trim().to_string()
    };
    Ok(())
}

fn setup_privilege(cfg: &mut Config, paths: &Paths) -> Result<()> {
    let user = util::current_user();
    println!();
    let want = Confirm::with_theme(&theme())
        .with_prompt("Allow the agent to use sudo?")
        .default(cfg.privilege.sudo)
        .interact()?;
    cfg.privilege.sudo = want;
    if !want {
        println!("sudo disabled: commands that try to escalate are refused.");
        return Ok(());
    }
    if user.is_empty() || user == "root" {
        return Ok(());
    }
    let admin_group = if std::path::Path::new("/etc/debian_version").exists() { "sudo" } else { "wheel" };
    if in_group(admin_group) {
        println!("{user} is already in the {admin_group} group.");
    } else {
        let add = Confirm::with_theme(&theme())
            .with_prompt(format!("{user} is not in the {admin_group} group. Add it now? (runs: sudo usermod -aG {admin_group} {user})"))
            .default(true)
            .interact()?;
        if add {
            let ok = std::process::Command::new("sudo").args(["usermod", "-aG", admin_group, &user]).status().map(|s| s.success()).unwrap_or(false);
            let ok = ok
                || std::process::Command::new("su")
                    .args(["-c", &format!("usermod -aG {admin_group} {user}")])
                    .status()
                    .map(|s| s.success())
                    .unwrap_or(false);
            if ok {
                println!("Added. Group membership takes effect at your next login.");
            } else {
                println!("Could not add {user} to {admin_group}; as root run: usermod -aG {admin_group} {user}");
            }
        }
    }
    if passwordless_sudo() {
        println!("sudo already works without a password, so unattended runs can use it.");
        return Ok(());
    }
    println!("sudo currently asks for a password. The agent has no terminal, so until that changes");
    println!("`sudo` inside narsil fails immediately rather than hanging.");
    let nopasswd = Confirm::with_theme(&theme())
        .with_prompt(format!(
            "Install a passwordless sudo rule for {user} (/etc/sudoers.d/narsil)? This gives anything running as {user}, including an unattended agent, root without a prompt"
        ))
        .default(false)
        .interact()?;
    if nopasswd {
        let tmp = paths.runtime_dir.join("narsil.sudoers");
        std::fs::write(&tmp, format!("# Installed by `narsil setup`. Remove this file to undo.\n{user} ALL=(ALL) NOPASSWD: ALL\n"))?;
        let tmp_s = tmp.to_string_lossy().to_string();
        let valid = std::process::Command::new("sudo").args(["visudo", "-cf", &tmp_s]).status().map(|s| s.success()).unwrap_or(false);
        let installed = valid
            && std::process::Command::new("sudo")
                .args(["install", "-o", "root", "-g", "root", "-m", "0440", &tmp_s, "/etc/sudoers.d/narsil"])
                .status()
                .map(|s| s.success())
                .unwrap_or(false);
        let _ = std::fs::remove_file(&tmp);
        if installed {
            println!("Installed /etc/sudoers.d/narsil. Undo with: sudo rm /etc/sudoers.d/narsil");
        } else {
            println!("The rule was not installed.");
        }
    }
    Ok(())
}
