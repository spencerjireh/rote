//! GitPlumbing: every git invocation rote makes, and nothing else.
//!
//! Principle 3 — git is plumbing, never surface. The user never types a git
//! command through rote, and no other module shells out to git.

use anyhow::{bail, Context, Result};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Raw result of one git invocation.
#[derive(Debug)]
pub struct GitOutput {
    pub stdout: Vec<u8>,
    pub stderr: String,
    pub code: i32,
}

impl GitOutput {
    pub fn stdout_utf8(&self) -> String {
        String::from_utf8_lossy(&self.stdout).into_owned()
    }

    pub fn stdout_trimmed(&self) -> String {
        self.stdout_utf8().trim().to_string()
    }
}

/// Run git, accepting only the listed exit codes.
///
/// `ok_codes` exists because git overloads exit status: `diff --no-index`
/// returns 1 to mean "the files differ", which is the normal path for us, not a
/// failure. Encoding that per-invocation here keeps every caller from having to
/// remember which codes are benign.
fn run_in(dir: Option<&Path>, args: &[&str], ok_codes: &[i32]) -> Result<GitOutput> {
    run_with_stdin(dir, args, ok_codes, None)
}

fn run_with_stdin(
    dir: Option<&Path>,
    args: &[&str],
    ok_codes: &[i32],
    stdin_bytes: Option<&[u8]>,
) -> Result<GitOutput> {
    let mut cmd = Command::new("git");
    if let Some(d) = dir {
        cmd.arg("-C").arg(d);
    }
    cmd.args(args);
    cmd.stdin(if stdin_bytes.is_some() {
        Stdio::piped()
    } else {
        Stdio::null()
    });
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());

    let mut child = cmd
        .spawn()
        .with_context(|| format!("cannot run `git {}`", args.join(" ")))?;

    if let Some(bytes) = stdin_bytes {
        child
            .stdin
            .as_mut()
            .context("git stdin unavailable")?
            .write_all(bytes)
            .context("cannot write to git stdin")?;
        // Dropping closes the pipe so git sees EOF.
        drop(child.stdin.take());
    }

    let out = child
        .wait_with_output()
        .with_context(|| format!("`git {}` failed to complete", args.join(" ")))?;
    let code = out.status.code().unwrap_or(-1);
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();

    if !ok_codes.contains(&code) {
        let where_ = dir
            .map(|d| format!(" in {}", d.display()))
            .unwrap_or_default();
        bail!(
            "git {}{} failed (exit {code}):\n{}",
            args.join(" "),
            where_,
            stderr.trim()
        );
    }

    Ok(GitOutput {
        stdout: out.stdout,
        stderr,
        code,
    })
}

/// Resolve the repository's git directory (handles worktrees and `.git` files).
pub fn git_dir(repo: &Path) -> Result<PathBuf> {
    let out = run_in(Some(repo), &["rev-parse", "--absolute-git-dir"], &[0])?;
    Ok(PathBuf::from(out.stdout_trimmed()))
}

/// The commit `HEAD` points at.
pub fn head_commit(repo: &Path) -> Result<String> {
    let out = run_in(Some(repo), &["rev-parse", "HEAD"], &[0, 128])?;
    if out.code == 128 {
        bail!(
            "{} has no commits yet.\n\
             rote needs a commit to anchor the shadow against — make one first.",
            repo.display()
        );
    }
    Ok(out.stdout_trimmed())
}

/// An interrupted git operation in the real repo, if any.
///
/// Sync cannot run while one of these is in flight, so `start` and `done` both
/// check before doing any work (DESIGN.md §1).
pub fn in_progress_operation(repo: &Path) -> Result<Option<&'static str>> {
    let gd = git_dir(repo)?;
    let checks: [(&str, &str); 5] = [
        ("MERGE_HEAD", "a merge"),
        ("rebase-merge", "a rebase"),
        ("rebase-apply", "a rebase"),
        ("CHERRY_PICK_HEAD", "a cherry-pick"),
        ("BISECT_LOG", "a bisect"),
    ];
    for (entry, label) in checks {
        if gd.join(entry).exists() {
            return Ok(Some(label));
        }
    }
    Ok(None)
}

