//! The watch engine against real trees.
//!
//! The divergence state machine is unit-tested in `src/engine.rs` with no I/O
//! and no sleeps. These cover what only a repository can show: classification
//! against real files, the interaction with recompute, and the baseline
//! discipline that keeps one hunk's success from looking like its neighbour's
//! failure.
//!
//! Time is passed to `step` explicitly, so nothing here sleeps either.

mod common;

use common::Fixture;
use rote::config::Config;
use rote::engine::{Engine, EngineEvent, Origin, RealClock, Tick};
use rote::hunks::Status;
use rote::session::{self, Manifest};
use rote::shadow;
use rote::state;
use std::path::PathBuf;

/// A started session with the agent's edit already in the shadow.
fn started(real: &str, shadow_body: &str) -> (Fixture, Config, Engine) {
    let fx = Fixture::new();
    fx.write("a.rs", real);
    fx.commit_all("initial");
    let project = fx.project();
    project.ensure_state_dir().unwrap();
    let cfg = Config::default();
    let baseline = shadow::sync(&project, &cfg).unwrap();
    std::fs::write(project.shadow_dir.join("a.rs"), shadow_body).unwrap();
    Manifest::new(&project, "test task".into(), baseline)
        .save(&project)
        .unwrap();
    let engine = Engine::new(project, cfg.clone(), Box::new(RealClock::default()));
    (fx, cfg, engine)
}

fn manifest(fx: &Fixture) -> Manifest {
    Manifest::load(&fx.project()).unwrap().expect("a session")
}

fn changed() -> EngineEvent {
    EngineEvent::Changed(Origin::Real, PathBuf::from("a.rs"))
}

/// Past the recompute floor, so a step that wants to re-diff actually can.
const LATER: u64 = 5_000;

#[test]
fn the_first_step_builds_the_queue() {
    let (fx, _cfg, mut engine) = started("fn a() {\n}\n", "fn a() {\n    work();\n}\n");

    let out = engine.step(None, Tick(0)).unwrap();

    assert_eq!(manifest(&fx).pending().count(), 1);
    // And it publishes a snapshot describing it, so a pane has something to draw.
    let snap = match &out[0] {
        state::Event::Snapshot(s) => s.clone(),
        other => panic!("expected a snapshot, got {other:?}"),
    };
    assert_eq!(snap.counts.pending, 1);
    let active = snap.active.expect("an active hunk");
    assert_eq!(active.hunk.new_lines, vec!["    work();"]);
    assert_eq!(active.position, 1);
    assert_eq!(active.total, 1);
    // The anchor is resolved against the real file, so a client needs no files.
    assert_eq!(active.anchor_line, 2);
    assert!(active.real_path.ends_with("a.rs"));
}

#[test]
fn typing_the_active_hunk_advances_the_queue() {
    let (fx, _cfg, mut engine) = started("fn a() {\n}\n", "fn a() {\n    work();\n}\n");
    engine.step(None, Tick(0)).unwrap();
    assert_eq!(manifest(&fx).pending().count(), 1);

    // The user types it. No command, no editor — just a file on disk.
    fx.write("a.rs", "fn a() {\n    work();\n}\n");
    engine.step(Some(changed()), Tick(100)).unwrap();

    let m = manifest(&fx);
    assert_eq!(m.count(Status::Typed), 1);
    assert_eq!(m.pending().count(), 0);
}

#[test]
fn a_half_typed_save_does_not_raise_a_question() {
    // The behaviour the whole design turns on, observed end to end.
    let (fx, _cfg, mut engine) = started("fn a() {\n}\n", "fn a() {\n    work();\n}\n");
    engine.step(None, Tick(0)).unwrap();

    fx.write("a.rs", "fn a() {\n    wo\n}\n");
    engine.step(Some(changed()), Tick(100)).unwrap();
    // Well past the two-second grace window.
    engine.step(None, Tick(100_000)).unwrap();

    let m = manifest(&fx);
    assert!(
        m.hunks[0].pending_divergence.is_none(),
        "a prefix is an unfinished agreement, not a disagreement"
    );
    assert_eq!(m.pending().count(), 1, "still theirs to finish");
}

