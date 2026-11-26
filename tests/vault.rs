//! The Obsidian vault, checked through a running daemon: what the user has
//! closed stays closed to every tool, what they have written is where narsil
//! looks first, and narsil's own notes live inside the vault.

mod common;

use common::{Narsil, script};

const NOTES: [(&str, &str); 6] = [
    ("Work/Career history.md", "I joined Northwind in 2019 as a platform engineer. PENGUIN-1.\n"),
    ("Work/Projects.md", "The billing migration. The search rewrite.\n"),
    ("House/Boiler.md", "Serviced each October. The engineer is on 01234.\n"),
    ("Journal/Diary.md", "Worried about my career. ZEBRA-9-SECRET.\n"),
    ("Journal/2026/November.md", "More private thoughts. ZEBRA-9-SECRET.\n"),
    ("Recipes/Bread.md", "Flour, water, salt, yeast.\n"),
];

#[test]
fn what_is_closed_in_the_vault_stays_closed_to_every_tool() {
    let narsil = Narsil::with_vault(&NOTES, &["Journal"]);
    let vault = narsil.vault();
    let v = vault.to_string_lossy();
    let above = vault.parent().unwrap().to_string_lossy().into_owned();
    let steps: Vec<String> = [
        r#"!tool vault_search {"query":"zebra"}"#,
        r#"!tool vault_search {"query":"career"}"#,
        r#"!tool vault_read {"note":"Journal/Diary"}"#,
        r#"!tool vault_read {"note":"Diary"}"#,
        r#"!tool read_file {"path":"VAULT/Journal/Diary.md"}"#,
        r#"!tool read_file {"path":"VAULT/Work/../Journal/Diary.md"}"#,
        r#"!tool grep {"pattern":"ZEBRA","path":"VAULT"}"#,
        r#"!tool grep {"pattern":"ZEBRA","path":"ABOVE"}"#,
        r#"!tool grep {"pattern":"ZEBRA","path":"VAULT/Journal/Diary.md"}"#,
        r#"!tool glob {"pattern":"**/*.md","path":"VAULT"}"#,
        r#"!tool list_dir {"path":"VAULT"}"#,
        r#"!tool list_dir {"path":"VAULT/Journal"}"#,
        "!bash cat VAULT/Journal/Diary.md",
        "!bash ls",
        r#"!tool write_file {"path":"VAULT/Journal/new.md","content":"x"}"#,
        r#"!tool edit_file {"path":"VAULT/Journal/Diary.md","old_string":"Worried","new_string":"Happy"}"#,
        r#"!tool vault_write {"note":"Journal/new","content":"x"}"#,
        r#"!tool vault_read {"note":"Work/Career history"}"#,
        r#"!tool grep {"pattern":"PENGUIN","path":"VAULT"}"#,
        "!say Finished looking.",
    ]
    .iter()
    .map(|s| s.replace("VAULT", &v).replace("ABOVE", &above))
    .collect();
    let steps: Vec<&str> = steps.iter().map(String::as_str).collect();
    let session = narsil.ask(&narsil.dir("elsewhere"), &script(&steps));
    let calls = session.calls();
    assert_eq!(calls.len(), 19, "every step ran: {calls:#?}");

    // Nothing that was closed reached the model, by any route.
    for (i, call) in calls.iter().enumerate() {
        assert!(!call.result.contains("9-SECRET"), "call {i} ({} {}) leaked a closed note:\n{}", call.name, call.input, call.result);
    }
    assert!(!session.prompt().contains("Journal"), "the prompt does not name what is closed:\n{}", session.prompt());
    assert!(session.prompt().contains("closed parts of the vault"));

    // Asked for outright, the answer is that it is closed.
    for i in [2, 4, 5, 8, 11, 14, 15, 16] {
        assert!(calls[i].failed && calls[i].result.contains("closed to narsil"), "call {i} ({} {}): {}", calls[i].name, calls[i].input, calls[i].result);
    }
    // Looked for by name or by content, it is simply not there.
    assert!(calls[0].result.contains("No notes match"), "{}", calls[0].result);
    assert!(calls[1].result.contains("Work/Career history.md") && !calls[1].result.contains("Journal"), "{}", calls[1].result);
    assert!(calls[3].failed && !calls[3].result.contains("Journal"), "{}", calls[3].result);
    for i in [6, 7, 9, 10] {
        assert!(!calls[i].failed && !calls[i].result.contains("Journal") && !calls[i].result.contains("Diary"), "call {i} ({}): {}", calls[i].name, calls[i].result);
    }
    assert!(calls[9].result.contains("Work/Career history.md") && calls[10].result.contains("Work/"), "what is open is still listed");
    // A shell command cannot be held to what is closed, so on the vault it does not run; elsewhere it does.
    assert!(calls[12].failed && calls[12].result.contains("a shell command cannot be held to that"), "{}", calls[12].result);
    assert!(!calls[13].failed, "{}", calls[13].result);
    // Nothing was written into the closed part either.
    assert!(!vault.join("Journal/new.md").exists());
    assert!(std::fs::read_to_string(vault.join("Journal/Diary.md")).unwrap().starts_with("Worried"));
    // And the rest of the vault works as before.
    assert!(!calls[17].failed && calls[17].result.contains("PENGUIN-1"), "{}", calls[17].result);
    assert!(!calls[18].failed && calls[18].result.contains("PENGUIN-1"), "{}", calls[18].result);
}

