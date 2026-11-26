//! Which local models this machine can run, asked of llmfit
//! (https://github.com/AlexsJones/llmfit), which knows the hardware and a
//! catalogue of open models: whether one fits in memory, how fast it should
//! be, how much context it has, whether it can call tools.
//!
//! narsil adds what an agent harness knows. For "agentic" work a model has to
//! call tools, and has to hold a harness's instructions with room left to
//! work, so those are asked for. Only what the runtimes installed here can
//! serve is put forward. And since llmfit lists a model at the heaviest
//! quantisation that fits, which is also the slowest, the most capable models
//! that are too slow that way are looked at again at a lighter one.
//!
//! Offered to the agent when llmfit is installed.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use serde_json::Value;

use super::{ToolCtx, ToolOutput, opt_str, opt_u64};
use crate::config::Config;

/// The context an agent is sized for: narsil's own instructions and tools
/// take about 8,000 tokens, and the work needs the rest.
const AGENT_CONTEXT: u64 = 32_768;
/// Below this many tokens a second, an agent's many steps are too slow to wait for.
const USABLE_TPS: f64 = 8.0;
/// The quantisation tried for a capable model that is too slow at the one llmfit chose.
const LIGHTER_QUANT: &str = "Q4_K_M";
/// How many models llmfit is asked for: everything that fits, since those a
/// runtime here can serve are a fraction of them. It answers in a couple of seconds.
const POOL: &str = "2000";

/// Where the llmfit program is, if it is installed and not switched off.
pub fn program(cfg: &Config) -> Option<PathBuf> {
    let name = cfg.llmfit.command.trim();
    if name.is_empty() {
        return None;
    }
    let given = crate::util::expand_tilde(name);
    if given.components().count() > 1 {
        return given.is_file().then_some(given);
    }
    installed(name)
}

/// A program by name: on the PATH, or in one of the usual places, since the
/// daemon may have been started with a shorter PATH than a shell has (by
/// systemd, say).
fn installed(name: &str) -> Option<PathBuf> {
    let mut dirs: Vec<PathBuf> = std::env::var_os("PATH").map(|path| std::env::split_paths(&path).collect()).unwrap_or_default();
    dirs.extend(["/home/linuxbrew/.linuxbrew/bin", "/opt/homebrew/bin", "/usr/local/bin"].map(PathBuf::from));
    dirs.extend(["~/.cargo/bin", "~/.local/bin"].map(crate::util::expand_tilde));
    dirs.into_iter().map(|dir| dir.join(name)).find(|p| p.is_file())
}

/// Run llmfit and read what it prints as JSON.
async fn ask(program: &PathBuf, args: &[String]) -> Result<Value> {
    let run = tokio::process::Command::new(program).args(args).stdin(std::process::Stdio::null()).kill_on_drop(true).output();
    let out = tokio::time::timeout(Duration::from_secs(60), run)
        .await
        .map_err(|_| anyhow!("llmfit did not answer within a minute"))?
        .with_context(|| format!("running {}", program.display()))?;
    let text = String::from_utf8_lossy(&out.stdout);
    let value: Value = serde_json::from_str(text.trim()).map_err(|_| {
        let said = String::from_utf8_lossy(&out.stderr);
        anyhow!("llmfit {}: {}", args.join(" "), crate::util::first_line(if said.trim().is_empty() { text.trim() } else { said.trim() }, 300))
    })?;
    if let Some(message) = value.pointer("/error/message").and_then(Value::as_str) {
        bail!("{message}");
    }
    Ok(value)
}

fn text<'a>(v: &'a Value, key: &str) -> &'a str {
    v.get(key).and_then(Value::as_str).unwrap_or("")
}

fn number(v: &Value, key: &str) -> f64 {
    v.get(key).and_then(Value::as_f64).unwrap_or(0.0)
}

fn quality(m: &Value) -> f64 {
    m.pointer("/score_components/quality").and_then(Value::as_f64).unwrap_or(0.0)
}

/// Tokens a second: measured on this machine where llmfit has that, else its estimate.
fn speed(m: &Value) -> f64 {
    m.get("measured_tps").and_then(Value::as_f64).unwrap_or_else(|| number(m, "estimated_tps"))
}