/// Clone the real repo into the shadow location.
///
/// `--no-hardlinks` trades disk for isolation: an agent running `git gc` in the
/// shadow must not be able to touch the real repo's object store.
pub fn clone_no_hardlinks(src: &Path, dst: &Path) -> Result<()> {
    if let Some(parent) = dst.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("cannot create {}", parent.display()))?;
    }
    run_in(
        None,
        &[
            "clone",
            "--no-hardlinks",
            "--quiet",
            &src.to_string_lossy(),
            &dst.to_string_lossy(),
        ],
        &[0],
    )?;
    Ok(())
}

pub fn remotes(repo: &Path) -> Result<Vec<String>> {
    let out = run_in(Some(repo), &["remote"], &[0])?;
    Ok(out
        .stdout_utf8()
        .lines()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
        .collect())
}

/// Remove every remote, so a stray `git push` in the shadow fails harmlessly.
pub fn remove_all_remotes(repo: &Path) -> Result<()> {
    for name in remotes(repo)? {
        run_in(Some(repo), &["remote", "remove", &name], &[0])?;
    }
    Ok(())
}

/// Fetch the real repo's HEAD into a local ref by explicit path (no remote).
pub fn fetch_head_to_ref(shadow: &Path, real: &Path, refname: &str) -> Result<()> {
    let refspec = format!("+HEAD:{refname}");
    run_in(
        Some(shadow),
        &["fetch", "--quiet", &real.to_string_lossy(), &refspec],
        &[0],
    )?;
    Ok(())
}

/// Detach HEAD at the commit it already points at.
///
/// Deliberately takes no ref. `checkout --detach <ref>` would have to move files
/// into place and so refuses when the working tree is dirty — which it always is
/// on the second sync, since the first one applied the real tree's uncommitted
/// changes. Detaching in place touches no files and always succeeds; the
/// subsequent `reset --hard` is what moves HEAD onto the baseline.
pub fn detach_head(repo: &Path) -> Result<()> {
    run_in(Some(repo), &["checkout", "--quiet", "--detach"], &[0])?;
    Ok(())
}

pub fn reset_hard(repo: &Path, refname: &str) -> Result<()> {
    run_in(Some(repo), &["reset", "--quiet", "--hard", refname], &[0])?;
    Ok(())
}

/// `git clean -fdx`, excluding the caller's preserve patterns.
pub fn clean(repo: &Path, exclude: &[String]) -> Result<()> {
    let mut args: Vec<String> = vec!["clean".into(), "-fdxq".into()];
    for pat in exclude {
        args.push("-e".into());
        args.push(pat.clone());
    }
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    run_in(Some(repo), &refs, &[0])?;
    Ok(())
}

/// Staged + unstaged changes against HEAD, binary-safe.
pub fn diff_head_binary(repo: &Path) -> Result<Vec<u8>> {
    let out = run_in(
        Some(repo),
        &["diff", "HEAD", "--binary", "--no-color", "--no-ext-diff"],
        &[0, 1],
    )?;
    Ok(out.stdout)
}

/// Apply a patch to the working tree.
pub fn apply_patch(repo: &Path, patch: &[u8]) -> Result<()> {
    if patch.is_empty() {
        return Ok(());
    }
    run_with_stdin(
        Some(repo),
        &["apply", "--whitespace=nowarn", "-"],
        &[0],
        Some(patch),
    )?;
    Ok(())
}

/// Untracked-but-not-ignored paths, relative to the repo root.
pub fn untracked_files(repo: &Path) -> Result<Vec<PathBuf>> {
    let out = run_in(
        Some(repo),
        &["ls-files", "--others", "--exclude-standard", "-z"],
        &[0],
    )?;
    Ok(split_nul_paths(&out.stdout))
}

