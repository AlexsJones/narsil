//! Whether the tokens narsil reports are the ones a backend said were used,
//! checked against the real thing. A recorder stands between narsil and the
//! backend and keeps every request and every streamed answer; the test reads
//! the backend's own figures out of what was recorded, by the rules its
//! documentation gives, and holds narsil's record to them call for call.
//!
//! Where a backend offers a second opinion, that is asked too: Anthropic's
//! token counter is given each request again, and an llama.cpp server is
//! asked to tokenize each prompt.
//!
//! These call a real model, so they run only when asked for:
//!
//!   NARSIL_LIVE_PROVIDER=anthropic NARSIL_LIVE_MODEL=claude-sonnet-5-5 \
//!   NARSIL_LIVE_KEY_ENV=MY_KEY_VARIABLE \
//!   cargo test --test accuracy -- --ignored --nocapture
//!
//! For a server of your own (llama.cpp, Ollama, LM Studio, vLLM), name where it is:
//!
//!   NARSIL_LIVE_PROVIDER=local NARSIL_LIVE_MODEL=llama3.2 \
//!   NARSIL_LIVE_BASE_URL=http://127.0.0.1:11434/v1 cargo test --test accuracy -- --ignored --nocapture

mod common;

use common::{Exchange, Narsil, Wire};
use serde_json::{Value, json};

/// What one answered request used, as (the whole prompt, of it read from a
/// cache, of it written to one, output).
type Used = (i64, i64, i64, i64);

fn var(name: &str) -> String {
    std::env::var(name).unwrap_or_default().trim().to_string()
}

/// Where a preset's backend is, as narsil's setup has it.
fn address(provider: &str) -> &'static str {
    match provider {
        "anthropic" => "https://api.anthropic.com",
        "openai" => "https://api.openai.com/v1",
        "gemini" => "https://generativelanguage.googleapis.com/v1beta/openai",
        "openrouter" => "https://openrouter.ai/api/v1",
        "deepseek" => "https://api.deepseek.com/v1",
        "mistral" => "https://api.mistral.ai/v1",
        "groq" => "https://api.groq.com/openai/v1",
        "xai" => "https://api.x.ai/v1",
        other => panic!("NARSIL_LIVE_BASE_URL is needed for the provider {other:?}"),
    }
}

/// The JSON of each `data:` line of a stream.
fn events(response: &str) -> Vec<Value> {
    response.lines().filter_map(|l| l.strip_prefix("data:")).filter_map(|d| serde_json::from_str(d.trim()).ok()).collect()
}

fn num(v: &Value, path: &str) -> Option<i64> {
    v.pointer(path).and_then(Value::as_i64)
}

/// Anthropic's own figures for a streamed message: `message_start` has the
/// input, and each `message_delta` the running totals, which replace it.
/// The three input fields are apart, and together are the prompt.
fn anthropic_said(response: &str) -> Option<Used> {
    let (mut input, mut read, mut write, mut output) = (None, 0, 0, 0);
    for event in events(response) {
        let usage = match event["type"].as_str() {
            Some("message_start") => &event["message"]["usage"],
            Some("message_delta") => &event["usage"],
            _ => continue,
        };
        input = num(usage, "/input_tokens").or(input);
        read = num(usage, "/cache_read_input_tokens").unwrap_or(read);
        write = num(usage, "/cache_creation_input_tokens").unwrap_or(write);
        output = num(usage, "/output_tokens").unwrap_or(output);
    }
    Some((input? + read + write, read, write, output))
}

/// An OpenAI-style backend's own figures: the last `usage` it sent. The
/// prompt includes what was cached; output is whatever the total has beyond
/// the prompt, where a backend counts reasoning there and not in
/// `completion_tokens`. An llama.cpp server's timings say what it evaluated
/// and what it took from its cache, which together are the prompt.
fn openai_said(response: &str) -> Option<Used> {
    let all = events(response);
    let usage = all.iter().rev().find_map(|e| e.get("usage").filter(|u| u.is_object()).or_else(|| e.pointer("/x_groq/usage")))?;
    let timings = all.iter().rev().find_map(|e| e.get("timings").filter(|t| t.is_object()));
    let server = timings.map(|t| (num(t, "/cache_n").unwrap_or(0), num(t, "/prompt_n").unwrap_or(0)));
    let stated = num(usage, "/prompt_tokens")?;
    let prompt = stated.max(server.map(|(cached, evaluated)| cached + evaluated).unwrap_or(0));
    let read = num(usage, "/prompt_tokens_details/cached_tokens")
        .filter(|n| *n > 0)
        .or(num(usage, "/prompt_cache_hit_tokens").filter(|n| *n > 0))
        .or(server.map(|(cached, _)| cached))
        .unwrap_or(0);
    let write = num(usage, "/prompt_tokens_details/cache_write_tokens").or(num(usage, "/prompt_tokens_details/created_cache_tokens")).unwrap_or(0);
    let completion = num(usage, "/completion_tokens").unwrap_or(0);
    let output = num(usage, "/total_tokens").map(|total| total - stated).filter(|n| *n > completion).unwrap_or(completion);
    Some((prompt, read, write, output))
}

