//! M7 acceptance: `doctor`, `setup`, project-aware `init`, and the `start`
//! preflight, driven through the built binary.

mod common;

use common::cli::{claude_stub_with_help as claude_stub, rote_bin, stderr, stdout, Cli};
use common::Fixture;
use std::path::Path;
use std::process::Command;

// ---------------------------------------------------------------- doctor

#[test]
fn doctor_runs_outside_a_repository_and_marks_repo_lines_unavailable() {
    let r = Cli::new();
    let elsewhere = r.fx.root.path().join("not-a-repo");
    std::fs::create_dir_all(&elsewhere).unwrap();

    let out = r.run_in(&["doctor"], &elsewhere);
    let text = stdout(&out);

    // Machine-level checks still report.
    assert!(text.contains("git"), "{text}");
    assert!(text.contains("editor"), "{text}");
    // Repository-scoped ones degrade rather than erroring.
    assert!(text.contains("not in a git repository"), "{text}");
    assert!(
        !stderr(&out).contains("not inside a git repository"),
        "doctor must not fail outside a repo: {}",
        stderr(&out)
    );
}

#[test]
fn doctor_reports_a_missing_claude_and_exits_nonzero() {
    let r = Cli::new();
    // Point claude_cmd at something that cannot resolve.
    std::fs::create_dir_all(r.global_config().parent().unwrap()).unwrap();
    std::fs::write(
        r.global_config(),
        "claude_cmd = [\"definitely-not-a-real-binary-xyz\"]\n",
    )
    .unwrap();

    let out = r.run(&["doctor"]);
    let text = stdout(&out);

    assert!(text.contains("not on PATH"), "{text}");
    assert!(text.contains("FAIL"), "{text}");
    assert!(text.contains("rote setup"), "names the fix: {text}");
    assert!(
        !out.status.success(),
        "doctor must exit non-zero on failure"
    );
    assert!(text.contains("need attention"), "{text}");
}

#[test]
fn doctor_passes_when_everything_resolves() {
    let r = Cli::new();
    r.fx.write("a.rs", "fn a() {}\n");
    r.fx.commit_all("initial");
    r.fx.write(".rote.toml", "[checks]\ncommands = [\"true\"]\n");

    let stub = claude_stub(&r.bin);
    std::fs::create_dir_all(r.global_config().parent().unwrap()).unwrap();
    std::fs::write(
        r.global_config(),
        format!("claude_cmd = [\"{}\"]\neditor = \"sh\"\n", stub.display()),
    )
    .unwrap();

    let out = r.run(&["doctor"]);
    let text = stdout(&out);

    assert!(out.status.success(), "{text}\n{}", stderr(&out));
    assert!(text.contains("all good"), "{text}");
    assert!(
        text.contains("present in --help"),
        "reviewer flag check: {text}"
    );
    assert!(
        text.contains("1 check(s)"),
        "reads the project config: {text}"
    );
    assert!(!text.contains("FAIL"), "{text}");
}

#[test]
fn a_missing_editor_no_longer_fails_doctor() {
    // rote does not launch an editor as part of the loop any more — it only
    // offers to, from the `o` key in the watch pane. A machine without one
    // transcribes perfectly well, so this must not be a failure.
    let r = Cli::new();
    r.fx.write("a.rs", "fn a() {}\n");
    r.fx.commit_all("initial");
    r.fx.write(".rote.toml", "[checks]\ncommands = [\"true\"]\n");

    let stub = claude_stub(&r.bin);
    std::fs::create_dir_all(r.global_config().parent().unwrap()).unwrap();
    std::fs::write(
        r.global_config(),
        format!(
            "claude_cmd = [\"{}\"]\neditor = \"definitely-not-a-real-editor-xyz\"\n",
            stub.display()
        ),
    )
    .unwrap();

    let out = r.run(&["doctor"]);
    let text = stdout(&out);

    assert!(out.status.success(), "{text}\n{}", stderr(&out));
    assert!(text.contains("editor"), "still reported: {text}");
    // Doctor truncates the detail column from the left, so match a fragment
    // that survives it.
    assert!(
        text.contains("open action"),
        "and says what is actually lost: {text}"
    );
    assert!(!text.contains("FAIL"), "{text}");
}

