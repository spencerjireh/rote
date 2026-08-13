//! M7 acceptance: `doctor`, `setup`, project-aware `init`, and the `start`
//! preflight, driven through the built binary.

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

/// A `claude` stand-in whose `--help` advertises the tool-restriction flag.
fn claude_stub(dir: &Path) -> PathBuf {
    let p = dir.join("claude-stub");
    write_exec(
        &p,
        "#!/bin/sh\n\
         case \"$1\" in --help) echo '  --tools <tools...>  Use \"\" to disable all tools';; esac\n\
         exit 0\n",
    );
    p
}

struct Run {
    fx: Fixture,
    /// Extra directory prepended to PATH, for stubbing binaries.
    bin: PathBuf,
}

impl Run {
    fn new() -> Self {
        let fx = Fixture::new();
        let bin = fx.root.path().join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        Self { fx, bin }
    }

    fn cmd(&self, args: &[&str], cwd: &Path) -> Output {
        self.cmd_with_input(args, cwd, "")
    }

    fn cmd_with_input(&self, args: &[&str], cwd: &Path, stdin_text: &str) -> Output {
        use std::io::Write as _;
        let path = format!(
            "{}:{}",
            self.bin.display(),
            std::env::var("PATH").unwrap_or_default()
        );
        let mut child = Command::new(rote_bin())
            .args(args)
            .current_dir(cwd)
            .env("PATH", path)
            .env("XDG_CACHE_HOME", &self.fx.xdg_cache)
            .env("XDG_DATA_HOME", &self.fx.xdg_data)
            .env("XDG_CONFIG_HOME", &self.fx.xdg_config)
            .env("NO_COLOR", "1")
            .env_remove("ROTE_EDITOR")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .as_mut()
            .unwrap()
            .write_all(stdin_text.as_bytes())
            .unwrap();
        drop(child.stdin.take());
        child.wait_with_output().unwrap()
    }

    fn in_repo(&self, args: &[&str]) -> Output {
        self.cmd(args, &self.fx.repo)
    }

    fn global_config(&self) -> PathBuf {
        self.fx.xdg_config.join("rote/config.toml")
    }
}

fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}
fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

// ---------------------------------------------------------------- doctor

