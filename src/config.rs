//! Configuration: where files live, what is in them, and the defaults.
//!
//! User-editable files, all under the config dir (`~/.config/narsil`):
//!   config.toml       backends, models, permissions, goals, voice
//!   SOUL.md           who the agent is: voice, style, how it gives feedback
//!   AGENTS.md         how the agent works: standing operating instructions
//!   credentials.toml  API keys (0600)
//! Projects may add their own `AGENTS.md`, read from the working directory up.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::util;

#[derive(Clone, Debug)]
pub struct Paths {
    pub config_dir: PathBuf,
    pub data_dir: PathBuf,
    pub state_dir: PathBuf,
    pub runtime_dir: PathBuf,
}

impl Paths {
    /// `NARSIL_HOME` puts everything under one directory (handy for testing or
    /// running a second, isolated instance). Otherwise XDG locations are used.
    pub fn resolve() -> Self {
        let var = |k: &str| std::env::var_os(k).filter(|v| !v.is_empty()).map(PathBuf::from);
        let mut p = if let Some(h) = var("NARSIL_HOME") {
            Paths {
                config_dir: h.join("config"),
                data_dir: h.join("data"),
                state_dir: h.join("state"),
                runtime_dir: h.join("run"),
            }
        } else {
            let home = util::home();
            let state_dir = var("XDG_STATE_HOME").unwrap_or_else(|| home.join(".local/state")).join("narsil");
            Paths {
                config_dir: var("XDG_CONFIG_HOME").unwrap_or_else(|| home.join(".config")).join("narsil"),
                data_dir: var("XDG_DATA_HOME").unwrap_or_else(|| home.join(".local/share")).join("narsil"),
                runtime_dir: var("XDG_RUNTIME_DIR")
                    .map(|d| d.join("narsil"))
                    .unwrap_or_else(|| state_dir.join("run")),
                state_dir,
            }
        };
        // Unix socket paths are limited to ~108 bytes; allow relocating just that.
        if let Some(r) = var("NARSIL_RUNTIME_DIR") {
            p.runtime_dir = r;
        }
        p
    }

    pub fn ensure(&self) -> Result<()> {
        for d in [&self.config_dir, &self.data_dir, &self.state_dir, &self.runtime_dir] {
            std::fs::create_dir_all(d).with_context(|| format!("creating {}", d.display()))?;
        }
        set_mode(&self.runtime_dir, 0o700);
        Ok(())
    }

    pub fn config_file(&self) -> PathBuf {
        self.config_dir.join("config.toml")
    }
    pub fn soul_file(&self) -> PathBuf {
        self.config_dir.join("SOUL.md")
    }
    pub fn agents_file(&self) -> PathBuf {
        self.config_dir.join("AGENTS.md")
    }
    pub fn credentials_file(&self) -> PathBuf {
        self.config_dir.join("credentials.toml")
    }
    pub fn db_file(&self) -> PathBuf {
        self.data_dir.join("narsil.db")
    }
    pub fn log_file(&self) -> PathBuf {
        self.state_dir.join("daemon.log")
    }
    pub fn socket(&self) -> PathBuf {
        self.runtime_dir.join("narsil.sock")
    }
    pub fn pid_file(&self) -> PathBuf {
        self.runtime_dir.join("narsil.pid")
    }
}

pub fn set_mode(path: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode));
}

// ---------------------------------------------------------------------------

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
#[serde(default)]
pub struct Config {
    pub model: ModelConfig,
    pub providers: BTreeMap<String, ProviderConfig>,
    pub agents: AgentsConfig,
    pub context: ContextConfig,
    pub goals: GoalsConfig,
    pub permissions: PermissionsConfig,
    pub privilege: PrivilegeConfig,
    pub shell: ShellConfig,
    pub retry: RetryConfig,
    pub daemon: DaemonConfig,
    pub voice: VoiceConfig,
    pub notify: NotifyConfig,
    pub tui: TuiConfig,
    pub obsidian: ObsidianConfig,
    pub memory: MemoryConfig,
    pub telemetry: TelemetryConfig,
    pub keys: KeysConfig,
    pub llmfit: LlmfitConfig,
}

/// llmfit (https://github.com/AlexsJones/llmfit) knows which local models
/// this machine can run. When it is installed the agent can ask it.
#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(default)]
pub struct LlmfitConfig {
    /// The llmfit program: a name to find on the PATH, or a path. Empty: do not use it.
    pub command: String,
    /// Recommend only what these can serve: "llama.cpp", "Ollama", "LM Studio",
    /// "MLX". Empty: whichever of them are installed.
    pub runtimes: Vec<String>,
}

impl Default for LlmfitConfig {
    fn default() -> Self {
        LlmfitConfig { command: "llmfit".into(), runtimes: Vec::new() }
    }
}

