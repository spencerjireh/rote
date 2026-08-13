//! M4 acceptance: the transcription loop, driven end to end through the CLI
//! with a scripted editor standing in for the user's keystrokes.

mod common;

use common::Fixture;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn rote_bin() -> PathBuf {
    // The test binary lives in target/<profile>/deps/; rote is two levels up.
    let mut p = std::env::current_exe().unwrap();
    p.pop();
    p.pop();
    p.join("rote")
}

/// Write a shell script that acts as `$ROTE_EDITOR`, applying a canned edit.
///
/// It ignores the `+LINE` argument and rewrites the whole file, which is all a
/// test needs: what matters is what lands on disk before rote reads it back.
fn scripted_editor(dir: &Path, name: &str, body: &str) -> PathBuf {
    let path = dir.join(name);
    let script = format!(
        "#!/bin/sh\n\
         # last argument is the file; earlier ones may include +LINE\n\
         for a in \"$@\"; do f=\"$a\"; done\n\
         cat > \"$f\" <<'ROTE_EOF'\n{body}ROTE_EOF\n",
    );
    std::fs::write(&path, script).unwrap();
    let mut perms = std::fs::metadata(&path).unwrap().permissions();
    std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o755);
    std::fs::set_permissions(&path, perms).unwrap();
    path
}

struct Cli {
    fx: Fixture,
    editor: Option<PathBuf>,
}

impl Cli {
    fn new(fx: Fixture) -> Self {
        Self { fx, editor: None }
    }

    fn with_editor(mut self, editor: PathBuf) -> Self {
        self.editor = Some(editor);
        self
    }

    fn run(&self, args: &[&str]) -> Output {
        self.run_with_input(args, "")
    }

    fn run_with_input(&self, args: &[&str], stdin_text: &str) -> Output {
        use std::io::Write as _;
        let mut cmd = Command::new(rote_bin());
        cmd.args(args)
            .current_dir(&self.fx.repo)
            .env("XDG_CACHE_HOME", &self.fx.xdg_cache)
            .env("XDG_DATA_HOME", &self.fx.xdg_data)
            .env("XDG_CONFIG_HOME", &self.fx.xdg_config)
            .env("NO_COLOR", "1")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        if let Some(e) = &self.editor {
            cmd.env("ROTE_EDITOR", e);
        }
        let mut child = cmd.spawn().unwrap();
        child
            .stdin
            .as_mut()
            .unwrap()
            .write_all(stdin_text.as_bytes())
            .unwrap();
        drop(child.stdin.take());
        child.wait_with_output().unwrap()
    }

    fn shadow(&self) -> PathBuf {
        self.fx.project().shadow_dir
    }
}

fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

/// A started session whose shadow already holds the agent's edit.
fn session_with_agent_edit() -> Cli {
    let fx = Fixture::new();
    fx.write("a.rs", "fn a() {\n}\n");
    fx.commit_all("initial");
    let cli = Cli::new(fx);
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
    let cli = Cli::new(fx);
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
fn mark_sets_status_from_outside() {
    let cli = session_with_agent_edit();
    let text = stdout(&cli.run(&["next", "--json"]));
    let hunk: serde_json::Value = serde_json::from_str(text.lines().next().unwrap()).unwrap();
    let id = hunk["id"].as_str().unwrap();

    let out = cli.run(&["mark", id, "typed"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let status = stdout(&cli.run(&["status"]));
    assert!(status.contains("1 typed"), "{status}");

    // An unknown status is refused rather than silently accepted.
    let bad = cli.run(&["mark", id, "finished"]);
    assert!(!bad.status.success());
    assert!(String::from_utf8_lossy(&bad.stderr).contains("unknown status"));
}

#[test]
fn a_generated_file_is_gated_on_bytes_not_typing() {
    let fx = Fixture::new();
    fx.write("Cargo.lock", "version = 3\n");
    fx.commit_all("initial");
    let cli = Cli::new(fx);
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
