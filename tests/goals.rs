//! Where a goal ends: that the agent says so before it works, that one with
//! no end is shown as such and does not run for ever unasked, and that a goal
//! stops at what it is allowed. And the pace of the work: small steps taken
//! one request at a time are pointed out.

mod common;

use common::{Narsil, script};

#[test]
fn a_goal_with_no_finish_line_is_marked_and_pauses_for_the_user_to_decide() {
    // An open-ended goal is let run for about a second here, where an hour is the default.
    let narsil = Narsil::with(|_, config| config.set("goals", "open_ended_hours", "0.0003"));
    let dir = narsil.dir("site");
    let goal = narsil.goal(
        &dir,
        &script(&[r#"!tool goal_define {"open_ended":true,"done_when":[]}"#, "!bash echo polishing", "!bash sleep 2", "!say Still improving."]),
    );
    narsil.goal_reaches(&goal, &["paused"]);
    let note = narsil.text("SELECT note FROM goals WHERE id = ?1", &[&goal]);
    assert!(note.contains("no finish line"), "{note}");
    assert_eq!(narsil.text("SELECT done_when FROM goals WHERE id = ?1", &[&goal]), "open");

    // The agent was told what it means, and the user sees it wherever the goal is listed.
    let session = narsil.goal_session(&goal);
    let said = &session.calls_to("goal_define")[0].result;
    assert!(said.contains("open-ended") && said.contains("paused after"), "{said}");
    let listed = narsil.ok(&dir, &["goal", "list"]);
    assert!(listed.contains("[open-ended, runs until stopped]"), "{listed}");
    assert!(session.told_before_starting().iter().any(|t| t.contains("goal_define")) || session.everything().contains("First call goal_define"));

    // Set going again, it has the same time again: it is not stopped at once.
    narsil.ok(&dir, &["goal", "resume", &goal]);
    let resumed: i64 = narsil.text("SELECT resumed_at - created_at FROM goals WHERE id = ?1", &[&goal]).parse().unwrap();
    assert!(resumed >= 1, "{resumed}");
    assert_eq!(narsil.text("SELECT status FROM goals WHERE id = ?1", &[&goal]), "active");
}

#[test]
fn a_goal_that_says_what_done_means_is_held_to_it_and_one_that_does_not_is_asked() {
    let narsil = Narsil::new();
    let dir = narsil.dir("api");
    let goal = narsil.goal(
        &dir,
        &script(&[
            "!bash echo a",
            "!bash echo b",
            "!bash echo c",
            r#"!tool goal_define {"open_ended":false,"done_when":["the tests pass","the README says how to run it"]}"#,
            r#"!tool goal_complete {"summary":"Both hold.","lessons":[]}"#,
            "!say Done.",
        ]),
    );
    narsil.goal_reaches(&goal, &["completed"]);
    assert_eq!(narsil.text("SELECT done_when FROM goals WHERE id = ?1", &[&goal]), "the tests pass\nthe README says how to run it");
    let session = narsil.goal_session(&goal);
    // Three steps in without saying where it ends: asked to.
    let bash = session.calls_to("bash");
    assert!(!bash[1].result.contains("goal_define") && bash[2].result.contains("This goal has not said where it ends"), "{bash:#?}");
    assert!(session.calls_to("goal_define")[0].result.contains("done when: the tests pass; the README says how to run it"));
    assert!(!narsil.ok(&dir, &["goal", "list", "--all"]).contains("open-ended"));
}

#[test]
fn a_goal_stops_at_the_tokens_it_is_allowed() {
    let narsil = Narsil::with(|_, config| config.set("goals", "max_tokens", "3000"));
    let dir = narsil.dir("x");
    let steps: Vec<String> = (0..12).map(|i| format!("!bash echo {i}")).collect();
    let goal = narsil.goal(&dir, &script(&steps.iter().map(String::as_str).collect::<Vec<_>>()));
    narsil.goal_reaches(&goal, &["paused"]);
    let note = narsil.text("SELECT note FROM goals WHERE id = ?1", &[&goal]);
    assert!(note.contains("token budget reached"), "{note}");
    let used: i64 = narsil.text("SELECT SUM(input + cache_read + cache_write + output) FROM model_usage", &[]).parse().unwrap();
    assert!(used >= 3000, "{used}");
}

#[test]
fn small_steps_taken_one_request_at_a_time_are_pointed_out_once_a_run() {
    let narsil = Narsil::new();
    let dir = narsil.dir("x");
    std::fs::write(dir.join("a.txt"), "one\ntwo\n").unwrap();
    let mut steps: Vec<&str> = vec!["!bash cat a.txt"; 5];
    steps.extend(["!bash cat a.txt; wc -l a.txt", "!bash sed -i 's/one/1/' a.txt", "!say Done."]);
    let session = narsil.ask(&dir, &script(&steps));
    let told: Vec<bool> = session.calls_to("bash").iter().map(|c| c.result.contains("small steps in a row")).collect();
    // The fifth look in a row is where it is said; two things in one command is not a small step.
    assert_eq!(told, vec![false, false, false, false, true, false, false]);
    assert!(session.calls_to("bash")[4].result.contains("put the commands in one"));

    // What a tool gives back is cut to a size that is worth carrying in every later request.
    let big = narsil.ask(&dir, &script(&["!bash seq 1 20000", "!say Done."]));
    let len = big.calls_to("bash")[0].result.len();
    assert!((8_000..=13_000).contains(&len), "{len}");
}