#[test]
fn doctor_flags_a_repo_with_no_commits() {
    let r = Cli::new();
    let stub = claude_stub(&r.bin);
    std::fs::create_dir_all(r.global_config().parent().unwrap()).unwrap();
    std::fs::write(
        r.global_config(),
        format!("claude_cmd = [\"{}\"]\neditor = \"sh\"\n", stub.display()),
    )
    .unwrap();

    // Fixture repo is initialized but has no commit yet.
    let out = r.run(&["doctor"]);
    let text = stdout(&out);
    assert!(text.contains("no commits yet"), "{text}");
    assert!(!out.status.success());
}

#[test]
fn doctor_reports_a_missing_global_config_and_points_at_setup() {
    let r = Cli::new();
    r.fx.write("a.rs", "fn a() {}\n");
    r.fx.commit_all("initial");

    let text = stdout(&r.run(&["doctor"]));
    assert!(text.contains("global config"), "{text}");
    assert!(text.contains("missing"), "{text}");
    assert!(text.contains("→ rote setup"), "{text}");
}

// ---------------------------------------------------------------- setup

#[test]
fn setup_writes_a_config_that_loads_back() {
    let r = Cli::new();
    let out = r.run(&["setup"]);
    assert!(out.status.success(), "{}", stderr(&out));

    let path = r.global_config();
    assert!(path.is_file(), "setup must write the global config");

    // It has to be valid input to the real loader, not just plausible TOML.
    let cfg = rote::config::Config::load(&path, Path::new("/nonexistent")).unwrap();
    assert_eq!(cfg.claude_cmd, vec!["claude".to_string()]);
    assert_eq!(cfg.claude_continue_cmd, vec!["claude", "--continue"]);
    assert!(cfg.color);
}

#[test]
fn setup_refuses_to_overwrite_without_force() {
    let r = Cli::new();
    assert!(r.run(&["setup"]).status.success());

    let second = r.run(&["setup"]);
    assert!(!second.status.success());
    assert!(
        stderr(&second).contains("already exists"),
        "{}",
        stderr(&second)
    );
    assert!(stderr(&second).contains("--force"), "{}", stderr(&second));

    assert!(r.run(&["setup", "--force"]).status.success());
}

#[test]
fn setup_works_outside_a_repository() {
    let r = Cli::new();
    let elsewhere = r.fx.root.path().join("no-repo-here");
    std::fs::create_dir_all(&elsewhere).unwrap();

    let out = r.run_in(&["setup"], &elsewhere);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(r.global_config().is_file());
}

#[test]
fn setup_preserves_a_detected_claude_path() {
    let r = Cli::new();
    let stub = claude_stub(&r.bin);
    std::fs::create_dir_all(r.global_config().parent().unwrap()).unwrap();
    std::fs::write(
        r.global_config(),
        format!("claude_cmd = [\"{}\"]\n", stub.display()),
    )
    .unwrap();

    assert!(r.run(&["setup", "--force"]).status.success());
    let cfg = rote::config::Config::load(&r.global_config(), Path::new("/nonexistent")).unwrap();
    assert_eq!(
        cfg.claude_cmd,
        vec![stub.to_string_lossy().into_owned()],
        "a working claude_cmd must survive a re-run"
    );
}

// ---------------------------------------------------------------- init

#[test]
fn init_prefills_checks_for_a_rust_project() {
    let r = Cli::new();
    r.fx.write(
        "Cargo.toml",
        "[package]\nname = \"x\"\nversion = \"0.1.0\"\n",
    );
    r.fx.commit_all("initial");

    let out = r.run(&["init"]);
    let text = stdout(&out);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(text.contains("detected a Rust project"), "{text}");
    assert!(text.contains("cargo test"), "names what it chose: {text}");

    let body = r.fx.read(".rote.toml");
    assert!(body.contains("\"cargo test\""), "{body}");
    assert!(
        body.contains("Cargo.lock"),
        "verbatim is ecosystem-shaped: {body}"
    );
    assert!(
        !body.contains("commands = []"),
        "checks must not be empty: {body}"
    );

    // And the written checks actually run.
    let cfg = rote::config::Config::load(Path::new("/nonexistent"), &r.fx.repo.join(".rote.toml"))
        .unwrap();
    for cmd in &cfg.check_commands {
        let head = cmd.split_whitespace().next().unwrap();
        assert!(
            rote::detect::find_on_path(head).is_some(),
            "wrote a check whose command does not exist: {cmd}"
        );
    }
}

