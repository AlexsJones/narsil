//! Credentials are handled with care, checked through a running daemon. The
//! agent can read and use them: that is not what is guarded. What is, is that
//! it is told how to handle one the moment it has one in hand, and that they
//! do not spread into what narsil keeps or shows.

mod common;

use common::{Narsil, script};

fn token() -> String {
    // Shaped like a GitHub token; not one.
    format!("ghp_{}", "N4rs1lT3st".repeat(4))
}

#[test]
fn a_credential_can_be_read_and_used_and_the_agent_is_told_how_to_handle_it() {
    let narsil = Narsil::new();
    let home = narsil.user_home();
    let secret = token();
    std::fs::write(home.join(".git-credentials"), format!("https://someone:{secret}@github.com\n")).unwrap();
    let dir = narsil.dir("project");
    std::fs::write(dir.join("deploy.sh"), format!("#!/bin/sh\n[ \"$1\" = \"{secret}\" ] && echo deployed || {{ echo denied; exit 1; }}\n")).unwrap();

    let steps = script(&[
        "!bash cat ~/.git-credentials",
        &format!("!bash sh deploy.sh {secret}"),
        "!bash cat ~/.git-credentials",
        "!bash ls ~/.ssh 2>/dev/null; echo listed",
        "!say Deployed.",
    ]);
    let session = narsil.ask(&dir, &steps);
    let calls = session.calls_to("bash");

    // Nothing is refused or hidden from the agent: it reads the file, gets the token, and uses it.
    assert!(!calls[0].failed && calls[0].result.contains(&secret), "{:?}", calls[0]);
    assert!(!calls[1].failed && calls[1].result.starts_with("deployed"), "{:?}", calls[1]);

    // With the token in hand, it is told what care to take: once, not every time.
    let told: Vec<bool> = calls.iter().map(|c| c.result.contains("[narsil] This output holds a GitHub token")).collect();
    assert_eq!(told, vec![true, false, false, false], "{calls:#?}");
    let said = &calls[0].result;
    for part in ["only with the service it belongs to", "Do not repeat it in your replies", "do not put it in memory"] {
        assert!(said.contains(part), "the reminder should say {part:?}: {said}");
    }
}

#[test]
fn a_credential_does_not_spread_into_what_narsil_keeps_or_shows() {
    let narsil = Narsil::new();
    let dir = narsil.dir("project");
    let secret = token();
    std::fs::write(dir.join(".env"), format!("API_TOKEN={secret}\n")).unwrap();

    let steps = script(&[
        "!bash cat .env",
        &format!(r#"!tool memory_save {{"title":"How to deploy","body":"Run deploy.sh with the token {secret} from .env.","kind":"lesson","scope":"project"}}"#),
        r#"!tool work_plan {"directory":".","items":[{"title":"Deploy the service"}]}"#,
        r#"!tool work_next {"claim":"Deploy the service"}"#,
        &format!(r#"!tool work_done {{"item":"Deploy the service","result":"Deployed with {secret}.","evidence":"deploy.sh printed deployed","learned":[{{"title":"Deploys need the API token","body":"The token is {secret}; pass it as the first argument."}}]}}"#),
        "!say Deployed.",
    ]);
    let session = narsil.ask(&dir, &steps);
    assert!(session.calls().iter().all(|c| !c.failed), "{:#?}", session.calls());
    // The agent itself was given the real thing.
    assert!(session.calls_to("bash")[0].result.contains(&secret));

    // Memory: the entry is kept, the credential in it is not, and the agent is told why.
    let saved = &session.calls_to("memory_save")[0].result;
    assert!(saved.contains("A credential in it was masked") && saved.contains("Say where the credential lives"), "{saved}");
    let notes = narsil.notes_in(&narsil.memory());
    assert_eq!(notes.len(), 2, "{notes:?}");
    for note in &notes {
        let text = std::fs::read_to_string(narsil.memory().join(note)).unwrap();
        assert!(!text.contains(&secret) && text.contains("[masked by narsil: a GitHub token]"), "{note}:\n{text}");
    }
    assert!(narsil.column("SELECT body FROM memories", &[]).iter().all(|b| !b.contains(&secret)));

    // The work note, which whoever picks the work up will read.
    let (_, work) = narsil.note(&narsil.work(), "Deploy the service").expect("the item's note");
    assert!(!work.contains(&secret) && work.contains("Deployed with [masked by narsil: a GitHub token]."), "{work}");
    assert!(session.calls_to("work_done")[0].result.contains("A credential in it was masked"));

    // And what is shown to a person: the trace.
    for args in [vec!["trace", session.id.as_str()], vec!["trace", session.id.as_str(), "--timeline"]] {
        let shown = narsil.ok(&dir, &args);
        assert!(!shown.contains(&secret), "the trace shows the credential:\n{shown}");
    }
    assert!(narsil.ok(&dir, &["trace", &session.id]).contains("[masked by narsil: a GitHub token]"));
    assert!(!narsil.ok(&dir, &["memory", "list", "--all"]).contains(&secret));

    // A later session is told how to deploy, without being handed the token by memory.
    let later = narsil.ask(&dir, "How do I deploy the service?");
    assert!(!later.everything().contains(&secret), "memory handed the credential on");
}

#[test]
fn masking_can_be_switched_off() {
    let narsil = Narsil::with(|_, config| config.set("permissions", "mask_secrets", "false"));
    let secret = token();
    let session = narsil.ask(
        &narsil.dir("project"),
        &format!(r#"!tool memory_save {{"title":"The token","body":"It is {secret}.","kind":"note","scope":"global"}} ;; !say Saved."#),
    );
    assert!(!session.calls()[0].result.contains("masked"));
    let (_, note) = narsil.note(&narsil.memory(), "The token").expect("the note");
    assert!(note.contains(&secret), "{note}");
}
