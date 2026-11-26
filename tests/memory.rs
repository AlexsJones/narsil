//! What narsil remembers, checked through a running daemon: notes are written
//! as work goes, and come back, unasked, when they bear on what is asked next.
//!
//! Each test starts its own narsil with the scripted model, so what is being
//! checked is what narsil does with what a model tells it, not whether a
//! model thinks to tell it (see `tests/live.rs` for that).

mod common;

use common::{Narsil, script};

/// Session one hits a dead end while working and says so as it closes the
/// item. That is all the telling narsil gets.
fn find_nothing_about_eldermoor(narsil: &Narsil, dir: &std::path::Path) {
    let steps = script(&[
        r#"!tool work_plan {"directory":".","root":"Eldermoor history site","items":[{"title":"Research Eldermoor"},{"title":"Write the pages","blocked_by":["Research Eldermoor"]}]}"#,
        r#"!tool work_next {"claim":"Research Eldermoor"}"#,
        r#"!tool work_done {"item":"Research Eldermoor","result":"Nothing to build from.","evidence":"Three searches, no results.","learned":[{"title":"No public data about Eldermoor","body":"Searched Wikipedia, OpenStreetMap and the web for Eldermoor: nothing. It appears to be fictional.","dead_end":true,"applies":"anywhere"}]}"#,
        "!say I could not find anything about Eldermoor.",
    ]);
    narsil.ask(dir, &format!("Build a website about the history of Eldermoor.\n{steps}"));
}

fn recalled(told: &[String]) -> Vec<&String> {
    told.iter().filter(|n| n.starts_with("[narsil] Recalled from memory:")).collect()
}

#[test]
fn a_dead_end_met_while_working_is_put_in_front_of_the_next_session_that_asks() {
    let narsil = Narsil::new();
    find_nothing_about_eldermoor(&narsil, &narsil.dir("first"));

    // It became a note a person can open, saying what it is and where it came from.
    let (_, note) = narsil.note(&narsil.memory(), "No public data about Eldermoor").expect("a note on disk");
    for part in ["kind: dead_end", "scope: global", "It appears to be fictional.", "while working on [[Research Eldermoor]]"] {
        assert!(note.contains(part), "the note should have {part:?}:\n{note}");
    }

    // A new session, in another directory, asking much the same, is told before the model is called.
    let second = narsil.ask(&narsil.dir("second"), "Please make me a site about the history of Eldermoor");
    let told = second.told_before_starting();
    assert_eq!(told.len(), 1, "{told:?}");
    assert!(told[0].starts_with("[narsil] Recalled from memory: No public data about Eldermoor."), "{}", told[0]);
    assert!(told[0].contains("TRIED BEFORE AND FAILED") && told[0].contains("It appears to be fictional."), "{}", told[0]);
    assert!(told[0].contains("do not retry what is marked as tried and failed"), "{}", told[0]);

    // Something unrelated brings nothing back.
    let third = narsil.ask(&narsil.dir("second"), "Tidy the kitchen cupboards for me");
    assert!(third.notes().is_empty(), "{:?}", third.notes());
}

#[test]
fn a_lesson_about_one_project_stays_with_that_project() {
    let narsil = Narsil::new();
    let shop = narsil.dir("shop");
    let steps = script(&[
        r#"!tool work_plan {"directory":".","items":[{"title":"Run the checkout tests"}]}"#,
        r#"!tool work_next {"claim":"Run the checkout tests"}"#,
        r#"!tool work_done {"item":"Run the checkout tests","result":"They pass.","evidence":"make checkout-test: 12 passed","learned":[{"title":"Checkout tests need the payments stub","body":"The checkout tests hang unless the payments stub is started first with make stub.","applies":"this project"}]}"#,
        "!say Passing.",
    ]);
    let first = narsil.ask(&shop, &format!("Get the checkout tests passing.\n{steps}"));
    let closed = &first.calls_to("work_done")[0];
    assert!(!closed.failed && closed.result.contains("Saved to memory for next time: Checkout tests need the payments stub"), "{}", closed.result);

    let (path, note) = narsil.note(&narsil.memory(), "Checkout tests need the payments stub").expect("a note on disk");
    assert!(note.contains("kind: lesson") && note.contains(&format!("project:{}", shop.display())), "{note}");
    assert!(!path.to_string_lossy().contains("/Global/"), "a project's lesson is filed under the project: {}", path.display());

    // Asked again in the same project, it is there before the model starts, marked as learned.
    let again = narsil.ask(&shop, "Why do the checkout tests hang?");
    let told = again.told_before_starting();
    assert_eq!(recalled(&told).len(), 1, "{told:?}");
    assert!(told[0].contains("Checkout tests need the payments stub [project · learned "), "{}", told[0]);

    // The same question in another project is not answered with this project's quirks.
    let elsewhere = narsil.ask(&narsil.dir("blog"), "Why do the checkout tests hang?");
    assert!(elsewhere.notes().is_empty(), "{:?}", elsewhere.notes());
}