#[test]
fn init_prefills_from_package_json_scripts() {
    let r = Cli::new();
    r.fx.write(
        "package.json",
        r#"{"scripts":{"test":"jest","lint":"eslint ."}}"#,
    );
    r.fx.commit_all("initial");

    let text = stdout(&r.run(&["init"]));
    assert!(text.contains("detected a Node project"), "{text}");

    let body = r.fx.read(".rote.toml");
    assert!(body.contains("npm run test"), "{body}");
    assert!(body.contains("npm run lint"), "{body}");
    assert!(body.contains("package-lock.json"), "{body}");
}

#[test]
fn init_says_so_when_it_recognizes_nothing() {
    let r = Cli::new();
    r.fx.write("README.md", "# just docs\n");
    r.fx.commit_all("initial");

    let text = stdout(&r.run(&["init"]));
    assert!(text.contains("no project type detected"), "{text}");

    let body = r.fx.read(".rote.toml");
    assert!(body.contains("commands = []"), "{body}");
    assert!(
        body.contains("No project type detected"),
        "explains itself: {body}"
    );
}

#[test]
fn init_still_refuses_to_overwrite_without_force() {
    let r = Cli::new();
    r.fx.write("Cargo.toml", "[package]\nname = \"x\"\n");
    r.fx.commit_all("initial");

    assert!(r.run(&["init"]).status.success());
    let second = r.run(&["init"]);
    assert!(!second.status.success());
    assert!(stderr(&second).contains("--force"), "{}", stderr(&second));
}

// ---------------------------------------------------------------- start preflight

#[test]
fn start_refuses_when_claude_does_not_resolve_and_names_doctor() {
    let r = Cli::new();
    r.fx.write("a.rs", "fn a() {}\n");
    r.fx.commit_all("initial");
    std::fs::create_dir_all(r.global_config().parent().unwrap()).unwrap();
    std::fs::write(
        r.global_config(),
        "claude_cmd = [\"definitely-not-a-real-binary-xyz\"]\n",
    )
    .unwrap();

    let out = r.run(&["start", "some task"]);
    assert!(!out.status.success());
    let err = stderr(&out);
    assert!(err.contains("not on PATH"), "{err}");
    assert!(
        err.contains("rote doctor"),
        "points at the diagnostic: {err}"
    );

    // It refused before doing any work.
    assert!(
        !r.fx.project().session_json().exists(),
        "no session should have been opened"
    );
}

#[test]
fn start_no_launch_does_not_require_claude() {
    // --no-launch never execs, so a missing claude must not block it — this is
    // what every other integration test depends on.
    let r = Cli::new();
    r.fx.write("a.rs", "fn a() {}\n");
    r.fx.commit_all("initial");
    std::fs::create_dir_all(r.global_config().parent().unwrap()).unwrap();
    std::fs::write(r.global_config(), "claude_cmd = [\"nope-not-real\"]\n").unwrap();

    let out = r.run(&["start", "--no-launch", "t"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(r.fx.project().session_json().exists());
}

// ---------------------------------------------------------------- guard

#[test]
fn a_repo_containing_its_own_xdg_dirs_is_refused() {
    // The dotfiles-repo case, through the CLI: XDG roots inside the repository.
    let fx = Fixture::new();
    fx.write("a.rs", "fn a() {}\n");
    fx.commit_all("initial");

    let out = Command::new(rote_bin())
        .args(["status"])
        .current_dir(&fx.repo)
        .env("XDG_CACHE_HOME", fx.repo.join(".cache"))
        .env("XDG_DATA_HOME", fx.repo.join(".local/share"))
        .env("XDG_CONFIG_HOME", &fx.xdg_config)
        .env("NO_COLOR", "1")
        .output()
        .unwrap();

    assert!(!out.status.success(), "must refuse rather than proceed");
    let err = stderr(&out);
    assert!(err.contains("inside the repository"), "{err}");
    assert!(err.contains("XDG_CACHE_HOME"), "names the fix: {err}");
}
