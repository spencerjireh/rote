//! M5 acceptance: the full lifecycle with a stubbed `claude` and a scripted
//! editor — init → start → agent edits → next×N → done.

mod common;

use common::cli::{stderr, stdout, stub_claude, Cli};
use common::Fixture;

#[test]
fn the_full_lifecycle_ends_idle_with_shadow_matching_real() {
    let fx = Fixture::new();
    fx.write("a.rs", "fn a() {\n}\n");
    fx.commit_all("initial");
    let cli = Cli::with_fixture(fx);

    let (stub, payload_path) =
        stub_claude(cli.fx.root.path(), "claude-stub", "no issues found.", 0);
    cli.use_claude_stub(&stub);
    cli.write_project_config("[checks]\ncommands = [\"true\"]\n");

    // init -> start -> the agent edits the shadow
    assert!(cli.run(&["init", "--force"]).status.success());
    cli.write_project_config("[checks]\ncommands = [\"true\"]\n");
    let start = cli.run(&["start", "--no-launch", "add work"]);
    assert!(start.status.success(), "{}", stderr(&start));
    std::fs::write(cli.shadow().join("a.rs"), "fn a() {\n    work();\n}\n").unwrap();

    // The user types it. No editor launch and no command: `rote watch` sees the
    // write and classifies it, exiting once the queue drains.
    let watcher = cli.spawn(&[
        "watch",
        "--headless",
        "--exit-when-empty",
        "--timeout",
        "20000",
    ]);
    std::thread::sleep(std::time::Duration::from_millis(600));
    cli.fx.write("a.rs", "fn a() {\n    work();\n}\n");
    let watched = watcher.wait_with_output().unwrap();
    assert!(
        watched.status.success(),
        "watch did not drain the queue: {}",
        String::from_utf8_lossy(&watched.stderr)
    );

    // done
    let done = cli.run_with_input(&["done"], "y\n");
    let text = stdout(&done);
    assert!(done.status.success(), "{}", stderr(&done));
    assert!(
        text.contains("check: true"),
        "checks run in the real tree: {text}"
    );
    assert!(text.contains("Reviewer findings"), "{text}");
    assert!(text.contains("no issues found."), "{text}");
    assert!(text.contains("session closed. state: idle"), "{text}");

    // The session is gone and the shadow matches the real tree again.
    let project = cli.fx.project();
    assert!(!project.session_json().exists());
    assert_eq!(
        std::fs::read_to_string(project.shadow_dir.join("a.rs")).unwrap(),
        "fn a() {\n    work();\n}\n"
    );

    // Both archive files are present.
    let entries: Vec<String> = std::fs::read_dir(project.archive_dir())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(entries.len(), 2, "manifest + patch: {entries:?}");
    assert!(entries.iter().any(|e| e.ends_with(".json")));
    assert!(entries.iter().any(|e| e.ends_with(".patch")));

    // The reviewer received the documented payload.
    let payload = std::fs::read_to_string(&payload_path).unwrap();
    assert!(payload.contains("== TASK ==\nadd work"), "{payload}");
    assert!(
        payload.contains("== SESSION DIFF (baseline → current working tree) =="),
        "{payload}"
    );
    assert!(
        payload.contains("== MANUAL DIVERGENCES (proposal vs. what was typed) =="),
        "{payload}"
    );
    assert!(payload.contains("== SKIPPED HUNKS ==\n(none)"), "{payload}");
    assert!(
        payload.contains("+    work();"),
        "the session diff carries the change: {payload}"
    );
}

#[test]
fn the_reviewer_is_invoked_with_tool_restriction_and_the_prompt() {
    let fx = Fixture::new();
    fx.write("a.rs", "fn a() {\n}\n");
    fx.commit_all("initial");
    let cli = Cli::with_fixture(fx);
    let (stub, _) = stub_claude(cli.fx.root.path(), "claude-stub", "ok", 0);
    cli.use_claude_stub(&stub);

    cli.run(&["start", "--no-launch", "t"]);
    std::fs::write(cli.shadow().join("a.rs"), "fn a() {\n    work();\n}\n").unwrap();
    cli.run(&["skip"]);
    cli.run_with_input(&["done", "--no-checks"], "y\n");

    let args = std::fs::read_to_string(cli.fx.root.path().join("claude-stub.args")).unwrap();
    assert!(args.contains("-p"), "{args}");
    assert!(
        args.contains("typed in manually"),
        "the review prompt: {args}"
    );
    assert!(args.contains("--tools"), "tool restriction: {args}");
}

#[test]
fn a_failing_check_stops_the_pipeline_and_keeps_the_session() {
    let fx = Fixture::new();
    fx.write("a.rs", "fn a() {\n}\n");
    fx.commit_all("initial");
    let cli = Cli::with_fixture(fx);
    let (stub, _) = stub_claude(cli.fx.root.path(), "claude-stub", "ok", 0);
    cli.use_claude_stub(&stub);

    cli.run(&["start", "--no-launch", "t"]);
    std::fs::write(cli.shadow().join("a.rs"), "fn a() {\n    work();\n}\n").unwrap();
    cli.write_project_config("[checks]\ncommands = [\"false\"]\n");
    cli.run(&["skip"]);

    let done = cli.run_with_input(&["done"], "y\n");
    assert!(!done.status.success());
    assert!(stderr(&done).contains("check failed"), "{}", stderr(&done));
    assert!(
        stderr(&done).contains("--no-checks"),
        "names the escape: {}",
        stderr(&done)
    );

    // The session survives so the user can fix and retry.
    assert!(cli.fx.project().session_json().exists());
}