/// Ask a backend something directly, outside narsil.
fn post(url: &str, headers: &[(&str, String)], body: &Value) -> Result<Value, String> {
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    runtime.block_on(async {
        let mut request = reqwest::Client::new().post(url).json(body);
        for (name, value) in headers {
            request = request.header(*name, value);
        }
        let answer = request.send().await.map_err(|e| e.to_string())?;
        let status = answer.status();
        let text = answer.text().await.map_err(|e| e.to_string())?;
        if !status.is_success() {
            return Err(format!("{status}: {}", text.chars().take(300).collect::<String>()));
        }
        serde_json::from_str(&text).map_err(|e| e.to_string())
    })
}

#[test]
#[ignore = "calls a real model"]
fn the_tokens_narsil_reports_are_the_ones_the_backend_said_it_used() {
    let (provider, model, key_env) = (var("NARSIL_LIVE_PROVIDER"), var("NARSIL_LIVE_MODEL"), var("NARSIL_LIVE_KEY_ENV"));
    assert!(!provider.is_empty() && !model.is_empty(), "set NARSIL_LIVE_PROVIDER and NARSIL_LIVE_MODEL (see the top of tests/accuracy.rs)");
    let base = match var("NARSIL_LIVE_BASE_URL") {
        given if !given.is_empty() => given,
        _ => address(&provider).to_string(),
    };
    // `https://host/some/path` as the place to pass requests on to, and the path narsil keeps.
    let scheme_end = base.find("://").expect("a base URL with a scheme") + 3;
    let (origin, path) = match base[scheme_end..].find('/') {
        Some(at) => (&base[..scheme_end + at], base[scheme_end + at..].trim_end_matches('/')),
        None => (base.as_str(), ""),
    };
    let anthropic = provider == "anthropic";
    let wire = Wire::to(origin);
    let through = wire.url(path);
    let mut setup = vec!["--provider", provider.as_str(), "--base-url", through.as_str(), "--model", model.as_str(), "--no-test"];
    if !key_env.is_empty() {
        assert!(std::env::var(&key_env).is_ok_and(|v| !v.is_empty()), "the variable {key_env} (named by NARSIL_LIVE_KEY_ENV) holds no key");
        setup.extend(["--api-key-env", key_env.as_str()]);
    }
    let narsil = Narsil::with_backend(&setup, |_, _| {});
    let dir = narsil.dir("x");
    std::fs::write(dir.join("notes.txt"), "The harbour light is green.\n").unwrap();

    // Two requests in one session: the second arrives with the first already in a cache, where there is one.
    let first = narsil.ask(&dir, "Read the file notes.txt in this directory and tell me what colour the harbour light is.");
    let second = narsil.ask_again(&first, &dir, "Now run `echo narsil` in a shell and tell me exactly what it printed.");
    println!("{provider}/{model}\n  first: {}\n  second: {}", first.said.trim().replace('\n', "\n    "), second.said.trim().replace('\n', "\n    "));

    // What the backend said, read from the recording.
    let answered: Vec<Exchange> = wire
        .exchanges()
        .into_iter()
        .filter(|e| e.status == 200 && (e.path.ends_with("/messages") || e.path.ends_with("/chat/completions")))
        .collect();
    let said: Vec<Used> = answered
        .iter()
        .map(|e| if anthropic { anthropic_said(&e.response) } else { openai_said(&e.response) }.unwrap_or_else(|| panic!("no usage in an answer from the backend:\n{}", e.response)))
        .collect();
    assert!(said.len() >= 2, "the backend answered {} requests", said.len());

    // What narsil recorded, call for call.
    let rows = narsil.column(
        "SELECT (input + cache_read + cache_write) || ' ' || cache_read || ' ' || cache_write || ' ' || output || ' ' || estimated FROM model_usage ORDER BY id",
        &[],
    );
    let recorded: Vec<(Used, bool)> = rows
        .iter()
        .map(|row| {
            let n: Vec<i64> = row.split(' ').map(|x| x.parse().unwrap()).collect();
            ((n[0], n[1], n[2], n[3]), n[4] != 0)
        })
        .collect();
    println!("  call   prompt   cached  written   output    (the backend)      prompt   cached  written   output    (narsil)");
    for (i, used) in said.iter().enumerate() {
        let mine = recorded.get(i).map(|(u, _)| *u).unwrap_or_default();
        println!("  {:>4} {:>8} {:>8} {:>8} {:>8}                    {:>8} {:>8} {:>8} {:>8}", i + 1, used.0, used.1, used.2, used.3, mine.0, mine.1, mine.2, mine.3);
    }
    assert_eq!(recorded.len(), said.len(), "narsil recorded {} calls and the backend answered {}", recorded.len(), said.len());
    // Call for call the same, but for a backend that gave a prompt as far smaller than it can have
    // been (one that counts only what it newly evaluated): there narsil puts back what was left
    // out, and says the count is partly its own. A conversation only grows, so the size is held
    // to the calls around it.
    let mut corrected = 0;
    for (i, ((mine, estimated), theirs)) in recorded.iter().zip(&said).enumerate() {
        if !estimated {
            assert_eq!(mine, theirs, "call {}: narsil's record is not what the backend said", i + 1);
            continue;
        }
        corrected += 1;
        let around = said.iter().map(|u| u.0).max().unwrap();
        println!("  call {}: the backend gave a prompt of {}, where those around it are {around}; narsil has it as ~{}", i + 1, theirs.0, mine.0);
        assert!(theirs.0 * 5 < around * 2, "call {}: narsil put aside a count that was plausible ({} of about {around})", i + 1, theirs.0);
        assert!(mine.0 * 10 >= around * 7 && mine.0 * 10 <= around * 13, "call {}: narsil's ~{} is not near {around}", i + 1, mine.0);
        assert_eq!((mine.0 - mine.1, mine.3), (theirs.0, theirs.3), "call {}: what the backend did count was changed", i + 1);
    }

    // And what it shows adds up to what it recorded.
    let report: Value = serde_json::from_str(&narsil.ok(&dir, &["usage", "--json"])).unwrap();
    let total = &report["total"];
    let said: Vec<Used> = recorded.iter().map(|(used, _)| *used).collect();
    let sum = |part: fn(&Used) -> i64| said.iter().map(part).sum::<i64>();
    assert_eq!(total["estimated"], corrected > 0);
    assert_eq!(total["calls"], said.len());
    assert_eq!(total["input"], sum(|u| u.0));
    assert_eq!(total["cache_read"], sum(|u| u.1));
    assert_eq!(total["cache_write"], sum(|u| u.2));
    assert_eq!(total["output"], sum(|u| u.3));
    assert_eq!(report["requests"].as_array().unwrap().len(), 2, "{report}");
    println!("  in all: {} calls, {} in ({}% cached), {} out", total["calls"], total["input"], total["cached_pct"], total["output"]);

    // A second opinion on the size of each prompt, where the backend will give one.
    if anthropic {
        let key = std::env::var(&key_env).unwrap();
        let headers = [("x-api-key", key), ("anthropic-version", "2023-06-01".to_string())];
        for (i, (exchange, used)) in answered.iter().zip(&said).enumerate() {
            // The same model, instructions, tools and conversation, to the counter that takes no account of caching.
            let mut body = json!({});
            for field in ["model", "system", "tools", "tool_choice", "messages", "thinking"] {
                if let Some(value) = exchange.request.get(field) {
                    body[field] = value.clone();
                }
            }
            let counted = post(&format!("{origin}/v1/messages/count_tokens"), &headers, &body).unwrap_or_else(|e| panic!("count_tokens: {e}"));
            let counted = num(&counted, "/input_tokens").expect("input_tokens");
            println!("  call {}: Anthropic's counter says {counted}, the reply said {} (off by {})", i + 1, used.0, used.0 - counted);
            // The counter is documented as an estimate; it is not far out.
            assert!((used.0 - counted).abs() <= (counted / 50).max(20), "call {}: counted {counted}, used {}", i + 1, used.0);
        }
    } else if let Ok(counted) = post(&format!("{origin}/apply-template"), &[], &json!({ "messages": answered[0].request["messages"], "tools": answered[0].request["tools"] })) {
        // An llama.cpp server: have it lay each prompt out and tokenize it, apart from answering it.
        for (i, (exchange, used)) in answered.iter().zip(&said).enumerate() {
            let prompt = if i == 0 { counted.clone() } else { post(&format!("{origin}/apply-template"), &[], &json!({ "messages": exchange.request["messages"], "tools": exchange.request["tools"] })).unwrap() };
            let tokens = post(&format!("{origin}/tokenize"), &[], &json!({ "content": prompt["prompt"], "add_special": true, "parse_special": true })).unwrap();
            let counted = tokens["tokens"].as_array().map(|t| t.len() as i64).unwrap_or(0);
            println!("  call {}: tokenizing the prompt gives {counted}, the reply said {} (off by {})", i + 1, used.0, used.0 - counted);
            assert!((used.0 - counted).abs() <= (counted / 50).max(8), "call {}: tokenized {counted}, used {}", i + 1, used.0);
        }
    }
}