/// The hardware, in a line.
pub fn hardware(system: &Value) -> String {
    let s = system.get("system").unwrap_or(system);
    let mut out = format!("{}, {} cores, {:.0} GB of memory", text(s, "cpu_name"), number(s, "cpu_cores"), number(s, "total_ram_gb"));
    if s.get("has_gpu").and_then(Value::as_bool) == Some(true) {
        let shared = s.get("unified_memory").and_then(Value::as_bool) == Some(true);
        out.push_str(&format!(
            "; GPU {} with {:.0} GB{} ({})",
            text(s, "gpu_name"),
            number(s, "gpu_vram_gb"),
            if shared { ", shared with the CPU" } else { "" },
            text(s, "backend")
        ));
    } else {
        out.push_str("; no GPU found");
    }
    out
}

const LLAMA_CPP: &str = "llama.cpp";
const OLLAMA: &str = "Ollama";
const LM_STUDIO: &str = "LM Studio";
const MLX: &str = "MLX";

/// The runtimes to recommend for: those named in the configuration, or else
/// those on this machine. llmfit reports llama.cpp (its command line names no
/// other), and the rest are found by their programs.
pub fn runtimes(system: &Value, cfg: &Config) -> Vec<String> {
    let known = [LLAMA_CPP, OLLAMA, LM_STUDIO, MLX];
    let named: Vec<String> = cfg.llmfit.runtimes.iter().map(|r| r.trim()).filter(|r| !r.is_empty()).map(|r| {
        let plain = |s: &str| s.to_lowercase().replace([' ', '.', '-', '_'], "");
        known.iter().find(|k| plain(k) == plain(r)).map(|k| k.to_string()).unwrap_or_else(|| r.to_string())
    }).collect();
    if !named.is_empty() {
        return named;
    }
    let reported = |name: &str| system.pointer(&format!("/providers/{name}/available")).and_then(Value::as_bool) == Some(true);
    let have = [
        (LLAMA_CPP, reported(LLAMA_CPP) || installed("llama-server").is_some()),
        (OLLAMA, reported("ollama") || installed("ollama").is_some()),
        (LM_STUDIO, reported("lmstudio") || installed("lms").is_some()),
        (MLX, reported("mlx") || installed("mlx_lm.server").is_some()),
    ];
    have.iter().filter(|(_, there)| *there).map(|(name, _)| name.to_string()).collect()
}

/// What was asked for, and what there is here to serve it with.
pub struct Asked<'a> {
    pub purpose: &'a str,
    pub context: Option<u64>,
    pub runtimes: Vec<String>,
    pub limit: usize,
}

impl Asked<'_> {
    fn has(&self, runtime: &str) -> bool {
        self.runtimes.iter().any(|r| r == runtime)
    }

    /// Whether every runtime here serves GGUF files: llama.cpp and what is built on it.
    fn gguf_only(&self) -> bool {
        !self.runtimes.is_empty() && self.runtimes.iter().all(|r| matches!(r.as_str(), LLAMA_CPP | OLLAMA | LM_STUDIO))
    }

    /// Whether a model can be served by what is installed: with only GGUF
    /// runtimes, a GGUF build of it has to exist (or Ollama has to carry it).
    fn serves(&self, model: &Value) -> bool {
        !self.gguf_only()
            || model.get("installed").and_then(Value::as_bool) == Some(true)
            || model.get("gguf_sources").and_then(Value::as_array).is_some_and(|s| !s.is_empty())
            || (self.has(OLLAMA) && !text(model, "ollama_name").is_empty())
    }

    /// How to serve a model for narsil with what is installed, where a build of it is known.
    fn serve(&self, m: &Value, quant: &str) -> Option<String> {
        let source = m.get("gguf_sources").and_then(Value::as_array).and_then(|s| s.first()).map(|s| text(s, "repo")).unwrap_or("");
        let tag = if quant.is_empty() { String::new() } else { format!(":{quant}") };
        let context = number(m, "effective_context_length");
        if !source.is_empty() && (self.has(LLAMA_CPP) || self.runtimes.is_empty()) {
            return Some(format!("serve: llama-server -hf {source}{tag} -c {context} -ngl all --jinja --port 8080"));
        }
        if self.has(OLLAMA) && (!source.is_empty() || !text(m, "ollama_name").is_empty()) {
            // Ollama's own build has its own quantisation; the one named is had from the GGUF.
            return Some(match source.is_empty() {
                true => format!("serve: ollama run {}", text(m, "ollama_name")),
                false => format!("serve: ollama run hf.co/{source}{tag}"),
            });
        }
        (!source.is_empty()).then(|| format!("GGUF build: {source}{}", if quant.is_empty() { String::new() } else { format!(" ({quant})") }))
    }

    /// Where narsil finds the model once it is served.
    fn endpoint(&self) -> &'static str {
        match () {
            _ if self.has(LLAMA_CPP) || self.runtimes.is_empty() => "http://127.0.0.1:8080/v1",
            _ if self.has(OLLAMA) => "http://127.0.0.1:11434/v1",
            _ if self.has(LM_STUDIO) => "http://127.0.0.1:1234/v1",
            _ => "http://127.0.0.1:8080/v1",
        }
    }
}