#[test]
fn a_broken_reviewer_does_not_block_closing() {
    let fx = Fixture::new();
    fx.write("a.rs", "fn a() {\n}\n");
    fx.commit_all("initial");
    let cli = Cli::with_fixture(fx);
    // A stub that fails outright.
    let (stub, _) = stub_claude(cli.fx.root.path(), "broken", "", 3);
    cli.use_claude_stub(&stub);

    cli.run(&["start", "--no-launch", "t"]);
    std::fs::write(cli.shadow().join("a.rs"), "fn a() {\n    work();\n}\n").unwrap();
    cli.run(&["skip"]);

    let done = cli.run_with_input(&["done", "--no-checks"], "y\n");
    assert!(
        done.status.success(),
        "review is advisory: {}",
        stderr(&done)
    );
    assert!(
        stderr(&done).contains("reviewer did not run"),
        "{}",
        stderr(&done)
    );
    assert!(
        stdout(&done).contains("session closed"),
        "{}",
        stdout(&done)
    );
}

#[test]
fn pending_hunks_require_consent_and_become_skipped() {
    let fx = Fixture::new();
    fx.write("a.rs", "fn a() {\n}\n");
    fx.commit_all("initial");
    let cli = Cli::with_fixture(fx);
    let (stub, _) = stub_claude(cli.fx.root.path(), "claude-stub", "ok", 0);
    cli.use_claude_stub(&stub);

    cli.run(&["start", "--no-launch", "t"]);
    std::fs::write(cli.shadow().join("a.rs"), "fn a() {\n    work();\n}\n").unwrap();
    cli.run(&["next", "--json"]); // builds the queue without classifying

    // Declining leaves the session open.
    let declined = cli.run_with_input(&["done", "--no-checks", "--no-review"], "n\n");
    assert!(
        stdout(&declined).contains("still pending"),
        "{}",
        stdout(&declined)
    );
    assert!(
        stdout(&declined).contains("left the session open"),
        "{}",
        stdout(&declined)
    );
    assert!(cli.fx.project().session_json().exists());

    // Consenting closes it, marking them skipped.
    let closed = cli.run_with_input(&["done", "--no-checks", "--no-review"], "y\ny\n");
    assert!(
        stdout(&closed).contains("session closed"),
        "{}",
        stdout(&closed)
    );
    assert!(!cli.fx.project().session_json().exists());
}

/// Start a session with one pending hunk and a curation already recorded for it.
fn session_with_a_curation(cli: &Cli) -> rote::curator::Cache {
    let (stub, _) = stub_claude(cli.fx.root.path(), "claude-stub", "ok", 0);
    cli.use_claude_stub(&stub);
    cli.run(&["start", "--no-launch", "t"]);
    std::fs::write(cli.shadow().join("a.rs"), "fn a() {\n    work();\n}\n").unwrap();
    cli.run(&["next", "--json"]); // builds the queue

    let project = cli.fx.project();
    let manifest = rote::session::Manifest::load(&project).unwrap().unwrap();
    let key = manifest.head_of_queue().unwrap().key.clone();

    let mut cache = rote::curator::Cache::new(manifest.session_stamp());
    cache.entries.insert(
        key,
        rote::curator::Entry {
            rank: Some(1),
            note: Some("the thing everything else needs".into()),
        },
    );
    cache.save(&project).unwrap();
    cache
}

#[test]
fn closing_a_session_files_the_curator_cache_with_the_archive() {
    let fx = Fixture::new();
    fx.write("a.rs", "fn a() {\n}\n");
    fx.commit_all("initial");
    let cli = Cli::with_fixture(fx);
    session_with_a_curation(&cli);

    let closed = cli.run_with_input(&["done", "--no-checks", "--no-review"], "y\ny\n");
    assert!(closed.status.success(), "{}", stderr(&closed));

    let project = cli.fx.project();
    assert!(
        !project.curator_json().exists(),
        "the live curation goes with the session"
    );
    let entries: Vec<String> = std::fs::read_dir(project.archive_dir())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert!(
        entries.iter().any(|e| e.ends_with(".curator.json")),
        "and it is filed beside the manifest: {entries:?}"
    );
}