#[test]
fn doctor_runs_outside_a_repository_and_marks_repo_lines_unavailable() {
    let r = Run::new();
    let elsewhere = r.fx.root.path().join("not-a-repo");
    std::fs::create_dir_all(&elsewhere).unwrap();

    let out = r.cmd(&["doctor"], &elsewhere);
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
    let r = Run::new();
    // Point claude_cmd at something that cannot resolve.
    std::fs::create_dir_all(r.global_config().parent().unwrap()).unwrap();
    std::fs::write(
        r.global_config(),
        "claude_cmd = [\"definitely-not-a-real-binary-xyz\"]\n",
    )
    .unwrap();

    let out = r.in_repo(&["doctor"]);
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
    let r = Run::new();
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

    let out = r.in_repo(&["doctor"]);
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
fn doctor_flags_a_repo_with_no_commits() {
    let r = Run::new();
    let stub = claude_stub(&r.bin);
    std::fs::create_dir_all(r.global_config().parent().unwrap()).unwrap();
    std::fs::write(
        r.global_config(),
        format!("claude_cmd = [\"{}\"]\neditor = \"sh\"\n", stub.display()),
    )
    .unwrap();

    // Fixture repo is initialized but has no commit yet.
    let out = r.in_repo(&["doctor"]);
    let text = stdout(&out);
    assert!(text.contains("no commits yet"), "{text}");
    assert!(!out.status.success());
}

#[test]
fn doctor_reports_a_missing_global_config_and_points_at_setup() {
    let r = Run::new();
    r.fx.write("a.rs", "fn a() {}\n");
    r.fx.commit_all("initial");

    let text = stdout(&r.in_repo(&["doctor"]));
    assert!(text.contains("global config"), "{text}");
    assert!(text.contains("missing"), "{text}");
    assert!(text.contains("→ rote setup"), "{text}");
}

// ---------------------------------------------------------------- setup

#[test]
fn setup_writes_a_config_that_loads_back() {
    let r = Run::new();
    let out = r.in_repo(&["setup"]);
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
    let r = Run::new();
    assert!(r.in_repo(&["setup"]).status.success());

    let second = r.in_repo(&["setup"]);
    assert!(!second.status.success());
    assert!(
        stderr(&second).contains("already exists"),
        "{}",
        stderr(&second)
    );
    assert!(stderr(&second).contains("--force"), "{}", stderr(&second));

    assert!(r.in_repo(&["setup", "--force"]).status.success());
}

#[test]
fn setup_works_outside_a_repository() {
    let r = Run::new();
    let elsewhere = r.fx.root.path().join("no-repo-here");
    std::fs::create_dir_all(&elsewhere).unwrap();

    let out = r.cmd(&["setup"], &elsewhere);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(r.global_config().is_file());
}

#[test]
fn setup_preserves_a_detected_claude_path() {
    let r = Run::new();
    let stub = claude_stub(&r.bin);
    std::fs::create_dir_all(r.global_config().parent().unwrap()).unwrap();
    std::fs::write(
        r.global_config(),
        format!("claude_cmd = [\"{}\"]\n", stub.display()),
    )
    .unwrap();

    assert!(r.in_repo(&["setup", "--force"]).status.success());
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
    let r = Run::new();
    r.fx.write(
        "Cargo.toml",
        "[package]\nname = \"x\"\nversion = \"0.1.0\"\n",
    );
    r.fx.commit_all("initial");

    let out = r.in_repo(&["init"]);
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
    let r = Run::new();
    r.fx.write(
        "package.json",
        r#"{"scripts":{"test":"jest","lint":"eslint ."}}"#,
    );
    r.fx.commit_all("initial");

    let text = stdout(&r.in_repo(&["init"]));
    assert!(text.contains("detected a Node project"), "{text}");

    let body = r.fx.read(".rote.toml");
    assert!(body.contains("npm run test"), "{body}");
    assert!(body.contains("npm run lint"), "{body}");
    assert!(body.contains("package-lock.json"), "{body}");
}

#[test]
fn init_says_so_when_it_recognizes_nothing() {
    let r = Run::new();
    r.fx.write("README.md", "# just docs\n");
    r.fx.commit_all("initial");

    let text = stdout(&r.in_repo(&["init"]));
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
    let r = Run::new();
    r.fx.write("Cargo.toml", "[package]\nname = \"x\"\n");
    r.fx.commit_all("initial");

    assert!(r.in_repo(&["init"]).status.success());
    let second = r.in_repo(&["init"]);
    assert!(!second.status.success());
    assert!(stderr(&second).contains("--force"), "{}", stderr(&second));
}

// ---------------------------------------------------------------- start preflight

#[test]
fn start_refuses_when_claude_does_not_resolve_and_names_doctor() {
    let r = Run::new();
    r.fx.write("a.rs", "fn a() {}\n");
    r.fx.commit_all("initial");
    std::fs::create_dir_all(r.global_config().parent().unwrap()).unwrap();
    std::fs::write(
        r.global_config(),
        "claude_cmd = [\"definitely-not-a-real-binary-xyz\"]\n",
    )
    .unwrap();

    let out = r.in_repo(&["start", "some task"]);
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
    let r = Run::new();
    r.fx.write("a.rs", "fn a() {}\n");
    r.fx.commit_all("initial");
    std::fs::create_dir_all(r.global_config().parent().unwrap()).unwrap();
    std::fs::write(r.global_config(), "claude_cmd = [\"nope-not-real\"]\n").unwrap();

    let out = r.in_repo(&["start", "--no-launch", "t"]);
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