/// Whether this is a lab's own release, as against a community fine-tune or
/// re-upload: llmfit names the lab ("Alibaba" for `Qwen/…`) where it knows
/// one, and otherwise gives the uploading account in lower case.
fn from_a_lab(m: &Value) -> bool {
    let account = text(m, "name").split('/').next().unwrap_or("");
    let provider = text(m, "provider");
    !provider.is_empty() && provider != account.to_lowercase()
}

/// Whether a quantisation is heavier than the lighter one that is tried.
fn heavier(quant: &str) -> bool {
    let bits = |q: &str| q.strip_prefix('Q').and_then(|rest| rest.chars().next()).and_then(|c| c.to_digit(10));
    matches!((bits(quant), bits(LIGHTER_QUANT)), (Some(have), Some(lighter)) if have > lighter)
}

/// Models that llmfit cannot tell apart (the same size, speed and scores),
/// with the one to name first: a lab's own release before a fine-tune of it,
/// the newest before the older, the plainest name.
struct Group<'a> {
    lead: &'a Value,
    rest: Vec<&'a Value>,
}

fn grouped<'a>(models: &[&'a Value]) -> Vec<Group<'a>> {
    let tenths = |x: f64| (x * 10.0).round() as i64;
    let mut order = Vec::new();
    let mut alike: HashMap<[i64; 4], Vec<&'a Value>> = HashMap::new();
    for m in models {
        let key = [tenths(number(m, "score")), tenths(quality(m)), tenths(speed(m)), tenths(number(m, "params_b"))];
        alike.entry(key).or_insert_with(|| {
            order.push(key);
            Vec::new()
        })
        .push(m);
    }
    order
        .into_iter()
        .filter_map(|key| {
            let mut all = alike.remove(&key)?;
            all.sort_by(|a, b| {
                from_a_lab(b)
                    .cmp(&from_a_lab(a))
                    .then(text(b, "release_date").cmp(text(a, "release_date")))
                    .then(text(a, "name").len().cmp(&text(b, "name").len()))
            });
            let lead = all.remove(0);
            Some(Group { lead, rest: all })
        })
        .collect()
}

/// Labs' own releases, where there are any: for work that depends on tool
/// calls, they are the safer choice than a fine-tune nobody has tried it on.
fn labs_first(groups: Vec<Group<'_>>) -> Vec<Group<'_>> {
    match groups.iter().any(|g| from_a_lab(g.lead)) {
        true => groups.into_iter().filter(|g| from_a_lab(g.lead)).collect(),
        false => groups,
    }
}

/// The others that llmfit cannot tell from a group's first: labs' releases by name, the rest counted.
fn alike(group: &Group) -> Option<String> {
    if group.rest.is_empty() {
        return None;
    }
    let labs: Vec<&str> = group.rest.iter().filter(|m| from_a_lab(m)).map(|m| text(m, "name")).collect();
    let mut same: Vec<String> = labs.iter().take(3).map(|s| s.to_string()).collect();
    match (labs.len().saturating_sub(3), group.rest.len() - labs.len()) {
        (0, 0) => {}
        (0, 1) => same.push("1 community fine-tune or re-upload".to_string()),
        (0, n) => same.push(format!("{n} community fine-tunes or re-uploads")),
        (more, n) => same.push(format!("{} more", more + n)),
    }
    Some(format!("the same size, speed and score: {}", same.join(", ")))
}

/// One model, as a few lines to read.
fn entry(rank: usize, group: &Group, asked: &Asked) -> String {
    let m = group.lead;
    let parts = m.get("score_components").cloned().unwrap_or(Value::Null);
    let rate = match m.get("measured_tps").and_then(Value::as_f64) {
        Some(tps) => format!("{tps:.0} tok/s measured"),
        None => format!("about {:.0} tok/s ({})", number(m, "estimated_tps"), text(m, "estimate_confidence")),
    };
    let tools = m.get("capability_ids").and_then(Value::as_array).is_some_and(|c| c.iter().any(|v| v == "tool_use"));
    let quant = text(m, "best_quant");
    let mut out = format!(
        "{rank}. {} ({} parameters, {})\n   score {:.0}: quality {:.0}, speed {:.0}, fit {:.0}, context {:.0}\n   {} fit · {rate} · needs {:.1} of {:.0} GB{} · context {} of {} · {} · {}{}",
        text(m, "name"),
        text(m, "parameter_count"),
        text(m, "category").to_lowercase(),
        number(m, "score"),
        quality(m),
        number(&parts, "speed"),
        number(&parts, "fit"),
        number(&parts, "context"),
        text(m, "fit_level"),
        number(m, "memory_required_gb"),
        number(m, "memory_available_gb"),
        if quant.is_empty() { String::new() } else { format!(" at {quant}") },
        number(m, "effective_context_length"),
        number(m, "context_length"),
        if tools { "calls tools" } else { "no tool calling" },
        if text(m, "license").is_empty() { "licence not given" } else { text(m, "license") },
        if m.get("installed").and_then(Value::as_bool) == Some(true) { " · already installed" } else { "" },
    );
    if let Some(same) = alike(group) {
        out.push_str(&format!("\n   {same}"));
    }
    if let Some(how) = asked.serve(m, quant) {
        out.push_str(&format!("\n   {how}"));
    }
    out
}

fn fitting<'a>(found: &'a Value, asked: &Asked) -> Vec<&'a Value> {
    found.get("models").and_then(Value::as_array).map(|m| m.iter().filter(|m| asked.serves(m)).collect()).unwrap_or_default()
}

