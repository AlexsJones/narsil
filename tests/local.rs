//! Asking which local models suit this machine, checked through a running
//! daemon with a stand-in for llmfit: a script that answers as llmfit does
//! and writes down what it was asked. narsil's part is what it asks for (for
//! agent work: models that call tools, sized for an agent's context, that the
//! runtimes here can serve), and that the answer reaches the agent whole.

mod common;

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use common::{Config, Narsil, script};
use serde_json::{Value, json};

/// A model as llmfit describes one.
fn model(name: &str, provider: &str, quality: f64, tps: f64, params: f64, gguf: bool) -> Value {
    let short = name.split('/').next_back().unwrap();
    json!({
        "name": name, "provider": provider, "parameter_count": format!("{params}B"), "params_b": params, "category": "Reasoning",
        "score": (quality + tps) / 2.0, "score_components": { "quality": quality, "speed": tps * 2.0, "fit": 100.0, "context": 100.0 },
        "fit_level": "Perfect", "estimated_tps": tps, "estimate_confidence": "estimated", "measured_tps": null,
        "memory_required_gb": params * 1.1, "memory_available_gb": 64.0, "best_quant": "Q8_0",
        "effective_context_length": 32768, "context_length": 131072, "capability_ids": ["tool_use"], "license": "apache-2.0",
        "installed": false, "ollama_name": null, "release_date": null,
        "gguf_sources": if gguf { json!([{ "provider": "unsloth", "repo": format!("unsloth/{short}-GGUF") }]) } else { json!([]) },
    })
}

/// Put a stand-in llmfit in an instance being set up, with llama.cpp as the
/// runtime to recommend for.
fn stand_in(base: &Path, config: &mut Config) {
    let dir = base.join("llmfit");
    std::fs::create_dir_all(&dir).unwrap();
    let system = json!({
        "providers": { "llama.cpp": { "available": true } },
        "system": { "cpu_name": "Test CPU", "cpu_cores": 16, "total_ram_gb": 64.0, "has_gpu": true, "gpu_name": "Test GPU", "gpu_vram_gb": 24.0, "unified_memory": false, "backend": "CUDA" }
    });
    let recommend = json!({ "models": [
        model("acme/Small-Fast-8B", "Acme", 80.0, 40.0, 8.0, true),
        model("bigco/No-GGUF-27B", "BigCo", 95.0, 12.0, 27.0, false),
        model("acme/Big-Capable-32B", "Acme", 94.0, 11.0, 32.0, true),
        model("acme/Huge-70B", "Acme", 97.0, 3.0, 70.0, true),
    ] });
    let plan = json!({
        "model_name": "acme/Huge-70B", "quantization": "Q4_K_M", "context": 32768, "disk_size_gb": 40.0,
        "minimum": { "vram_gb": 42.0, "ram_gb": 8.0, "cpu_cores": 4 }, "recommended": { "vram_gb": 48.0, "ram_gb": 16.0, "cpu_cores": 8 },
        "current": { "estimated_tps": 9.2, "fit_level": "Good", "run_mode": "Gpu" }, "estimate_notice": "Estimates, not a benchmark."
    });
    let unclear = json!({ "error": { "kind": "invalid_selector", "message": "Multiple models match 'Capable'. Please be more specific:\n  - acme/Big-Capable-32B\n  - other/Capable-7B" } });
    for (name, value) in [
        ("system", &system),
        ("recommend", &recommend),
        ("plan", &plan),
        ("info", &unclear),
    ] {
        std::fs::write(
            dir.join(format!("{name}.json")),
            serde_json::to_string_pretty(value).unwrap(),
        )
        .unwrap();
    }
    let program = dir.join("llmfit");
    let answers = "#!/bin/sh\nhere=$(dirname \"$0\")\necho \"$*\" >> \"$here/asked\"\nfor word in \"$@\"; do\n  case \"$word\" in\n    \
                   system|recommend|plan|info) cat \"$here/$word.json\"; exit 0 ;;\n  esac\ndone\necho 'error: unrecognized subcommand' >&2\nexit 2\n";
    std::fs::write(&program, answers).unwrap();
    std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
    config.set("llmfit", "command", &format!("\"{}\"", program.display()));
    config.set("llmfit", "runtimes", "[\"llama.cpp\"]");
}

/// What the stand-in was asked, a line for each time it was run.
fn asked(narsil: &Narsil) -> Vec<String> {
    let file: PathBuf = narsil.home().parent().unwrap().join("llmfit/asked");
    std::fs::read_to_string(file)
        .unwrap_or_default()
        .lines()
        .map(str::to_string)
        .collect()
}

