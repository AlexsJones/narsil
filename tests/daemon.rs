//! Stopping and restarting the daemon, checked on a real one.

mod common;

use std::time::{Duration, Instant};

use common::Narsil;

#[test]
fn stopping_does_not_wait_on_a_daemon_that_has_already_gone() {
    let narsil = Narsil::new();
    // The daemon as the child of a process that stays alive and does not collect it when it
    // exits: this test. That is the position of a TUI that started the daemon and is still open.
    let mut daemon = narsil.spawn_daemon();
    narsil.wait("the daemon to answer", || narsil.try_run(&narsil.dir("x"), &["status"]).1.contains("is running"));

    let began = Instant::now();
    let said = narsil.ok(&narsil.dir("x"), &["stop"]);
    assert!(said.contains("narsil stopped"), "{said}");
    assert!(began.elapsed() < Duration::from_secs(10), "stop waited {:?} on a daemon that had already exited", began.elapsed());

    // And a restart from here brings a new one up at once.
    let began = Instant::now();
    let said = narsil.ok(&narsil.dir("x"), &["restart"]);
    assert!(said.contains("narsil started"), "{said}");
    assert!(began.elapsed() < Duration::from_secs(10), "restart took {:?}", began.elapsed());
    let _ = daemon.wait();
}

#[test]
fn setting_up_again_puts_the_new_settings_in_force_in_a_daemon_that_is_running() {
    let narsil = Narsil::new();
    let dir = narsil.dir("x");
    // A session starts the daemon, with the model that was set up first.
    narsil.ask(&dir, "!say hello");
    assert!(narsil.ok(&dir, &["status"]).contains("mock/mock"));

    // Setting up another model while it runs: the daemon is started again, so that it is the one in use.
    let said = narsil.ok(&dir, &["setup", "--yes", "--provider", "mock", "--model", "second", "--no-test"]);
    assert!(said.contains("The new settings are in force"), "{said}");
    let status = narsil.ok(&dir, &["status"]);
    assert!(status.contains("is running") && status.contains("mock/second"), "{status}");

    // And the next session is given it.
    narsil.ask(&dir, "!say again");
    let models = narsil.column("SELECT model FROM sessions ORDER BY created_at, rowid", &[]);
    assert_eq!(models, vec!["mock/mock", "mock/second"]);
}