/// The models worth a second look at a lighter quantisation: at least as
/// capable as anything that runs at a usable speed, and too slow themselves
/// at the quantisation llmfit chose for them.
pub fn worth_a_lighter_quant(found: &Value, asked: &Asked) -> Vec<String> {
    let fit = fitting(found, asked);
    let best_usable = fit.iter().filter(|m| speed(m) >= USABLE_TPS).map(|m| quality(m)).fold(0.0, f64::max);
    let mut slow: Vec<&Value> =
        fit.iter().copied().filter(|m| speed(m) < USABLE_TPS && quality(m) >= best_usable && heavier(text(m, "best_quant"))).collect();
    slow.sort_by(|a, b| quality(b).total_cmp(&quality(a)).then(speed(b).total_cmp(&speed(a))));
    labs_first(grouped(&slow)).iter().take(3).map(|g| text(g.lead, "name").to_string()).collect()
}

/// What llmfit recommends, as an answer to read. Three ways of choosing:
/// llmfit's own score; the most capable that run at a usable speed (an agent
/// is better served by a more capable model than a faster one, once it is
/// fast enough); and those more capable still that a lighter quantisation
/// makes usable, from `lighter`: each a model's name and llmfit's plan for it.
pub fn recommendation(found: &Value, system: &Value, asked: &Asked, lighter: &[(String, Value)]) -> String {
    let all = found.get("models").and_then(Value::as_array).map(Vec::len).unwrap_or(0);
    let fit = fitting(found, asked);
    let mut out = format!("This machine: {}.\n", hardware(system));
    out.push_str(&match asked.runtimes.is_empty() {
        true => "No runtime to serve a model was found here (llama.cpp, Ollama, LM Studio, MLX): one has to be installed first.\n".to_string(),
        false => format!("Runtimes found here: {}.\n", asked.runtimes.join(", ")),
    });
    let what = match (asked.purpose, asked.context) {
        ("agentic", Some(c)) => format!("models that call tools, sized for a {c}-token context (an agent's instructions and tools alone take about 8,000)"),
        (p, Some(c)) => format!("{p} models, sized for a {c}-token context"),
        (p, None) => format!("{p} models"),
    };
    if fit.is_empty() {
        out.push_str(&format!("\nllmfit found no {what} that fit this machine well{}.", if all == 0 { "" } else { " and that the runtimes here can serve" }));
        return out;
    }

    let by_score = grouped(&fit);
    out.push_str(&format!(
        "\nAsked for: {what}. {} fit well and can be served here. The best by llmfit's score (quality, speed, fit and context together):\n\n",
        fit.len()
    ));
    let mut shown: Vec<&str> = Vec::new();
    for (i, group) in by_score.iter().take(asked.limit).enumerate() {
        out.push_str(&entry(i + 1, group, asked));
        out.push_str("\n\n");
        shown.push(text(group.lead, "name"));
    }

    let mut usable: Vec<&Value> = fit.iter().copied().filter(|m| speed(m) >= USABLE_TPS).collect();
    usable.sort_by(|a, b| quality(b).total_cmp(&quality(a)).then(number(b, "params_b").total_cmp(&number(a, "params_b"))));
    let capable: Vec<Group> = labs_first(grouped(&usable)).into_iter().filter(|g| !shown.contains(&text(g.lead, "name"))).take(3).collect();
    if !capable.is_empty() {
        out.push_str(&format!("The most capable that run at a usable speed here ({USABLE_TPS:.0} tok/s or more), where not already above:\n\n"));
        for (i, group) in capable.iter().enumerate() {
            out.push_str(&entry(i + 1, group, asked));
            out.push_str("\n\n");
        }
    }

    let quicker: Vec<(&Group, &Value)> = lighter
        .iter()
        .filter_map(|(name, plan)| Some((by_score.iter().find(|g| text(g.lead, "name") == name)?, plan)))
        .filter(|(_, plan)| plan.pointer("/current/estimated_tps").and_then(Value::as_f64).unwrap_or(0.0) >= USABLE_TPS)
        .collect();
    if !quicker.is_empty() {
        out.push_str(
            "More capable still, at a lighter quantisation. llmfit lists a model at the heaviest quantisation that fits, which is \
             also the slowest; these are too slow that way and usable this way, at a small cost in quality:\n\n",
        );
        for (i, (group, plan)) in quicker.iter().enumerate() {
            let m = group.lead;
            let now = plan.get("current").cloned().unwrap_or(Value::Null);
            out.push_str(&format!(
                "{}. {} ({} parameters, {}), quality {:.0}\n   at {}: about {:.0} tok/s, {} fit, needs {:.1} GB (at {}, llmfit's choice: about {:.0} tok/s, {:.1} GB)",
                i + 1,
                text(m, "name"),
                text(m, "parameter_count"),
                text(m, "category").to_lowercase(),
                quality(m),
                text(plan, "quantization"),
                number(&now, "estimated_tps"),
                text(&now, "fit_level"),
                plan.pointer("/minimum/vram_gb").and_then(Value::as_f64).unwrap_or(0.0),
                text(m, "best_quant"),
                speed(m),
                number(m, "memory_required_gb"),
            ));
            if let Some(same) = alike(group) {
                out.push_str(&format!("\n   {same}"));
            }
            if let Some(how) = asked.serve(m, text(plan, "quantization")) {
                out.push_str(&format!("\n   {how}"));
            }
            out.push_str("\n\n");
        }
    }
    out.push_str(
        "Speeds marked estimated are llmfit's projections for this hardware, not measurements. For agent work, weigh quality over \
         speed once the speed is usable: an agent takes many steps, and a wrong one costs more than a slow one. A lab's own release \
         is a safer choice than a fine-tune of it, whose tool calling may be untested.\n\
         To use one with narsil: serve it as given (or with any OpenAI-compatible server), then ",
    );
    out.push_str(&format!("`narsil setup --provider local --base-url {}`, and choose it with /model.", asked.endpoint()));
    out
}

