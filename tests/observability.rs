//! What narsil records about a run says whose it is, the same way everywhere:
//! with several sessions, subagents and goals at work at once, one id finds
//! everything about one of them, in the log, the status, the totals and the
//! trace.

mod common;

use common::{Narsil, script};

#[test]
fn with_several_sessions_at_work_everything_says_whose_it_is() {
    let narsil = Narsil::with(|_, config| config.set("telemetry", "level", "\"debug\""));
    let (a_dir, b_dir, g_dir) = (narsil.dir("a"), narsil.dir("b"), narsil.dir("g"));

    // Two chats, one of them with a subagent, and a goal, all running at the same time.
    let both = std::thread::scope(|scope| {
        let a = scope.spawn(|| {
            narsil.ask(
                &a_dir,
                &script(&[
                    "!bash sleep 1; echo from-a",
                    r#"!tool spawn_agents {"agents":[{"name":"counter","task":"!bash echo from-child ~~ !say Counted."}]}"#,
                    "!say A is done.",
                ]),
            )
            .id
        });
        let b = scope.spawn(|| narsil.ask(&b_dir, &script(&["!bash sleep 1; echo from-b", "!say B is done."])).id);
        (a.join().unwrap(), b.join().unwrap())
    });
    let (a, b) = both;
    let goal = narsil.goal(&g_dir, &script(&["!bash echo from-goal", r#"!tool goal_complete {"summary":"Done.","lessons":[]}"#, "!say Done."]));
    assert_eq!(narsil.goal_reaches(&goal, &["completed", "blocked", "paused"]), "completed");
    let g = narsil.goal_session(&goal).id;
    let child = narsil.text("SELECT id FROM sessions WHERE parent_id = ?1", &[&a]);
    assert!(!child.is_empty());

    // Every line written while a session was at work names it; a subagent's names the session it works for too.
    let log = std::fs::read_to_string(narsil.home().join("state/daemon.log")).unwrap();
    let about = |what: &str| log.lines().filter(|l| l.contains(what)).map(str::to_string).collect::<Vec<_>>();
    let steps = about("model call used");
    assert!(steps.len() >= 9, "{log}");
    for line in &steps {
        assert!(
            [format!("[{a}] "), format!("[{b}] "), format!("[{a}>{child} counter] "), format!("[{g} goal:{goal}] ")].iter().any(|owner| line.contains(owner.as_str())),
            "a line that does not say whose it is: {line}"
        );
    }
    assert!(about("goal is now completed").iter().all(|l| l.contains(&format!("[{g} goal:{goal}] "))), "{:?}", about("goal is now completed"));
    assert!(about("command=\"Send\"").iter().all(|l| l.contains(&format!("[{a}] ")) || l.contains(&format!("[{b}] "))), "{:?}", about("command=\"Send\""));

    // `narsil logs --session` gives one session's lines, its subagent's among them, and nobody else's.
    let lines = |id: &str| narsil.ok(&a_dir, &["logs", "--session", id, "-n", "500"]);
    let of_a = lines(&a);
    assert!(of_a.contains(&format!("[{a}>{child} counter] ")) && of_a.lines().count() >= 6, "{of_a}");
    assert!(!of_a.contains(&b) && !of_a.contains(&g) && !of_a.contains(&goal), "{of_a}");
    let of_b = lines(&b);
    assert!(of_b.lines().count() >= 3 && of_b.lines().all(|l| l.contains(&format!("[{b}] "))), "{of_b}");
    // By the goal's id, or by a session id given in short.
    assert!(lines(&goal).lines().all(|l| l.contains(&format!("goal:{goal}]"))) && lines(&goal).lines().count() >= 3, "{}", lines(&goal));
    assert_eq!(lines(&a[..4]), of_a);
    assert!(lines(&child).lines().all(|l| l.contains(&format!("[{a}>{child} counter] "))));

    // The totals are split by session, named the same way, and the trace names the subagent as the log does.
    let stats = narsil.ok(&a_dir, &["stats"]);
    for owner in [a.clone(), b.clone(), format!("{g} goal:{goal}")] {
        assert!(stats.lines().any(|l| l.starts_with(&format!("{owner} "))), "no line for {owner} in:\n{stats}");
    }
    let trace = narsil.ok(&a_dir, &["trace", &a]);
    assert!(trace.contains(&format!("session {a} ")) && trace.contains(&format!("subagent {a}>{child} counter ")), "{trace}");
    let timeline = narsil.ok(&a_dir, &["trace", &a, "--timeline"]);
    assert!(timeline.contains(&format!("[{a}]")) && timeline.contains(&format!("[{a}>{child} counter]")), "{timeline}");
    assert!(narsil.ok(&g_dir, &["trace", &goal]).contains(&format!("goal {goal} turn 1")));
}

#[test]
fn what_is_in_flight_is_named_as_it_is_in_the_log() {
    let narsil = Narsil::new();
    let dir = narsil.dir("slow");
    let goal = narsil.goal(&dir, &script(&["!bash sleep 3; echo slow", r#"!tool goal_complete {"summary":"Done.","lessons":[]}"#, "!say Done."]));
    narsil.wait("the command to be running", || narsil.text("SELECT status FROM tool_calls WHERE name = 'bash'", &[]) == "running");
    let session = narsil.goal_session(&goal).id;
    let status = narsil.ok(&dir, &["status"]);
    assert!(status.contains(&format!("running:  1 session(s): {session}")), "{status}");
    assert!(status.contains(&format!("now:      [{session} goal:{goal}] tool call bash sleep 3; echo slow")), "{status}");
    assert_eq!(narsil.goal_reaches(&goal, &["completed", "blocked", "paused"]), "completed");
}