#[test]
fn the_notes_on_disk_are_the_memory() {
    let narsil = Narsil::new();
    let dir = narsil.dir("records");
    let saved = narsil.ask(
        &dir,
        r#"!tool memory_save {"title":"Eldermoor has no parish records","body":"Looked for Eldermoor parish records in three archives: none exist.","kind":"dead_end","scope":"global"} ;; !say Saved."#,
    );
    assert!(!saved.calls()[0].failed, "{:?}", saved.calls());
    let (path, note) = narsil.note(&narsil.memory(), "Eldermoor has no parish records").expect("a note on disk");

    // Edited by hand: the next session is told what the note says now.
    std::thread::sleep(std::time::Duration::from_millis(20));
    std::fs::write(&path, note.replace("none exist.", "none exist online. The county archive in Durham holds them on paper.")).unwrap();
    let told = narsil.ask(&dir, "Find the Eldermoor parish records").told_before_starting();
    assert!(told.len() == 1 && told[0].contains("county archive in Durham"), "{told:?}");

    // Written by hand from nothing: it is remembered like any other.
    std::fs::write(
        path.with_file_name("Vellmoor is a made-up place.md"),
        "---\nscope: global\nkind: dead_end\n---\n\nVellmoor was invented for a novel. There is nothing to look up.\n",
    )
    .unwrap();
    let told = narsil.ask(&dir, "What is the population of Vellmoor?").told_before_starting();
    assert!(told.len() == 1 && told[0].contains("Vellmoor is a made-up place") && told[0].contains("TRIED BEFORE AND FAILED"), "{told:?}");

    // Deleted by hand: it is gone, from what is recalled and from what is listed.
    std::fs::remove_file(&path).unwrap();
    assert!(narsil.ask(&dir, "Find the Eldermoor parish records").notes().is_empty());
    let listed = narsil.ok(&dir, &["memory", "list", "--all"]);
    assert!(listed.contains("Vellmoor is a made-up place") && !listed.contains("parish records"), "{listed}");
}

