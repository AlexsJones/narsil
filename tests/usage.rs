//! What narsil reports of token use, checked through a running daemon: that
//! what a request is said to have used is what its calls to the model used
//! (subagents and compaction included, nothing twice), and that each
//! backend's own way of reporting usage arrives as the same numbers in the
//! receipt and in `narsil usage`. `tests/accuracy.rs` holds the same to real
//! backends.

mod common;

use std::io::{Read, Write};
use std::net::TcpListener;

use common::{Narsil, script};
use serde_json::Value;

/// `narsil usage --json` for the session tree that used a model last.
fn usage(narsil: &Narsil) -> Value {
    serde_json::from_str(&narsil.ok(&narsil.dir("x"), &["usage", "--json"])).expect("usage as JSON")
}

fn n(v: &Value) -> i64 {
    v.as_i64().unwrap_or_else(|| panic!("not a number: {v}"))
}

/// One number from the record of model calls.
fn sum(narsil: &Narsil, what: &str, when: &str) -> i64 {
    narsil.text(&format!("SELECT COALESCE({what}, 0) FROM model_usage WHERE {when}"), &[]).parse().unwrap()
}

#[test]
fn what_a_request_used_is_what_its_calls_used_with_its_subagents_counted_in() {
    let narsil = Narsil::new();
    let dir = narsil.dir("x");
    let first = narsil.ask(
        &dir,
        &script(&[
            "!bash echo one",
            r#"!tool spawn_agents {"agents":[{"name":"helper","task":"!bash echo from the helper ~~ !say The helper is done."}]}"#,
            "!bash seq 1 300",
            "!say All done.",
        ]),
    );
    // Four calls of its own and two of the subagent's: one request, and its receipt says so.
    let receipt = first.said.lines().find(|l| l.contains(" calls · ")).unwrap_or_else(|| panic!("no receipt in:\n{}", first.said));
    assert!(receipt.starts_with("(6 calls · ") && receipt.contains(" in (") && receipt.contains("% cached) · "), "{receipt}");

    let second = narsil.ask_again(&first, &dir, &script(&["!bash echo again", "!say Second done."]));
    assert!(second.said.contains("(2 calls · "), "{}", second.said);

    let report = usage(&narsil);
    let total = &report["total"];
    // The record of calls is where every number comes from, so they add up to it exactly.
    assert_eq!(n(&total["calls"]), sum(&narsil, "COUNT(*)", "1"));
    assert_eq!(n(&total["calls"]), 8);
    assert_eq!(n(&total["input"]), sum(&narsil, "SUM(input + cache_read + cache_write)", "1"));
    assert_eq!(n(&total["cache_read"]), sum(&narsil, "SUM(cache_read)", "1"));
    assert_eq!(n(&total["output"]), sum(&narsil, "SUM(output)", "1"));
    // "in" is everything sent: read from a cache, written to one, or neither.
    assert_eq!(n(&total["input"]), n(&total["input_new"]) + n(&total["cache_read"]) + n(&total["cache_write"]));
    assert!(n(&total["cache_read"]) > 0 && n(&total["cache_write"]) > 0, "{total}");
    assert_eq!(total["estimated"], false);

    // The session's own and its subagent's are apart, and together are the whole.
    assert_eq!(n(&report["subagents"]["count"]), 1);
    assert_eq!(n(&report["own"]["calls"]), 6);
    assert_eq!(n(&report["subagents"]["tokens"]["calls"]), 2);
    for part in ["input", "cache_read", "cache_write", "output"] {
        assert_eq!(n(&report["own"][part]) + n(&report["subagents"]["tokens"][part]), n(&total[part]), "{part}");
    }
    // It agrees with the running total each session keeps, which is counted separately.
    let kept: i64 = narsil.text("SELECT SUM(input_tokens) FROM sessions", &[]).parse().unwrap();
    assert_eq!(n(&total["input"]), kept);
    let kept_out: i64 = narsil.text("SELECT SUM(output_tokens) FROM sessions", &[]).parse().unwrap();
    assert_eq!(n(&total["output"]), kept_out);

    // By request: the subagent's calls are in the request it worked on, and the requests are the whole.
    let requests = report["requests"].as_array().unwrap();
    assert_eq!(requests.iter().map(|q| n(&q["tokens"]["calls"])).collect::<Vec<_>>(), vec![6, 2]);
    assert!(requests[0]["asked"].as_str().unwrap().starts_with("!bash echo one"), "{}", requests[0]);
    assert_eq!(requests.iter().map(|q| n(&q["tokens"]["input"])).sum::<i64>(), n(&total["input"]));
    assert_eq!(requests.iter().map(|q| n(&q["tokens"]["output"])).sum::<i64>(), n(&total["output"]));

    // What a call carries now is the size the backend last gave, and its parts make it up exactly.
    let context = &report["context"];
    let whole: i64 = narsil.text("SELECT context_tokens FROM sessions WHERE parent_id IS NULL", &[]).parse().unwrap();
    assert_eq!(n(&context["tokens"]), whole);
    assert_eq!(context["parts"].as_array().unwrap().iter().map(|p| n(&p["tokens"])).sum::<i64>(), whole);
    assert_eq!(context["tool_results"][0]["tool"], "bash");

    // And as a person reads it.
    let said = narsil.ok(&dir, &["usage"]);
    assert!(said.contains("this session") && said.contains("1 subagent") && said.contains("by request"), "{said}");
    assert!(said.contains("what a call carries now"), "{said}");
}