/// Every path git reports as staged, unstaged, or untracked-not-ignored.
///
/// `status --porcelain=v1 -z` is the single enumeration source (DESIGN.md §5);
/// `ls-files -mo` is deliberately not used, since it reports a different set.
pub fn status_paths(repo: &Path) -> Result<Vec<PathBuf>> {
    let out = run_in(
        Some(repo),
        &["status", "--porcelain=v1", "-z", "--untracked-files=all"],
        &[0],
    )?;
    Ok(parse_status_z(&out.stdout))
}

/// Diff two paths outside any repository context.
///
/// Exit 1 means "they differ" and is the expected result, not an error.
/// `--no-textconv` / `--no-ext-diff` keep this a byte comparison rather than a
/// comparison of some filter's rendering (DESIGN.md §5, §9.11).
pub fn diff_no_index(a: &Path, b: &Path, unified: usize) -> Result<Vec<u8>> {
    let unified_arg = format!("--unified={unified}");
    let out = run_in(
        None,
        &[
            "diff",
            "--no-index",
            "--no-textconv",
            "--no-ext-diff",
            "--no-color",
            &unified_arg,
            "--histogram",
            "--",
            &a.to_string_lossy(),
            &b.to_string_lossy(),
        ],
        &[0, 1],
    )?;
    Ok(out.stdout)
}

/// Check out a commit into a scratch directory as a detached worktree.
///
/// Used to rebuild the session's starting state for the reviewer's diff.
pub fn materialize_worktree(repo: &Path, commit: &str, dest: &Path) -> Result<()> {
    run_in(
        Some(repo),
        &[
            "worktree",
            "add",
            "--detach",
            "--force",
            &dest.to_string_lossy(),
            commit,
        ],
        &[0],
    )?;
    Ok(())
}

/// Forget worktrees whose directories are gone.
pub fn prune_worktrees(repo: &Path) -> Result<()> {
    run_in(Some(repo), &["worktree", "prune"], &[0])?;
    Ok(())
}

/// Rewrite a `--no-index` diff's headers to repo-relative paths.
///
/// `--no-index` names both operands by the path it was handed, so a raw diff
/// reads `a/Users/…/repo/x.rs b/Users/…/cache/rote/…/shadow/x.rs`. That is
/// unreadable and, more to the point, cannot be `git apply`-ed anywhere. The
/// archived residue patch exists to be recoverable, so the headers are rewritten
/// to the ordinary `a/x.rs` / `b/x.rs` form. `/dev/null` sides are left alone —
/// they are what marks an add or a delete.
pub fn relativize_no_index_diff(diff: &[u8], rel: &str) -> Vec<u8> {
    let text = String::from_utf8_lossy(diff);
    let mut out = String::with_capacity(text.len());
    for line in text.lines() {
        if line.starts_with("diff --git ") {
            out.push_str(&format!("diff --git a/{rel} b/{rel}"));
        } else if let Some(rest) = line.strip_prefix("--- ") {
            out.push_str(&rewrite_side("---", rest, rel));
        } else if let Some(rest) = line.strip_prefix("+++ ") {
            out.push_str(&rewrite_side("+++", rest, rel));
        } else {
            out.push_str(line);
        }
        out.push('\n');
    }
    out.into_bytes()
}

fn rewrite_side(marker: &str, rest: &str, rel: &str) -> String {
    let path = rest.split('\t').next().unwrap_or(rest).trim();
    if path == "/dev/null" {
        return format!("{marker} /dev/null");
    }
    let prefix = if marker == "---" { "a" } else { "b" };
    format!("{marker} {prefix}/{rel}")
}

fn split_nul_paths(bytes: &[u8]) -> Vec<PathBuf> {
    bytes
        .split(|b| *b == 0)
        .filter(|s| !s.is_empty())
        .map(|s| PathBuf::from(String::from_utf8_lossy(s).into_owned()))
        .collect()
}

