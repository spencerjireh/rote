//! M3 acceptance: the state machine and recompute against real trees.
//!
//! The seven reconciliation rules are unit-tested directly in `src/session.rs`
//! against `reconcile`. These tests cover what only a repository can show:
//! drift, the self-truing property, archiving, and lock survival.

mod common;

use common::Fixture;
use rote::config::Config;
use rote::hunks::Status;
use rote::paths::Lock;
use rote::session::{self, Manifest, State, Terminal};
use rote::shadow;

/// A started session with the agent's edit already in the shadow.
fn started(body_real: &str, body_shadow: &str) -> (Fixture, Config, Manifest) {
    let fx = Fixture::new();
    fx.write("a.rs", body_real);
    fx.commit_all("initial");
    let project = fx.project();
    let cfg = Config::default();
    let baseline = shadow::sync(&project, &cfg).unwrap();
    std::fs::write(project.shadow_dir.join("a.rs"), body_shadow).unwrap();
    let manifest = Manifest::new(&project, "test task".into(), baseline);
    (fx, cfg, manifest)
}

#[test]
fn manifest_round_trips_through_disk() {
    let (fx, _cfg, manifest) = started("fn a() {}\n", "fn a() { x(); }\n");
    let project = fx.project();
    project.ensure_state_dir().unwrap();
    manifest.save(&project).unwrap();

    let loaded = Manifest::load(&project).unwrap().expect("session present");
    assert_eq!(loaded, manifest);
    assert_eq!(loaded.state, State::Working);
}

#[test]
fn no_session_reads_as_idle() {
    let fx = Fixture::new();
    fx.write("a.rs", "x\n");
    fx.commit_all("initial");
    assert!(Manifest::load(&fx.project()).unwrap().is_none());
}

#[test]
fn recompute_builds_the_queue_from_the_trees() {
    let (fx, cfg, mut manifest) = started("fn a() {\n}\n", "fn a() {\n    work();\n}\n");
    let project = fx.project();

    let report = session::recompute(&mut manifest, &project, &cfg).unwrap();

    assert_eq!(report.added, 1);
    assert!(!report.drift);
    assert_eq!(manifest.pending().count(), 1);
    assert_eq!(
        manifest.head_of_queue().unwrap().new_lines,
        vec!["    work();"]
    );
}

#[test]
fn typing_a_hunk_retires_it_from_the_queue() {
    // The self-truing property: recompute needs no help to notice.
    let (fx, cfg, mut manifest) = started("fn a() {\n}\n", "fn a() {\n    work();\n}\n");
    let project = fx.project();
    session::recompute(&mut manifest, &project, &cfg).unwrap();

    let id = manifest.head_of_queue().unwrap().id.clone();
    manifest.find_mut(&id).unwrap().status = Status::Typed;
    // The user actually types it into the real tree.
    fx.write("a.rs", "fn a() {\n    work();\n}\n");

    let report = session::recompute(&mut manifest, &project, &cfg).unwrap();

    assert_eq!(report.added, 0);
    assert_eq!(manifest.pending().count(), 0);
    assert_eq!(manifest.count(Status::Typed), 1, "history is kept");
    assert!(report.warnings.is_empty(), "no warning for the normal path");
}

#[test]
fn a_skipped_hunk_never_returns_across_real_recomputes() {
    let (fx, cfg, mut manifest) = started("fn a() {\n}\n", "fn a() {\n    work();\n}\n");
    let project = fx.project();
    session::recompute(&mut manifest, &project, &cfg).unwrap();

    let id = manifest.head_of_queue().unwrap().id.clone();
    manifest.find_mut(&id).unwrap().status = Status::Skipped;

    // The region still differs between the trees on every single recompute.
    for round in 0..5 {
        let report = session::recompute(&mut manifest, &project, &cfg).unwrap();
        assert_eq!(report.added, 0, "round {round}");
        assert_eq!(
            manifest.pending().count(),
            0,
            "round {round}: skip is durable"
        );
        assert_eq!(manifest.hunks.len(), 1, "round {round}: no duplicates");
    }
}