#[test]
fn a_compaction_is_counted_as_a_call_like_any_other() {
    let narsil = Narsil::with(|_, config| config.set("context", "compact_at_tokens", "2600"));
    let dir = narsil.dir("x");
    let steps: Vec<String> = (0..8).map(|i| format!("!bash seq 1 {}", 300 + i)).chain(["!say Done.".to_string()]).collect();
    let session = narsil.ask(&dir, &script(&steps.iter().map(String::as_str).collect::<Vec<_>>()));
    let compactions = sum(&narsil, "COUNT(*)", "purpose = 'compaction'");
    assert!(compactions >= 1, "nothing was compacted:\n{}", session.said);

    // The summary's call is a call like any other: in the total, and in the request it was made for.
    let report = usage(&narsil);
    assert_eq!(n(&report["total"]["calls"]), sum(&narsil, "COUNT(*)", "1"));
    assert_eq!(n(&report["total"]["calls"]), 9 + compactions);
    assert_eq!(n(&report["total"]["input"]), sum(&narsil, "SUM(input + cache_read + cache_write)", "1"));
    let kept: i64 = narsil.text("SELECT SUM(input_tokens) FROM sessions", &[]).parse().unwrap();
    assert_eq!(n(&report["total"]["input"]), kept);
    assert_eq!(report["requests"].as_array().unwrap().len(), 1);
}

/// A stand-in for an OpenAI-style backend. It answers every chat request with
/// the same word, and ends the stream with the chunks given: a backend's own
/// way of saying what was used.
fn backend(last: &[&str]) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let last: Vec<String> = last.iter().map(|s| s.to_string()).collect();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut request = Vec::new();
            let mut buf = [0u8; 16384];
            // The head, then as much body as it says there is.
            let (head_end, length) = loop {
                let Ok(read) = stream.read(&mut buf) else { break (0, 0) };
                if read == 0 {
                    break (0, 0);
                }
                request.extend_from_slice(&buf[..read]);
                if let Some(at) = request.windows(4).position(|w| w == b"\r\n\r\n") {
                    let head = String::from_utf8_lossy(&request[..at]).to_lowercase();
                    let length = head.lines().find_map(|l| l.strip_prefix("content-length:")).and_then(|v| v.trim().parse::<usize>().ok()).unwrap_or(0);
                    break (at + 4, length);
                }
            };
            while head_end > 0 && request.len() < head_end + length {
                match stream.read(&mut buf) {
                    Ok(read) if read > 0 => request.extend_from_slice(&buf[..read]),
                    _ => break,
                }
            }
            let asked = String::from_utf8_lossy(&request[..head_end.min(request.len())]).lines().next().unwrap_or("").to_string();
            if !(asked.starts_with("POST") && asked.contains("/chat/completions")) {
                let _ = stream.write_all(b"HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\nconnection: close\r\n\r\n");
                continue;
            }
            let mut body = String::from("data: {\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"Four.\"}}]}\n\n");
            for chunk in &last {
                body.push_str(&format!("data: {chunk}\n\n"));
            }
            body.push_str("data: [DONE]\n\n");
            let _ = write!(stream, "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}", body.len());
        }
    });
    port
}

