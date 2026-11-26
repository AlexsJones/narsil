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