#[test]
fn a_settled_disagreement_becomes_a_question_and_the_queue_moves_on() {
    let (fx, _cfg, mut engine) = started("fn a() {\n}\n", "fn a() {\n    work();\n}\n");
    engine.step(None, Tick(0)).unwrap();

    fx.write("a.rs", "fn a() {\n    my_own_way();\n}\n");
    engine.step(Some(changed()), Tick(100)).unwrap();
    assert!(
        manifest(&fx).hunks[0].pending_divergence.is_none(),
        "not while the file might still be moving"
    );

    engine.step(None, Tick(100 + 2_000)).unwrap();

    let m = manifest(&fx);
    let q = m.hunks[0]
        .pending_divergence
        .as_ref()
        .expect("a question, once the file stopped moving");
    assert_eq!(q.proposed, vec!["    work();"]);
    assert_eq!(q.actual, vec!["    my_own_way();"]);
    assert_eq!(
        m.hunks[0].status,
        Status::Pending,
        "an unanswered question is not a terminal status"
    );
}

#[test]
fn a_recompute_between_the_disagreement_and_the_question_does_not_lose_it() {
    // Found by a daemon test that only passed when other tests happened to slow
    // it down. Once the user's own text is in the tree, a recompute re-diffs
    // that region as (theirs -> proposal), which is a *different* id: rule 5
    // drops the entry the watchdog armed against and rule 6 appends the new
    // one. The candidate then looked "gone" and was disarmed — and because the
    // baseline had already refreshed to include their text, nothing could ever
    // re-arm it. The disagreement became unaskable.
    let (fx, _cfg, mut engine) = started("fn a() {\n}\n", "fn a() {\n    work();\n}\n");
    engine.step(None, Tick(0)).unwrap();
    let armed_id = manifest(&fx).active().unwrap().id.clone();

    // They type their own version, and it settles.
    fx.write("a.rs", "fn a() {\n    my_own_way();\n}\n");
    engine.step(Some(changed()), Tick(100)).unwrap();

    // A full recompute lands before the grace window expires.
    engine
        .step(
            Some(EngineEvent::Command(state::Command::Refresh.into())),
            Tick(200),
        )
        .unwrap();
    engine.step(None, Tick(LATER)).unwrap();
    let m = manifest(&fx);
    assert!(
        m.find(&armed_id).is_none(),
        "the recompute should have re-identified the region"
    );

    // The question must still arrive, on whatever the region is called now.
    engine.step(None, Tick(LATER + 5_000)).unwrap();

    let m = manifest(&fx);
    let asked = m
        .pending()
        .find(|h| h.pending_divergence.is_some())
        .expect("the disagreement must survive being re-identified");
    let q = asked.pending_divergence.as_ref().unwrap();
    assert_eq!(q.actual, vec!["    my_own_way();"]);
    assert_eq!(q.proposed, vec!["    work();"]);
}

#[test]
fn fixing_the_text_withdraws_an_unanswered_question() {
    let (fx, _cfg, mut engine) = started("fn a() {\n}\n", "fn a() {\n    work();\n}\n");
    engine.step(None, Tick(0)).unwrap();
    fx.write("a.rs", "fn a() {\n    my_own_way();\n}\n");
    engine.step(Some(changed()), Tick(100)).unwrap();
    engine.step(None, Tick(2_100)).unwrap();
    assert!(manifest(&fx).hunks[0].pending_divergence.is_some());

    // They think better of it and type the proposal after all.
    fx.write("a.rs", "fn a() {\n    work();\n}\n");
    engine.step(Some(changed()), Tick(2_200)).unwrap();

    let m = manifest(&fx);
    assert_eq!(m.count(Status::Typed), 1);
    assert!(
        m.hunks[0].pending_divergence.is_none(),
        "the question is moot"
    );
}