/// Key bindings in the TUI. A binding is modifiers and a key joined by `+`
/// (`ctrl+t`, `alt+right`, `f1`); several for one action are separated by spaces.
#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(default)]
pub struct KeysConfig {
    pub new_tab: String,
    pub close_tab: String,
    pub next_tab: String,
    pub prev_tab: String,
    /// Pressed before a digit to go straight to that tab. Two keystrokes, but
    /// no terminal keeps this for itself.
    pub goto_tab: String,
    /// A modifier that, held with a digit, also goes to that tab: `alt`,
    /// `ctrl`, or `none`. Off by default: many terminals take alt+digit for
    /// their own tabs, and ctrl+digit does not reach a program in most.
    pub goto_tab_with: String,
    pub keys: String,
    pub interrupt: String,
    pub panel: String,
    pub expand_tools: String,
    pub thinking: String,
    pub voice: String,
    pub scroll_up: String,
    pub scroll_down: String,
    pub files: String,
    pub quit: String,
}

impl Default for KeysConfig {
    fn default() -> Self {
        KeysConfig {
            new_tab: "ctrl+t".into(),
            close_tab: "alt+w".into(),
            next_tab: "alt+right".into(),
            prev_tab: "alt+left".into(),
            goto_tab: "ctrl+g".into(),
            goto_tab_with: "none".into(),
            keys: "f1 alt+k".into(),
            interrupt: "esc".into(),
            panel: "tab".into(),
            expand_tools: "ctrl+o".into(),
            thinking: "alt+t".into(),
            voice: "f2".into(),
            scroll_up: "pageup".into(),
            scroll_down: "pagedown".into(),
            files: "shift+tab".into(),
            quit: "ctrl+d".into(),
        }
    }
}