/// Parse `status --porcelain=v1 -z`.
///
/// Entries are `XY <path>\0`, except renames/copies which append a second
/// `<origin>\0` field that must be consumed as data, not read as a new entry.
fn parse_status_z(bytes: &[u8]) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut fields = bytes.split(|b| *b == 0).filter(|s| !s.is_empty());
    while let Some(entry) = fields.next() {
        if entry.len() < 4 {
            continue;
        }
        let status = &entry[..2];
        let path = &entry[3..];
        out.push(PathBuf::from(String::from_utf8_lossy(path).into_owned()));
        // R/C carry an extra origin-path field.
        if status.contains(&b'R') || status.contains(&b'C') {
            if let Some(origin) = fields.next() {
                out.push(PathBuf::from(String::from_utf8_lossy(origin).into_owned()));
            }
        }
    }
    out.sort();
    out.dedup();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_status_entries() {
        let raw = b" M src/main.rs\0?? new.txt\0A  staged.rs\0";
        let got = parse_status_z(raw);
        assert_eq!(
            got,
            vec![
                PathBuf::from("new.txt"),
                PathBuf::from("src/main.rs"),
                PathBuf::from("staged.rs"),
            ]
        );
    }

    #[test]
    fn rename_entry_consumes_its_origin_field() {
        // Without consuming the origin, "old.rs" would be misread as a new entry
        // with "ol" as its status code.
        let raw = b"R  new.rs\0old.rs\0?? other.txt\0";
        let got = parse_status_z(raw);
        assert_eq!(
            got,
            vec![
                PathBuf::from("new.rs"),
                PathBuf::from("old.rs"),
                PathBuf::from("other.txt"),
            ]
        );
    }

    #[test]
    fn splits_nul_separated_paths() {
        let raw = b"a.txt\0dir/b.txt\0";
        assert_eq!(
            split_nul_paths(raw),
            vec![PathBuf::from("a.txt"), PathBuf::from("dir/b.txt")]
        );
    }

    #[test]
    fn relativize_rewrites_absolute_no_index_headers() {
        let raw = b"diff --git a/tmp/repo/src/x.rs b/tmp/cache/shadow/src/x.rs\n\
                    index f79c691..7527576 100644\n\
                    --- a/tmp/repo/src/x.rs\n\
                    +++ b/tmp/cache/shadow/src/x.rs\n\
                    @@ -1,2 +1,3 @@\n fn main() {\n+    work();\n }\n";
        let out = String::from_utf8(relativize_no_index_diff(raw, "src/x.rs")).unwrap();
        assert!(out.contains("diff --git a/src/x.rs b/src/x.rs"), "{out}");
        assert!(out.contains("--- a/src/x.rs"), "{out}");
        assert!(out.contains("+++ b/src/x.rs"), "{out}");
        assert!(
            !out.contains("/tmp/cache"),
            "no absolute paths survive: {out}"
        );
        // Body is untouched.
        assert!(out.contains("+    work();"));
        assert!(out.contains("index f79c691..7527576"));
    }

    #[test]
    fn relativize_preserves_dev_null_sides() {
        let raw = b"diff --git a/dev/null b/tmp/shadow/new.rs\n\
                    --- /dev/null\n+++ b/tmp/shadow/new.rs\n@@ -0,0 +1 @@\n+added\n";
        let out = String::from_utf8(relativize_no_index_diff(raw, "new.rs")).unwrap();
        assert!(
            out.contains("--- /dev/null"),
            "the add marker must survive: {out}"
        );
        assert!(out.contains("+++ b/new.rs"), "{out}");
    }

    #[test]
    fn empty_status_yields_nothing() {
        assert!(parse_status_z(b"").is_empty());
        assert!(split_nul_paths(b"").is_empty());
    }
}
