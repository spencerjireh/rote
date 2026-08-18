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
use rote::engine::{Engine, EngineEvent, Origin, Tick};
use rote::hunks::Status;
use rote::session::{self, Manifest};
use rote::shadow;
use rote::state;
use std::path::{Path, PathBuf};

/// A started session with the agent's edit already in the shadow.
///
/// The curator is off. `Config::default()` turns it on, and these tests drive a
/// real engine against a real tree — leaving it on would have every one of them
/// try to spawn the developer's actual `claude`. `curated` below is the opt-in.
fn started(real: &str, shadow_body: &str) -> (Fixture, Config, Engine) {
    started_with(real, shadow_body, |cfg| cfg.curator_enabled = false)
}

fn started_with(
    real: &str,
    shadow_body: &str,
    tune: impl FnOnce(&mut Config),
) -> (Fixture, Config, Engine) {
    let fx = Fixture::new();
    fx.write("a.rs", real);
    fx.commit_all("initial");
    let project = fx.project();
    project.ensure_state_dir().unwrap();
    let mut cfg = Config::default();
    tune(&mut cfg);
    let baseline = shadow::sync(&project, &cfg).unwrap();
    std::fs::write(project.shadow_dir.join("a.rs"), shadow_body).unwrap();
    Manifest::new(&project, "test task".into(), baseline)
        .save(&project)
        .unwrap();
    let engine = Engine::new(project, cfg.clone());
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
    // Two hunks, because the second half of the name needs somewhere to move *to*.
    // With one hunk the queue cannot demonstrate advancing, which is how the
    // behaviour went unimplemented under a test named for it.
    let fx = Fixture::new();
    fx.write("a.rs", "fn a() {\n}\n");
    fx.write("b.rs", "fn b() {\n}\n");
    fx.commit_all("initial");
    let project = fx.project();
    project.ensure_state_dir().unwrap();
    let cfg = Config {
        curator_enabled: false,
        ..Config::default()
    };
    let baseline = shadow::sync(&project, &cfg).unwrap();
    std::fs::write(
        project.shadow_dir.join("a.rs"),
        "fn a() {\n    work();\n}\n",
    )
    .unwrap();
    std::fs::write(
        project.shadow_dir.join("b.rs"),
        "fn b() {\n    more();\n}\n",
    )
    .unwrap();
    Manifest::new(&project, "test task".into(), baseline)
        .save(&project)
        .unwrap();
    let mut engine = Engine::new(project, cfg.clone());

    engine.step(None, Tick(0)).unwrap();
    let head = manifest(&fx).active().unwrap().id.clone();

    fx.write("a.rs", "fn a() {\n    my_own_way();\n}\n");
    engine
        .step(
            Some(EngineEvent::Changed(Origin::Real, PathBuf::from("a.rs"))),
            Tick(100),
        )
        .unwrap();
    assert!(
        manifest(&fx)
            .find(&head)
            .unwrap()
            .pending_divergence
            .is_none(),
        "not while the file might still be moving"
    );

    engine.step(None, Tick(100 + 2_000)).unwrap();

    let m = manifest(&fx);
    let asked = m
        .questioned()
        .expect("a question, once the file stopped moving");
    let q = asked.pending_divergence.as_ref().unwrap();
    assert_eq!(q.proposed, vec!["    work();"]);
    assert_eq!(q.actual, vec!["    my_own_way();"]);
    assert_eq!(
        asked.status,
        Status::Pending,
        "an unanswered question is not a terminal status"
    );

    // And the queue moved on, which is the half that was never implemented.
    let active = m.active().expect("something else to be getting on with");
    assert_ne!(
        active.id, asked.id,
        "a question must not hold the head of the queue"
    );
    assert_eq!(active.file, "b.rs");
    assert_eq!(
        m.queue_view().last().map(|h| h.id.clone()),
        Some(asked.id.clone()),
        "it sorts last, not out: it is still pending and still answerable"
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

// ------------------------------------------------------------------ curator

/// A `claude` stand-in for the curator that records every invocation.
///
/// `stub_claude` overwrites its argv log each call, which is exactly what a
/// "how many times was this asked?" test must not do.
fn curator_stub(dir: &Path, name: &str, body: &str) -> (PathBuf, PathBuf) {
    let script = dir.join(name);
    let calls = dir.join(format!("{name}.calls"));
    common::cli::write_exec(
        &script,
        &format!(
            "#!/bin/sh\n\
             cat > /dev/null\n\
             echo call >> {calls}\n\
             {body}\n",
            calls = calls.display(),
        ),
    );
    (script, calls)
}

fn call_count(calls: &Path) -> usize {
    std::fs::read_to_string(calls)
        .map(|s| s.lines().count())
        .unwrap_or(0)
}

/// Three hunks, in three files, with a curator that answers `reply`.
///
/// `a.rs` leads on file order, so a curator that puts `c.rs` first has visibly
/// done something. Three rather than two so that typing one still leaves a
/// queue worth curating — at two, `MIN_HUNKS_TO_CURATE` would suppress the
/// second pass by itself and mask whatever the tombstone is doing.
fn three_hunks_with_curator(reply: &str) -> (Fixture, Engine, PathBuf) {
    let fx = Fixture::new();
    fx.write("a.rs", "fn a() {\n}\n");
    fx.write("b.rs", "fn b() {\n}\n");
    fx.write("c.rs", "fn c() {\n}\n");
    fx.commit_all("initial");
    let project = fx.project();
    project.ensure_state_dir().unwrap();

    let (stub, calls) = curator_stub(fx.root.path(), "curator-stub", reply);
    let cfg = Config {
        claude_cmd: vec![stub.to_string_lossy().into_owned()],
        ..Config::default()
    };

    let baseline = shadow::sync(&project, &cfg).unwrap();
    std::fs::write(project.shadow_dir.join("a.rs"), "fn a() {\n    one();\n}\n").unwrap();
    std::fs::write(project.shadow_dir.join("b.rs"), "fn b() {\n    two();\n}\n").unwrap();
    std::fs::write(
        project.shadow_dir.join("c.rs"),
        "fn c() {\n    three();\n}\n",
    )
    .unwrap();
    Manifest::new(&project, "test task".into(), baseline)
        .save(&project)
        .unwrap();

    let engine = Engine::new(project, cfg);
    (fx, engine, calls)
}

/// Keep stepping until the manifest satisfies `done`, or give up loudly.
///
/// A curation runs on its own thread, so there is nothing to join and a fixed
/// sleep would be either slow or flaky.
fn step_until(
    engine: &mut Engine,
    fx: &Fixture,
    what: &str,
    mut done: impl FnMut(&Manifest) -> bool,
) {
    for i in 0..200u64 {
        engine.step(None, Tick(10_000 + i * 50)).unwrap();
        if done(&manifest(fx)) {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
    panic!("timed out waiting for {what}");
}

/// The order of the pending queue, by file.
fn queue_files(m: &Manifest) -> Vec<String> {
    m.queue_view().iter().map(|h| h.file.clone()).collect()
}

const REORDER: &str = concat!(
    r#"printf '%s' '{"order":[{"hunk":3,"note":"needed by the rest"},"#,
    r#"{"hunk":2,"note":"needed by a"},{"hunk":1,"note":"uses b"}]}'"#
);

#[test]
fn the_curator_puts_the_queue_in_the_order_it_chose() {
    let (fx, mut engine, calls) = three_hunks_with_curator(REORDER);

    // Nothing typed and nothing started, so there is no place to lose — the
    // curator's first choice wins outright.
    engine.step(None, Tick(0)).unwrap();
    assert_eq!(
        queue_files(&manifest(&fx)),
        vec!["a.rs", "b.rs", "c.rs"],
        "file order"
    );

    step_until(&mut engine, &fx, "the curation to land", |m| {
        m.hunks.iter().any(|h| h.curator_rank.is_some())
    });

    let m = manifest(&fx);
    assert_eq!(
        queue_files(&m),
        vec!["c.rs", "b.rs", "a.rs"],
        "teaching order"
    );
    let head = m.head_of_queue().unwrap();
    assert_eq!(head.curator_note.as_deref(), Some("needed by the rest"));
    assert_eq!(call_count(&calls), 1, "one pass, not one per step");
}

#[test]
fn a_curation_that_lands_mid_transcription_leaves_the_head_alone() {
    // "Freeze current, reorder ahead." Once you are in a hunk, a curation that
    // arrives must not move it out from under your cursor — only the queue
    // behind it may be reordered.
    let (fx, mut engine, _) = three_hunks_with_curator(REORDER);

    // A shadow write starts the settle clock, so nothing is curated yet.
    engine
        .step(
            Some(EngineEvent::Changed(Origin::Shadow, PathBuf::from("a.rs"))),
            Tick(0),
        )
        .unwrap();
    engine.step(None, Tick(500)).unwrap();
    assert_eq!(queue_files(&manifest(&fx)), vec!["a.rs", "b.rs", "c.rs"]);

    // The user starts typing the head. Half a line is enough to be "in it".
    fx.write("a.rs", "fn a() {\n    on\n}\n");
    step_until(&mut engine, &fx, "the curation to land", |m| {
        m.hunks.iter().any(|h| h.curator_rank.is_some())
    });

    let m = manifest(&fx);
    assert_eq!(
        queue_files(&m),
        vec!["a.rs", "c.rs", "b.rs"],
        "the hunk being typed keeps the head; everything behind it reorders"
    );
    assert_eq!(
        m.queue_view()[1].curator_note.as_deref(),
        Some("needed by the rest")
    );
}

#[test]
fn a_curator_that_cannot_run_leaves_the_deterministic_order_alone() {
    let (fx, mut engine, calls) = three_hunks_with_curator("echo 'no thanks' >&2\nexit 4");

    engine.step(None, Tick(0)).unwrap();
    step_until(&mut engine, &fx, "the failed pass to be recorded", |_| {
        call_count(&calls) >= 1
    });
    // Let the outcome be absorbed.
    for i in 0..10 {
        engine.step(None, Tick(20_000 + i * 50)).unwrap();
    }

    let m = manifest(&fx);
    assert_eq!(
        queue_files(&m),
        vec!["a.rs", "b.rs", "c.rs"],
        "exactly as before"
    );
    assert!(m.hunks.iter().all(|h| h.curator_rank.is_none()));
    assert!(m.hunks.iter().all(|h| h.curator_note.is_none()));
}

#[test]
fn a_curator_that_cannot_run_is_not_asked_again_for_the_same_queue() {
    // Without a tombstone on failure this is one wasted call per keystroke: the
    // uncached set would still be the pending set, and typing shrinks it.
    let (fx, mut engine, calls) = three_hunks_with_curator("exit 4");

    engine.step(None, Tick(0)).unwrap();
    step_until(&mut engine, &fx, "the failed pass", |_| {
        call_count(&calls) >= 1
    });
    for i in 0..10 {
        engine.step(None, Tick(20_000 + i * 50)).unwrap();
    }

    // The part that matters. Typing removes a hunk from `pending`, which
    // *shrinks* the set the trigger fingerprints — so the fingerprint guard
    // alone does not survive it. Only an entry written for every key the failed
    // pass was given does.
    fx.write("a.rs", "fn a() {\n    one();\n}\n");
    engine.step(Some(changed()), Tick(30_000)).unwrap();
    assert_eq!(manifest(&fx).pending().count(), 2, "still worth curating");
    for i in 0..40 {
        engine.step(None, Tick(31_000 + i * 200)).unwrap();
    }

    assert_eq!(call_count(&calls), 1, "asked once, not once per hunk typed");
}

#[test]
fn the_note_the_curator_wrote_survives_typing_next_to_its_hunk() {
    let (fx, mut engine, _) = three_hunks_with_curator(REORDER);
    engine.step(None, Tick(0)).unwrap();
    step_until(&mut engine, &fx, "the curation to land", |m| {
        m.hunks.iter().any(|h| h.curator_note.is_some())
    });

    let before = manifest(&fx);
    let b_id = before
        .hunks
        .iter()
        .find(|h| h.file == "b.rs")
        .unwrap()
        .id
        .clone();

    // Type a.rs. b.rs is in another file, so its id does not move here — what
    // this proves is that ordinary progress does not clear the curation.
    fx.write("a.rs", "fn a() {\n    one();\n}\n");
    engine.step(Some(changed()), Tick(30_000)).unwrap();

    let after = manifest(&fx);
    let b = after.find(&b_id).expect("b.rs still queued");
    assert_eq!(b.curator_note.as_deref(), Some("needed by a"));
    assert_eq!(after.count(Status::Typed), 1, "and a.rs was typed");
}

#[test]
fn a_curation_that_lands_after_the_session_closed_writes_nothing() {
    let (fx, mut engine, calls) = three_hunks_with_curator(REORDER);
    engine.step(None, Tick(0)).unwrap();
    step_until(&mut engine, &fx, "the pass to be launched", |_| {
        call_count(&calls) >= 1
    });

    let project = fx.project();
    std::fs::remove_file(project.session_json()).unwrap();
    let out = engine.step(None, Tick(30_000)).unwrap();

    assert!(
        matches!(out.as_slice(), [state::Event::Closed { .. }]),
        "closing wins over an outcome still in flight: {out:?}"
    );
}

// -------------------------------------------------------------- how it came

/// The manifest's verdict for the one hunk in `a.rs`.
fn arrival(fx: &Fixture) -> rote::hunks::Input {
    manifest(fx).hunks[0].input
}

#[test]
fn a_hunk_typed_a_few_characters_at_a_time_is_recorded_as_typed() {
    // The only thing rote can honestly infer. A partial save proves a human was
    // in the loop; one paste of the whole hunk never produces one.
    let (fx, _cfg, mut engine) = started("fn a() {\n}\n", "fn a() {\n    work();\n}\n");
    engine.step(None, Tick(0)).unwrap();

    fx.write("a.rs", "fn a() {\n    wo\n}\n");
    engine.step(Some(changed()), Tick(100)).unwrap();
    fx.write("a.rs", "fn a() {\n    work();\n}\n");
    engine.step(Some(changed()), Tick(200)).unwrap();

    let m = manifest(&fx);
    assert_eq!(m.count(Status::Typed), 1);
    assert_eq!(m.hunks[0].input, rote::hunks::Input::Typed);
}

#[test]
fn a_hunk_that_arrives_whole_in_one_save_is_left_unknown() {
    // A paste and a careful typist who saves once are byte-identical, so this
    // has to stay `unknown` rather than guess. `unknown` is a real answer.
    let (fx, _cfg, mut engine) = started("fn a() {\n}\n", "fn a() {\n    work();\n}\n");
    engine.step(None, Tick(0)).unwrap();

    fx.write("a.rs", "fn a() {\n    work();\n}\n");
    engine.step(Some(changed()), Tick(100)).unwrap();

    assert_eq!(manifest(&fx).count(Status::Typed), 1);
    assert_eq!(arrival(&fx), rote::hunks::Input::Unknown);
}

#[test]
fn the_engine_never_records_a_hunk_as_pasted() {
    // Whatever the save pattern. Only a front end may say this word.
    let (fx, _cfg, mut engine) = started("fn a() {\n}\n", "fn a() {\n    work();\n}\n");
    engine.step(None, Tick(0)).unwrap();
    for (i, body) in [
        "fn a() {\n    w\n}\n",
        "fn a() {\n    work\n}\n",
        "fn a() {\n    work();\n}\n",
    ]
    .iter()
    .enumerate()
    {
        fx.write("a.rs", body);
        engine
            .step(Some(changed()), Tick(100 + i as u64 * 100))
            .unwrap();
    }
    assert_ne!(arrival(&fx), rote::hunks::Input::Pasted);
}

#[test]
fn an_inference_never_overwrites_a_reported_paste() {
    // You paste a hunk, your plugin says so, then you fix a character in it —
    // which the engine sees as typing. Without the guard in `Input::fill` that
    // correction erases the report.
    let (fx, _cfg, mut engine) = started("fn a() {\n}\n", "fn a() {\n    work();\n}\n");
    engine.step(None, Tick(0)).unwrap();
    let id = manifest(&fx).active().unwrap().id.clone();

    engine
        .step(
            Some(EngineEvent::Command(
                state::Command::Report {
                    hunk_id: id.clone(),
                    input: state::Reported::Pasted,
                }
                .into(),
            )),
            Tick(50),
        )
        .unwrap();
    assert_eq!(arrival(&fx), rote::hunks::Input::Pasted);

    // Now type it, partially and then fully.
    fx.write("a.rs", "fn a() {\n    wo\n}\n");
    engine.step(Some(changed()), Tick(100)).unwrap();
    fx.write("a.rs", "fn a() {\n    work();\n}\n");
    engine.step(Some(changed()), Tick(200)).unwrap();

    assert_eq!(manifest(&fx).count(Status::Typed), 1);
    assert_eq!(
        arrival(&fx),
        rote::hunks::Input::Pasted,
        "the engine fills in unknowns; it does not overrule a front end"
    );
}

#[test]
fn reporting_how_a_hunk_arrived_does_not_move_its_status_or_the_queue() {
    // The constitutional one. DESIGN §1: a front end cannot assert its way to a
    // finished session, and `report` must not be the crack in that.
    let (fx, _cfg, mut engine) = started("fn a() {\n}\n", "fn a() {\n    work();\n}\n");
    engine.step(None, Tick(0)).unwrap();
    let before = manifest(&fx);
    let id = before.active().unwrap().id.clone();

    engine
        .step(
            Some(EngineEvent::Command(
                state::Command::Report {
                    hunk_id: id.clone(),
                    input: state::Reported::Typed,
                }
                .into(),
            )),
            Tick(50),
        )
        .unwrap();

    let after = manifest(&fx);
    assert_eq!(after.hunks[0].status, Status::Pending, "still to be typed");
    assert_eq!(after.pending().count(), before.pending().count());
    assert_eq!(after.active().unwrap().id, id, "the queue did not move");
    assert_eq!(after.hunks[0].input, rote::hunks::Input::Typed);
}

#[test]
fn a_repeated_report_does_not_move_the_generation() {
    let (fx, _cfg, mut engine) = started("fn a() {\n}\n", "fn a() {\n    work();\n}\n");
    engine.step(None, Tick(0)).unwrap();
    let id = manifest(&fx).active().unwrap().id.clone();

    let report = |engine: &mut Engine, t: u64| {
        engine
            .step(
                Some(EngineEvent::Command(
                    state::Command::Report {
                        hunk_id: id.clone(),
                        input: state::Reported::Pasted,
                    }
                    .into(),
                )),
                Tick(t),
            )
            .unwrap();
    };

    report(&mut engine, 50);
    let first = manifest(&fx).generation;
    report(&mut engine, 60);
    assert_eq!(
        manifest(&fx).generation,
        first,
        "nothing changed, so no subscriber's snapshot is invalidated"
    );
}

#[test]
fn a_report_for_a_hunk_that_does_not_exist_is_refused() {
    let (fx, _cfg, mut engine) = started("fn a() {\n}\n", "fn a() {\n    work();\n}\n");
    engine.step(None, Tick(0)).unwrap();
    let before = manifest(&fx).generation;

    let mut notices = Vec::new();
    let outcome = engine
        .apply(
            &state::Request {
                wire_version: state::WIRE_VERSION,
                generation: None,
                command: state::Command::Report {
                    hunk_id: "h-nope".into(),
                    input: state::Reported::Pasted,
                },
            },
            Tick(50),
            &mut notices,
        )
        .unwrap();

    assert!(matches!(outcome, state::Outcome::Rejected { .. }));
    assert_eq!(manifest(&fx).generation, before);
}

/// Send one command at a chosen generation and report the outcome.
fn apply_at(
    engine: &mut Engine,
    generation: Option<u64>,
    command: state::Command,
) -> state::Outcome {
    let mut notices = Vec::new();
    engine
        .apply(
            &state::Request {
                wire_version: state::WIRE_VERSION,
                generation,
                command,
            },
            Tick(50),
            &mut notices,
        )
        .unwrap()
}

#[test]
fn a_show_with_a_stale_generation_is_refused() {
    // It returned before reaching `mutate`, where the check lives, so a client
    // that had fallen behind was told its verb landed. §12 says the generation is
    // checked whenever it is present — writing nothing is not an exemption.
    let (fx, _cfg, mut engine) = started("fn a() {\n}\n", "fn a() {\n    work();\n}\n");
    engine.step(None, Tick(0)).unwrap();
    let current = manifest(&fx).generation;

    let outcome = apply_at(
        &mut engine,
        Some(current.saturating_sub(1)),
        state::Command::Show { hunk_id: None },
    );

    assert!(
        matches!(outcome, state::Outcome::Stale { current: c } if c == current),
        "got {outcome:?}"
    );
    assert_eq!(manifest(&fx).generation, current, "and nothing was written");
}

#[test]
fn a_show_at_the_current_generation_is_applied_and_writes_nothing() {
    let (fx, _cfg, mut engine) = started("fn a() {\n}\n", "fn a() {\n    work();\n}\n");
    engine.step(None, Tick(0)).unwrap();
    let current = manifest(&fx).generation;

    let outcome = apply_at(
        &mut engine,
        Some(current),
        state::Command::Show { hunk_id: None },
    );

    assert!(
        matches!(outcome, state::Outcome::Applied),
        "got {outcome:?}"
    );
    assert_eq!(
        manifest(&fx).generation,
        current,
        "no id means no state to move"
    );
}

#[test]
fn a_refresh_is_applied_however_stale_the_client_is() {
    // The deliberate exception, pinned so it cannot be "fixed" into a refusal.
    // The pane attaches its generation to every command and `g` maps to refresh,
    // so checking it would refuse the client that has fallen behind the one verb
    // that recovers from that.
    let (fx, _cfg, mut engine) = started("fn a() {\n}\n", "fn a() {\n    work();\n}\n");
    engine.step(None, Tick(0)).unwrap();
    let current = manifest(&fx).generation;

    let outcome = apply_at(&mut engine, Some(0), state::Command::Refresh);

    assert!(
        matches!(outcome, state::Outcome::Applied),
        "got {outcome:?}"
    );
    assert_eq!(manifest(&fx).generation, current);
}
