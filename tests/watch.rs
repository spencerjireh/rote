//! `rote watch` end to end: a running process, a real editor-shaped write, and
//! a queue that empties without anybody issuing a command.
//!
//! These are the only tests that run rote as a long-lived process. Every one of
//! them passes `--timeout`, so a wedged watcher fails the suite instead of
//! hanging the machine running it.

mod common;

use common::cli::{stderr, stdout, Cli};
use common::Fixture;
use std::time::Duration;

/// Generous: this bounds a real filesystem notification plus a git recompute.
const TIMEOUT_MS: u64 = 20_000;

fn session(real: &str, shadow: &str) -> Cli {
    let fx = Fixture::new();
    fx.write("a.rs", real);
    fx.commit_all("initial");
    let cli = Cli::with_fixture(fx);
    let out = cli.run(&["start", "--no-launch", "add work"]);
    assert!(out.status.success(), "{}", stderr(&out));
    std::fs::write(cli.shadow().join("a.rs"), shadow).unwrap();
    cli
}

#[test]
fn typing_in_your_editor_drives_the_queue_to_empty() {
    // The whole point of the stage, observed from outside: no `rote next`, no
    // editor launch, no command between the keystroke and the queue moving.
    let cli = session("fn a() {\n}\n", "fn a() {\n    work();\n}\n");

    let child = cli.spawn(&[
        "watch",
        "--headless",
        "--exit-when-empty",
        "--timeout",
        &TIMEOUT_MS.to_string(),
    ]);

    // Let the pane build its queue, then type the hunk exactly as an editor
    // would: write the whole file.
    std::thread::sleep(Duration::from_millis(600));
    cli.fx.write("a.rs", "fn a() {\n    work();\n}\n");

    let out = child.wait_with_output().unwrap();
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(
        out.status.success(),
        "watch should exit cleanly once the queue drains:\nstdout: {text}\nstderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(text.contains("work();"), "the pane drew the hunk: {text}");

    // And the manifest agrees: typed, not merely gone.
    let status = stdout(&cli.run(&["status"]));
    assert!(status.contains("1 typed"), "{status}");
    assert!(status.contains("0 pending"), "{status}");
}

#[test]
fn the_pane_draws_a_jump_target_for_the_active_hunk() {
    let cli = session("fn a() {\n}\n", "fn a() {\n    work();\n}\n");

    // No typing: let it time out with the hunk still on screen.
    let child = cli.spawn(&["watch", "--headless", "--timeout", "1500"]);
    let out = child.wait_with_output().unwrap();
    let text = String::from_utf8_lossy(&out.stdout).into_owned();

    assert!(text.contains("a.rs:2"), "the jump target: {text}");
    assert!(text.contains("+     work();"), "{text}");
    assert!(text.contains("[s]kip"), "the footer: {text}");
    // Timing out is a failure, and it says so rather than exiting 0 quietly.
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("timed out"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn two_panes_over_one_daemon_both_see_the_same_hunk() {
    // The inversion this stage is for. Two panes used to be refused, because
    // each would have run its own engine and they would have corrupted each
    // other. Now neither runs one: they are both clients of the daemon that
    // does, so a second pane is ordinary rather than dangerous.
    let cli = session("fn a() {\n}\n", "fn a() {\n    work();\n}\n");

    let first = cli.spawn(&["watch", "--headless", "--timeout", "6000"]);
    let second = cli.spawn(&["watch", "--headless", "--timeout", "6000"]);

    let a = first.wait_with_output().unwrap();
    let b = second.wait_with_output().unwrap();
    let (ta, tb) = (
        String::from_utf8_lossy(&a.stdout).into_owned(),
        String::from_utf8_lossy(&b.stdout).into_owned(),
    );

    for (name, text) in [("first", &ta), ("second", &tb)] {
        assert!(
            text.contains("+     work();"),
            "the {name} pane drew the hunk: {text}"
        );
        assert!(text.contains("a.rs:2"), "the {name} pane: {text}");
    }
}

#[test]
fn a_second_local_pane_is_refused() {
    // `--local` runs an engine in this process, so it still needs the token —
    // and saying so names the thing to run instead.
    let cli = session("fn a() {\n}\n", "fn a() {\n    work();\n}\n");
    let mut first = cli.spawn(&["watch", "--local", "--headless", "--timeout", "4000"]);
    std::thread::sleep(Duration::from_millis(600));

    let second = cli.run(&["watch", "--local", "--headless", "--timeout", "1000"]);
    assert!(!second.status.success());
    let err = stderr(&second);
    assert!(err.contains("owns this project's queue"), "{err}");
    assert!(err.contains("rote watch"), "and names the fix: {err}");

    let _ = first.wait();
}

#[test]
fn watch_refuses_while_the_repo_is_mid_merge() {
    // A rebase in flight makes every classification nonsense, and the pane
    // would report it confidently.
    let cli = session("fn a() {\n}\n", "fn a() {\n    work();\n}\n");
    std::fs::write(cli.fx.repo.join(".git/MERGE_HEAD"), "deadbeef\n").unwrap();

    let out = cli.run(&["watch", "--headless", "--timeout", "1000"]);
    assert!(!out.status.success());
    assert!(
        stderr(&out).contains("merge") || stderr(&out).contains("in progress"),
        "{}",
        stderr(&out)
    );
}