#[test]
fn typing_one_hunk_does_not_raise_a_question_about_its_neighbour() {
    // The baseline-retake bug, and it is not theoretical. Every hunk in a file
    // is judged against one snapshot; accepting the first changes the file, so
    // the second's untouched text now differs from `before` for reasons that
    // have nothing to do with the user — and untouched text is rarely a prefix
    // of its proposal, so it reads as a flat disagreement.
    // Both regions sit inside the file rather than at its edges, so this tests
    // the baseline discipline and not the anchoring fallbacks.
    let (fx, _cfg, mut engine) = started(
        "one\ntwo\nthree\nfour\nfive\nsix\nseven\neight\nnine\nten\n",
        "one\nTWO\nthree\nfour\nfive\nsix\nseven\neight\nNINE\nten\n",
    );
    engine.step(None, Tick(0)).unwrap();
    assert_eq!(manifest(&fx).pending().count(), 2, "two separate regions");

    // Type only the first.
    fx.write(
        "a.rs",
        "one\nTWO\nthree\nfour\nfive\nsix\nseven\neight\nnine\nten\n",
    );
    engine.step(Some(changed()), Tick(100)).unwrap();
    // Let any armed candidate mature.
    engine.step(None, Tick(100_000)).unwrap();

    let m = manifest(&fx);
    assert_eq!(m.count(Status::Typed), 1, "the first hunk landed");
    assert!(
        m.hunks.iter().all(|h| h.pending_divergence.is_none()),
        "typing one hunk must not accuse the next one: {:?}",
        m.hunks
            .iter()
            .filter(|h| h.pending_divergence.is_some())
            .map(|h| (&h.file, &h.new_lines))
            .collect::<Vec<_>>()
    );
}

#[test]
fn starting_one_hunk_does_not_accuse_the_untouched_hunks_beside_it() {
    // Caught by hand, not by the suite. `classify` decides "untouched" by
    // comparing the whole file, which is right for one editor session over one
    // hunk and wrong for a watcher: a file usually holds several hunks, so
    // typing a single character into one made every other hunk in the file look
    // touched — and untouched text is rarely a prefix of its proposal, so each
    // of them armed a question about work not yet begun.
    let (fx, _cfg, mut engine) = started(
        "one\ntwo\nthree\nfour\nfive\nsix\nseven\neight\nnine\nten\n",
        "one\nTWO\nthree\nfour\nfive\nsix\nseven\neight\nNINE\nten\n",
    );
    engine.step(None, Tick(0)).unwrap();
    assert_eq!(manifest(&fx).pending().count(), 2);

    // Begin the first hunk. Nothing is finished, and the second is untouched.
    fx.write(
        "a.rs",
        "one\nTW\nthree\nfour\nfive\nsix\nseven\neight\nnine\nten\n",
    );
    engine.step(Some(changed()), Tick(100)).unwrap();
    engine.step(None, Tick(100_000)).unwrap();

    let m = manifest(&fx);
    let accused: Vec<&Vec<String>> = m
        .hunks
        .iter()
        .filter(|h| h.pending_divergence.is_some())
        .map(|h| &h.new_lines)
        .collect();
    assert!(
        accused.is_empty(),
        "typing into a file must not raise questions about the rest of it: {accused:?}"
    );
    assert_eq!(m.count(Status::Typed), 0, "nothing was finished");
}

#[test]
fn a_save_that_changes_nothing_relevant_does_not_re_diff_the_trees() {
    // The fast path exists so the keystroke loop never shells out to git. A
    // save that types nothing changes no status, so nothing needs recomputing.
    let (fx, _cfg, mut engine) = started("fn a() {\n}\n", "fn a() {\n    work();\n}\n");
    engine.step(None, Tick(0)).unwrap();
    let before = manifest(&fx).generation;

    // A save that leaves the hunk exactly as it was.
    fx.write("a.rs", "fn a() {\n}\n");
    for t in [100, 200, 300, 400] {
        engine.step(Some(changed()), Tick(t)).unwrap();
    }

    assert_eq!(
        manifest(&fx).generation,
        before,
        "no status changed, so nothing should have been written"
    );
}

