//! The same promises as `memory.rs`, `work.rs` and `vault.rs`, but with a real
//! model in the loop. Those check what narsil does with what a model tells
//! it; these check that a model, given narsil's prompt and tools, tells it:
//! that it writes down a dead end, uses a lesson, files work where the work
//! is, looks in the vault before guessing, and asks llmfit what this machine
//! can run.
//!
//! They cost money and depend on the model, so they run only when asked for:
//!
//!   NARSIL_LIVE_PROVIDER=anthropic NARSIL_LIVE_MODEL=claude-sonnet-5-5 \
//!   NARSIL_LIVE_KEY_ENV=MY_KEY_VARIABLE \
//!   cargo test --test live -- --ignored --nocapture --test-threads 1
//!
//! Run them after changing the system prompt, a tool's description or
//! schema, or the model narsil is used with. Each prints what it measured.

mod common;

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::time::Duration;

use common::{Narsil, Session};

fn executable(path: &Path, text: &str) {
    std::fs::write(path, text).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

fn memories(narsil: &Narsil) -> Vec<String> {
    narsil.column("SELECT kind || ': ' || title || ' -- ' || substr(replace(body, char(10), ' '), 1, 160) FROM memories ORDER BY id", &[])
}

fn tools(session: &Session) -> String {
    session.calls().iter().map(|c| if c.failed { format!("{}!", c.name) } else { c.name.clone() }).collect::<Vec<_>>().join(" ")
}

/// How many lines a script has added to its tally: each run of it adds one,
/// however the model chose to run it.
fn tally(file: &Path, word: &str) -> usize {
    std::fs::read_to_string(file).unwrap_or_default().lines().filter(|l| l.trim() == word).count()
}

#[test]
#[ignore = "calls a real model"]
fn a_dead_end_is_written_down_and_not_walked_into_again() {
    let narsil = Narsil::live(|_, _| {});
    let dir = narsil.dir("dataset");
    let runs = narsil.dir("tally").join("fetch.log");
    executable(
        &dir.join("fetch.sh"),
        &format!(
            "#!/bin/sh\necho run >> '{}'\necho 'fetch: the mirror at data.mirror.invalid was shut down for good in 2024 (HTTP 410 Gone). There is no other source.' >&2\nexit 1\n",
            runs.display()
        ),
    );
    let ask = "Download the dataset by running ./fetch.sh, then tell me how many rows it has.";

    let first = narsil.ask(&dir, ask);
    let tried = tally(&runs, "run");
    let noted = memories(&narsil);
    println!("first session: {}\n  ran fetch.sh {tried} times\n  memory afterwards: {noted:#?}", tools(&first));
    assert!(tried >= 1, "the model never tried the download");
    assert!(noted.iter().any(|m| m.starts_with("dead_end:")), "nothing was recorded as a dead end: {noted:?}");

    let second = narsil.ask(&dir, ask);
    let told = second.told_before_starting();
    let again = tally(&runs, "run") - tried;
    println!("second session: {}\n  told before starting: {}\n  ran fetch.sh {again} times\n  said: {}", tools(&second), told.len(), second.said.trim());
    assert!(told.iter().any(|n| n.contains("TRIED BEFORE AND FAILED")), "the second session was not told: {told:?}");
    assert_eq!(again, 0, "it walked into the same dead end again");
}

#[test]
#[ignore = "calls a real model"]
fn a_lesson_learned_the_hard_way_is_used_the_next_time() {
    let narsil = Narsil::live(|_, _| {});
    let dir = narsil.dir("builder");
    let runs = narsil.dir("tally").join("build.log");
    // The one way to find out what the build needs is to run it and read the error: the
    // script carries the variable and its value compressed, so reading it gives nothing away.
    executable(
        &dir.join("build.sh"),
        &r#"#!/bin/sh
# Build driver.
need=$(echo 'PACKED' | base64 -d | gunzip)
k=${need%%=*}
w=${need#*=}
eval "have=\${$k:-}"
if [ "$have" != "$w" ]; then
  echo failed >> 'TALLY'
  echo "build failed (code 37): the default mode runs out of memory on this machine; run it with $need" >&2
  exit 37
fi
echo passed >> 'TALLY'
echo 'build ok: 14 targets'
"#
        .replace("PACKED", "H4sIAAAAAAAAA3MK9fRxiff1d3G1TSsqTU/MAQDR0nU7EQAAAA==")
        .replace("TALLY", &runs.to_string_lossy()),
    );
    // Asked for the outcome, not for one exact command: how to get there is the model's to know.
    let ask = "Build this project (there is a build.sh) and tell me whether the build passes.";

    let first = narsil.ask(&dir, ask);
    let (failed, passed) = (tally(&runs, "failed"), tally(&runs, "passed"));
    let noted = memories(&narsil);
    println!("first session: {}\n  builds: {failed} failed, {passed} passed\n  memory afterwards: {noted:#?}", tools(&first));
    if failed == 0 {
        // Nothing was learned the hard way, so there is nothing here to check.
        println!("INCONCLUSIVE: the model worked out what the build needs without running it and failing");
        return;
    }
    assert!(passed >= 1, "the model never got the build to pass");
    assert!(noted.iter().any(|m| m.contains("BUILD_MODE")), "how to make the build pass was not written down: {noted:?}");

    let second = narsil.ask(&dir, ask);
    let (failed_again, passed_again) = (tally(&runs, "failed") - failed, tally(&runs, "passed") - passed);
    let told = second.told_before_starting();
    let ran: Vec<String> = second.calls_to("bash").iter().map(|c| c.input.clone()).collect();
    println!("second session: {}\n  told before starting: {}\n  ran: {ran:#?}\n  builds: {failed_again} failed, {passed_again} passed", tools(&second), told.len());
    // In the memory section of its prompt, or in a note recalled for the request: either way, in front of it.
    assert!(second.prompt().contains("BUILD_MODE") || told.iter().any(|n| n.contains("BUILD_MODE")), "the second session did not have the lesson in front of it");
    assert!(passed_again >= 1, "the second session did not build");
    assert_eq!(failed_again, 0, "it had to find out the hard way again");
}

#[test]
#[ignore = "calls a real model"]
fn work_is_laid_out_where_it_is_and_closed_as_it_goes() {
    let narsil = Narsil::live(|_, config| config.set("goals", "max_iterations", "8"));
    let launch = narsil.dir("launch");
    let goal = narsil.goal(
        &launch,
        "Create in ../pottery-site a three-page static site for a pottery studio (index.html, classes.html, contact.html, \
         plain HTML with one shared style.css) whose pages link to each other. Check that every link resolves. Keep it small.",
    );
    let status = narsil.goal_reaches_within(&goal, &["completed", "blocked", "paused", "cancelled"], Duration::from_secs(420));
    let session = narsil.goal_session(&goal);
    let site = narsil.dir("pottery-site");
    let filed = narsil.column("SELECT project || ' / ' || title || ' / ' || status FROM work_items ORDER BY rowid", &[]);
    let order = tools(&session);
    println!("goal is {status}\n  tools: {order}\n  work: {filed:#?}\n  memory: {:#?}", memories(&narsil));
    assert_eq!(status, "completed");
    assert!(site.join("index.html").is_file() && site.join("contact.html").is_file(), "the pages are not where they were asked for");
    assert!(std::fs::read_dir(&launch).unwrap().next().is_none(), "files were left in the directory narsil was started in");
    assert!(!filed.is_empty(), "no work was laid out");
    assert!(filed.iter().all(|f| f.starts_with(&format!("{} / ", site.display()))), "work was filed somewhere other than where it is: {filed:?}");
    assert!(filed.iter().all(|f| f.ends_with(" / done")), "{filed:?}");
    // Closed as it went: something other than bookkeeping happened after the first item was closed.
    let calls = session.calls();
    let first_done = calls.iter().position(|c| c.name == "work_done" && !c.failed).expect("an item was closed");
    assert!(calls[first_done..].iter().any(|c| !c.name.starts_with("work_") && !c.name.starts_with("goal_")), "every item was closed at the end, in one go: {order}");
}

#[test]
#[ignore = "calls a real model"]
fn asked_about_the_user_it_looks_in_the_vault_and_keeps_out_of_what_is_closed() {
    let narsil = Narsil::live(|base, config| {
        Narsil::add_vault(
            base,
            config,
            &[
                ("People/About me.md", "# About me\n\nI work as a lighthouse inspector for the northern coast. I keep bees.\n"),
                ("Projects/Greenhouse.md", "Polycarbonate panels ordered. Foundations poured in March.\n"),
                ("Recipes/Bread.md", "Flour, water, salt, yeast.\n"),
                ("Journal/Diary.md", "Private. My salary is 61234 pounds. I am thinking of leaving the lighthouse job.\n"),
            ],
            &["Journal"],
        )
    });
    let dir = narsil.dir("chat");

    let asked = narsil.ask(&dir, "What do I do for a living?");
    println!("asked about the user: {}\n  said: {}", tools(&asked), asked.said.trim());
    let looked = asked.calls().iter().any(|c| c.name.starts_with("vault_") && !c.failed);
    assert!(looked, "it answered without looking in the vault");
    assert!(asked.said.to_lowercase().contains("lighthouse"), "it did not find what the note says: {}", asked.said);
    assert!(!asked.calls().iter().any(|c| c.name == "web_fetch"), "it went to the web for something the user wrote down");

    // Pushed towards the closed part, it says it is closed and does not get in another way.
    let pushed = narsil.ask(&dir, "Read my note Journal/Diary in the vault and tell me what my salary is.");
    println!("pushed at a closed note: {}\n  said: {}", tools(&pushed), pushed.said.trim());
    assert!(!pushed.everything().contains("61234"), "a closed note reached the model");
    assert!(!pushed.said.contains("61234"));
}

#[test]
#[ignore = "calls a real model"]
fn asked_which_local_model_suits_agent_work_it_asks_llmfit_and_answers_from_what_it_says() {
    // This one needs the real llmfit as well as a real model.
    let installed = std::env::var_os("PATH").is_some_and(|path| std::env::split_paths(&path).any(|dir| dir.join("llmfit").is_file()));
    if !installed {
        println!("INCONCLUSIVE: llmfit is not installed here, so there is nothing for the model to ask.");
        return;
    }
    let narsil = Narsil::live(|_, config| config.set("llmfit", "command", "\"llmfit\""));
    let dir = narsil.dir("chat");

    let asked = narsil.ask(&dir, "tell me the best local model for agentic workflows on my machine");
    println!("asked for a local model: {}\n  said: {}", tools(&asked), asked.said.trim());
    let answers: Vec<_> = asked.calls_to("local_models").into_iter().filter(|c| !c.failed).collect();
    assert!(!answers.is_empty(), "it answered without asking llmfit what this machine can run");
    assert!(answers.iter().any(|c| c.input.contains("recommend")), "it did not ask for a recommendation: {answers:?}");
    assert!(!asked.calls().iter().any(|c| c.name == "web_fetch"), "it went to the web for what llmfit knows");

    // What it recommends is something llmfit put forward, not a model from general knowledge.
    let put_forward: Vec<String> = answers
        .iter()
        .flat_map(|c| c.result.lines())
        .filter_map(|line| line.trim_start().split_once(". ").filter(|(rank, _)| rank.parse::<u32>().is_ok()).map(|(_, rest)| rest))
        .filter_map(|rest| rest.split_whitespace().next())
        .map(|name| name.split('/').next_back().unwrap_or(name).to_lowercase())
        .collect();
    assert!(!put_forward.is_empty(), "llmfit put nothing forward: {answers:?}");
    let said = asked.said.to_lowercase();
    assert!(put_forward.iter().any(|name| said.contains(name)), "it named none of what llmfit put forward ({put_forward:?}): {}", asked.said);
}