#[test]
fn asked_for_a_model_for_agent_work_narsil_asks_llmfit_for_ones_that_call_tools_and_can_be_served_here()
 {
    let narsil = Narsil::with(stand_in);
    let dir = narsil.dir("anywhere");

    // With llmfit installed, the agent is offered the tool.
    let offered = narsil.ask(&dir, "!tools");
    assert!(
        offered
            .said
            .split_whitespace()
            .any(|tool| tool == "local_models"),
        "{}",
        offered.said
    );

    let session = narsil.ask(
        &dir,
        &script(&[r#"!tool local_models {"action":"recommend"}"#, "!say Told."]),
    );
    let calls = session.calls_to("local_models");
    assert!(!calls[0].failed, "{:?}", calls[0]);
    let said = &calls[0].result;

    // What it asked llmfit: the machine, then every model that calls tools and fits with an
    // agent's context, for the runtime that is here; then the capable one that is too slow
    // as listed, again at a lighter quantisation.
    let asked = asked(&narsil);
    assert_eq!(
        asked,
        vec![
            "system --json",
            "--max-context 32768 recommend -n 2000 --min-fit good --capability tool_use --runtime llamacpp",
            "plan acme/Huge-70B --context 32768 --quant Q4_K_M --json",
        ],
        "{asked:#?}"
    );

    // The answer the agent is given: the machine, the models, how fast, and how to serve one for narsil.
    assert!(said.starts_with("This machine: Test CPU, 16 cores, 64 GB of memory; GPU Test GPU with 24 GB (CUDA).\nRuntimes found here: llama.cpp."), "{said}");
    assert!(
        said.contains("1. acme/Small-Fast-8B (8B parameters, reasoning)")
            && said.contains("about 40 tok/s (estimated)"),
        "{said}"
    );
    assert!(
        said.contains("serve: llama-server -hf unsloth/Small-Fast-8B-GGUF:Q8_0 -c 32768"),
        "{said}"
    );
    assert!(
        said.contains("acme/Big-Capable-32B") && !said.contains("No-GGUF-27B"),
        "{said}"
    );
    let lighter = &said[said
        .find("More capable still")
        .expect("the lighter quantisation")..];
    assert!(
        lighter.contains("1. acme/Huge-70B")
            && lighter.contains("at Q4_K_M: about 9 tok/s, Good fit, needs 42.0 GB"),
        "{lighter}"
    );
    assert!(
        said.contains("narsil setup --provider local --base-url http://127.0.0.1:8080/v1"),
        "{said}"
    );
}

#[test]
fn a_purpose_a_plan_and_an_unclear_name_go_to_llmfit_as_asked() {
    let narsil = Narsil::with(stand_in);
    let dir = narsil.dir("anywhere");
    let session = narsil.ask(
        &dir,
        &script(&[
            r#"!tool local_models {"action":"recommend","purpose":"coding","limit":1}"#,
            r#"!tool local_models {"action":"plan","model":"acme/Huge-70B","quant":"Q5_K_M","context":16384}"#,
            r#"!tool local_models {"action":"info","model":"Capable"}"#,
            r#"!tool local_models {"action":"hardware"}"#,
            r#"!tool local_models {"action":"recommend","purpose":"poetry"}"#,
            "!say Told.",
        ]),
    );
    let calls = session.calls_to("local_models");
    let asked = asked(&narsil);

    // Another purpose is llmfit's use case, with no context of narsil's choosing.
    assert!(
        asked.contains(
            &"recommend -n 2000 --min-fit good --use-case coding --runtime llamacpp".to_string()
        ),
        "{asked:#?}"
    );
    assert!(
        !calls[0].failed
            && calls[0].result.contains("Asked for: coding models.")
            && !calls[0].result.contains("2. acme/"),
        "{:?}",
        calls[0]
    );

    // A plan is for the quantisation and the context named, and says how the model would run here.
    assert!(
        asked.contains(&"plan acme/Huge-70B --context 16384 --quant Q5_K_M --json".to_string()),
        "{asked:#?}"
    );
    assert!(
        calls[1]
            .result
            .contains("on this machine: about 9 tok/s (GPU), Good fit"),
        "{:?}",
        calls[1]
    );
    assert!(
        calls[1]
            .result
            .contains("needs at least: 42 GB of GPU memory, 8 GB of RAM, 4 cores"),
        "{:?}",
        calls[1]
    );

    // What llmfit cannot answer, it says why, and the agent is given that.
    assert!(
        calls[2].failed
            && calls[2].result.contains("Multiple models match 'Capable'")
            && calls[2].result.contains("acme/Big-Capable-32B"),
        "{:?}",
        calls[2]
    );

    assert!(
        !calls[3].failed
            && calls[3]
                .result
                .starts_with("Test CPU, 16 cores, 64 GB of memory"),
        "{:?}",
        calls[3]
    );
    // A purpose llmfit has no word for is refused before it is asked.
    assert!(
        calls[4].failed && calls[4].result.contains("unknown purpose"),
        "{:?}",
        calls[4]
    );
    assert!(
        !asked.iter().any(|line| line.contains("poetry")),
        "{asked:#?}"
    );
}

#[test]
fn without_llmfit_the_tool_is_not_offered_and_nothing_is_made_up() {
    // The instance a test gets has no llmfit, as on a machine where it is not installed.
    let narsil = Narsil::new();
    let dir = narsil.dir("anywhere");
    let offered = narsil.ask(&dir, "!tools");
    assert!(
        offered.said.contains("bash") && !offered.said.contains("local_models"),
        "{}",
        offered.said
    );

    // A model that calls it all the same is told there is nothing to ask.
    let session = narsil.ask(
        &dir,
        &script(&[r#"!tool local_models {"action":"recommend"}"#, "!say Told."]),
    );
    let call = &session.calls_to("local_models")[0];
    assert!(
        call.failed && call.result.contains("llmfit is not installed"),
        "{call:?}"
    );
}