impl KeysConfig {
    /// The binding for an action, by its name in `[keys]`.
    pub fn get(&self, name: &str) -> &str {
        match name {
            "new_tab" => &self.new_tab,
            "close_tab" => &self.close_tab,
            "next_tab" => &self.next_tab,
            "prev_tab" => &self.prev_tab,
            "goto_tab" => &self.goto_tab,
            "keys" => &self.keys,
            "interrupt" => &self.interrupt,
            "panel" => &self.panel,
            "expand_tools" => &self.expand_tools,
            "thinking" => &self.thinking,
            "voice" => &self.voice,
            "scroll_up" => &self.scroll_up,
            "scroll_down" => &self.scroll_down,
            "files" => &self.files,
            "quit" => &self.quit,
            _ => "",
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(default)]
pub struct TelemetryConfig {
    /// How much goes to the log: error, warn, info, debug or trace. Also accepts
    /// a filter such as `info,narsil::provider=debug`. `NARSIL_LOG` overrides it.
    pub level: String,
    /// `text`, or `json` for one JSON object per line.
    pub log_format: String,
    /// How long recorded spans (the data behind `narsil trace` and `narsil stats`) are kept.
    pub keep_days: i64,
}

impl Default for TelemetryConfig {
    fn default() -> Self {
        TelemetryConfig { level: "info".into(), log_format: "text".into(), keep_days: 30 }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(default)]
pub struct MemoryConfig {
    /// Folder for the memory notes. Empty: `memory` in narsil's data directory.
    /// Not used while memory is kept in an Obsidian vault.
    pub dir: String,
    /// How often, in seconds, the daemon looks at the memory and work notes
    /// for changes made by hand (an answer written into a question's note, say).
    pub rescan_secs: u64,
}

impl Default for MemoryConfig {
    fn default() -> Self {
        MemoryConfig { dir: String::new(), rescan_secs: 20 }
    }
}

/// Optional link to an Obsidian vault. Off while `vault` is empty.
#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(default)]
pub struct ObsidianConfig {
    /// Path of the vault folder.
    pub vault: String,
    /// Folder inside the vault for narsil's own notes.
    pub folder: String,
    /// Keep the memory notes in the vault, under `<folder>/Memory`.
    pub memory_in_vault: bool,
    /// Keep a note per goal with its status, its work items and its progress.
    pub goal_notes: bool,
    /// Parts of the vault narsil must not read, search or write: folder or note
    /// names, paths from the top of the vault, or glob patterns.
    pub exclude: Vec<String>,
    /// When a request arrives, look in the vault for notes on its subject and
    /// name the best matches to the agent before it starts.
    pub lookup: bool,
}

impl Default for ObsidianConfig {
    fn default() -> Self {
        ObsidianConfig {
            vault: String::new(),
            folder: "narsil".into(),
            memory_in_vault: true,
            goal_notes: true,
            exclude: Vec::new(),
            lookup: true,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(default)]
pub struct ModelConfig {
    /// `provider/model-id`, where `provider` names a `[providers.*]` table.
    pub default: String,
    /// Model for parallel subagents. Empty: same as the session's model.
    pub subagent: String,
    /// Reasoning effort where the backend supports it: low|medium|high|xhigh|max.
    pub effort: String,
    /// auto | off. `auto` requests adaptive thinking on models that have it.
    pub thinking: String,
    /// Output-token cap per response. 0: the provider default.
    pub max_output_tokens: u32,
}

impl Default for ModelConfig {
    fn default() -> Self {
        ModelConfig {
            default: String::new(),
            subagent: String::new(),
            effort: "high".into(),
            thinking: "auto".into(),
            max_output_tokens: 0,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(default)]
pub struct ProviderConfig {
    /// Wire protocol: "anthropic" (Messages API) or "openai" (Chat Completions,
    /// spoken by OpenAI, Gemini, OpenRouter, DeepSeek, Mistral, Groq, xAI,
    /// llama.cpp, Ollama, LM Studio, vLLM, ...).
    pub kind: String,
    pub base_url: String,
    /// Environment variable holding the API key.
    pub api_key_env: String,
    /// Compact the conversation once it reaches this many tokens. 0: use
    /// `[context] compact_at_tokens`. Set this below a local model's context size.
    pub compact_at_tokens: i64,
    /// Seconds to wait for the next byte of a streaming response.
    pub timeout_secs: u64,
    /// Anthropic: let the API re-run a safety-classifier refusal on a fallback model.
    pub refusal_fallback: bool,
    /// OpenAI-compatible: value for `reasoning_effort`. Empty: not sent.
    pub reasoning_effort: String,
    /// OpenAI-compatible: send prior `reasoning_content` back with assistant turns.
    pub echo_reasoning: bool,
    /// Extra HTTP headers.
    pub headers: BTreeMap<String, String>,
}

impl Default for ProviderConfig {
    fn default() -> Self {
        ProviderConfig {
            kind: "openai".into(),
            base_url: String::new(),
            api_key_env: String::new(),
            compact_at_tokens: 0,
            timeout_secs: 900,
            refusal_fallback: true,
            reasoning_effort: String::new(),
            echo_reasoning: false,
            headers: BTreeMap::new(),
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(default)]
pub struct AgentsConfig {
    /// Subagents running at once per `spawn_agents` call.
    pub max_parallel: usize,
    /// How deep subagents may nest. 1: only the main agent can spawn.
    pub max_depth: i64,
}

impl Default for AgentsConfig {
    fn default() -> Self {
        AgentsConfig { max_parallel: 4, max_depth: 1 }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(default)]
pub struct ContextConfig {
    pub compact_at_tokens: i64,
    /// Instruction files looked up from the working directory upward.
    pub project_files: Vec<String>,
    /// Characters of durable memory inlined into the system prompt; the rest
    /// is listed by title and reachable with `memory_search`.
    pub memory_budget_chars: usize,
    /// Tool output beyond this many bytes is elided in the middle.
    pub tool_output_limit: usize,
}

impl Default for ContextConfig {
    fn default() -> Self {
        ContextConfig {
            compact_at_tokens: 150_000,
            project_files: vec!["AGENTS.md".into()],
            memory_budget_chars: 8000,
            tool_output_limit: 12_000,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(default)]
pub struct GoalsConfig {
    /// Stop a goal after this many turns. 0: no limit.
    pub max_iterations: i64,
    /// Stop a goal after this many hours since it was created. 0: no limit.
    pub max_hours: f64,
    /// Pause a goal after this many consecutive turns in which the agent used
    /// no tools (it is talking, not working).
    pub stall_limit: i64,
    /// Pause a goal once it has used this many tokens, in and out together,
    /// with its subagents. 0: no limit.
    pub max_tokens: i64,
    /// Pause a goal that has no finish line (the agent said so) after this
    /// many hours, so that going on is something you chose. 0: never.
    pub open_ended_hours: f64,
}

impl Default for GoalsConfig {
    fn default() -> Self {
        GoalsConfig { max_iterations: 0, max_hours: 0.0, stall_limit: 3, max_tokens: 0, open_ended_hours: 1.0 }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(default)]
pub struct PermissionsConfig {
    /// Interactive sessions: "ask" before shell commands and file writes, or "auto".
    pub mode: String,
    /// Autonomous goals: "auto" (unattended) or "ask" (waits for you).
    pub goal_mode: String,
    /// Shell commands that never need approval, matched as a prefix of a
    /// simple command (no pipes, redirects or chaining).
    pub allow: Vec<String>,
    /// Shell commands refused outright, matched as substrings.
    pub deny: Vec<String>,
    /// Mask anything shaped like a token or a private key in what narsil
    /// writes down to keep (memory, work notes, a goal's journal) and in what
    /// it shows (the TUI, traces). The agent itself still sees and can use it.
    pub mask_secrets: bool,
}

impl Default for PermissionsConfig {
    fn default() -> Self {
        PermissionsConfig {
            mode: "ask".into(),
            goal_mode: "auto".into(),
            allow: [
                "ls", "pwd", "cat", "head", "tail", "wc", "file", "stat", "du", "df", "rg", "grep", "which",
                "uname", "git status", "git diff", "git log", "git show",
            ]
            .iter()
            .map(|s| s.to_string())
            .collect(),
            deny: ["rm -rf /", "rm -rf ~", "mkfs", ":(){", "dd if=/dev/zero of=/dev/", "> /dev/sd"]
                .iter()
                .map(|s| s.to_string())
                .collect(),
            mask_secrets: true,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(default)]
pub struct PrivilegeConfig {
    /// Whether the agent may use sudo. `narsil setup` makes sure the account
    /// is in the wheel group when this is on.
    pub sudo: bool,
}

impl Default for PrivilegeConfig {
    fn default() -> Self {
        PrivilegeConfig { sudo: true }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(default)]
pub struct ShellConfig {
    pub program: String,
    pub default_timeout_secs: u64,
    pub max_timeout_secs: u64,
    /// Extra environment for commands the agent runs.
    pub env: BTreeMap<String, String>,
}

impl Default for ShellConfig {
    fn default() -> Self {
        ShellConfig {
            program: "bash".into(),
            default_timeout_secs: 120,
            max_timeout_secs: 7200,
            env: BTreeMap::new(),
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(default)]
pub struct RetryConfig {
    /// Attempts per model call in interactive sessions. Goals retry without limit.
    pub max_attempts: u32,
    pub max_backoff_secs: u64,
}

impl Default for RetryConfig {
    fn default() -> Self {
        RetryConfig { max_attempts: 8, max_backoff_secs: 300 }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
#[serde(default)]
pub struct DaemonConfig {
    /// After a crash or `narsil stop`, also resume interrupted chat sessions on
    /// start. Goals always resume; chats otherwise wait for `/continue`.
    pub resume_interrupted_chats: bool,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(default)]
pub struct VoiceConfig {
    pub enabled: bool,
    /// Records until interrupted. `{file}` is the WAV path to write.
    pub record_command: String,
    /// Prints the transcript of `{file}` on stdout.
    pub transcribe_command: String,
    /// Send the transcript immediately instead of placing it in the input box.
    pub auto_send: bool,
    /// Optional text-to-speech: receives each finished reply on stdin.
    pub speak_command: String,
}

impl Default for VoiceConfig {
    fn default() -> Self {
        VoiceConfig {
            enabled: false,
            record_command: "pw-record --rate 16000 --channels 1 {file}".into(),
            transcribe_command: String::new(),
            auto_send: false,
            speak_command: String::new(),
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
#[serde(default)]
pub struct NotifyConfig {
    /// Run when a goal completes, blocks or pauses. `{message}` is substituted
    /// (shell-quoted). Example: notify-send narsil {message}
    pub command: String,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(default)]
pub struct TuiConfig {
    pub mouse: bool,
    pub show_thinking: bool,
    /// The colour theme: `terminal` for the terminal's own colours, or one of
    /// the themes `/theme` lists.
    pub theme: String,
}

impl Default for TuiConfig {
    fn default() -> Self {
        TuiConfig { mouse: true, show_thinking: true, theme: "terminal".into() }
    }
}

impl Config {
    pub fn load(paths: &Paths) -> Result<Config> {
        let file = paths.config_file();
        if !file.exists() {
            return Ok(Config::default());
        }
        let text = std::fs::read_to_string(&file).with_context(|| format!("reading {}", file.display()))?;
        toml::from_str(&text).with_context(|| format!("parsing {}", file.display()))
    }

    pub fn save(&self, paths: &Paths) -> Result<()> {
        paths.ensure()?;
        std::fs::write(paths.config_file(), render_config(self))?;
        Ok(())
    }

    pub fn is_configured(&self) -> bool {
        !self.model.default.is_empty()
    }

    /// Token count at which a session on this provider is compacted.
    pub fn compact_limit(&self, provider: &str) -> i64 {
        match self.providers.get(provider) {
            Some(p) if p.compact_at_tokens > 0 => p.compact_at_tokens,
            _ => self.context.compact_at_tokens,
        }
    }
}

/// Split `provider/model` at the first slash; model ids may contain slashes.
pub fn split_model_ref(model_ref: &str) -> (&str, &str) {
    match model_ref.split_once('/') {
        Some((p, m)) => (p, m),
        None => (model_ref, ""),
    }
}

// ---------------------------------------------------------------------------

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
#[serde(default)]
pub struct Credentials {
    pub keys: BTreeMap<String, String>,
}

impl Credentials {
    pub fn load(paths: &Paths) -> Credentials {
        std::fs::read_to_string(paths.credentials_file())
            .ok()
            .and_then(|t| toml::from_str(&t).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, paths: &Paths) -> Result<()> {
        paths.ensure()?;
        let file = paths.credentials_file();
        std::fs::write(&file, toml::to_string_pretty(self)?)?;
        set_mode(&file, 0o600);
        Ok(())
    }
}

/// Key lookup order: the provider's environment variable, then credentials.toml.
pub fn api_key(paths: &Paths, name: &str, provider: &ProviderConfig) -> String {
    if !provider.api_key_env.is_empty() {
        if let Ok(v) = std::env::var(&provider.api_key_env) {
            if !v.is_empty() {
                return v;
            }
        }
    }
    Credentials::load(paths).keys.get(name).cloned().unwrap_or_default()
}

// ---------------------------------------------------------------------------

fn q(s: &str) -> String {
    toml::Value::String(s.to_string()).to_string()
}

fn qlist(items: &[String]) -> String {
    toml::Value::Array(items.iter().map(|s| toml::Value::String(s.clone())).collect()).to_string()
}

fn qmap(map: &BTreeMap<String, String>) -> String {
    let inner: Vec<String> = map.iter().map(|(k, v)| format!("{} = {}", q(k), q(v))).collect();
    format!("{{ {} }}", inner.join(", ")).replace("{  }", "{}")
}

/// Write the config with comments, so the file documents itself.
pub fn render_config(c: &Config) -> String {
    let mut s = String::new();
    s.push_str("# narsil configuration. Edit freely; `narsil setup` rewrites this file\n");
    s.push_str("# (keeping your values, not your comments). After editing, run\n");
    s.push_str("# `narsil restart` or use /reload in the TUI.\n\n");

    s.push_str("[model]\n");
    s.push_str("# provider/model-id. The provider part names a [providers.*] table below.\n");
    s.push_str(&format!("default = {}\n", q(&c.model.default)));
    s.push_str("# Model for parallel subagents. Empty: the same model as the session.\n");
    s.push_str(&format!("subagent = {}\n", q(&c.model.subagent)));
    s.push_str("# Reasoning effort where supported: low | medium | high | xhigh | max\n");
    s.push_str(&format!("effort = {}\n", q(&c.model.effort)));
    s.push_str("# auto: adaptive thinking on models that have it. off: do not request it.\n");
    s.push_str(&format!("thinking = {}\n", q(&c.model.thinking)));
    s.push_str("# Output-token cap per response. 0: provider default.\n");
    s.push_str(&format!("max_output_tokens = {}\n\n", c.model.max_output_tokens));

    s.push_str("# Backends. kind = \"anthropic\" (Messages API) or \"openai\" (Chat Completions,\n");
    s.push_str("# which also covers Gemini, OpenRouter, DeepSeek, Mistral, Groq, xAI, llama.cpp,\n");
    s.push_str("# Ollama, LM Studio and vLLM). Add as many as you like and switch with /model.\n");
    for (name, p) in &c.providers {
        s.push_str(&format!("[providers.{}]\n", toml_key(name)));
        s.push_str(&format!("kind = {}\n", q(&p.kind)));
        s.push_str(&format!("base_url = {}\n", q(&p.base_url)));
        s.push_str("# Environment variable with the API key (credentials.toml is the fallback).\n");
        s.push_str(&format!("api_key_env = {}\n", q(&p.api_key_env)));
        s.push_str("# Compact the conversation at this many tokens. 0: [context] compact_at_tokens.\n");
        s.push_str(&format!("compact_at_tokens = {}\n", p.compact_at_tokens));
        s.push_str(&format!("timeout_secs = {}\n", p.timeout_secs));
        if p.kind == "anthropic" {
            s.push_str("# Re-run a safety-classifier refusal on a fallback model (server side).\n");
            s.push_str(&format!("refusal_fallback = {}\n", p.refusal_fallback));
        } else {
            s.push_str("# Sent as `reasoning_effort` when non-empty.\n");
            s.push_str(&format!("reasoning_effort = {}\n", q(&p.reasoning_effort)));
            s.push_str("# Send earlier reasoning_content back with assistant turns (some backends need it).\n");
            s.push_str(&format!("echo_reasoning = {}\n", p.echo_reasoning));
        }
        s.push_str(&format!("headers = {}\n\n", qmap(&p.headers)));
    }

    s.push_str("[agents]\n");
    s.push_str("# Subagents running at once, and how deep they may nest.\n");
    s.push_str(&format!("max_parallel = {}\n", c.agents.max_parallel));
    s.push_str(&format!("max_depth = {}\n\n", c.agents.max_depth));

    s.push_str("[context]\n");
    s.push_str("# Summarize and start a fresh context once a conversation reaches this size.\n");
    s.push_str(&format!("compact_at_tokens = {}\n", c.context.compact_at_tokens));
    s.push_str("# Instruction files read from the working directory upward.\n");
    s.push_str(&format!("project_files = {}\n", qlist(&c.context.project_files)));
    s.push_str("# Characters of durable memory inlined into the prompt (rest: titles only).\n");
    s.push_str(&format!("memory_budget_chars = {}\n", c.context.memory_budget_chars));
    s.push_str(&format!("tool_output_limit = {}\n\n", c.context.tool_output_limit));

    s.push_str("[goals]\n");
    s.push_str("# Limits for autonomous goals. 0 means no limit.\n");
    s.push_str(&format!("max_iterations = {}\n", c.goals.max_iterations));
    s.push_str(&format!("max_hours = {:?}\n", c.goals.max_hours));
    s.push_str("# Pause after this many consecutive turns with no tool use.\n");
    s.push_str(&format!("stall_limit = {}\n", c.goals.stall_limit));
    s.push_str("# Pause a goal once it has used this many tokens, in and out, with its subagents. 0: no limit.\n");
    s.push_str(&format!("max_tokens = {}\n", c.goals.max_tokens));
    s.push_str("# A goal with no finish line (the agent says so when it starts) pauses after this many hours,\n");
    s.push_str("# so that going on is something you chose. Limits count from when it was last resumed. 0: never.\n");
    s.push_str(&format!("open_ended_hours = {}\n\n", c.goals.open_ended_hours));

    s.push_str("[permissions]\n");
    s.push_str("# ask: confirm shell commands and file writes. auto: run without asking.\n");
    s.push_str(&format!("mode = {}\n", q(&c.permissions.mode)));
    s.push_str("# Goals run unattended, so they default to auto.\n");
    s.push_str(&format!("goal_mode = {}\n", q(&c.permissions.goal_mode)));
    s.push_str("# Simple commands starting with one of these never ask.\n");
    s.push_str(&format!("allow = {}\n", qlist(&c.permissions.allow)));
    s.push_str("# Commands containing one of these are always refused.\n");
    s.push_str(&format!("deny = {}\n", qlist(&c.permissions.deny)));
    s.push_str("# The agent can read and use credentials. This keeps them from spreading: anything shaped like\n");
    s.push_str("# a token or a private key is masked in what narsil writes down to keep (memory, work notes,\n");
    s.push_str("# a goal's journal) and in what it shows (the TUI, traces).\n");
    s.push_str(&format!("mask_secrets = {}\n\n", c.permissions.mask_secrets));

    s.push_str("[privilege]\n");
    s.push_str("# Let the agent use sudo. Setup checks wheel membership; unattended runs also\n");
    s.push_str("# need passwordless sudo (setup offers it), since nobody is there to type one.\n");
    s.push_str(&format!("sudo = {}\n\n", c.privilege.sudo));

    s.push_str("[shell]\n");
    s.push_str(&format!("program = {}\n", q(&c.shell.program)));
    s.push_str(&format!("default_timeout_secs = {}\n", c.shell.default_timeout_secs));
    s.push_str(&format!("max_timeout_secs = {}\n", c.shell.max_timeout_secs));
    s.push_str(&format!("env = {}\n\n", qmap(&c.shell.env)));

    s.push_str("[retry]\n");
    s.push_str("# Attempts per model call in chats. Goals keep retrying through outages.\n");
    s.push_str(&format!("max_attempts = {}\n", c.retry.max_attempts));
    s.push_str(&format!("max_backoff_secs = {}\n\n", c.retry.max_backoff_secs));

    s.push_str("[daemon]\n");
    s.push_str("# Goals always resume after a crash or restart. Chats wait for /continue\n");
    s.push_str("# unless this is true.\n");
    s.push_str(&format!("resume_interrupted_chats = {}\n\n", c.daemon.resume_interrupted_chats));

    s.push_str("[voice]\n");
    s.push_str("# F2 in the TUI records, then transcribes into the input box.\n");
    s.push_str(&format!("enabled = {}\n", c.voice.enabled));
    s.push_str(&format!("record_command = {}\n", q(&c.voice.record_command)));
    s.push_str("# Must print the transcript of {file}. Example:\n");
    s.push_str("#   whisper-cli -m ~/models/ggml-base.en.bin -f {file} -nt -np\n");
    s.push_str(&format!("transcribe_command = {}\n", q(&c.voice.transcribe_command)));
    s.push_str(&format!("auto_send = {}\n", c.voice.auto_send));
    s.push_str("# Optional text-to-speech; gets each finished reply on stdin.\n");
    s.push_str(&format!("speak_command = {}\n\n", q(&c.voice.speak_command)));

    s.push_str("[notify]\n");
    s.push_str("# Run when a goal completes, blocks or pauses. {message} is substituted.\n");
    s.push_str("#   command = \"notify-send narsil {message}\"\n");
    s.push_str(&format!("command = {}\n\n", q(&c.notify.command)));

    s.push_str("[tui]\n");
    s.push_str(&format!("mouse = {}\n", c.tui.mouse));
    s.push_str(&format!("show_thinking = {}\n", c.tui.show_thinking));
    s.push_str("# The colour theme: \"terminal\" keeps your terminal's own colours. /theme in the TUI lists the\n");
    s.push_str("# others (gruvbox, solarized, dracula, nord, tokyo-night, ...), tries them, and saves your choice here.\n");
    s.push_str(&format!("theme = {}\n\n", q(&c.tui.theme)));

    s.push_str("[obsidian]\n");
    s.push_str("# Path of an Obsidian vault. Empty: no integration. With a vault set, the agent can\n");
    s.push_str("# search, read and write its notes (vault_search, vault_read, vault_write).\n");
    s.push_str(&format!("vault = {}\n", q(&c.obsidian.vault)));
    s.push_str("# Folder inside the vault where narsil keeps its own notes.\n");
    s.push_str(&format!("folder = {}\n", q(&c.obsidian.folder)));
    s.push_str("# Keep the memory notes in the vault, under <folder>/Memory, so they sit in your graph\n");
    s.push_str("# next to your own notes. false: memory stays in [memory] dir.\n");
    s.push_str(&format!("memory_in_vault = {}\n", c.obsidian.memory_in_vault));
    s.push_str("# Keep a note per goal in <folder>/Goals with its status, its work items and its progress.\n");
    s.push_str(&format!("goal_notes = {}\n", c.obsidian.goal_notes));
    s.push_str("# Parts of the vault narsil must not read, search or write. A bare name (\"Journal\") closes a\n");
    s.push_str("# folder or note of that name wherever it is; a path (\"Areas/Health\") is taken from the top of\n");
    s.push_str("# the vault; * and ? are wildcards (\"*.private.md\"). This holds for narsil's vault and file\n");
    s.push_str("# tools, and shell commands are refused on the vault while anything is listed here. It is a\n");
    s.push_str("# guard against the agent wandering, not a sandbox: see docs/obsidian.md.\n");
    s.push_str(&format!("exclude = [{}]\n", c.obsidian.exclude.iter().map(|e| q(e)).collect::<Vec<_>>().join(", ")));
    s.push_str("# When you ask for something, look in the vault for notes on the subject and name the best\n");
    s.push_str("# matches to the agent, so that what you have written down is the first place it looks.\n");
    s.push_str(&format!("lookup = {}\n\n", c.obsidian.lookup));

    s.push_str("[memory]\n");
    s.push_str("# Memory is a folder of Markdown notes that link to each other with [[Title]]. Edit,\n");
    s.push_str("# add or delete notes there with any editor and narsil follows. Empty: the `memory`\n");
    s.push_str("# folder in narsil's data directory (`narsil config path` shows it).\n");
    s.push_str(&format!("dir = {}\n", q(&c.memory.dir)));
    s.push_str("# How often, in seconds, the notes are looked at for changes made by hand.\n");
    s.push_str(&format!("rescan_secs = {}\n\n", c.memory.rescan_secs));

    s.push_str("[telemetry]\n");
    s.push_str("# What goes to the daemon log: error, warn, info, debug or trace, or a filter such as\n");
    s.push_str("# \"info,narsil::provider=debug\". The NARSIL_LOG environment variable overrides it.\n");
    s.push_str(&format!("level = {}\n", q(&c.telemetry.level)));
    s.push_str("# \"text\", or \"json\" for one JSON object per line.\n");
    s.push_str(&format!("log_format = {}\n", q(&c.telemetry.log_format)));
    s.push_str("# Every session run, model call and tool call is recorded with its timing; `narsil trace`\n");
    s.push_str("# and `narsil stats` read that. Days to keep it.\n");
    s.push_str(&format!("keep_days = {}\n\n", c.telemetry.keep_days));

    s.push_str("[llmfit]\n");
    s.push_str("# llmfit knows which local models this machine can run (brew install llmfit). When it is\n");
    s.push_str("# installed the agent can ask it: \"what is the best local model for agent work on this machine?\"\n");
    s.push_str("# The program's name or path; empty to leave it out.\n");
    s.push_str(&format!("command = {}\n", q(&c.llmfit.command)));
    s.push_str("# Recommend only what these can serve: \"llama.cpp\", \"Ollama\", \"LM Studio\", \"MLX\".\n");
    s.push_str("# Empty: whichever of them are installed.\n");
    s.push_str(&format!("runtimes = [{}]\n\n", c.llmfit.runtimes.iter().map(|r| q(r)).collect::<Vec<_>>().join(", ")));

    s.push_str("[keys]\n");
    s.push_str("# Key bindings in the TUI. A binding is modifiers (ctrl, alt, shift) and a key joined by +,\n");
    s.push_str("# such as \"ctrl+t\", \"alt+right\" or \"f1\"; several for one action are separated by spaces.\n");
    s.push_str("# The keys window in the TUI (f1, or /keys) lists what is in force. Editing keys are fixed.\n");
    for name in [
        "new_tab", "close_tab", "next_tab", "prev_tab", "goto_tab", "keys", "interrupt", "panel", "expand_tools", "thinking", "voice",
        "scroll_up", "scroll_down", "files", "quit",
    ] {
        s.push_str(&format!("{name} = {}\n", q(c.keys.get(name))));
    }
    s.push_str("# goto_tab is pressed before a digit: ctrl+g then 3 goes to tab 3 (also left/right, t new, w close).\n");
    s.push_str("# goto_tab_with is a modifier that, held with a digit, does the same in one stroke: \"alt\" or \"ctrl\".\n");
    s.push_str("# It is off because many terminals (Ptyxis, GNOME Terminal, Ghostty) keep alt+digit for their own tabs.\n");
    s.push_str(&format!("goto_tab_with = {}\n", q(&c.keys.goto_tab_with)));
    s
}

fn toml_key(k: &str) -> String {
    if !k.is_empty() && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') {
        k.to_string()
    } else {
        q(k)
    }
}

pub const DEFAULT_SOUL: &str = r#"# Soul

This file is who you are when you work with me. I edit it; you live by it.

## Voice
- Direct and plain. Lead with the answer, then the reasoning that matters.
- No flattery, no filler, no apologies for things that are not mistakes.
- Match my register: short question, short answer.

## Feedback
- Tell me when I am wrong, and say why. I would rather be corrected than agreed with.
- When you review my work, name the most important problem first.
- Separate what you verified from what you assume.

## Judgment
- Prefer the simple thing that works over the clever thing.
- If a request is ambiguous and the answer changes what you would do, ask once. Otherwise pick the sensible reading and say which one you picked.
- Finish what you start. If you cannot, say exactly where you stopped and why.

## Creative work
- For writing and design, offer a point of view rather than a menu of options.
"#;

pub const DEFAULT_AGENTS: &str = r#"# Operating instructions

Standing rules for how you work on this machine. Project-specific rules belong in an
`AGENTS.md` inside the project; those are read in addition to this file.

## Working
- Read before you write: look at the existing code and conventions first.
- Make the change that was asked for. Do not refactor or tidy things nobody mentioned.
- Verify your work by running it (build, tests, the command itself) before saying it is done.
- Report results honestly, including failures and anything you skipped.

## Safety
- Look before deleting or overwriting. Never discard uncommitted work without asking.
- Commit or push only when asked.
- Treat credentials as secrets: do not print them or copy them into files.

## Memory
- Save durable facts with `memory_save`: my preferences, corrections I give you, and
  things about a project that the code does not already say.
- Do not save what is only relevant to the current task; use task notes for that.

## Long-running goals
- Record milestones with `goal_update` so a restart or a compacted context loses nothing.
- If you are blocked on something only I can provide, call `goal_blocked` and stop.
"#;

/// Create SOUL.md and AGENTS.md if they do not exist yet. Returns what was written.
pub fn write_default_guidance(paths: &Paths) -> Result<Vec<PathBuf>> {
    paths.ensure()?;
    let mut written = Vec::new();
    for (file, body) in [(paths.soul_file(), DEFAULT_SOUL), (paths.agents_file(), DEFAULT_AGENTS)] {
        if !file.exists() {
            std::fs::write(&file, body)?;
            written.push(file);
        }
    }
    Ok(written)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rendered_config_round_trips() {
        let mut c = Config::default();
        c.model.default = "openrouter/anthropic/some-model".into();
        let mut p = ProviderConfig::default();
        p.base_url = "https://example.test/v1".into();
        p.headers.insert("X-Title".into(), "narsil \"quoted\"".into());
        c.providers.insert("openrouter".into(), p);
        c.providers.insert("claude".into(), ProviderConfig { kind: "anthropic".into(), ..Default::default() });
        c.shell.env.insert("FOO".into(), "bar baz".into());
        c.goals.max_hours = 7.5;

        let text = render_config(&c);
        let back: Config = toml::from_str(&text).expect("rendered config parses");
        assert_eq!(back.model.default, c.model.default);
        assert_eq!(back.providers.len(), 2);
        assert_eq!(back.providers["openrouter"].headers["X-Title"], "narsil \"quoted\"");
        assert_eq!(back.shell.env["FOO"], "bar baz");
        assert_eq!(back.goals.max_hours, 7.5);
        assert_eq!(back.permissions.allow, c.permissions.allow);
    }

    #[test]
    fn model_ref_splits_on_first_slash() {
        assert_eq!(split_model_ref("openrouter/anthropic/x"), ("openrouter", "anthropic/x"));
        assert_eq!(split_model_ref("local/qwen"), ("local", "qwen"));
    }
}
