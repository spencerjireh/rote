//! `rote doctor`: does this machine have what rote needs?
//!
//! Read-only, and deliberately runs outside a repository — it is the command you
//! reach for when something is wrong, which is often before you have a repo to
//! be in. Every line either passes or names the command that fixes it, and the
//! exit code is non-zero if anything needs attention, so it works as a script
//! gate.
//!
//! Takes the repo root already resolved, as an `Option`, rather than doing its
//! own discovery: "not in a repository" is a reported state here, not an error,
//! so there is nothing for this module to do with the failure.

use crate::config::Config;
use crate::detect;
use crate::git;
use crate::model;
use crate::paths::{self, ProjectPaths};
use anyhow::Result;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

/// One line of `rote doctor` output.
struct Check {
    label: &'static str,
    detail: String,
    state: CheckState,
    /// What to run to fix it.
    fix: Option<String>,
}

enum CheckState {
    Ok,
    Failed,
    /// Not applicable here — reported, but never a failure.
    Skipped,
}

impl Check {
    fn ok(label: &'static str, detail: impl Into<String>) -> Self {
        Self {
            label,
            detail: detail.into(),
            state: CheckState::Ok,
            fix: None,
        }
    }
    fn failed(label: &'static str, detail: impl Into<String>, fix: impl Into<String>) -> Self {
        Self {
            label,
            detail: detail.into(),
            state: CheckState::Failed,
            fix: Some(fix.into()),
        }
    }
    fn skipped(label: &'static str, detail: impl Into<String>) -> Self {
        Self {
            label,
            detail: detail.into(),
            state: CheckState::Skipped,
            fix: None,
        }
    }
}

/// Diagnose this machine. Read-only, and works outside a repository.
pub fn run(repo_root: Option<PathBuf>, cfg: &Config, deep: bool, color: bool) -> Result<ExitCode> {
    let mut checks = Vec::new();

    // git
    match std::process::Command::new("git").arg("--version").output() {
        Ok(out) if out.status.success() => {
            let v = String::from_utf8_lossy(&out.stdout);
            checks.push(Check::ok(
                "git",
                v.trim().trim_start_matches("git version "),
            ));
        }
        _ => checks.push(Check::failed("git", "not found", "install git")),
    }

    // claude, and the reviewer flag it will be handed
    let mut claude = detect::probe_claude(cfg);
    if deep && claude.found() {
        detect::deep_probe_claude(cfg, &mut claude);
    }
    match &claude.resolved {
        Some(path) => {
            checks.push(Check::ok("claude", tilde(path)));
            let flag = model::TOOL_FLAGS.first().copied().unwrap_or("--tools");
            match claude.tool_flag_supported {
                Some(true) => checks.push(Check::ok(
                    "reviewer tools",
                    format!("{flag} present in --help"),
                )),
                _ => checks.push(Check::failed(
                    "reviewer tools",
                    format!("{flag} not found in --help"),
                    "review will fail soft; rote done still works with --no-review",
                )),
            }
            match claude.deep_ok {
                Some(true) => checks.push(Check::ok("reviewer probe", "ran end to end")),
                Some(false) => checks.push(Check::failed(
                    "reviewer probe",
                    "invocation failed",
                    "check `claude -p` works, or run rote done --no-review",
                )),
                None => {}
            }
        }
        None => checks.push(Check::failed(
            "claude",
            format!("{} not on PATH", cfg.claude_cmd.join(" ")),
            "install claude, or set claude_cmd via `rote setup`",
        )),
    }

    // editor
    // The editor is informational now, never a failure. rote does not launch
    // one as part of the loop — it only offers to, from the `o` key in the
    // watch pane — so a machine without one is a machine that transcribes
    // perfectly well, and `doctor` must not exit non-zero over it.
    let editor = detect::probe_editor(cfg);
    match &editor.resolved {
        Some(p) => checks.push(Check::ok(
            "editor",
            format!("{} ({})", editor.command.join(" "), tilde(p)),
        )),
        None => checks.push(Check::skipped(
            "editor",
            format!(
                "{} not on PATH — the open action in `rote watch` will not work",
                editor.command.join(" ")
            ),
        )),
    }

    // global config
    let global = paths::global_config_path()?;
    if global.is_file() {
        checks.push(Check::ok("global config", tilde(&global)));
    } else {
        checks.push(Check::failed("global config", "missing", "rote setup"));
    }

    // Repository-scoped checks degrade when we are not in one.
    match repo_root {
        Some(repo_root) => {
            let commits = git::head_commit(&repo_root)
                .map(|_| "has commits".to_string())
                .unwrap_or_else(|_| "no commits yet".to_string());
            let has_commits = commits == "has commits";
            if has_commits {
                checks.push(Check::ok(
                    "repository",
                    format!("{}  ({commits})", tilde(&repo_root)),
                ));
            } else {
                checks.push(Check::failed(
                    "repository",
                    format!("{}  ({commits})", tilde(&repo_root)),
                    "make a commit — rote needs one to anchor the shadow",
                ));
            }

            match ProjectPaths::resolve(&repo_root) {
                Ok(project) => {
                    checks.push(Check::ok("shadow location", tilde(&project.shadow_dir)));
                    let pc = project.project_config();
                    if pc.is_file() {
                        let cfg_here =
                            Config::load(&paths::global_config_path()?, &pc).unwrap_or_default();
                        let n = cfg_here.check_commands.len();
                        let detail = if n == 0 {
                            format!("{}  (no checks configured)", tilde(&pc))
                        } else {
                            format!("{}  ({n} check(s))", tilde(&pc))
                        };
                        checks.push(Check::ok("project config", detail));
                    } else {
                        checks.push(Check::failed("project config", "missing", "rote init"));
                    }
                }
                Err(e) => checks.push(Check::failed(
                    "shadow location",
                    first_line(&format!("{e:#}")),
                    "set XDG_CACHE_HOME outside the repository",
                )),
            }
        }
        None => {
            checks.push(Check::skipped("repository", "not in a git repository"));
            checks.push(Check::skipped("shadow location", "n/a"));
            checks.push(Check::skipped("project config", "n/a"));
        }
    }

    // Render.
    let width = checks.iter().map(|c| c.label.len()).max().unwrap_or(0);
    let mut failures = 0;
    for c in &checks {
        let (mark, painted) = match c.state {
            CheckState::Ok => ("ok", paint_green("ok", color)),
            CheckState::Failed => {
                failures += 1;
                ("FAIL", paint_red("FAIL", color))
            }
            CheckState::Skipped => ("--", paint_dim("--", color)),
        };
        let _ = mark;
        println!(
            "  {:<width$}  {:<44}  {}",
            c.label,
            truncate(&c.detail, 44),
            painted,
            width = width
        );
        if let Some(fix) = &c.fix {
            println!("  {:<width$}  → {fix}", "", width = width);
        }
    }

    if failures == 0 {
        println!("\nall good.");
        Ok(ExitCode::SUCCESS)
    } else {
        println!("\n{failures} check(s) need attention.");
        Ok(ExitCode::FAILURE)
    }
}