/// What narsil reports for one request to a backend that ends its stream with
/// these chunks: (input neither cached nor written, read from cache, written
/// to cache, output, whether it is an estimate), and the receipt.
fn reported(last: &[&str]) -> ((i64, i64, i64, i64, bool), String) {
    let url = format!("http://127.0.0.1:{}/v1", backend(last));
    let narsil = Narsil::with_backend(&["--provider", "custom", "--base-url", &url, "--model", "stand-in", "--no-test"], |_, _| {});
    let session = narsil.ask(&narsil.dir("x"), "What is two and two?");
    assert!(session.said.contains("Four."), "{}", session.said);
    let report = usage(&narsil);
    let t = &report["total"];
    assert_eq!(n(&t["calls"]), 1, "{report}");
    let receipt = session.said.lines().find(|l| l.contains("1 call · ")).unwrap_or_else(|| panic!("no receipt in:\n{}", session.said)).to_string();
    ((n(&t["input_new"]), n(&t["cache_read"]), n(&t["cache_write"]), n(&t["output"]), t["estimated"] == true), receipt)
}

const DONE: &str = r#"{"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#;

#[test]
fn each_backends_way_of_reporting_usage_arrives_as_the_same_numbers() {
    // OpenAI: a chunk of its own; cached tokens are part of prompt_tokens.
    let (used, receipt) = reported(&[
        DONE,
        r#"{"choices":[],"usage":{"prompt_tokens":6006,"completion_tokens":300,"total_tokens":6306,"prompt_tokens_details":{"cached_tokens":5760},"completion_tokens_details":{"reasoning_tokens":128}}}"#,
    ]);
    assert_eq!(used, (246, 5760, 0, 300, false));
    assert!(receipt.starts_with("(1 call · 6.0k in (96% cached) · 300 out · "), "{receipt}");

    // DeepSeek: on the last chunk, with a choice in it, and its own names for cache hits and misses.
    let (used, _) = reported(&[
        r#"{"choices":[{"index":0,"delta":{"content":""},"finish_reason":"stop"}],"usage":{"prompt_tokens":6100,"completion_tokens":11,"total_tokens":6111,"prompt_tokens_details":{"cached_tokens":5968},"prompt_cache_hit_tokens":5968,"prompt_cache_miss_tokens":132}}"#,
    ]);
    assert_eq!(used, (132, 5968, 0, 11, false));

    // xAI: reasoning is counted in the total and not in completion_tokens.
    let (used, receipt) = reported(&[
        DONE,
        r#"{"choices":[],"usage":{"prompt_tokens":6032,"completion_tokens":9,"total_tokens":6135,"prompt_tokens_details":{"cached_tokens":6},"completion_tokens_details":{"reasoning_tokens":94}}}"#,
    ]);
    assert_eq!(used, (6026, 6, 0, 103, false));
    assert!(receipt.contains("· 103 out ·"), "{receipt}");

    // OpenRouter: what was written to the cache is named apart from what was read.
    let (used, _) = reported(&[
        DONE,
        r#"{"choices":[{"index":0,"delta":{"content":""},"finish_reason":"stop"}],"usage":{"prompt_tokens":6194,"completion_tokens":2,"total_tokens":6196,"prompt_tokens_details":{"cached_tokens":0,"cache_write_tokens":6100}}}"#,
    ]);
    assert_eq!(used, (94, 0, 6100, 2, false));

    // An llama.cpp server that says what it took from its cache only in its timings.
    let (used, _) = reported(&[
        DONE,
        r#"{"choices":[],"usage":{"completion_tokens":24,"prompt_tokens":5120,"total_tokens":5144},"timings":{"cache_n":5000,"prompt_n":120,"predicted_n":24}}"#,
    ]);
    assert_eq!(used, (120, 5000, 0, 24, false));

    // A server that counts only what it newly evaluated, as Ollama has at times: 150 tokens, for a request
    // of thousands. What it left out is put back as read from its cache, and the count is marked.
    let ((new, cached, _, output, estimated), receipt) = reported(&[DONE, r#"{"choices":[],"usage":{"prompt_tokens":150,"completion_tokens":21,"total_tokens":171}}"#]);
    assert_eq!((new, output, estimated), (150, 21, true));
    assert!(cached > 3000, "{cached}");
    assert!(receipt.starts_with("(1 call · ~") && receipt.contains("% cached) · ~21 out"), "{receipt}");

    // A backend that says nothing: narsil's own estimate, and every figure is marked as one.
    let ((_, _, _, _, estimated), receipt) = reported(&[DONE]);
    assert!(estimated);
    assert!(receipt.starts_with("(1 call · ~") && receipt.contains(" in · ~"), "{receipt}");
}

#[test]
fn a_model_changed_in_the_middle_of_a_turn_takes_over_at_the_next_step() {
    let narsil = Narsil::new();
    let dir = narsil.dir("x");
    // A turn of four requests to the model, with a pause before the third, and a subagent
    // that is at work across the change.
    let turn = script(&[
        "!bash echo one",
        r#"!tool spawn_agents {"agents":[{"name":"helper","task":"!bash echo a ~~ !after 3 !bash echo b ~~ !say The helper is done."}]}"#,
        "!bash echo three",
        "!say Done.",
    ]);
    narsil.start();
    let running = narsil.begin(&dir, &["run", "--auto", &turn]);
    // While the subagent waits on its model, the session is handed to another.
    narsil.wait("the subagent to be at work", || sum(&narsil, "COUNT(*)", "session_id != root_id") >= 1);
    let root = narsil.text("SELECT id FROM sessions WHERE parent_id IS NULL", &[]);
    let said = narsil.ok(&dir, &["sessions", "model", &root, "mock/second"]);
    assert!(said.contains("takes over at its next step"), "{said}");
    // Not yet: the request in flight and what it asked for are finished first.
    assert_eq!(narsil.text("SELECT model FROM sessions WHERE id = ?1", &[&root]), "mock/mock");
    let out = running.wait_with_output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));

    // Each request, in order, with the model it went to: the first two of the session's on the
    // old one, the rest on the new one; and the subagent's own, the same way.
    let calls = |whose: &str| narsil.column(&format!("SELECT model FROM model_usage WHERE {whose} ORDER BY id"), &[]);
    assert_eq!(calls("session_id = root_id"), vec!["mock", "mock", "second", "second"]);
    // The subagent was in the middle of its second request when the change came: that one finishes
    // where it was sent, and its next goes to the new model, while the session still waits on it.
    // (Its second request may or may not have been sent when the change came; whichever it was,
    // it starts on the old model, ends on the new one, and does not go back.)
    let helper = calls("session_id != root_id");
    let changed = helper.iter().position(|m| m == "second").expect("the subagent never changed");
    assert!(helper.len() == 3 && changed >= 1 && helper[..changed].iter().all(|m| m == "mock") && helper[changed..].iter().all(|m| m == "second"), "{helper:?}");
    assert_eq!(narsil.column("SELECT model FROM sessions ORDER BY created_at, rowid", &[]), vec!["mock/second", "mock/second"]);

    // The transcript says where the change came, after the results the old model had asked for.
    let notes = narsil.column(
        "SELECT substr(blocks, 1, 400) FROM messages WHERE session_id = ?1 AND kind = 'note' AND blocks LIKE '%model for this conversation changed%'",
        &[&root],
    );
    assert_eq!(notes.len(), 1, "{notes:?}");
    assert!(notes[0].contains("from mock/mock to mock/second"), "{notes:?}");
    let order = narsil.column("SELECT role || ':' || kind FROM messages WHERE session_id = ?1 ORDER BY id", &[&root]);
    let at = order.iter().position(|m| m == "user:note").unwrap();
    assert_eq!(order[at - 1], "user:message", "the note is not after the tool results: {order:?}");

    // And `narsil usage` has both models.
    let report = usage(&narsil);
    let models: Vec<&str> = report["by_model"].as_array().unwrap().iter().map(|m| m["model"].as_str().unwrap()).collect();
    assert_eq!(models, vec!["mock/mock", "mock/second"]);

    // Between turns, a change is made at once.
    let said = narsil.ok(&dir, &["sessions", "model", &root, "mock/third"]);
    assert!(said.contains("now uses mock/third"), "{said}");
    assert_eq!(narsil.text("SELECT model FROM sessions WHERE id = ?1", &[&root]), "mock/third");
}
