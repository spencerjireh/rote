//! M4 acceptance: the transcription loop, driven end to end through the CLI
//! with a scripted editor standing in for the user's keystrokes.

mod common;

use common::cli::{scripted_editor, stdout, Cli};
use common::Fixture;

/// A started session whose shadow already holds the agent's edit.
fn session_with_agent_edit() -> Cli {
    let fx = Fixture::new();
    fx.write("a.rs", "fn a() {\n}\n");
    fx.commit_all("initial");
    let cli = Cli::with_fixture(fx);
    let out = cli.run(&["start", "--no-launch", "add work"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    std::fs::write(cli.shadow().join("a.rs"), "fn a() {\n    work();\n}\n").unwrap();
    cli
}

#[test]
fn a_correctly_typed_hunk_drives_the_queue_to_empty() {
    let cli = session_with_agent_edit();
    let editor = scripted_editor(
        cli.fx.root.path(),
        "type-it.sh",
        "fn a() {\n    work();\n}\n",
    );
    let cli = cli.with_editor(editor);

    let out = cli.run(&["next"]);
    let text = stdout(&out);
    assert!(text.contains("hunk 1/1"), "{text}");
    assert!(text.contains("+     work();"), "{text}");
    assert!(text.contains("typed — a.rs"), "{text}");

    // The real tree now holds the change, so the queue is empty.
    assert_eq!(cli.fx.read("a.rs"), "fn a() {\n    work();\n}\n");
    let again = stdout(&cli.run(&["next"]));
    assert!(again.contains("nothing to transcribe"), "{again}");
    assert!(again.contains("rote done"), "{again}");
}

#[test]
fn an_untouched_file_leaves_the_hunk_pending() {
    let cli = session_with_agent_edit();
    // An editor that changes nothing at all.
    let editor = scripted_editor(cli.fx.root.path(), "noop.sh", "fn a() {\n}\n");
    let cli = cli.with_editor(editor);

    let text = stdout(&cli.run(&["next"]));
    assert!(text.contains("untouched — a.rs"), "{text}");
    assert!(text.contains("run `rote next` to try again"), "{text}");

    // Still queued.
    let status = stdout(&cli.run(&["status"]));
    assert!(status.contains("1 pending"), "{status}");
}

#[test]
fn a_variant_triggers_the_divergence_prompt_and_records_both_versions() {
    let cli = session_with_agent_edit();
    let editor = scripted_editor(
        cli.fx.root.path(),
        "variant.sh",
        "fn a() {\n    work_my_way();\n}\n",
    );
    let cli = cli.with_editor(editor);

    // "k" keeps the user's version.
    let out = cli.run_with_input(&["next"], "k\n");
    let text = stdout(&out);
    assert!(
        text.contains("your version differs from the proposal"),
        "{text}"
    );
    assert!(text.contains("proposal │     work();"), "{text}");
    assert!(text.contains("yours    │     work_my_way();"), "{text}");
    assert!(text.contains("diverged — a.rs"), "{text}");

    // Both versions are stored on the hunk.
    let manifest = std::fs::read_to_string(cli.fx.project().session_json()).unwrap();
    assert!(manifest.contains("\"status\": \"diverged\""), "{manifest}");
    assert!(manifest.contains("work_my_way()"), "{manifest}");
    assert!(manifest.contains("\"proposed\""), "{manifest}");
}

#[test]
fn a_kept_divergence_does_not_reappear() {
    // The stickiness rule, observed through the CLI rather than the unit tests.
    let cli = session_with_agent_edit();
    let editor = scripted_editor(
        cli.fx.root.path(),
        "variant.sh",
        "fn a() {\n    mine();\n}\n",
    );
    let cli = cli.with_editor(editor);
    cli.run_with_input(&["next"], "k\n");

    for round in 0..3 {
        let text = stdout(&cli.run(&["next"]));
        assert!(
            text.contains("nothing to transcribe"),
            "round {round}: a kept divergence must not come back: {text}"
        );
    }
}

#[test]
fn retry_reopens_the_editor_and_can_succeed() {
    let cli = session_with_agent_edit();
    // A script that produces the wrong text first, then the right text.
    let marker = cli.fx.root.path().join("attempted");
    let script = cli.fx.root.path().join("two-pass.sh");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\n\
             for a in \"$@\"; do f=\"$a\"; done\n\
             if [ -f {m} ]; then\n\
               printf 'fn a() {{\\n    work();\\n}}\\n' > \"$f\"\n\
             else\n\
               touch {m}\n\
               printf 'fn a() {{\\n    wrong();\\n}}\\n' > \"$f\"\n\
             fi\n",
            m = marker.display()
        ),
    )
    .unwrap();
    let mut perms = std::fs::metadata(&script).unwrap().permissions();
    std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o755);
    std::fs::set_permissions(&script, perms).unwrap();
    let cli = cli.with_editor(script);

    // "r" retries; the second pass types it correctly.
    let text = stdout(&cli.run_with_input(&["next"], "r\n"));
    assert!(text.contains("typed — a.rs"), "{text}");
    assert_eq!(cli.fx.read("a.rs"), "fn a() {\n    work();\n}\n");
}

