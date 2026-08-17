//! Environment and project detection, shared by `doctor`, `setup`, and `init`.
//!
//! One surface so the three commands cannot disagree about what they found.
//! These functions probe and return values; none of them print, and none of them
//! write. `doctor` renders the reports, `setup` and `init` act on them.

use crate::config::Config;
use crate::model;
use crate::present;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

/// Look up an executable the way a shell would.
///
/// An absolute or relative path is taken as-is (so a configured stub or a
/// wrapper script resolves); a bare name is searched along `PATH`.
pub fn find_on_path(name: &str) -> Option<PathBuf> {
    let candidate = Path::new(name);
    if candidate.components().count() > 1 || candidate.is_absolute() {
        return is_executable(candidate).then(|| candidate.to_path_buf());
    }
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .map(|dir| dir.join(name))
            .find(|p| is_executable(p))
    })
}

fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

/// What we know about the configured claude command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaudeReport {
    pub command: Vec<String>,
    pub resolved: Option<PathBuf>,
    /// Whether `--help` advertises the tool-restriction flag rote passes.
    pub tool_flag_supported: Option<bool>,
    /// Set by a `--deep` probe: the reviewer path actually ran.
    pub deep_ok: Option<bool>,
}

impl ClaudeReport {
    pub fn found(&self) -> bool {
        self.resolved.is_some()
    }
}

/// The flag name rote passes to restrict the reviewer's tools.
fn tool_flag_name() -> &'static str {
    model::TOOL_FLAGS.first().copied().unwrap_or("--tools")
}

/// Resolve claude and check the reviewer flag it will be handed.
///
/// The flag check parses `--help` rather than invoking the model: free, offline,
/// and enough to catch a rename. It is not enough to catch a flag that exists
/// but behaves differently — that needs `deep_probe_claude`, which is what would
/// have caught the original wrong `--allowedTools` guess.
pub fn probe_claude(cfg: &Config) -> ClaudeReport {
    let command = cfg.claude_cmd.clone();
    let resolved = command.first().and_then(|c| find_on_path(c));

    let tool_flag_supported = resolved.as_ref().map(|path| {
        let flag = tool_flag_name();
        Command::new(path)
            .arg("--help")
            .stdin(Stdio::null())
            .output()
            .map(|out| {
                let text = String::from_utf8_lossy(&out.stdout);
                text.contains(flag)
            })
            .unwrap_or(false)
    });

    ClaudeReport {
        command,
        resolved,
        tool_flag_supported,
        deep_ok: None,
    }
}

/// How long the probe gets. Shorter than the reviewer's: this asks for four
/// words, and `doctor` is something you run while waiting for it.
const PROBE_TIMEOUT: Duration = Duration::from_secs(60);

/// Actually run the reviewer path once, end to end.
///
/// Costs a token spend, so it is only reached via `rote doctor --deep`.
pub fn deep_probe_claude(cfg: &Config, report: &mut ClaudeReport) {
    let Some(path) = report.resolved.clone() else {
        report.deep_ok = Some(false);
        return;
    };
    // The resolved path, then whatever the configured command carried after its
    // program name.
    let mut cmd = vec![path.to_string_lossy().into_owned()];
    cmd.extend_from_slice(&cfg.claude_cmd[1..]);

    // Through `model::run` rather than a second hand-rolled spawn. The copy this
    // replaces had no timeout at all, so `doctor --deep` against a wedged claude
    // hung for as long as you left it.
    let ok = model::run(
        &model::Invocation {
            claude_cmd: &cmd,
            prompt: "Reply with exactly: ROTE OK",
            extra_args: &cfg.review_model_args,
            timeout: PROBE_TIMEOUT,
        },
        "probe\n",
    )
    .map(|out| out.status.success())
    .unwrap_or(false);

    report.deep_ok = Some(ok);
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditorReport {
    pub command: Vec<String>,
    pub resolved: Option<PathBuf>,
}

impl EditorReport {
    pub fn found(&self) -> bool {
        self.resolved.is_some()
    }
}

/// Resolve the editor rote would launch, using the same chain `next` uses.
pub fn probe_editor(cfg: &Config) -> EditorReport {
    let command = present::editor_command(&cfg.editor);
    let resolved = command.first().and_then(|c| find_on_path(c));
    EditorReport { command, resolved }
}

/// Which ecosystem a repository belongs to, for check and lockfile defaults.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProjectKind {
    Rust,
    Node,
    Python,
    Go,
    Unknown,
}

