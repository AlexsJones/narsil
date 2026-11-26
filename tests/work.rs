//! The memory of work in flight, checked through a running daemon: what is
//! planned, taken and finished is on record as it happens, survives the
//! daemon dying, and is in front of whoever picks the work up next.

mod common;

use common::{Narsil, script};

const DONE: &str = r#""evidence":"checked by hand","learned":[]"#;

#[test]
fn work_is_filed_in_the_directory_it_is_about() {
    let narsil = Narsil::new();
    let launch = narsil.dir("launch");
    let steps = script(&[
        r#"!tool work_plan {"directory":"../site","root":"Bakery site","items":[{"title":"Write the index page"}]}"#,
        "!bash pwd",
        r#"!tool work_next {"claim":"Write the index page"}"#,
        "!bash echo '<h1>Bakery</h1>' > index.html",
        &format!(r#"!tool work_done {{"item":"Write the index page","result":"index.html written.",{DONE}}}"#),
        "!say Done.",
    ]);
    let session = narsil.ask(&launch, &format!("Make a small site for the bakery in ../site.\n{steps}"));
    let site = narsil.dir("site");

    // The plan moved the session to where the work is, and said so.
    let planned = &session.calls_to("work_plan")[0];
    assert!(planned.result.starts_with(&format!("The working directory is now {}", site.display())), "{}", planned.result);
    assert_eq!(session.cwd(), site.to_string_lossy());
    assert_eq!(session.calls_to("bash")[0].result.lines().next(), Some(site.to_string_lossy().as_ref()), "commands run there too");
    assert!(site.join("index.html").is_file() && std::fs::read_dir(&launch).unwrap().next().is_none(), "nothing is left where narsil was started");

    // The work is on record under that directory, as notes and in the index.
    assert_eq!(narsil.column("SELECT DISTINCT project FROM work_items", &[]), vec![site.to_string_lossy()]);
    assert_eq!(narsil.notes_in(&narsil.work()), vec!["site/Bakery site.md", "site/Write the index page.md"]);

    // So a session started in that directory later finds it, and one started elsewhere is told it was done.
    assert!(narsil.ok(&site, &["work"]).contains("Write the index page"));
    let elsewhere = narsil.ask(&narsil.dir("other"), "What have you built before?");
    assert!(elsewhere.prompt().contains("# Done before, in other directories") && elsewhere.prompt().contains("Bakery site"), "{}", elsewhere.prompt());
}

#[test]
fn progress_is_on_record_as_it_happens_and_survives_the_daemon_being_killed() {
    let narsil = Narsil::new();
    let dir = narsil.dir("report");
    let steps = script(&[
        r#"!tool work_plan {"directory":".","items":[{"title":"Gather the figures"},{"title":"Write the summary","blocked_by":["Gather the figures"]}]}"#,
        r#"!tool work_next {"claim":"Gather the figures"}"#,
        "!bash sleep 3; echo gathered",
        &format!(r#"!tool work_done {{"item":"Gather the figures","result":"Figures are in figures.csv.",{DONE}}}"#),
        r#"!tool work_next {"claim":"Write the summary"}"#,
        &format!(r#"!tool work_done {{"item":"Write the summary","result":"Summary is in summary.md.",{DONE}}}"#),
        r#"!tool goal_complete {"summary":"Report written.","lessons":[]}"#,
        "!say Done.",
    ]);
    let goal = narsil.goal(&dir, &format!("Write the quarterly report.\n{steps}"));

    // Mid-command: the item is already marked as taken, on disk, where a person or a later session can see it.
    narsil.wait("the command to be running", || narsil.text("SELECT status FROM tool_calls WHERE name = 'bash'", &[]) == "running");
    let in_flight = |narsil: &Narsil| narsil.note(&narsil.work(), "Gather the figures").expect("the item's note").1;
    assert!(in_flight(&narsil).contains("status: in_progress"), "{}", in_flight(&narsil));
    assert_eq!(narsil.item("Write the summary"), "open");

    // The daemon dies with the command still running.
    narsil.kill();
    assert!(in_flight(&narsil).contains("status: in_progress"));

    // Started again, it picks the goal up from the record and finishes it.
    narsil.start();
    assert_eq!(narsil.goal_reaches(&goal, &["completed", "blocked", "paused"]), "completed");
    assert_eq!(narsil.column("SELECT title || ': ' || status FROM work_items WHERE parent IS NOT NULL OR goal = '' ORDER BY rowid", &[]).len(), 2);
    assert_eq!(narsil.item("Gather the figures"), "done");
    assert_eq!(narsil.item("Write the summary"), "done");
    let note = in_flight(&narsil);
    assert!(note.contains("status: done") && note.contains("Figures are in figures.csv.") && note.contains("Evidence: checked by hand"), "{note}");
    // The command that was cut off is on record as cut off, not as having succeeded.
    let cut = &narsil.goal_session(&goal).calls_to("bash")[0];
    assert!(cut.failed && !cut.result.contains("gathered"), "{cut:?}");
}

#[test]
fn a_new_session_in_the_directory_is_told_where_the_work_stands() {
    let narsil = Narsil::new();
    let dir = narsil.dir("api");
    let steps = script(&[
        r#"!tool work_plan {"directory":".","root":"Orders API","items":[{"title":"Draft the schema"},{"title":"Write the handlers","blocked_by":["Draft the schema"]},{"title":"Write the docs"}]}"#,
        r#"!tool work_next {"claim":"Draft the schema"}"#,
        &format!(r#"!tool work_done {{"item":"Draft the schema","result":"Schema is in schema.sql, three tables.",{DONE}}}"#),
        "!say Stopping for today.",
    ]);
    narsil.ask(&dir, &steps);

    let next = narsil.ask(&dir, "What is left to do?");
    let prompt = next.prompt();
    let at = prompt.find("# Work in this project").unwrap_or_else(|| panic!("no work section in:\n{prompt}"));
    let section = &prompt[at..];
    for part in ["Orders API: 1 of 3 done", "Ready now (2)", "- Write the handlers", "- Write the docs"] {
        assert!(section.contains(part), "the work section should have {part:?}:\n{}", &section[..section.len().min(1500)]);
    }

    // Any item can be read in full, finished ones included: that is how later work builds on earlier.
    let read = narsil.ask(&dir, r#"!tool work_next {"show":"Draft the schema"} ;; !say Read."#);
    let shown = &read.calls()[0].result;
    assert!(shown.contains("Schema is in schema.sql, three tables.") && shown.contains("checked by hand"), "{shown}");

    // A session somewhere else is not handed this project's unfinished work.
    let elsewhere = narsil.ask(&narsil.dir("unrelated"), "What is left to do?");
    assert!(!elsewhere.prompt().contains("Write the handlers"), "{}", elsewhere.prompt());
}

#[test]
fn nothing_is_called_finished_without_evidence_or_with_work_still_open() {
    let narsil = Narsil::new();
    let steps = script(&[
        r#"!tool work_plan {"directory":".","items":[{"title":"Rotate the API key"}]}"#,
        r#"!tool goal_complete {"summary":"All done.","lessons":[]}"#,
        r#"!tool work_next {"claim":"Rotate the API key"}"#,
        r#"!tool work_done {"item":"Rotate the API key","result":"Rotated.","learned":[]}"#,
        &format!(r#"!tool work_done {{"item":"Rotate the API key","result":"Rotated; old key revoked.",{DONE}}}"#),
        r#"!tool goal_complete {"summary":"Key rotated.","lessons":[]}"#,
        "!say Done.",
    ]);
    let goal = narsil.goal(&narsil.dir("keys"), &format!("Rotate the API key.\n{steps}"));
    assert_eq!(narsil.goal_reaches(&goal, &["completed", "blocked", "paused"]), "completed");
    let session = narsil.goal_session(&goal);

    let complete = session.calls_to("goal_complete");
    assert!(complete[0].failed && complete[0].result.contains("Rotate the API key"), "declared complete with an item open: {:?}", complete[0]);
    assert!(!complete[1].failed, "{:?}", complete[1]);
    let done = session.calls_to("work_done");
    assert!(done[0].failed && done[0].result.contains("evidence"), "closed without evidence: {:?}", done[0]);
    assert!(!done[1].failed, "{:?}", done[1]);
}

#[test]
fn a_subagent_that_stops_without_finishing_gives_its_item_back() {
    let narsil = Narsil::new();
    let steps = script(&[
        r#"!tool work_plan {"directory":".","items":[{"title":"Survey the API"},{"title":"Write the client"}]}"#,
        r#"!tool spawn_agents {"agents":[{"name":"quitter","work":"Survey the API","task":"!say I looked and stopped."},{"name":"finisher","work":"Write the client","task":"!tool work_done {\"item\":\"Write the client\",\"result\":\"Client is in client.py.\",\"evidence\":\"imports cleanly\",\"learned\":[]} ~~ !say Finished."}]}"#,
        "!say Both are back.",
    ]);
    narsil.ask(&narsil.dir("client"), &steps);

    let log = |title: &str| narsil.text("SELECT log FROM work_items WHERE title = ?1", &[title]);
    assert_eq!(narsil.item("Write the client"), "done");
    assert!(log("Write the client").contains("done by subagent finisher"), "{}", log("Write the client"));
    // Not left looking as if someone still has it: it is ready for whoever comes next, and its history says why.
    assert_eq!(narsil.item("Survey the API"), "open");
    assert_eq!(narsil.text("SELECT claimed_by || claimed_agent FROM work_items WHERE title = 'Survey the API'", &[]), "");
    assert!(log("Survey the API").contains("subagent quitter ended without closing it"), "{}", log("Survey the API"));
    assert!(narsil.ok(&narsil.dir("client"), &["work"]).contains("Survey the API"));
}

#[test]
fn a_question_holds_a_goal_until_it_is_answered_in_its_note() {
    let narsil = Narsil::new();
    let steps = script(&[
        r#"!tool work_plan {"directory":".","items":[{"title":"Colour for the door","question":true,"description":"Which colour should the front door be?"},{"title":"Paint the door","blocked_by":["Colour for the door"]}]}"#,
        "!say I need to know the colour.",
        r#"!tool work_next {"claim":"Paint the door"}"#,
        &format!(r#"!tool work_done {{"item":"Paint the door","result":"Painted.",{DONE}}}"#),
        r#"!tool goal_complete {"summary":"Door painted.","lessons":[]}"#,
        "!say Done.",
    ]);
    let goal = narsil.goal(&narsil.dir("door"), &format!("Paint the front door.\n{steps}"));

    // With nothing left that does not wait on the user, the goal stops and says why.
    assert_eq!(narsil.goal_reaches(&goal, &["completed", "blocked", "paused"]), "blocked");
    assert!(narsil.text("SELECT note FROM goals WHERE id = ?1", &[&goal]).contains("Colour for the door"));
    assert_eq!(narsil.item("Paint the door"), "open");

    // The user answers where the question is: in its note.
    let (path, note) = narsil.note(&narsil.work(), "Colour for the door").expect("the question's note");
    assert!(note.contains("status: waiting"), "{note}");
    std::fs::write(&path, format!("{}\n## Result\n\nBritish racing green.\n", note.replace("status: waiting", "status: done"))).unwrap();

    // The goal notices, carries on, and the answer is handed to the item that waited for it.
    assert_eq!(narsil.goal_reaches(&goal, &["completed", "paused", "cancelled"]), "completed");
    let taken = &narsil.goal_session(&goal).calls_to("work_next")[0];
    assert!(!taken.failed && taken.result.contains("British racing green."), "{}", taken.result);
    assert_eq!(narsil.item("Paint the door"), "done");
}

#[test]
fn the_work_is_still_in_front_of_the_agent_after_its_context_is_compacted() {
    // Compact once the conversation holds a few screens of output on top of the prompt.
    let narsil = Narsil::with(|_, config| config.set("context", "compact_at_tokens", "9000"));
    let dir = narsil.dir("long");
    let filler = "!bash head -c 6000 /dev/zero | tr '\\0' 'x'";
    let steps = script(&[
        r#"!tool work_plan {"directory":".","root":"Long job","items":[{"title":"Stage one"},{"title":"Stage two","blocked_by":["Stage one"]}]}"#,
        r#"!tool work_next {"claim":"Stage one"}"#,
        filler,
        filler,
        filler,
        filler,
        filler,
        filler,
        filler,
        filler,
        &format!(r#"!tool work_done {{"item":"Stage one","result":"Stage one is finished.",{DONE}}}"#),
        "!say Carrying on.",
    ]);
    let session = narsil.ask(&dir, &steps);
    let compactions = narsil.column("SELECT id FROM messages WHERE session_id = ?1 AND kind = 'compaction'", &[&session.id]);
    assert!(!compactions.is_empty(), "the conversation should have been compacted (context is {} tokens)", narsil.text("SELECT context_tokens FROM sessions WHERE id = ?1", &[&session.id]));

    // The prompt the session runs under after compaction was taken afresh, with the work as it then stood.
    let prompt = session.prompt();
    let at = prompt.find("# Work in this project").unwrap_or_else(|| panic!("no work section after compaction:\n{prompt}"));
    assert!(prompt[at..].contains("Stage one") && prompt[at..].contains("Stage two"), "{}", &prompt[at..]);
    // And the work went on to the end across it.
    assert_eq!(narsil.item("Stage one"), "done");
}

#[test]
fn a_reminder_comes_when_work_is_laid_out_and_none_of_it_is_taken() {
    let narsil = Narsil::new();
    let mut steps = vec![r#"!tool work_plan {"directory":".","items":[{"title":"First thing"},{"title":"Second thing"}]}"#.to_string()];
    steps.extend(std::iter::repeat_n("!bash true".to_string(), 8));
    steps.push(r#"!tool work_next {"claim":"First thing"}"#.to_string());
    steps.extend(std::iter::repeat_n("!bash true".to_string(), 9));
    steps.push("!say Enough.".to_string());
    let steps: Vec<&str> = steps.iter().map(String::as_str).collect();
    let session = narsil.ask(&narsil.dir("drift"), &script(&steps));

    let reminded: Vec<usize> = session
        .calls_to("bash")
        .iter()
        .enumerate()
        .filter(|(_, c)| c.result.contains("[narsil] Work is laid out for this project, but no item is in progress"))
        .map(|(i, _)| i + 1)
        .collect();
    // After eight calls with nothing taken; and not at all once an item is in hand.
    assert_eq!(reminded, vec![8], "{:#?}", session.calls_to("bash").iter().map(|c| &c.result).collect::<Vec<_>>());
}

#[test]
fn a_request_that_turns_into_building_is_laid_out_under_what_was_asked() {
    let narsil = Narsil::new();
    let dir = narsil.dir("game");
    let write = |name: &str| format!(r#"!tool write_file {{"path":"{name}","content":"x"}}"#);
    let steps = [
        write("index.html"),
        write("game.js"),
        write("style.css"),
        r#"!tool work_plan {"directory":".","items":[{"title":"Draw the board"},{"title":"Handle the keys","blocked_by":["Draw the board"]}]}"#.to_string(),
        write("keys.js"),
        "!say Under way.".to_string(),
    ];
    let steps: Vec<&str> = steps.iter().map(String::as_str).collect();
    // Asked in passing, not given as a goal.
    let session = narsil.ask(&dir, &format!("Build me a small game for the browser.\n{}", script(&steps)));

    // On the third file, with nothing laid out, it is told to lay the work out: once.
    let writes = session.calls_to("write_file");
    let told: Vec<bool> = writes.iter().map(|c| c.result.contains("[narsil] This has become more than a couple of steps")).collect();
    assert_eq!(told, vec![false, false, true, false], "{writes:#?}");
    assert!(writes[2].result.contains("work_plan") && writes[2].result.contains("side panel"), "{}", writes[2].result);

    // The plan hangs under what was asked, as a goal's does under its objective: that is what the panel shows.
    assert_eq!(narsil.text("SELECT title FROM work_items WHERE parent IS NULL", &[]), "Build me a small game for the browser.");
    assert_eq!(narsil.column("SELECT title FROM work_items WHERE parent IS NOT NULL ORDER BY rowid", &[]), vec!["Draw the board", "Handle the keys"]);

    // A small job never sees any of this.
    let quick = narsil.ask(&narsil.dir("fix"), &script(&[&write("a.txt"), &write("b.txt"), "!say Fixed."]));
    assert!(quick.calls().iter().all(|c| !c.result.contains("more than a couple of steps")));
    // Nor does work that is already laid out and going.
    let more = narsil.ask(&dir, &script(&[&write("one.js"), &write("two.js"), &write("three.js"), "!say More."]));
    assert!(more.calls().iter().all(|c| !c.result.contains("more than a couple of steps")), "{:#?}", more.calls());
}

#[test]
fn clearing_drops_what_is_unfinished_and_keeps_what_was_done() {
    let narsil = Narsil::new();
    let dir = narsil.dir("abandoned");
    let steps = script(&[
        r#"!tool work_plan {"directory":".","root":"Old idea","items":[{"title":"First part"},{"title":"Second part"},{"title":"Third part","blocked_by":["Second part"]}]}"#,
        r#"!tool work_next {"claim":"First part"}"#,
        &format!(r#"!tool work_done {{"item":"First part","result":"The first part is done.",{DONE}}}"#),
        r#"!tool work_next {"claim":"Second part"}"#,
        "!say Stopping here.",
    ]);
    narsil.ask(&dir, &steps);
    assert_eq!(narsil.item("Second part"), "in_progress");

    let said = narsil.ok(&dir, &["work", "--clear"]);
    assert!(said.starts_with("dropped 3 unfinished items"), "{said}");
    let status = |title: &str| narsil.item(title);
    assert_eq!((status("First part"), status("Second part"), status("Third part"), status("Old idea")), ("done".into(), "dropped".into(), "dropped".into(), "dropped".into()));
    // On record as the user's doing, in the note itself.
    let (_, note) = narsil.note(&narsil.work(), "Second part").expect("the item's note");
    assert!(note.contains("status: dropped") && note.contains("cleared by the user"), "{note}");
    assert_eq!(narsil.ok(&dir, &["work", "--clear"]).trim(), "nothing unfinished here");

    // A new session here starts with a clean slate, not with someone's abandoned plan.
    let fresh = narsil.ask(&dir, "What is there to do?");
    assert!(!fresh.prompt().contains("Ready now") && !fresh.prompt().contains("Second part"), "{}", fresh.prompt());
}