#[test]
fn skip_is_durable_across_repeated_next_calls() {
    let cli = session_with_agent_edit();

    let text = stdout(&cli.run(&["skip"]));
    assert!(text.contains("skipped — a.rs"), "{text}");
    assert!(text.contains("will not come back"), "{text}");

    for round in 0..3 {
        let out = stdout(&cli.run(&["next"]));
        assert!(
            out.contains("nothing to transcribe"),
            "round {round}: skip must be durable: {out}"
        );
    }
    let status = stdout(&cli.run(&["status"]));
    assert!(status.contains("1 skipped"), "{status}");
}

#[test]
fn a_session_holding_a_skip_and_a_divergence_still_terminates() {
    let fx = Fixture::new();
    fx.write(
        "a.rs",
        "one\ntwo\nthree\nfour\nfive\nsix\nseven\neight\nnine\n",
    );
    fx.commit_all("initial");
    let cli = Cli::with_fixture(fx);
    cli.run(&["start", "--no-launch", "two edits"]);
    // Two edits far enough apart to be separate hunks.
    std::fs::write(
        cli.shadow().join("a.rs"),
        "ONE\ntwo\nthree\nfour\nfive\nsix\nseven\neight\nNINE\n",
    )
    .unwrap();

    // Skip the first, diverge on the second.
    let skipped = stdout(&cli.run(&["skip"]));
    assert!(skipped.contains("skipped"), "{skipped}");

    let editor = scripted_editor(
        cli.fx.root.path(),
        "mine.sh",
        "one\ntwo\nthree\nfour\nfive\nsix\nseven\neight\nMY_NINE\n",
    );
    let cli = cli.with_editor(editor);
    let diverged = stdout(&cli.run_with_input(&["next"], "k\n"));
    assert!(diverged.contains("diverged"), "{diverged}");

    // The queue is now empty and stays empty — no looping.
    for round in 0..3 {
        let out = stdout(&cli.run(&["next"]));
        assert!(
            out.contains("nothing to transcribe"),
            "round {round}: {out}"
        );
    }
    let status = stdout(&cli.run(&["status"]));
    assert!(status.contains("0 pending"), "{status}");
    assert!(status.contains("1 diverged"), "{status}");
    assert!(status.contains("1 skipped"), "{status}");
}

#[test]
fn back_re_prints_without_changing_anything() {
    let cli = session_with_agent_edit();
    let editor = scripted_editor(cli.fx.root.path(), "type.sh", "fn a() {\n    work();\n}\n");
    let cli = cli.with_editor(editor);
    cli.run(&["next"]);

    let before = std::fs::read_to_string(cli.fx.project().session_json()).unwrap();
    let text = stdout(&cli.run(&["back"]));
    let after = std::fs::read_to_string(cli.fx.project().session_json()).unwrap();

    assert!(text.contains("status: typed"), "{text}");
    assert!(text.contains("display only"), "{text}");
    assert_eq!(before, after, "back must not modify the manifest");
}

#[test]
fn back_before_anything_was_presented_says_so() {
    let cli = session_with_agent_edit();
    let text = stdout(&cli.run(&["back"]));
    assert!(text.contains("nothing presented yet"), "{text}");
}

#[test]
fn next_json_emits_one_hunk_and_classifies_nothing() {
    let cli = session_with_agent_edit();
    let out = cli.run(&["next", "--json"]);
    let text = stdout(&out);
    let line = text.lines().next().expect("one JSON line");

    let hunk: serde_json::Value = serde_json::from_str(line).expect("valid JSON");
    assert_eq!(hunk["file"], "a.rs");
    assert_eq!(hunk["status"], "pending");
    assert_eq!(hunk["op"], "insert");
    assert_eq!(hunk["new_lines"][0], "    work();");
    assert!(hunk["id"].as_str().unwrap().starts_with("h-"));

    // The real tree is untouched and the hunk is still pending.
    assert_eq!(cli.fx.read("a.rs"), "fn a() {\n}\n");
    let status = stdout(&cli.run(&["status"]));
    assert!(status.contains("1 pending"), "{status}");
}

