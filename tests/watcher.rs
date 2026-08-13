//! The one test in this suite that waits on real wall-clock time.
//!
//! The filter is pure and unit-tested in `src/watcher.rs`; the engine's timing
//! is driven by an explicit `Tick`. This covers the single thing neither can:
//! that an ordinary write actually reaches the channel through the platform's
//! notification API. The ceiling is generous on purpose — it is here to catch
//! "no events ever arrive", not to measure latency.

mod common;

use common::Fixture;
use rote::config::Config;
use rote::engine::{EngineEvent, Origin};
use rote::watcher;
use std::sync::mpsc::RecvTimeoutError;
use std::time::{Duration, Instant};

/// Long enough that a loaded machine does not fail the suite. macOS fsevents
/// coalesce on a latency window, so anything tight here would be flaky.
const CEILING: Duration = Duration::from_secs(10);

#[test]
fn a_real_write_reaches_the_engine() {
    let fx = Fixture::new();
    fx.write("a.rs", "fn a() {}\n");
    fx.commit_all("initial");
    let project = fx.project();
    let cfg = Config::default();
    std::fs::create_dir_all(&project.shadow_dir).unwrap();

    let (tx, rx) = std::sync::mpsc::channel::<EngineEvent>();
    let _watch = watcher::spawn(&project, &cfg, move |ev| tx.send(ev).is_ok()).expect("a watcher");

    // Give the platform a moment to register the recursive watch before the
    // write, or the event can genuinely predate the subscription.
    std::thread::sleep(Duration::from_millis(300));
    fx.write("a.rs", "fn a() {\n    work();\n}\n");

    let deadline = Instant::now() + CEILING;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        match rx.recv_timeout(left) {
            Ok(EngineEvent::Changed(Origin::Real, rel)) if rel.to_string_lossy() == "a.rs" => {
                return; // what we came for
            }
            // Editors and filesystems generate plenty of other traffic; keep
            // reading until the one we care about turns up or time runs out.
            Ok(_) => continue,
            Err(RecvTimeoutError::Timeout) => {
                panic!("no event for a.rs within {}s", CEILING.as_secs())
            }
            Err(RecvTimeoutError::Disconnected) => panic!("the watcher hung up"),
        }
    }
}

#[test]
fn writes_under_git_never_reach_the_engine() {
    // The noisiest directory in the repository, and every write in it is
    // irrelevant to transcription.
    let fx = Fixture::new();
    fx.write("a.rs", "fn a() {}\n");
    fx.commit_all("initial");
    let project = fx.project();
    let cfg = Config::default();
    std::fs::create_dir_all(&project.shadow_dir).unwrap();

    let (tx, rx) = std::sync::mpsc::channel::<EngineEvent>();
    let _watch = watcher::spawn(&project, &cfg, move |ev| tx.send(ev).is_ok()).expect("a watcher");
    std::thread::sleep(Duration::from_millis(300));

    std::fs::write(project.repo_root.join(".git/ROTE_TEST"), "noise").unwrap();
    // Then a real write, so we have something to wait *for* rather than
    // asserting on a silence that would pass even if the watcher were dead.
    fx.write("a.rs", "fn a() {\n    work();\n}\n");

    let deadline = Instant::now() + CEILING;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        match rx.recv_timeout(left) {
            Ok(EngineEvent::Changed(_, rel)) => {
                let rel = rel.to_string_lossy().into_owned();
                assert!(
                    !rel.contains(".git"),
                    "a .git write reached the engine: {rel}"
                );
                if rel == "a.rs" {
                    return;
                }
            }
            Ok(_) => continue,
            Err(RecvTimeoutError::Timeout) => {
                panic!("the watcher never delivered the real write")
            }
            Err(RecvTimeoutError::Disconnected) => panic!("the watcher hung up"),
        }
    }
}