#[test]
fn transcription_alone_never_raises_the_drift_flag() {
    // The uncommitted digest changes on every keystroke; HEAD does not. Drift
    // must track HEAD alone or it fires constantly (DESIGN.md §9.2).
    let (fx, cfg, mut manifest) = started("fn a() {\n}\n", "fn a() {\n    work();\n}\n");
    let project = fx.project();
    session::recompute(&mut manifest, &project, &cfg).unwrap();

    fx.write("a.rs", "fn a() {\n    wo\n}\n"); // half-typed
    let mid = session::recompute(&mut manifest, &project, &cfg).unwrap();
    assert!(!mid.drift, "typing is not drift");

    fx.write("a.rs", "fn a() {\n    work();\n}\n"); // finished
    let done = session::recompute(&mut manifest, &project, &cfg).unwrap();
    assert!(!done.drift, "finishing is not drift either");
}

#[test]
fn a_commit_mid_session_raises_drift_but_does_not_block() {
    let (fx, cfg, mut manifest) = started("fn a() {\n}\n", "fn a() {\n    work();\n}\n");
    let project = fx.project();
    session::recompute(&mut manifest, &project, &cfg).unwrap();

    // The user commits something unrelated while the session is open.
    fx.write("other.rs", "fn other() {}\n");
    fx.commit_all("unrelated work");

    let report = session::recompute(&mut manifest, &project, &cfg).unwrap();

    assert!(report.drift, "HEAD moved");
    assert!(report.warnings[0].contains("baseline drift"));
    assert_eq!(manifest.pending().count(), 1, "best-effort, not blocked");
}

#[test]
fn abort_archives_the_residue_then_restores_the_shadow() {
    let (fx, cfg, mut manifest) = started("fn a() {\n}\n", "fn a() {\n    work();\n}\n");
    let project = fx.project();
    project.ensure_state_dir().unwrap();
    session::recompute(&mut manifest, &project, &cfg).unwrap();
    manifest.save(&project).unwrap();

    let archived =
        session::archive_and_clear(&mut manifest, &project, &cfg, Terminal::Aborted).unwrap();
    shadow::sync(&project, &cfg).unwrap();

    // The agent's untyped work is recoverable.
    assert!(archived.patch.exists());
    let patch = std::fs::read_to_string(&archived.patch).unwrap();
    assert!(
        patch.contains("work();"),
        "residue must hold the agent's change"
    );

    // The archived manifest records how it ended.
    let text = std::fs::read_to_string(&archived.manifest).unwrap();
    assert!(text.contains("\"terminal\": \"aborted\""), "{text}");
    assert!(text.contains("\"state\": \"idle\""));

    // And the session is gone.
    assert!(Manifest::load(&project).unwrap().is_none());
    // The shadow now matches the real tree again.
    let shadow_body = std::fs::read_to_string(project.shadow_dir.join("a.rs")).unwrap();
    assert_eq!(shadow_body, "fn a() {\n}\n");
}

#[test]
fn residue_patch_is_empty_when_everything_was_typed() {
    let (fx, cfg, mut manifest) = started("fn a() {\n}\n", "fn a() {\n    work();\n}\n");
    let project = fx.project();
    project.ensure_state_dir().unwrap();
    session::recompute(&mut manifest, &project, &cfg).unwrap();
    fx.write("a.rs", "fn a() {\n    work();\n}\n"); // typed exactly

    let archived =
        session::archive_and_clear(&mut manifest, &project, &cfg, Terminal::Done).unwrap();
    assert_eq!(archived.residue_bytes, 0, "nothing was left behind");
}

#[test]
fn a_manifest_from_another_version_is_refused_with_advice() {
    let fx = Fixture::new();
    fx.write("a.rs", "x\n");
    fx.commit_all("initial");
    let project = fx.project();
    project.ensure_state_dir().unwrap();
    std::fs::write(
        project.session_json(),
        r#"{"version":999,"state":"working","task":"","created_at":"2026-01-01T00:00:00Z",
            "project_root":"/x","shadow_dir":"/y",
            "baseline":{"head":"abc","uncommitted_digest":"","synced_at":"2026-01-01T00:00:00Z"},
            "hunks":[]}"#,
    )
    .unwrap();

    let err = Manifest::load(&project).unwrap_err().to_string();
    assert!(err.contains("manifest version 999"), "{err}");
}