#[test]
fn there_is_no_verb_that_marks_a_hunk_typed() {
    // `mark` is gone, and nothing replaces it. `Typed` is producible only by the
    // classifier, whose input is the hunk's own `new_lines` — which came from
    // the shadow. A front end cannot assert its way to a finished session.
    let cli = session_with_agent_edit();
    let out = cli.run(&["mark", "h-whatever", "typed"]);
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("unrecognized subcommand"), "{err}");
}

#[test]
fn skip_takes_a_hunk_id_and_skips_that_one() {
    // Addressed by id, not by position: the head of the queue can move between
    // the moment a front end renders a hunk and the moment the user skips it.
    let cli = session_with_agent_edit();
    cli.fx.write("b.rs", "fn b() {\n}\n");
    cli.fx.commit_all("second file");
    std::fs::write(cli.shadow().join("b.rs"), "fn b() {\n    more();\n}\n").unwrap();

    // Build the queue and take the *second* hunk's id.
    cli.run(&["next", "--json"]);
    let status = stdout(&cli.run(&["status"]));
    assert!(status.contains("2 pending"), "{status}");

    let manifest: serde_json::Value =
        serde_json::from_slice(&std::fs::read(cli.fx.project().session_json()).unwrap()).unwrap();
    let hunks = manifest["hunks"].as_array().unwrap();
    let b_id = hunks
        .iter()
        .find(|h| h["file"] == "b.rs")
        .expect("a hunk for b.rs")["id"]
        .as_str()
        .unwrap()
        .to_string();

    let out = cli.run(&["skip", &b_id]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(stdout(&out).contains("skipped — b.rs"), "{}", stdout(&out));

    // a.rs is untouched and still pending; only the named hunk was skipped.
    let status = stdout(&cli.run(&["status"]));
    assert!(status.contains("1 pending"), "{status}");
    assert!(status.contains("1 skipped"), "{status}");

    // An unknown id is refused rather than silently skipping the head.
    let bad = cli.run(&["skip", "h-nosuchhunk"]);
    assert!(!bad.status.success());
    assert!(String::from_utf8_lossy(&bad.stderr).contains("no hunk h-nosuchhunk"));
}

#[test]
fn a_generated_file_is_gated_on_bytes_not_typing() {
    let fx = Fixture::new();
    fx.write("Cargo.lock", "version = 3\n");
    fx.commit_all("initial");
    let cli = Cli::with_fixture(fx);
    cli.run(&["start", "--no-launch", "add a dep"]);
    std::fs::write(
        cli.shadow().join("Cargo.lock"),
        "version = 3\n\n[[package]]\nname = \"serde\"\n",
    )
    .unwrap();

    // No editor is launched, and the hunk stays pending while the files differ.
    let text = stdout(&cli.run(&["next"]));
    assert!(text.contains("generated file"), "{text}");
    assert!(text.contains("still differs from the shadow"), "{text}");
    assert!(
        !text.contains("opening"),
        "no editor for a lockfile: {text}"
    );

    // Once the user runs the real generating command the files match, so the
    // hunk leaves the queue the same way any typed hunk does — by no longer
    // existing in the diff. The gate's job is holding it pending until then.
    cli.fx.write(
        "Cargo.lock",
        "version = 3\n\n[[package]]\nname = \"serde\"\n",
    );
    let done = stdout(&cli.run(&["next"]));
    assert!(done.contains("nothing to transcribe"), "{done}");
}

#[test]
fn an_editor_that_exits_nonzero_is_treated_as_untouched() {
    let cli = session_with_agent_edit();
    let script = cli.fx.root.path().join("fail.sh");
    std::fs::write(&script, "#!/bin/sh\nexit 1\n").unwrap();
    let mut perms = std::fs::metadata(&script).unwrap().permissions();
    std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o755);
    std::fs::set_permissions(&script, perms).unwrap();
    let cli = cli.with_editor(script);

    let text = stdout(&cli.run(&["next"]));
    assert!(text.contains("untouched"), "{text}");
    assert_eq!(cli.fx.read("a.rs"), "fn a() {\n}\n", "real tree untouched");
}