impl ProjectKind {
    pub fn label(self) -> &'static str {
        match self {
            ProjectKind::Rust => "Rust",
            ProjectKind::Node => "Node",
            ProjectKind::Python => "Python",
            ProjectKind::Go => "Go",
            ProjectKind::Unknown => "unrecognized",
        }
    }
}

/// Identify the project from its manifest file.
pub fn project_kind(repo_root: &Path) -> ProjectKind {
    // Ordered, so a polyglot repo gets one answer rather than a coin flip.
    for (file, kind) in [
        ("Cargo.toml", ProjectKind::Rust),
        ("package.json", ProjectKind::Node),
        ("pyproject.toml", ProjectKind::Python),
        ("go.mod", ProjectKind::Go),
    ] {
        if repo_root.join(file).is_file() {
            return kind;
        }
    }
    ProjectKind::Unknown
}

/// Whether a shell command line runs at all, used to choose between spellings.
fn command_works(repo_root: &Path, line: &str) -> bool {
    Command::new("sh")
        .arg("-c")
        .arg(line)
        .current_dir(repo_root)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Check commands for this project, each naming a program that runs here.
///
/// The Rust case is why this probes rather than hardcodes: a Homebrew Rust with
/// no rustup has `cargo-clippy` and `cargo-fmt` on PATH but no `cargo clippy`
/// subcommand, so the obvious defaults would be written into every `.rote.toml`
/// and fail at the first `rote done`.
///
/// What gets verified is that the program a command names resolves and runs —
/// never the check itself, because running `cargo test` to decide whether to
/// write `cargo test` would make `rote init` cost a full test suite. Every
/// spelling is probed on its own: clippy and fmt are separate rustup components
/// and either can be missing while the other is present, so one cannot vouch for
/// the other.
///
/// A recognized project on a machine with no toolchain therefore gets no
/// commands, the same as an unrecognized one. An empty `[checks]` says plainly
/// that rote found nothing to run; a command that was never going to work says
/// the opposite until `rote done` contradicts it.
pub fn checks_for(kind: ProjectKind, repo_root: &Path) -> Vec<String> {
    // Only Node reads the manifest, so only Node pays for the read.
    let scripts = if kind == ProjectKind::Node {
        node_scripts(repo_root)
    } else {
        Vec::new()
    };
    checks_with(kind, &scripts, &|line| command_works(repo_root, line))
}

/// The command choice, with probing and the manifest read lifted out.
///
/// Split from `checks_for` so the choice can be tested without depending on which
/// toolchains the machine running the tests happens to have. "No toolchain gets no
/// commands" is otherwise the one branch that cannot be asserted on a developer
/// machine, being the one where everything is installed.
fn checks_with(kind: ProjectKind, scripts: &[String], works: &dyn Fn(&str) -> bool) -> Vec<String> {
    match kind {
        ProjectKind::Rust => {
            let mut out = Vec::new();
            if works("cargo --version") {
                out.push("cargo test".to_string());
            }
            if works("cargo clippy --version") {
                out.push("cargo clippy --all-targets -- -D warnings".into());
            } else if works("cargo-clippy --version") {
                out.push("cargo-clippy --all-targets -- -D warnings".into());
            }
            if works("cargo fmt --version") {
                out.push("cargo fmt --check".into());
            } else if works("cargo-fmt --version") {
                out.push("cargo-fmt --check".into());
            }
            out
        }
        ProjectKind::Node => {
            if !works("npm --version") {
                return Vec::new();
            }
            let mut out = Vec::new();
            for candidate in ["test", "lint", "typecheck"] {
                if scripts.iter().any(|s| s == candidate) {
                    out.push(format!("npm run {candidate}"));
                }
            }
            // `npm test` works even without an explicit script entry.
            if out.is_empty() {
                out.push("npm test".into());
            }
            out
        }
        ProjectKind::Python => {
            if works("uv --version") {
                vec!["uv run pytest".into()]
            } else if works("pytest --version") {
                vec!["pytest".into()]
            } else {
                Vec::new()
            }
        }
        // One probe for both: they are subcommands of the same binary.
        ProjectKind::Go if works("go version") => {
            vec!["go test ./...".into(), "go vet ./...".into()]
        }
        ProjectKind::Go | ProjectKind::Unknown => Vec::new(),
    }
}

/// Script names declared in a package.json, if it parses.
fn node_scripts(repo_root: &Path) -> Vec<String> {
    let Ok(text) = std::fs::read_to_string(repo_root.join("package.json")) else {
        return Vec::new();
    };
    let Ok(json) = serde_json::from_str::<serde_json::Value>(&text) else {
        return Vec::new();
    };
    json.get("scripts")
        .and_then(|s| s.as_object())
        .map(|o| o.keys().cloned().collect())
        .unwrap_or_default()
}

/// Generated files for this ecosystem, gated on bytes rather than typed.
pub fn verbatim_for(kind: ProjectKind) -> Vec<String> {
    let list: &[&str] = match kind {
        ProjectKind::Rust => &["Cargo.lock"],
        ProjectKind::Node => &["package-lock.json", "pnpm-lock.yaml", "yarn.lock"],
        ProjectKind::Python => &["poetry.lock", "uv.lock", "requirements.lock"],
        ProjectKind::Go => &["go.sum"],
        ProjectKind::Unknown => &["*.lock"],
    };
    list.iter().map(|s| s.to_string()).collect()
}

/// Everything `init` needs to write a project-shaped config.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Detected {
    pub kind: ProjectKind,
    pub checks: Vec<String>,
    pub verbatim: Vec<String>,
}

