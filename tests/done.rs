//! M5 acceptance: the full lifecycle with a stubbed `claude` and a scripted
//! editor — init → start → agent edits → next×N → done.

mod common;

use common::Fixture;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn rote_bin() -> PathBuf {
    let mut p = std::env::current_exe().unwrap();
    p.pop();
    p.pop();
    p.join("rote")
}

fn write_exec(path: &Path, body: &str) {
    std::fs::write(path, body).unwrap();
    let mut perms = std::fs::metadata(path).unwrap().permissions();
    std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o755);
    std::fs::set_permissions(path, perms).unwrap();
}

fn scripted_editor(dir: &Path, name: &str, body: &str) -> PathBuf {
    let path = dir.join(name);
    write_exec(
        &path,
        &format!(
            "#!/bin/sh\nfor a in \"$@\"; do f=\"$a\"; done\ncat > \"$f\" <<'ROTE_EOF'\n{body}ROTE_EOF\n"
        ),
    );
    path
}

/// A stand-in for `claude` that records the payload it was given on stdin.
fn stub_claude(dir: &Path, name: &str, reply: &str, exit_code: i32) -> (PathBuf, PathBuf) {
    let script = dir.join(name);
    let capture = dir.join(format!("{name}.payload"));
    let args_log = dir.join(format!("{name}.args"));
    write_exec(
        &script,
        &format!(
            "#!/bin/sh\n\
             printf '%s\\n' \"$@\" > {args}\n\
             cat > {cap}\n\
             printf '%s' '{reply}'\n\
             exit {exit_code}\n",
            args = args_log.display(),
            cap = capture.display(),
        ),
    );
    (script, capture)
}

struct Cli {
    fx: Fixture,
    editor: Option<PathBuf>,
    claude: Option<PathBuf>,
}

impl Cli {
    fn new(fx: Fixture) -> Self {
        Self {
            fx,
            editor: None,
            claude: None,
        }
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

    fn run(&self, args: &[&str]) -> Output {
        self.run_with_input(args, "")
    }

    fn shadow(&self) -> PathBuf {
        self.fx.project().shadow_dir
    }

    /// Point `claude_cmd` at a stub via the global config file.
    fn use_claude_stub(&mut self, stub: &Path) {
        let dir = self.fx.xdg_config.join("rote");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("config.toml"),
            format!("claude_cmd = [\"{}\"]\n", stub.display()),
        )
        .unwrap();
        self.claude = Some(stub.to_path_buf());
    }

    fn write_project_config(&self, body: &str) {
        std::fs::write(self.fx.repo.join(".rote.toml"), body).unwrap();
    }
}

fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}
fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

#[test]
fn the_full_lifecycle_ends_idle_with_shadow_matching_real() {
    let fx = Fixture::new();
    fx.write("a.rs", "fn a() {\n}\n");
    fx.commit_all("initial");
    let mut cli = Cli::new(fx);

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

    // next, with an editor that types it correctly
    cli.editor = Some(scripted_editor(
        cli.fx.root.path(),
        "type.sh",
        "fn a() {\n    work();\n}\n",
    ));
    let next = cli.run(&["next"]);
    assert!(stdout(&next).contains("typed"), "{}", stdout(&next));

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
    let mut cli = Cli::new(fx);
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
    assert!(args.contains("--allowedTools"), "tool restriction: {args}");
}

#[test]
fn a_failing_check_stops_the_pipeline_and_keeps_the_session() {
    let fx = Fixture::new();
    fx.write("a.rs", "fn a() {\n}\n");
    fx.commit_all("initial");
    let mut cli = Cli::new(fx);
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
    let mut cli = Cli::new(fx);
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
    let mut cli = Cli::new(fx);
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

#[test]
fn the_residue_patch_holds_what_was_skipped() {
    let fx = Fixture::new();
    fx.write("a.rs", "fn a() {\n}\n");
    fx.commit_all("initial");
    let mut cli = Cli::new(fx);
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
    let mut cli = Cli::new(fx);
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

    let cli = Cli::new(fx);
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
    let cli = Cli::new(fx);
    cli.run(&["start", "--no-launch", "t"]);

    let out = cli.run(&["talk"]);
    assert!(out.status.success());
    assert_eq!(
        stdout(&out).trim(),
        cli.shadow().to_string_lossy(),
        "talk without --attach just names the shadow"
    );
}