#[test]
fn concurrent_writers_do_not_lose_each_others_work() {
    // The regression that had no test. The old pattern — load unlocked, mutate,
    // then lock only for the write — let two processes both load the same
    // manifest, both mutate different hunks, and both save: last writer wins and
    // the other's mutation vanishes with nothing detecting it.
    let (fx, cfg, mut manifest) = started("fn a() {\n}\n", "fn a() {\n    work();\n}\n");
    let project = fx.project();
    project.ensure_state_dir().unwrap();

    // A queue with enough hunks that every thread has its own to claim.
    for i in 0..8 {
        fx.write(&format!("f{i}.rs"), &format!("fn f{i}() {{\n}}\n"));
    }
    fx.commit_all("more files");
    for i in 0..8 {
        std::fs::write(
            project.shadow_dir.join(format!("f{i}.rs")),
            format!("fn f{i}() {{\n    work();\n}}\n"),
        )
        .unwrap();
    }
    session::recompute(&mut manifest, &project, &cfg).unwrap();
    manifest.save(&project).unwrap();

    let ids: Vec<String> = manifest.pending().map(|h| h.id.clone()).collect();
    assert!(ids.len() >= 8, "expected a real queue, got {}", ids.len());
    let start_generation = manifest.generation;

    std::thread::scope(|scope| {
        for id in &ids {
            let project = &project;
            scope.spawn(move || {
                session::with_session(project, |m| {
                    m.find_mut(id).unwrap().status = Status::Skipped;
                    Ok(())
                })
                .unwrap();
            });
        }
    });

    let reloaded = Manifest::load(&project).unwrap().unwrap();
    for id in &ids {
        assert_eq!(
            reloaded.find(id).map(|h| h.status),
            Some(Status::Skipped),
            "hunk {id} lost its mutation to a concurrent writer"
        );
    }
    assert_eq!(
        reloaded.generation,
        start_generation + ids.len() as u64,
        "every write must bump the generation exactly once"
    );
}

#[test]
fn the_lock_survives_a_killed_process() {
    // kill -9 leaves the lock file behind with a PID that no longer exists.
    let fx = Fixture::new();
    fx.write("a.rs", "x\n");
    fx.commit_all("initial");
    let project = fx.project();
    project.ensure_state_dir().unwrap();

    let mut child = std::process::Command::new("true").spawn().unwrap();
    let dead = child.id();
    child.wait().unwrap();
    std::fs::write(
        project.lock_path(),
        format!(r#"{{"pid":{dead},"acquired_at":"2026-01-01T00:00:00Z"}}"#),
    )
    .unwrap();

    // The next invocation reclaims it rather than refusing forever.
    let _lock = Lock::acquire(&project.lock_path()).expect("stale lock must be reclaimed");
}

#[test]
fn a_live_holder_blocks_a_second_invocation() {
    let fx = Fixture::new();
    fx.write("a.rs", "x\n");
    fx.commit_all("initial");
    let project = fx.project();
    project.ensure_state_dir().unwrap();

    let _held = Lock::acquire(&project.lock_path()).unwrap();

    // Contention is exact and immediate.
    assert!(Lock::try_acquire(&project.lock_path()).unwrap().is_none());

    // A waiter that runs out of patience names the process to go and deal with.
    // Short timeout: the default 5s ceiling is for humans, not for tests.
    let err = Lock::acquire_within(&project.lock_path(), std::time::Duration::from_millis(200))
        .unwrap_err()
        .to_string();
    assert!(err.contains("another rote process"), "{err}");
    assert!(
        err.contains(&format!("pid {}", std::process::id())),
        "the holder should be named: {err}"
    );
}