pub fn detect_project(repo_root: &Path) -> Detected {
    let kind = project_kind(repo_root);
    Detected {
        kind,
        checks: checks_for(kind, repo_root),
        verbatim: verbatim_for(kind),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo_with(files: &[(&str, &str)]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for (name, body) in files {
            let p = dir.path().join(name);
            if let Some(parent) = p.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::write(p, body).unwrap();
        }
        dir
    }

    #[test]
    fn finds_a_bare_name_on_path() {
        // `sh` exists on every machine this runs on.
        assert!(find_on_path("sh").is_some());
        assert!(find_on_path("definitely-not-a-real-binary-xyz").is_none());
    }

    #[test]
    fn accepts_an_explicit_path_without_searching_path() {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("stub");
        std::fs::write(&script, "#!/bin/sh\n").unwrap();
        let mut perms = std::fs::metadata(&script).unwrap().permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o755);
        std::fs::set_permissions(&script, perms).unwrap();

        assert_eq!(
            find_on_path(&script.to_string_lossy()).as_deref(),
            Some(script.as_path())
        );
    }

    #[test]
    fn a_non_executable_file_does_not_resolve() {
        let dir = tempfile::tempdir().unwrap();
        let plain = dir.path().join("not-executable");
        std::fs::write(&plain, "text").unwrap();
        assert!(find_on_path(&plain.to_string_lossy()).is_none());
    }

    #[test]
    fn identifies_each_project_kind() {
        for (file, kind) in [
            ("Cargo.toml", ProjectKind::Rust),
            ("package.json", ProjectKind::Node),
            ("pyproject.toml", ProjectKind::Python),
            ("go.mod", ProjectKind::Go),
        ] {
            let dir = repo_with(&[(file, "")]);
            assert_eq!(project_kind(dir.path()), kind, "for {file}");
        }
    }

    #[test]
    fn an_unrecognized_project_yields_no_checks() {
        let dir = repo_with(&[("README.md", "# hi")]);
        let d = detect_project(dir.path());
        assert_eq!(d.kind, ProjectKind::Unknown);
        assert!(d.checks.is_empty(), "nothing is invented for unknown types");
        assert_eq!(d.verbatim, vec!["*.lock".to_string()]);
    }

    #[test]
    fn a_polyglot_repo_gets_one_answer() {
        let dir = repo_with(&[("Cargo.toml", ""), ("package.json", "{}")]);
        assert_eq!(
            project_kind(dir.path()),
            ProjectKind::Rust,
            "ordered, not arbitrary"
        );
    }

    /// A prober that says yes to exactly the listed probes.
    fn only<'a>(available: &'a [&'static str]) -> impl Fn(&str) -> bool + 'a {
        move |line: &str| available.contains(&line)
    }

    #[test]
    fn every_written_check_names_a_program_that_runs_here() {
        // Machine-dependent on purpose, and in the safe direction: it asserts
        // nothing about *which* commands are written, only that whatever was
        // written can be run. It is the property `rote done` depends on.
        let dir = repo_with(&[("Cargo.toml", "[package]\nname = \"x\"\n")]);
        for kind in [
            ProjectKind::Rust,
            ProjectKind::Node,
            ProjectKind::Python,
            ProjectKind::Go,
        ] {
            for c in checks_for(kind, dir.path()) {
                let mut words = c.split_whitespace();
                let head = words.next().unwrap();
                let probe = match (head, words.next()) {
                    // `go vet` is not a program; `go` is.
                    ("go", _) => "go version".to_string(),
                    // `test` is built into cargo, so cargo itself vouches for it.
                    // clippy and fmt are separate components and do not.
                    ("cargo", Some("test")) => "cargo --version".to_string(),
                    ("cargo", Some(sub)) => format!("cargo {sub} --version"),
                    ("uv", _) => "uv --version".to_string(),
                    _ => format!("{head} --version"),
                };
                assert!(
                    command_works(dir.path(), &probe),
                    "{kind:?}: written but does not run: {c}"
                );
            }
        }
    }

    #[test]
    fn cargo_test_is_not_written_without_cargo() {
        // It rode in unconditionally before, which made it the one Rust command
        // that could be written on a machine with no Rust at all.
        assert!(checks_with(ProjectKind::Rust, &[], &only(&[])).is_empty());
        assert_eq!(
            checks_with(ProjectKind::Rust, &[], &only(&["cargo --version"])),
            vec!["cargo test"]
        );
    }

    #[test]
    fn fmt_is_probed_separately_from_clippy() {
        // Separate rustup components: either can be absent while the other is
        // present, so neither vouches for the other.
        let checks = checks_with(
            ProjectKind::Rust,
            &[],
            &only(&["cargo --version", "cargo clippy --version"]),
        );
        assert_eq!(
            checks,
            vec!["cargo test", "cargo clippy --all-targets -- -D warnings"],
            "no fmt line without a fmt probe"
        );

        let hyphenated = checks_with(
            ProjectKind::Rust,
            &[],
            &only(&["cargo --version", "cargo-fmt --version"]),
        );
        assert_eq!(hyphenated, vec!["cargo test", "cargo-fmt --check"]);
    }

    #[test]
    fn a_recognized_project_with_no_toolchain_gets_no_commands() {
        for kind in [
            ProjectKind::Rust,
            ProjectKind::Node,
            ProjectKind::Python,
            ProjectKind::Go,
        ] {
            assert!(
                checks_with(kind, &["test".to_string()], &only(&[])).is_empty(),
                "{kind:?} must not write a command that cannot run"
            );
        }
    }

    #[test]
    fn node_checks_follow_declared_scripts() {
        let scripts = ["test".to_string(), "lint".to_string()];
        assert_eq!(
            checks_with(ProjectKind::Node, &scripts, &only(&["npm --version"])),
            vec!["npm run test", "npm run lint"]
        );
    }

    #[test]
    fn node_without_scripts_falls_back_to_npm_test() {
        assert_eq!(
            checks_with(ProjectKind::Node, &[], &only(&["npm --version"])),
            vec!["npm test"]
        );
    }

    #[test]
    fn a_malformed_package_json_does_not_panic() {
        let dir = repo_with(&[("package.json", "{ not json")]);
        // Through the real entry point, because parsing is what is under test.
        // Whether npm exists here decides between two shapes, and neither panics.
        let checks = checks_for(ProjectKind::Node, dir.path());
        assert!(
            checks.is_empty() || checks == vec!["npm test"],
            "{checks:?}"
        );
    }

    #[test]
    fn go_needs_only_one_probe_for_both_commands() {
        assert_eq!(
            checks_with(ProjectKind::Go, &[], &only(&["go version"])),
            vec!["go test ./...", "go vet ./..."]
        );
    }

    #[test]
    fn pytest_is_probed_when_there_is_no_uv() {
        assert_eq!(
            checks_with(ProjectKind::Python, &[], &only(&["uv --version"])),
            vec!["uv run pytest"]
        );
        assert_eq!(
            checks_with(ProjectKind::Python, &[], &only(&["pytest --version"])),
            vec!["pytest"]
        );
    }

    #[test]
    fn verbatim_lists_match_the_ecosystem() {
        assert_eq!(verbatim_for(ProjectKind::Rust), vec!["Cargo.lock"]);
        assert!(verbatim_for(ProjectKind::Node).contains(&"pnpm-lock.yaml".to_string()));
        assert!(verbatim_for(ProjectKind::Go).contains(&"go.sum".to_string()));
    }

    #[test]
    fn probing_a_missing_claude_reports_not_found() {
        let cfg = Config {
            claude_cmd: vec!["definitely-not-a-real-binary-xyz".into()],
            ..Config::default()
        };
        let report = probe_claude(&cfg);
        assert!(!report.found());
        assert_eq!(report.tool_flag_supported, None, "nothing to ask about");
    }

    #[test]
    fn the_tool_flag_check_reads_help_output() {
        // A stub whose --help mentions the flag rote passes.
        let dir = tempfile::tempdir().unwrap();
        let stub = dir.path().join("claude-stub");
        std::fs::write(
            &stub,
            format!(
                "#!/bin/sh\necho '  {} <tools...>  disable tools'\n",
                tool_flag_name()
            ),
        )
        .unwrap();
        let mut perms = std::fs::metadata(&stub).unwrap().permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o755);
        std::fs::set_permissions(&stub, perms).unwrap();

        let cfg = Config {
            claude_cmd: vec![stub.to_string_lossy().into_owned()],
            ..Config::default()
        };
        let report = probe_claude(&cfg);
        assert!(report.found());
        assert_eq!(report.tool_flag_supported, Some(true));
    }

    #[test]
    fn a_help_without_the_flag_is_reported_unsupported() {
        let dir = tempfile::tempdir().unwrap();
        let stub = dir.path().join("claude-stub");
        std::fs::write(&stub, "#!/bin/sh\necho 'usage: claude [options]'\n").unwrap();
        let mut perms = std::fs::metadata(&stub).unwrap().permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o755);
        std::fs::set_permissions(&stub, perms).unwrap();

        let cfg = Config {
            claude_cmd: vec![stub.to_string_lossy().into_owned()],
            ..Config::default()
        };
        assert_eq!(probe_claude(&cfg).tool_flag_supported, Some(false));
    }

    #[test]
    fn editor_probe_uses_the_configured_chain() {
        let cfg = Config {
            editor: "sh".into(),
            ..Config::default()
        };
        let report = probe_editor(&cfg);
        assert!(report.found());
        assert_eq!(report.command, vec!["sh".to_string()]);
    }
}