fn first_line(s: &str) -> String {
    s.lines().next().unwrap_or(s).to_string()
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let keep: String = s.chars().skip(s.chars().count() - max + 1).collect();
    format!("…{keep}")
}

/// Shorten a path under $HOME to `~/…` for display.
fn tilde(path: &Path) -> String {
    let Some(home) = directories::BaseDirs::new().map(|d| d.home_dir().to_path_buf()) else {
        return path.display().to_string();
    };
    match path.strip_prefix(&home) {
        Ok(rest) => format!("~/{}", rest.display()),
        Err(_) => path.display().to_string(),
    }
}

fn paint_green(s: &str, color: bool) -> String {
    use owo_colors::OwoColorize;
    if color {
        s.green().to_string()
    } else {
        s.to_string()
    }
}
fn paint_red(s: &str, color: bool) -> String {
    use owo_colors::OwoColorize;
    if color {
        s.red().to_string()
    } else {
        s.to_string()
    }
}
fn paint_dim(s: &str, color: bool) -> String {
    use owo_colors::OwoColorize;
    if color {
        s.dimmed().to_string()
    } else {
        s.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The detail column is fixed-width, so a long path is shortened from the
    /// *left*: the tail of a path is what identifies it.
    #[test]
    fn a_long_detail_keeps_its_tail_and_fits_the_column() {
        assert_eq!(truncate("short", 44), "short");
        let long = "/very/long/path/that/will/not/fit/in/the/column/config.toml";
        let out = truncate(long, 44);
        assert_eq!(out.chars().count(), 44);
        assert!(out.starts_with('…'));
        assert!(out.ends_with("config.toml"));
    }

    /// Counted in chars, not bytes. Slicing this by byte offset would panic on
    /// any non-ASCII path, which is a crash in the command people run *because*
    /// something is already wrong.
    #[test]
    fn truncating_counts_characters() {
        let s = "ünïcödé".repeat(20);
        let out = truncate(&s, 10);
        assert_eq!(out.chars().count(), 10);
    }

    /// git and claude answer `--version` with a banner; only the first line of
    /// it belongs in a one-line check.
    #[test]
    fn only_the_first_line_of_a_version_banner_is_reported() {
        assert_eq!(
            first_line("git version 2.39\nextra\nmore"),
            "git version 2.39"
        );
        assert_eq!(first_line("no newline"), "no newline");
        assert_eq!(first_line(""), "");
    }

    #[test]
    fn a_path_outside_home_is_left_alone() {
        assert_eq!(tilde(Path::new("/usr/bin/git")), "/usr/bin/git");
    }
}