#[test]
fn a_curator_cache_left_by_an_aborted_session_is_not_believed_by_the_next_one() {
    // The silent-wrong-order case. `abort` reaps the daemon best-effort, and a
    // `rote watch --local` pane has no daemon.json to reap — so its engine can
    // write the file again after the teardown removed it. If the next session
    // trusted that, it would inherit a teaching order built for a different set
    // of hunks and never re-curate, because every key would look considered.
    let fx = Fixture::new();
    fx.write("a.rs", "fn a() {\n}\n");
    fx.commit_all("initial");
    let cli = Cli::with_fixture(fx);
    let stale = session_with_a_curation(&cli);

    let out = cli.run_with_input(&["abort"], "y\n");
    assert!(out.status.success(), "{}", stderr(&out));

    cli.run(&["start", "--no-launch", "second"]);
    std::fs::write(cli.shadow().join("a.rs"), "fn a() {\n    work();\n}\n").unwrap();

    // Only now does the pane that outlived the teardown get around to writing.
    // Ordering matters: `start` deletes the file, so a write *before* it proves
    // nothing — this has to land inside the new session's lifetime, which is
    // exactly what a detached process does.
    let project = cli.fx.project();
    stale.save(&project).unwrap();
    cli.run(&["next", "--json"]);

    let manifest = rote::session::Manifest::load(&project).unwrap().unwrap();
    let head = manifest.head_of_queue().unwrap();
    assert_eq!(
        head.curator_note, None,
        "same hunk, same key — and still no note, because the stamp does not match"
    );
    assert_eq!(head.curator_rank, None);
}

#[test]
fn the_residue_patch_holds_what_was_skipped() {
    let fx = Fixture::new();
    fx.write("a.rs", "fn a() {\n}\n");
    fx.commit_all("initial");
    let cli = Cli::with_fixture(fx);
    let (stub, _) = stub_claude(cli.fx.root.path(), "claude-stub", "ok", 0);
    cli.use_claude_stub(&stub);

    cli.run(&["start", "--no-launch", "t"]);
    std::fs::write(
        cli.shadow().join("a.rs"),
        "fn a() {\n    skipped_work();\n}\n",
    )
    .unwrap();
    cli.run(&["skip"]);
    let done = cli.run_with_input(&["done", "--no-checks", "--no-review"], "y\n");
    assert!(done.status.success(), "{}", stderr(&done));

    let project = cli.fx.project();
    let patch_path = std::fs::read_dir(project.archive_dir())
        .unwrap()
        .map(|e| e.unwrap().path())
        .find(|p| p.extension().is_some_and(|e| e == "patch"))
        .expect("a residue patch");
    let patch = std::fs::read_to_string(&patch_path).unwrap();

    assert!(
        patch.contains("skipped_work()"),
        "the skipped change is recoverable: {patch}"
    );
    assert!(
        patch.contains("a/a.rs"),
        "repo-relative so it can be applied: {patch}"
    );
    assert!(
        !patch.contains("/xdg-cache/"),
        "no absolute shadow paths: {patch}"
    );
}

#[test]
fn an_empty_session_closes_without_checks_or_review() {
    let fx = Fixture::new();
    fx.write("a.rs", "fn a() {\n}\n");
    fx.commit_all("initial");
    let cli = Cli::with_fixture(fx);
    // A stub that would fail loudly if it were ever called.
    let (stub, _) = stub_claude(cli.fx.root.path(), "unused", "", 9);
    cli.use_claude_stub(&stub);
    cli.write_project_config("[checks]\ncommands = [\"false\"]\n");

    cli.run(&["start", "--no-launch", "nothing happens"]);
    // The agent changed nothing at all.
    let done = cli.run_with_input(&["done"], "y\n");

    assert!(done.status.success(), "{}", stderr(&done));
    assert!(
        stdout(&done).contains("no changes this session"),
        "{}",
        stdout(&done)
    );
    assert!(
        stdout(&done).contains("session closed"),
        "{}",
        stdout(&done)
    );
}

#[test]
fn done_refuses_while_the_repo_is_mid_merge() {
    let fx = Fixture::new();
    fx.write("f.txt", "base\n");
    fx.commit_all("base");
    fx.git(&["checkout", "--quiet", "-b", "other"]);
    fx.write("f.txt", "other\n");
    fx.commit_all("other side");
    fx.git(&["checkout", "--quiet", "main"]);
    fx.write("f.txt", "main\n");
    fx.commit_all("main side");

    let cli = Cli::with_fixture(fx);
    cli.run(&["start", "--no-launch", "t"]);

    let (ok, _) = common::git_allow_fail(&cli.fx.repo, &["merge", "other"]);
    assert!(!ok, "the fixture merge should conflict");

    let done = cli.run_with_input(&["done"], "y\n");
    assert!(!done.status.success());
    assert!(
        stderr(&done).contains("middle of a merge"),
        "{}",
        stderr(&done)
    );
    // Refused before doing anything, so the session is intact.
    assert!(cli.fx.project().session_json().exists());
}

#[test]
fn talk_prints_the_shadow_path() {
    let fx = Fixture::new();
    fx.write("a.rs", "x\n");
    fx.commit_all("initial");
    let cli = Cli::with_fixture(fx);
    cli.run(&["start", "--no-launch", "t"]);

    let out = cli.run(&["talk"]);
    assert!(out.status.success());
    assert_eq!(
        stdout(&out).trim(),
        cli.shadow().to_string_lossy(),
        "talk without --attach just names the shadow"
    );
}