#[test]
fn an_agent_edit_in_the_shadow_appends_new_work() {
    let (fx, _cfg, mut engine) = started("fn a() {\n}\n", "fn a() {\n    work();\n}\n");
    engine.step(None, Tick(0)).unwrap();
    assert_eq!(manifest(&fx).pending().count(), 1);

    // The user flipped back to the agent, which reworked the shadow.
    let project = fx.project();
    std::fs::write(
        project.shadow_dir.join("a.rs"),
        "fn a() {\n    work();\n}\n\nfn b() {\n    more();\n}\n",
    )
    .unwrap();
    engine
        .step(
            Some(EngineEvent::Changed(Origin::Shadow, PathBuf::from("a.rs"))),
            Tick(LATER),
        )
        .unwrap();
    // The re-diff is debounced, so it lands on a later step rather than this one.
    engine.step(None, Tick(LATER + 1_000)).unwrap();

    // Not a count: the two edits are within `CONTEXT_LINES` of each other, so
    // they arrive as one grown hunk rather than two. What matters is that the
    // new content reached the queue without anyone running a command.
    let m = manifest(&fx);
    let proposed: Vec<String> = m
        .pending()
        .flat_map(|h| h.new_lines.iter().cloned())
        .collect();
    assert!(
        proposed.iter().any(|l| l.contains("more();")),
        "the agent's rework must reach the queue on its own: {proposed:?}"
    );
}

#[test]
fn skip_and_resolve_come_in_as_commands() {
    let (fx, _cfg, mut engine) = started("fn a() {\n}\n", "fn a() {\n    work();\n}\n");
    engine.step(None, Tick(0)).unwrap();
    let id = manifest(&fx).active().unwrap().id.clone();

    engine
        .step(
            Some(EngineEvent::Command(
                state::Command::Skip {
                    hunk_id: id.clone(),
                }
                .into(),
            )),
            Tick(100),
        )
        .unwrap();

    let m = manifest(&fx);
    assert_eq!(m.count(Status::Skipped), 1);
    assert_eq!(m.last_presented.as_deref(), Some(id.as_str()));
}

#[test]
fn keeping_a_divergence_records_both_versions_and_is_not_re_offered() {
    let (fx, cfg, mut engine) = started("fn a() {\n}\n", "fn a() {\n    work();\n}\n");
    engine.step(None, Tick(0)).unwrap();
    fx.write("a.rs", "fn a() {\n    mine();\n}\n");
    engine.step(Some(changed()), Tick(100)).unwrap();
    engine.step(None, Tick(2_100)).unwrap();
    let id = manifest(&fx).hunks[0].id.clone();

    engine
        .step(
            Some(EngineEvent::Command(
                state::Command::Resolve {
                    hunk_id: id,
                    choice: state::Resolution::Keep,
                }
                .into(),
            )),
            Tick(2_200),
        )
        .unwrap();

    let m = manifest(&fx);
    assert_eq!(m.count(Status::Diverged), 1);
    let d = m.hunks[0].divergence.as_ref().expect("both versions kept");
    assert_eq!(d.proposed, vec!["    work();"]);
    assert_eq!(d.actual, vec!["    mine();"]);

    // And the recompute that follows must not offer it again.
    let project = fx.project();
    let mut reloaded = manifest(&fx);
    for round in 0..3 {
        session::recompute(&mut reloaded, &project, &cfg).unwrap();
        assert_eq!(
            reloaded.pending().count(),
            0,
            "round {round}: a kept divergence is final"
        );
    }
}

#[test]
fn a_closed_session_ends_the_loop() {
    let (fx, _cfg, mut engine) = started("fn a() {\n}\n", "fn a() {\n    work();\n}\n");
    engine.step(None, Tick(0)).unwrap();

    std::fs::remove_file(fx.project().session_json()).unwrap();
    let out = engine.step(None, Tick(100)).unwrap();

    assert!(
        matches!(out.as_slice(), [state::Event::Closed { .. }]),
        "the session going away means exit, not redraw: {out:?}"
    );
}
