//! The transcription verbs, driven through the CLI.
//!
//! There is no scripted editor here any more. Typing is what an editor does to
//! a file, so these tests write the file — which is both simpler and closer to
//! what actually happens. The loop itself (watch, classify, advance) is covered
//! in `tests/watch.rs`; this file covers the commands around it.

mod common;

use common::cli::{stdout, Cli};
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
fn next_prints_the_active_hunk_and_a_jump_target() {
    let cli = session_with_agent_edit();

    let text = stdout(&cli.run(&["next"]));
    assert!(text.contains("hunk 1/1"), "{text}");
    assert!(text.contains("+     work();"), "{text}");
    assert!(text.contains("a.rs:2"), "the jump target: {text}");

    // A printer, not a step: nothing was classified and nothing was launched.
    assert_eq!(cli.fx.read("a.rs"), "fn a() {\n}\n");
    assert!(!text.contains("opening"), "no editor is launched: {text}");
    let status = stdout(&cli.run(&["status"]));
    assert!(status.contains("1 pending"), "{status}");
}

#[test]
fn typing_the_hunk_retires_it_from_the_queue() {
    // The self-truing property, seen through the CLI: recompute reads the trees
    // and a region that now matches the shadow simply is not a hunk any more.
    //
    // Note what this does *not* do. With no `rote watch` running, nothing
    // classified the write, so the hunk is dropped rather than recorded as
    // typed — the session keeps no history of it. Recording is the watcher's
    // job, which is the whole reason it exists.
    let cli = session_with_agent_edit();
    cli.run(&["next"]);

    cli.fx.write("a.rs", "fn a() {\n    work();\n}\n");

    let again = stdout(&cli.run(&["next"]));
    assert!(again.contains("nothing to transcribe"), "{again}");
    let status = stdout(&cli.run(&["status"]));
    assert!(status.contains("0 pending"), "{status}");
    assert!(
        status.contains("0 typed"),
        "unrecorded, not typed: {status}"
    );
}

#[test]
fn an_untyped_hunk_stays_in_the_queue() {
    let cli = session_with_agent_edit();
    for round in 0..3 {
        let text = stdout(&cli.run(&["next"]));
        assert!(text.contains("+     work();"), "round {round}: {text}");
    }
    let status = stdout(&cli.run(&["status"]));
    assert!(status.contains("1 pending"), "{status}");
}

