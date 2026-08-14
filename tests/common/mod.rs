//! Fixture scaffolding for integration tests.
//!
//! These helpers shell out to git directly rather than through `rote::git`.
//! The "all git goes through git.rs" rule governs rote's runtime behavior; this
//! is test scaffolding that *builds the repositories rote is then run against*,
//! and it needs env control that production code has no business exposing.
//!
//! Every invocation is hermetic: no global or system gitconfig, explicit
//! identity, fixed default branch. A developer's `commit.gpgsign = true` or a
//! CI box with no identity configured must not change these results.

#![allow(dead_code)]

pub mod cli;
pub mod daemon;

use std::path::{Path, PathBuf};
use std::process::Command;
use tempfile::TempDir;

fn git_command(dir: &Path, args: &[&str]) -> Command {
    let mut cmd = Command::new("git");
    cmd.arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_AUTHOR_NAME", "rote test")
        .env("GIT_AUTHOR_EMAIL", "test@example.invalid")
        .env("GIT_COMMITTER_NAME", "rote test")
        .env("GIT_COMMITTER_EMAIL", "test@example.invalid")
        .env("GIT_AUTHOR_DATE", "2026-01-01T00:00:00Z")
        .env("GIT_COMMITTER_DATE", "2026-01-01T00:00:00Z");
    cmd
}

pub fn git(dir: &Path, args: &[&str]) -> String {
    let out = git_command(dir, args)
        .output()
        .unwrap_or_else(|e| panic!("cannot run git {args:?}: {e}"));
    assert!(
        out.status.success(),
        "git {args:?} failed in {}: {}",
        dir.display(),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// Run git where failure is the point (a deliberately conflicting merge).
///
/// Still carries the full hermetic env: without an identity, git aborts on
/// "committer identity unknown" *before* writing MERGE_HEAD, which looks like a
/// conflict to an exit-code check but leaves no interrupted operation behind.
pub fn git_allow_fail(dir: &Path, args: &[&str]) -> (bool, String) {
    let out = git_command(dir, args)
        .output()
        .unwrap_or_else(|e| panic!("cannot run git {args:?}: {e}"));
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// A temp git repo plus isolated XDG roots, so nothing touches the real
/// `~/.local/share/rote` or `~/.cache/rote`.
pub struct Fixture {
    pub root: TempDir,
    pub repo: PathBuf,
    pub xdg_cache: PathBuf,
    pub xdg_data: PathBuf,
    pub xdg_config: PathBuf,
}

impl Fixture {
    pub fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let base = root.path().canonicalize().unwrap();
        let repo = base.join("repo");
        let xdg_cache = base.join("xdg-cache");
        let xdg_data = base.join("xdg-data");
        let xdg_config = base.join("xdg-config");
        for d in [&repo, &xdg_cache, &xdg_data, &xdg_config] {
            std::fs::create_dir_all(d).unwrap();
        }

        Command::new("git")
            .args(["-c", "init.defaultBranch=main", "init", "--quiet"])
            .arg(&repo)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .output()
            .expect("git init");

        Self {
            root,
            repo,
            xdg_cache,
            xdg_data,
            xdg_config,
        }
    }

    /// Resolve rote's paths against this fixture's isolated roots.
    ///
    /// Explicit roots rather than XDG env vars, so tests stay parallel-safe.
    pub fn project(&self) -> rote::paths::ProjectPaths {
        rote::paths::ProjectPaths::resolve_in(&self.repo, &self.xdg_cache, &self.xdg_data).unwrap()
    }

    pub fn write(&self, rel: &str, body: &str) {
        let p = self.repo.join(rel);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&p, body).unwrap();
    }

    pub fn read(&self, rel: &str) -> String {
        std::fs::read_to_string(self.repo.join(rel)).unwrap()
    }

    pub fn git(&self, args: &[&str]) -> String {
        git(&self.repo, args)
    }

    pub fn commit_all(&self, message: &str) {
        self.git(&["add", "-A"]);
        self.git(&["commit", "--quiet", "-m", message]);
    }
}

/// Walk a tree into a sorted (relative path, contents) list, so two trees can be
/// compared byte for byte. `.git` is skipped: the shadow's history is its own.
pub fn snapshot_tree(root: &Path, skip: &[&str]) -> Vec<(String, Vec<u8>)> {
    let mut out = Vec::new();
    walk(root, root, skip, &mut out);
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

fn walk(base: &Path, dir: &Path, skip: &[&str], out: &mut Vec<(String, Vec<u8>)>) {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let rel = path
            .strip_prefix(base)
            .unwrap()
            .to_string_lossy()
            .to_string();
        if rel == ".git"
            || skip
                .iter()
                .any(|s| rel == *s || rel.starts_with(&format!("{s}/")))
        {
            continue;
        }
        let meta = match std::fs::symlink_metadata(&path) {
            Ok(m) => m,
            Err(_) => continue,
        };
        if meta.file_type().is_symlink() {
            let target = std::fs::read_link(&path).unwrap();
            out.push((
                format!("{rel} -> symlink"),
                target.to_string_lossy().as_bytes().to_vec(),
            ));
        } else if meta.is_dir() {
            walk(base, &path, skip, out);
        } else {
            out.push((rel, std::fs::read(&path).unwrap_or_default()));
        }
    }
}