#[test]
fn forgetting_an_entry_removes_its_note_and_stops_it_coming_back() {
    let narsil = Narsil::new();
    let dir = narsil.dir("forget");
    narsil.ask(
        &dir,
        r#"!tool memory_save {"title":"The staging database is read-only","body":"Writes to the staging database fail: it is a read-only replica.","kind":"dead_end","scope":"global"} ;; !say Saved."#,
    );
    assert_eq!(narsil.ask(&dir, "Load the fixtures into the staging database").told_before_starting().len(), 1);

    let forgot = narsil.ask(&dir, r#"!tool memory_forget {"title":"The staging database is read-only"} ;; !say Forgotten."#);
    assert!(!forgot.calls()[0].failed, "{:?}", forgot.calls());
    assert!(narsil.note(&narsil.memory(), "The staging database is read-only").is_none(), "{:?}", narsil.notes_in(&narsil.memory()));
    assert!(narsil.ask(&dir, "Load the fixtures into the staging database").notes().is_empty());
}

#[test]
fn memory_is_rebuilt_from_the_notes_when_the_database_is_lost() {
    let narsil = Narsil::new();
    find_nothing_about_eldermoor(&narsil, &narsil.dir("first"));
    assert_eq!(narsil.column("SELECT title FROM memories", &[]), vec!["No public data about Eldermoor"]);

    // Lose everything but the notes.
    narsil.stop();
    for file in ["narsil.db", "narsil.db-wal", "narsil.db-shm"] {
        let _ = std::fs::remove_file(narsil.home().join("data").join(file));
    }

    let after = narsil.ask(&narsil.dir("second"), "Please make me a site about the history of Eldermoor");
    let told = after.told_before_starting();
    assert!(told.len() == 1 && told[0].contains("TRIED BEFORE AND FAILED"), "{told:?}");
    assert_eq!(narsil.column("SELECT title || ' / ' || kind FROM memories", &[]), vec!["No public data about Eldermoor / dead_end"]);
}

#[test]
fn two_failures_in_a_row_ask_whether_this_is_a_dead_end() {
    let narsil = Narsil::new();
    let steps = script(&["!bash false", "!bash false", "!bash false", "!bash true", "!bash false", "!bash false", "!say Stopping."]);
    let session = narsil.ask(&narsil.dir("failing"), &steps);
    let asked: Vec<bool> = session.calls_to("bash").iter().map(|c| c.result.contains("[narsil] Two attempts in a row have failed")).collect();
    // On the second failure, not again on the third, and afresh after something has worked.
    assert_eq!(asked, vec![false, true, false, false, false, true], "{:#?}", session.calls_to("bash"));
    let nudge = &session.calls_to("bash")[1].result;
    assert!(nudge.contains("memory_save") && nudge.contains("dead_end"), "{nudge}");
}

#[test]
fn a_goal_is_told_what_memory_holds_and_leaves_what_it_learned() {
    let narsil = Narsil::new();
    find_nothing_about_eldermoor(&narsil, &narsil.dir("first"));

    let steps = script(&[
        r#"!tool goal_complete {"summary":"Nothing to write: there are no sources.","lessons":[{"title":"Eldermoor write-ups need sources from the user","body":"With no public sources, a history of Eldermoor can only be written from material the user supplies. Ask for it first."}]}"#,
        "!say Done.",
    ]);
    let goal = narsil.goal(&narsil.dir("goal"), &format!("Write up the history of Eldermoor.\n{steps}"));
    assert_eq!(narsil.goal_reaches(&goal, &["completed", "blocked", "paused"]), "completed");

    // Before its first turn, the goal's session was told about the dead end.
    let told = narsil.goal_session(&goal).told_before_starting();
    assert!(told.iter().any(|n| n.contains("No public data about Eldermoor") && n.contains("TRIED BEFORE AND FAILED")), "{told:?}");

    // What it learned is a note, tied to the record of the goal's work.
    let (_, note) = narsil.note(&narsil.memory(), "Eldermoor write-ups need sources from the user").expect("a note for the lesson");
    assert!(note.contains("kind: lesson") && note.contains("while working on [["), "{note}");
    let record = narsil.column("SELECT title FROM work_items WHERE goal = ?1", &[&goal]);
    assert_eq!(record.len(), 1, "a goal finished without a plan still leaves a record: {record:?}");
    assert!(note.contains(&format!("[[{}]]", record[0])), "{note}");
}

#[test]
fn taking_an_item_brings_what_memory_holds_on_its_subject() {
    let narsil = Narsil::new();
    let dir = narsil.dir("archive");
    narsil.ask(
        &dir,
        r#"!tool memory_save {"title":"The Eldermoor archive needs a login","body":"Downloads from the Eldermoor archive fail with 403 without the user's login. Ask for credentials first.","kind":"lesson","scope":"global"} ;; !say Saved."#,
    );
    let steps = script(&[
        r#"!tool work_plan {"directory":".","items":[{"title":"Fetch the Eldermoor archive"},{"title":"Count the lines in the result"}]}"#,
        r#"!tool work_next {"claim":"Fetch the Eldermoor archive"}"#,
        r#"!tool work_next {"claim":"Count the lines in the result"}"#,
        "!say Taken.",
    ]);
    let session = narsil.ask(&dir, &steps);
    let taken = session.calls_to("work_next");
    assert!(taken[0].result.contains("From memory, on this subject") && taken[0].result.contains("The Eldermoor archive needs a login"), "{}", taken[0].result);
    assert!(taken[0].result.contains("Ask for credentials first."), "{}", taken[0].result);
    // An item memory has nothing on is handed over without that.
    assert!(!taken[1].result.contains("From memory"), "{}", taken[1].result);
}

#[test]
fn working_out_how_to_make_something_pass_brings_a_reminder_to_write_it_down() {
    let narsil = Narsil::new();
    let dir = narsil.dir("builder");
    std::fs::write(
        dir.join("build.sh"),
        "#!/bin/sh\nif [ \"$BUILD_MODE\" != \"frugal\" ]; then echo 'build failed (code 37)' >&2; exit 37; fi\necho 'build ok'\n",
    )
    .unwrap();
    let steps = script(&[
        "!bash sh build.sh",
        "!bash cat build.sh",
        "!bash BUILD_MODE=frugal sh build.sh",
        "!bash BUILD_MODE=frugal sh build.sh",
        "!bash test -f made.txt",
        "!bash touch made.txt",
        "!bash test -f made.txt",
        "!say Built.",
    ]);
    let session = narsil.ask(&dir, &steps);
    let calls = session.calls_to("bash");
    let reminded: Vec<bool> = calls.iter().map(|c| c.result.contains("[narsil] This worked where")).collect();
    // Only when another form of the command that failed now works: not for an unrelated command,
    // not twice, and not when the very same command passes because something was fixed in between.
    assert_eq!(reminded, vec![false, false, true, false, false, false, false], "{calls:#?}");
    let reminder = &calls[2].result;
    assert!(reminder.contains("`sh build.sh` failed") && reminder.contains("memory_save") && reminder.contains("kind lesson"), "{reminder}");
    // Models like to end a command with `; echo exit=$?`, which makes the line as a whole succeed.
    // The status it printed is what counts.
    let steps = script(&["!bash sh build.sh; echo exit=$?", "!bash BUILD_MODE=frugal sh build.sh; echo exit=$?", "!say Built."]);
    let calls = narsil.ask(&dir, &steps).calls_to("bash");
    assert!(!calls[0].failed && calls[0].result.contains("exit=37"), "{:?}", calls[0]);
    assert!(calls[1].result.contains("[narsil] This worked where `sh build.sh; echo exit=$?` failed"), "{}", calls[1].result);
}

#[test]
fn stopping_after_a_failure_brings_the_question_whether_it_is_a_dead_end() {
    let narsil = Narsil::new();
    let dir = narsil.dir("dataset");
    std::fs::write(dir.join("fetch.sh"), "#!/bin/sh\necho 'fetch: the mirror was shut down for good' >&2\nexit 1\n").unwrap();
    // One decisive failure, and the model goes straight to telling the user. Asked before
    // the turn ends, it writes the dead end down; it is not asked a second time.
    let steps = script(&[
        "!bash sh fetch.sh",
        "!say I could not download the dataset.",
        r#"!tool memory_save {"title":"The dataset mirror is gone","body":"fetch.sh fails: the mirror was shut down for good. There is no other source for the dataset.","kind":"dead_end","scope":"project"}"#,
        "!say Recorded.",
    ]);
    let first = narsil.ask(&dir, &format!("Download the dataset with fetch.sh.\n{steps}"));
    let asked: Vec<String> = first.notes().into_iter().filter(|n| n.starts_with("[narsil] Before you finish")).collect();
    assert_eq!(asked.len(), 1, "{:?}", first.notes());
    assert!(asked[0].contains("`sh fetch.sh` failed in this turn") && asked[0].contains("kind dead_end"), "{}", asked[0]);
    assert_eq!(narsil.column("SELECT kind || ': ' || title FROM memories", &[]), vec!["dead_end: The dataset mirror is gone"]);

    // Which is what spares the next session the same attempt.
    let told = narsil.ask(&dir, "Download the dataset with fetch.sh").told_before_starting();
    assert!(told.len() == 1 && told[0].contains("TRIED BEFORE AND FAILED"), "{told:?}");

    // A turn in which what failed was then made to work, or in which a check merely said no, ends without the question.
    std::fs::write(dir.join("ok.sh"), "#!/bin/sh\n[ \"$1\" = go ] || exit 2\necho fine\n").unwrap();
    let steps = script(&["!bash sh ok.sh", "!bash sh ok.sh go", "!bash test -f nothing-here", "!bash grep -q absent fetch.sh", "!say All good."]);
    let calm = narsil.ask(&dir, &steps);
    assert!(calm.notes().iter().all(|n| !n.contains("Before you finish")), "{:?}", calm.notes());
    // And what failed in one turn is not brought up at the end of a later one.
    let steps = script(&["!bash sh fetch.sh", "!say Failed.", "!say Nothing to add."]);
    let failed = narsil.ask(&dir, &steps);
    let later = narsil.ask_again(&failed, &dir, "And what is the time?");
    let questions = later.notes().iter().filter(|n| n.contains("Before you finish")).count();
    assert_eq!(questions, 1, "asked once, in the turn it happened: {:?}", later.notes());
}

#[test]
fn a_goal_that_stops_names_what_it_found_to_be_a_dead_end() {
    let narsil = Narsil::new();
    let steps = script(&[
        r#"!tool goal_blocked {"reason":"I need sources for Vellmoor from you.","dead_ends":[{"title":"No public sources for Vellmoor","body":"Searched the web, Wikipedia and OpenStreetMap for Vellmoor: nothing. It may be fictional."}]}"#,
        "!say Waiting for you.",
    ]);
    let goal = narsil.goal(&narsil.dir("town"), &format!("Write a history of Vellmoor.\n{steps}"));
    assert_eq!(narsil.goal_reaches(&goal, &["completed", "blocked", "paused"]), "blocked");
    let blocked = &narsil.goal_session(&goal).calls_to("goal_blocked")[0];
    assert!(blocked.result.contains("Saved to memory for next time: No public sources for Vellmoor"), "{}", blocked.result);
    let (_, note) = narsil.note(&narsil.memory(), "No public sources for Vellmoor").expect("a note for the dead end");
    assert!(note.contains("kind: dead_end") && note.contains("It may be fictional."), "{note}");

    let told = narsil.ask(&narsil.dir("elsewhere"), "Tell me about the history of Vellmoor").told_before_starting();
    assert!(told.len() == 1 && told[0].contains("TRIED BEFORE AND FAILED"), "{told:?}");
}