pub async fn local_models(ctx: &ToolCtx<'_>) -> Result<ToolOutput> {
    let Some(program) = program(ctx.cfg) else {
        return Ok(ToolOutput::err(
            "llmfit is not installed, so there is nothing here that knows what this machine can run. \
             It can be installed with `brew install llmfit`; say so to the user rather than guessing from general knowledge.",
        ));
    };
    let input = &ctx.call.input;
    let action = opt_str(input, "action").unwrap_or("recommend");
    let args = |list: &[&str]| -> Vec<String> { list.iter().map(|s| s.to_string()).collect() };
    let out = match action {
        "hardware" => {
            let system = ask(&program, &args(&["system", "--json"])).await?;
            let have = runtimes(&system, ctx.cfg);
            format!("{}.\nRuntimes found here: {}.", hardware(&system), if have.is_empty() { "none".to_string() } else { have.join(", ") })
        }
        "recommend" => {
            let purpose = opt_str(input, "purpose").unwrap_or("agentic");
            let limit = opt_u64(input, "limit").unwrap_or(5).clamp(1, 10) as usize;
            let context = opt_u64(input, "context").or((purpose == "agentic").then_some(AGENT_CONTEXT));
            let system = ask(&program, &args(&["system", "--json"])).await?;
            let asked = Asked { purpose, context, runtimes: runtimes(&system, ctx.cfg), limit };
            let sized: Vec<String> = context.map(|c| vec!["--max-context".to_string(), c.to_string()]).unwrap_or_default();
            let mut ask_for = sized.clone();
            ask_for.extend(args(&["recommend", "-n", POOL, "--min-fit", "good"]));
            match purpose {
                "agentic" => ask_for.extend(args(&["--capability", "tool_use"])),
                "general" | "coding" | "reasoning" | "chat" | "multimodal" | "embedding" => ask_for.extend(args(&["--use-case", purpose])),
                other => bail!("unknown purpose {other:?}; use agentic, coding, reasoning, chat, general, multimodal or embedding"),
            }
            if asked.gguf_only() {
                // What the runtimes here can serve.
                ask_for.extend(args(&["--runtime", "llamacpp"]));
            }
            let found = ask(&program, &ask_for).await?;
            // The capable ones that are too slow as listed: how they do at a lighter quantisation.
            let plans = worth_a_lighter_quant(&found, &asked).into_iter().map(|name| {
                let mut plan = vec!["plan".to_string(), name.clone()];
                plan.extend(sized.iter().map(|s| s.replace("--max-context", "--context")));
                plan.extend(args(&["--quant", LIGHTER_QUANT, "--json"]));
                let program = &program;
                async move { ask(program, &plan).await.ok().map(|found| (name, found)) }
            });
            let lighter: Vec<(String, Value)> = futures_util::future::join_all(plans).await.into_iter().flatten().collect();
            recommendation(&found, &system, &asked, &lighter)
        }
        "plan" => {
            let Some(model) = opt_str(input, "model") else { bail!("`plan` needs a `model`") };
            let context = opt_u64(input, "context").unwrap_or(AGENT_CONTEXT);
            let mut plan_for = vec!["plan".to_string(), model.to_string(), "--context".into(), context.to_string()];
            if let Some(quant) = opt_str(input, "quant") {
                plan_for.extend(["--quant".to_string(), quant.to_string()]);
            }
            plan_for.push("--json".into());
            let plan = ask(&program, &plan_for).await?;
            let need = |level: &str| {
                let v = plan.get(level).cloned().unwrap_or(Value::Null);
                format!("{:.0} GB of GPU memory, {:.0} GB of RAM, {:.0} cores", number(&v, "vram_gb"), number(&v, "ram_gb"), number(&v, "cpu_cores"))
            };
            let now = plan.get("current").cloned().unwrap_or(Value::Null);
            let here = match now.get("estimated_tps").and_then(Value::as_f64) {
                Some(tps) => format!("about {tps:.0} tok/s ({}), {} fit", text(&now, "run_mode").to_uppercase(), text(&now, "fit_level")),
                None => "does not run".to_string(),
            };
            format!(
                "{} at {} with a {}-token context ({:.1} GB on disk):\n  on this machine: {here}\n  needs at least: {}\n  recommended: {}\n{}",
                text(&plan, "model_name"),
                text(&plan, "quantization"),
                number(&plan, "context"),
                number(&plan, "disk_size_gb"),
                need("minimum"),
                need("recommended"),
                text(&plan, "estimate_notice")
            )
        }
        "info" => {
            let Some(model) = opt_str(input, "model") else { bail!("`info` needs a `model`") };
            let context = opt_u64(input, "context").unwrap_or(AGENT_CONTEXT).to_string();
            let info = ask(&program, &args(&["--max-context", &context, "info", model, "--json"])).await?;
            let system = ask(&program, &args(&["system", "--json"])).await?;
            let asked = Asked { purpose: "", context: None, runtimes: runtimes(&system, ctx.cfg), limit: 5 };
            let models: Vec<&Value> = info.get("models").and_then(Value::as_array).map(|m| m.iter().collect()).unwrap_or_default();
            if models.is_empty() {
                bail!("llmfit knows no model called {model:?}");
            }
            models.iter().take(asked.limit).enumerate().map(|(i, m)| entry(i + 1, &Group { lead: m, rest: Vec::new() }, &asked)).collect::<Vec<_>>().join("\n\n")
        }
        other => bail!("unknown action {other:?}; use recommend, hardware, plan or info"),
    };
    Ok(ToolOutput::ok(out))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A model as llmfit describes one: `maker/name`, who llmfit says made it, and its numbers.
    fn model(name: &str, provider: &str, quality: f64, tps: f64, params: f64, gguf: bool) -> Value {
        let short = name.split('/').next_back().unwrap();
        json!({
            "name": name, "provider": provider, "parameter_count": format!("{params}B"), "params_b": params, "category": "Reasoning",
            "score": (quality + tps) / 2.0, "score_components": { "quality": quality, "speed": tps * 2.0, "fit": 100.0, "context": 100.0 },
            "fit_level": "Perfect", "estimated_tps": tps, "estimate_confidence": "estimated", "measured_tps": null,
            "memory_required_gb": params * 1.1, "memory_available_gb": 126.57, "best_quant": "Q8_0",
            "effective_context_length": 32768, "context_length": 131072, "capability_ids": ["tool_use"], "license": "apache-2.0",
            "installed": false, "ollama_name": null, "release_date": null,
            "gguf_sources": if gguf { json!([{ "provider": "unsloth", "repo": format!("unsloth/{short}-GGUF") }]) } else { json!([]) },
        })
    }

    fn plan(tps: f64, gb: f64) -> Value {
        json!({ "quantization": "Q4_K_M", "minimum": { "vram_gb": gb }, "current": { "estimated_tps": tps, "fit_level": "Perfect", "run_mode": "Gpu" } })
    }

    #[test]
    fn a_recommendation_reads_as_an_answer_for_this_machine() {
        let system = json!({
            "providers": { "llama.cpp": { "available": true }, "ollama": { "available": false } },
            "system": { "cpu_name": "Ryzen AI", "cpu_cores": 32, "total_ram_gb": 126.57, "has_gpu": true, "gpu_name": "Radeon 8060S", "gpu_vram_gb": 126.57, "unified_memory": true, "backend": "ROCm" }
        });
        assert_eq!(hardware(&system), "Ryzen AI, 32 cores, 127 GB of memory; GPU Radeon 8060S with 127 GB, shared with the CPU (ROCm)");
        let with = |runtimes: &[&str], purpose: &'static str, context: Option<u64>, limit: usize| Asked {
            purpose,
            context,
            runtimes: runtimes.iter().map(|r| r.to_string()).collect(),
            limit,
        };
        let asked = with(&[LLAMA_CPP], "agentic", Some(32768), 2);
        assert!(asked.gguf_only() && with(&[LLAMA_CPP, OLLAMA], "", None, 1).gguf_only());
        assert!(!with(&[LLAMA_CPP, MLX], "", None, 1).gguf_only() && !with(&[], "", None, 1).gguf_only());

        // In llmfit's order, best score first.
        let found = json!({ "models": [
            model("someone/Small-Fast-8B-remix", "someone", 80.0, 40.0, 8.0, true),
            model("acme/Small-Fast-8B", "Acme", 80.0, 40.0, 8.0, true),
            model("acme/Small-Fast-8B-Instruct", "Acme", 80.0, 40.0, 8.0, true),
            model("bigco/Only-For-vLLM-27B", "BigCo", 95.0, 12.0, 27.0, false),
            model("acme/Mid-14B", "Acme", 86.0, 22.0, 14.0, true),
            model("tinkerer/Wild-Merge-20B", "tinkerer", 96.0, 12.0, 20.0, true),
            model("acme/Big-Capable-32B", "Acme", 94.0, 11.0, 32.0, true),
            model("acme/Huge-70B", "Acme", 97.0, 3.0, 70.0, true),
            model("acme/Vast-120B", "Acme", 99.0, 1.0, 120.0, true),
            model("acme/Slow-And-Plain-40B", "Acme", 90.0, 4.0, 40.0, true),
        ] });

        // Too slow as listed, and at least as capable as anything usable: looked at again at a lighter quantisation.
        assert_eq!(worth_a_lighter_quant(&found, &asked), vec!["acme/Vast-120B", "acme/Huge-70B"]);
        let lighter = vec![("acme/Vast-120B".to_string(), plan(2.5, 70.0)), ("acme/Huge-70B".to_string(), plan(9.2, 42.0))];
        let said = recommendation(&found, &system, &asked, &lighter);

        // What was asked, of what machine, and what can serve it.
        assert!(said.starts_with("This machine: Ryzen AI, 32 cores"), "{said}");
        assert!(said.contains("Runtimes found here: llama.cpp.") && said.contains("models that call tools, sized for a 32768-token context"), "{said}");
        // By llmfit's score. Those it cannot tell apart are one entry, under the lab's own release.
        assert!(said.contains("1. acme/Small-Fast-8B (8B parameters, reasoning)") && said.contains("2. acme/Mid-14B"), "{said}");
        assert!(said.contains("the same size, speed and score: acme/Small-Fast-8B-Instruct, 1 community fine-tune or re-upload"), "{said}");
        assert!(!said.contains("1. someone/"), "{said}");
        assert!(said.contains("Perfect fit · about 40 tok/s (estimated) · needs 8.8 of 127 GB at Q8_0 · context 32768 of 131072 · calls tools · apache-2.0"), "{said}");
        assert!(said.contains("serve: llama-server -hf unsloth/Small-Fast-8B-GGUF:Q8_0 -c 32768 -ngl all --jinja --port 8080"), "{said}");
        // What llama.cpp cannot serve is not put forward.
        assert!(!said.contains("Only-For-vLLM"), "{said}");
        // The most capable at a usable speed: a lab's release, not the fine-tune that scores higher, nor what is too slow.
        let capable = &said[said.find("The most capable").expect("the second list")..said.find("More capable still").expect("the third list")];
        assert!(capable.contains("1. acme/Big-Capable-32B") && !capable.contains("Wild-Merge") && !capable.contains("Huge-70B"), "{capable}");
        // And what a lighter quantisation makes usable: the 70B, not the 120B that is still too slow.
        let quicker = &said[said.find("More capable still").unwrap()..];
        assert!(quicker.contains("1. acme/Huge-70B (70B parameters, reasoning), quality 97"), "{quicker}");
        assert!(quicker.contains("at Q4_K_M: about 9 tok/s, Perfect fit, needs 42.0 GB (at Q8_0, llmfit's choice: about 3 tok/s, 77.0 GB)"), "{quicker}");
        assert!(quicker.contains("serve: llama-server -hf unsloth/Huge-70B-GGUF:Q4_K_M -c 32768") && !quicker.contains("Vast-120B"), "{quicker}");
        assert!(said.contains("narsil setup --provider local"), "{said}");

        // With a runtime that is not held to GGUF files, every model that fits is put forward.
        assert!(recommendation(&found, &system, &with(&[LLAMA_CPP, MLX], "coding", None, 5), &[]).contains("bigco/Only-For-vLLM-27B"));
        // With Ollama alone, it is Ollama that serves, at Ollama's address.
        let ollama = recommendation(&found, &system, &with(&[OLLAMA], "agentic", Some(32768), 2), &lighter);
        assert!(ollama.contains("serve: ollama run hf.co/unsloth/Small-Fast-8B-GGUF:Q8_0") && !ollama.contains("llama-server"), "{ollama}");
        assert!(ollama.contains("--base-url http://127.0.0.1:11434/v1") && !ollama.contains("Only-For-vLLM"), "{ollama}");
        // Nothing fits, or nothing to serve with: it says so.
        assert!(recommendation(&json!({ "models": [] }), &system, &asked, &[]).contains("found no models that call tools"));
        let none = recommendation(&found, &system, &with(&[], "agentic", Some(32768), 1), &[]);
        assert!(none.contains("No runtime to serve a model was found here") && none.contains("bigco/Only-For-vLLM-27B"), "{none}");
        let bare = json!({ "providers": {}, "system": { "cpu_name": "CPU", "cpu_cores": 4, "total_ram_gb": 8.0, "has_gpu": false } });
        assert_eq!(hardware(&bare), "CPU, 4 cores, 8 GB of memory; no GPU found");
    }

    #[test]
    fn a_lab_release_is_told_from_a_fine_tune_and_a_heavy_quantisation_from_a_light_one() {
        let lab = |name: &str, provider: &str| from_a_lab(&json!({ "name": name, "provider": provider }));
        assert!(lab("Qwen/Qwen3.6-27B", "Alibaba") && lab("google/gemma-4-12B-it", "Google") && lab("meta-llama/Llama-3.2-3B-Instruct", "Meta"));
        assert!(!lab("DavidAU/Qwen3.5-27B-Remix", "davidau") && !lab("unsloth/Qwen3.6-27B", "unsloth") && !lab("nobody/x", ""));
        // The runtimes named in the configuration are the ones recommended for, whatever is installed.
        let mut cfg = Config::default();
        cfg.llmfit.runtimes = vec!["llamacpp".into(), " LM-Studio ".into(), "vLLM".into(), "".into()];
        assert_eq!(runtimes(&json!({}), &cfg), vec![LLAMA_CPP, LM_STUDIO, "vLLM"]);
        assert!(heavier("Q8_0") && heavier("Q6_K") && heavier("Q5_K_M"));
        assert!(!heavier("Q4_K_M") && !heavier("Q3_K_S") && !heavier("AWQ-4bit") && !heavier(""));
    }
}