#[test]
fn skip_is_durable() {
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
fn skip_takes_a_hunk_id_and_skips_that_one() {
    // Addressed by id, not by position: the head of the queue can move between
    // the moment a front end renders a hunk and the moment the user skips it.
    let cli = session_with_agent_edit();
    cli.fx.write("b.rs", "fn b() {\n}\n");
    cli.fx.commit_all("second file");
    std::fs::write(cli.shadow().join("b.rs"), "fn b() {\n    more();\n}\n").unwrap();

    cli.run(&["next", "--json"]);
    let status = stdout(&cli.run(&["status"]));
    assert!(status.contains("2 pending"), "{status}");

    let manifest: serde_json::Value =
        serde_json::from_slice(&std::fs::read(cli.fx.project().session_json()).unwrap()).unwrap();
    let b_id = manifest["hunks"]
        .as_array()
        .unwrap()
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

    let status = stdout(&cli.run(&["status"]));
    assert!(status.contains("1 pending"), "{status}");
    assert!(status.contains("1 skipped"), "{status}");

    // An unknown id is refused rather than silently skipping the head.
    let bad = cli.run(&["skip", "h-nosuchhunk"]);
    assert!(!bad.status.success());
    assert!(String::from_utf8_lossy(&bad.stderr).contains("no hunk h-nosuchhunk"));
}

#[test]
fn status_lists_each_file_once_however_the_queue_grew() {
    // `dedup` alone only drops *adjacent* repeats, and `manifest.hunks` is
    // storage order: reconcile appends a re-identified hunk to the tail, so a
    // file with two hunks can end up on both sides of another file's.
    let fx = Fixture::new();
    let mut a = String::from("fn one() {\n}\n");
    for i in 0..9 {
        a.push_str(&format!("// {i}\n"));
    }
    a.push_str("fn two() {\n}\n");
    fx.write("a.rs", &a);
    fx.write("b.rs", "fn b() {\n}\n");
    fx.commit_all("initial");
    let cli = Cli::with_fixture(fx);
    cli.run(&["start", "--no-launch", "two files"]);

    // Two distant hunks in a.rs, one in b.rs.
    let shadow_a = a
        .replace("fn one() {\n}", "fn one() {\n    one_work();\n}")
        .replace("fn two() {\n}", "fn two() {\n    two_work();\n}");
    std::fs::write(cli.shadow().join("a.rs"), &shadow_a).unwrap();
    std::fs::write(cli.shadow().join("b.rs"), "fn b() {\n    more();\n}\n").unwrap();
    cli.run(&["next", "--json"]);

    // Type something in a.rs's first region that matches neither side. Its id
    // moves, so recompute drops it and rule 6 appends the replacement behind
    // b.rs's hunk — which is the interleaving.
    cli.fx.write(
        "a.rs",
        &a.replace("fn one() {\n}", "fn one() {\n    a_third_thing();\n}"),
    );
    cli.run(&["next", "--json"]);

    let status = stdout(&cli.run(&["status"]));
    let line = status
        .lines()
        .find(|l| l.starts_with("files:"))
        .unwrap_or_else(|| panic!("a files line: {status}"));
    assert_eq!(
        line.matches("a.rs").count(),
        1,
        "each file once, however the queue grew: {line}"
    );
    assert_eq!(line.matches("b.rs").count(), 1, "{line}");
}

#[test]
fn a_kept_divergence_is_recorded_and_never_re_offered() {
    let cli = session_with_agent_edit();
    cli.run(&["next"]);

    // The user writes their own version, and the watcher raises the question.
    let watcher = cli.spawn(&["watch", "--headless", "--timeout", "6000"]);
    std::thread::sleep(std::time::Duration::from_millis(600));
    cli.fx.write("a.rs", "fn a() {\n    my_own_way();\n}\n");
    let _ = watcher.wait_with_output();

    let manifest: serde_json::Value =
        serde_json::from_slice(&std::fs::read(cli.fx.project().session_json()).unwrap()).unwrap();
    let h = &manifest["hunks"][0];
    assert!(
        h["pending_divergence"].is_object(),
        "a question should be open: {manifest}"
    );
    let id = h["id"].as_str().unwrap().to_string();

    let out = cli.run(&["resolve", &id, "keep"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        stdout(&out).contains("kept your version"),
        "{}",
        stdout(&out)
    );

    // Both versions are stored, and the proposal does not come back.
    let text = std::fs::read_to_string(cli.fx.project().session_json()).unwrap();
    assert!(text.contains("\"status\": \"diverged\""), "{text}");
    assert!(text.contains("my_own_way()"), "{text}");
    assert!(text.contains("\"proposed\""), "{text}");

    for round in 0..3 {
        let text = stdout(&cli.run(&["next"]));
        assert!(
            text.contains("nothing to transcribe"),
            "round {round}: a kept divergence must not come back: {text}"
        );
    }
}

#[test]
fn resolving_a_hunk_with_no_question_says_so() {
    let cli = session_with_agent_edit();
    let text = stdout(&cli.run(&["next", "--json"]));
    let hunk: serde_json::Value = serde_json::from_str(text.lines().next().unwrap()).unwrap();
    let id = hunk["id"].as_str().unwrap();

    let out = cli.run(&["resolve", id, "keep"]);
    assert!(out.status.success());
    assert!(
        stdout(&out).contains("no open question"),
        "{}",
        stdout(&out)
    );

    // And an unknown id is an error, not a shrug.
    let bad = cli.run(&["resolve", "h-nope", "retry"]);
    assert!(!bad.status.success());
    assert!(String::from_utf8_lossy(&bad.stderr).contains("no hunk h-nope"));
}

#[test]
fn show_re_prints_without_changing_anything() {
    let cli = session_with_agent_edit();
    cli.run(&["next"]);

    let before = std::fs::read_to_string(cli.fx.project().session_json()).unwrap();
    let text = stdout(&cli.run(&["show"]));
    assert!(text.contains("+     work();"), "{text}");
    assert!(text.contains("status: pending"), "{text}");
    assert!(text.contains("display only"), "{text}");
    let after = std::fs::read_to_string(cli.fx.project().session_json()).unwrap();
    assert_eq!(before, after, "`show` must change nothing");
}

#[test]
fn show_before_anything_was_presented_says_so() {
    let cli = session_with_agent_edit();
    let text = stdout(&cli.run(&["show"]));
    assert!(text.contains("nothing presented yet"), "{text}");
}

#[test]
fn show_takes_a_hunk_id() {
    let cli = session_with_agent_edit();
    let text = stdout(&cli.run(&["next", "--json"]));
    let hunk: serde_json::Value = serde_json::from_str(text.lines().next().unwrap()).unwrap();
    let id = hunk["id"].as_str().unwrap();

    let shown = stdout(&cli.run(&["show", id]));
    assert!(shown.contains("+     work();"), "{shown}");

    let missing = stdout(&cli.run(&["show", "h-gone"]));
    assert!(missing.contains("no hunk h-gone"), "{missing}");
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

    let text = stdout(&cli.run(&["next"]));
    assert!(text.contains("generated file"), "{text}");
    assert!(text.contains("still differs from the shadow"), "{text}");
    assert!(
        !text.contains("opening"),
        "no editor for a lockfile: {text}"
    );

    // Running the generating command yourself is what clears it.
    cli.fx.write(
        "Cargo.lock",
        "version = 3\n\n[[package]]\nname = \"serde\"\n",
    );
    let after = stdout(&cli.run(&["next"]));
    assert!(after.contains("nothing to transcribe"), "{after}");
}

#[test]
fn report_records_how_a_hunk_arrived_without_touching_its_status() {
    // The manual half of the input signal, on the path where nobody owns the
    // queue. It is also the only way a user with no editor plugin can correct
    // what the engine could not observe.
    let cli = session_with_agent_edit();
    let out = cli.run(&["next", "--json"]);
    let id = serde_json::from_str::<serde_json::Value>(stdout(&out).trim()).unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();

    let reported = cli.run(&["report", &id, "pasted"]);
    assert!(reported.status.success(), "{}", stdout(&reported));
    assert!(
        stdout(&reported).contains("pasted"),
        "{}",
        stdout(&reported)
    );

    let m = rote::session::Manifest::load(&cli.fx.project())
        .unwrap()
        .unwrap();
    let h = m.find(&id).unwrap();
    assert_eq!(h.input, rote::hunks::Input::Pasted);
    assert_eq!(h.status, rote::hunks::Status::Pending, "status untouched");

    // And a correction lands: last write wins, which is the point of having it.
    assert!(cli.run(&["report", &id, "typed"]).status.success());
    let m = rote::session::Manifest::load(&cli.fx.project())
        .unwrap()
        .unwrap();
    assert_eq!(m.find(&id).unwrap().input, rote::hunks::Input::Typed);
}

#[test]
fn report_names_a_hunk_that_does_not_exist() {
    let cli = session_with_agent_edit();
    let out = cli.run(&["report", "h-nope", "typed"]);
    assert!(!out.status.success());
}