#[test]
fn with_nothing_closed_the_vault_is_an_ordinary_folder_to_the_shell() {
    let narsil = Narsil::with_vault(&NOTES, &[]);
    let vault = narsil.vault();
    let session = narsil.ask(&narsil.dir("elsewhere"), &format!("!bash cat '{}/Journal/Diary.md' ;; !say Read.", vault.display()));
    let cat = &session.calls()[0];
    assert!(!cat.failed && cat.result.contains("ZEBRA-9-SECRET"), "{cat:?}");
    assert!(!session.prompt().contains("closed parts of the vault"));
}

#[test]
fn what_the_user_has_written_is_named_before_the_model_starts() {
    let narsil = Narsil::with_vault(&NOTES, &["Journal"]);
    let dir = narsil.dir("chat");

    let asked = narsil.ask(&dir, "Summarise my career history so far");
    let told = asked.told_before_starting();
    assert_eq!(told.len(), 1, "{told:?}");
    assert!(told[0].starts_with("[narsil] Obsidian: notes of the user's that may bear on this: Work/Career history.md."), "{}", told[0]);
    assert!(told[0].contains("first place to look") && told[0].contains("vault_read"), "{}", told[0]);
    // Only the note's name is given: the model reads it if it needs it, where the user can see it do so.
    assert!(!told[0].contains("Northwind") && !told[0].contains("Journal"), "{}", told[0]);
    assert!(asked.prompt().contains("The vault is the first place to look for anything about the user"));

    // A request the vault has nothing on gets no such note.
    assert!(narsil.ask(&dir, "Refactor the parser module").notes().is_empty());
}

#[test]
fn looking_in_the_vault_first_can_be_switched_off() {
    let narsil = Narsil::with(|base, config| {
        let vault = base.join("vault");
        std::fs::create_dir_all(vault.join(".obsidian")).unwrap();
        std::fs::create_dir_all(vault.join("Work")).unwrap();
        std::fs::write(vault.join("Work/Career history.md"), "I joined Northwind in 2019.\n").unwrap();
        std::fs::write(vault.join("Other.md"), "Something else.\n").unwrap();
        config.set("obsidian", "vault", &format!("{:?}", vault.to_string_lossy()));
        config.set("obsidian", "lookup", "false");
    });
    assert!(narsil.ask(&narsil.dir("chat"), "Summarise my career history so far").notes().is_empty());
}

#[test]
fn narsils_own_notes_live_in_the_vault_beside_the_users() {
    let narsil = Narsil::with_vault(&NOTES, &["Journal"]);
    let vault = narsil.vault();
    let steps = script(&[
        r#"!tool memory_save {"title":"Prefers short answers","body":"Keep answers short. See [[Career history]] for background.","kind":"user","scope":"global"}"#,
        r#"!tool work_plan {"directory":".","items":[{"title":"Tidy the notes"}]}"#,
        "!say Saved.",
    ]);
    let session = narsil.ask(&narsil.dir("chat"), &steps);
    assert!(session.calls().iter().all(|c| !c.failed), "{:#?}", session.calls());

    let own = narsil.notes_in(&vault.join("narsil"));
    assert!(own.contains(&"Memory/Global/Prefers short answers.md".to_string()), "{own:?}");
    assert!(own.iter().any(|p| p.starts_with("Work/") && p.ends_with("Tidy the notes.md")), "{own:?}");
    // Nothing of narsil's is written outside its own folder, and nothing is kept outside the vault.
    let users: Vec<String> = narsil.notes_in(&vault).into_iter().filter(|p| !p.starts_with("narsil/")).collect();
    assert_eq!(users.len(), NOTES.len(), "{users:?}");
    assert!(narsil.notes_in(&narsil.memory()).is_empty() && narsil.notes_in(&narsil.work()).is_empty());
}
