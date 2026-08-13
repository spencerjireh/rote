//! ShadowManager: the clone lifecycle and the real → shadow sync.
//!
//! The shadow is a full local clone, never a worktree, so an agent running any
//! git command it likes cannot reach the real repository's metadata. See
//! ARCHITECTURE.md § Shadow mechanism and DESIGN.md §3.

use crate::config::{build_globset, Config};
use crate::git;
use crate::paths::{assert_not_in_real_tree, write_atomic, ProjectPaths};
use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};
use std::fs;
use std::path::Path;

/// Where the real repo's HEAD is parked inside the shadow.
pub const BASELINE_REF: &str = "refs/rote/baseline";

/// What the real tree looked like when the session started.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Baseline {
    pub head: String,
    pub uncommitted_digest: String,
    pub synced_at: String,
}

/// Create the shadow clone if it is not already there.
pub fn ensure_shadow(project: &ProjectPaths) -> Result<()> {
    assert_not_in_real_tree(&project.shadow_dir, &project.repo_root);

    if project.shadow_dir.join(".git").exists() {
        return Ok(());
    }
    if project.shadow_dir.exists() {
        // A directory without .git is a half-made or corrupted shadow.
        fs::remove_dir_all(&project.shadow_dir)
            .with_context(|| format!("cannot clear {}", project.shadow_dir.display()))?;
    }
    git::clone_no_hardlinks(&project.repo_root, &project.shadow_dir)?;
    // No remotes at all: `git push` in the shadow then fails harmlessly rather
    // than pushing the agent's work somewhere real.
    git::remove_all_remotes(&project.shadow_dir)?;
    Ok(())
}

/// Refuse to act while the real repo is mid-operation.
pub fn ensure_no_operation_in_progress(project: &ProjectPaths) -> Result<()> {
    if let Some(op) = git::in_progress_operation(&project.repo_root)? {
        bail!(
            "the repository is in the middle of {op}.\n\
             Finish or abort it first, then re-run rote."
        );
    }
    Ok(())
}

/// Sync real → shadow. DESIGN.md §3, steps 1–8.
///
/// After this returns, the shadow working tree matches the real working tree
/// byte for byte across all source — modulo ignored files not on the copy
/// allowlist, and the preserved build directories.
pub fn sync(project: &ProjectPaths, cfg: &Config) -> Result<Baseline> {
    ensure_no_operation_in_progress(project)?;
    ensure_shadow(project)?;

    let real = &project.repo_root;
    let shadow = &project.shadow_dir;
    assert_not_in_real_tree(shadow, real);

    // 1. Real: resolve HEAD.
    let head = git::head_commit(real)?;

    // 2. Shadow: fetch that commit by explicit path and reset onto it detached.
    //    Order matters — detach in place first, then reset. Detaching *at* the
    //    baseline ref would need to move files and would refuse over the
    //    previous sync's uncommitted changes; `reset --hard` has no such qualms,
    //    and detaching first keeps the reset off any branch pointer.
    git::fetch_head_to_ref(shadow, real, BASELINE_REF)?;
    git::detach_head(shadow)?;
    git::reset_hard(shadow, BASELINE_REF)?;

    // 3. Shadow: clean, preserving build output so the agent's builds stay warm.
    git::clean(shadow, &cfg.shadow_preserve)?;

    // 4. Real: carry staged + unstaged changes across. Retained for steps 7–8.
    let uncommitted = git::diff_head_binary(real)?;
    git::apply_patch(shadow, &uncommitted).with_context(|| {
        format!(
            "cannot apply the real tree's uncommitted changes to the shadow at {}.\n\
             The shadow is disposable — `rm -rf {}` and re-run to rebuild it.",
            shadow.display(),
            shadow.display()
        )
    })?;

    // 5. Untracked-but-not-ignored files.
    for rel in git::untracked_files(real)? {
        copy_into_shadow(&real.join(&rel), &shadow.join(&rel))?;
    }

    // 6. Allowlisted gitignored files the agent's builds need.
    for pattern in &cfg.shadow_copy {
        let src = real.join(pattern);
        if src.exists() {
            copy_into_shadow(&src, &shadow.join(pattern))?;
        }
    }

    // 7. Persist the session-start baseline for the reviewer (§8 reads this).
    let patch_path = project.baseline_patch();
    assert_not_in_real_tree(&patch_path, real);
    write_atomic(&patch_path, &uncommitted)?;

    // 8. Report what the caller records in the manifest.
    Ok(Baseline {
        head,
        uncommitted_digest: digest_of(&uncommitted),
        synced_at: crate::now_iso8601(),
    })
}

/// SHA-256 of the uncommitted diff, or the empty string when there is none.
///
/// Archive and debugging only — deliberately not a drift signal, since it
/// changes the moment the user types their first character (DESIGN.md §9.2).
fn digest_of(bytes: &[u8]) -> String {
    if bytes.is_empty() {
        return String::new();
    }
    let mut h = Sha256::new();
    h.update(bytes);
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

/// Copy one path into the shadow, reproducing symlinks as symlinks rather than
/// following them (DESIGN.md §9.9).
fn copy_into_shadow(src: &Path, dst: &Path) -> Result<()> {
    if let Some(parent) = dst.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("cannot create {}", parent.display()))?;
    }
    let meta =
        fs::symlink_metadata(src).with_context(|| format!("cannot stat {}", src.display()))?;

    if meta.file_type().is_symlink() {
        let target =
            fs::read_link(src).with_context(|| format!("cannot read link {}", src.display()))?;
        // Replace whatever is there; the shadow is ours to overwrite.
        let _ = fs::remove_file(dst);
        std::os::unix::fs::symlink(&target, dst)
            .with_context(|| format!("cannot link {} -> {}", dst.display(), target.display()))?;
        return Ok(());
    }

    if meta.is_dir() {
        fs::create_dir_all(dst).with_context(|| format!("cannot create {}", dst.display()))?;
        return Ok(());
    }

    fs::copy(src, dst)
        .with_context(|| format!("cannot copy {} -> {}", src.display(), dst.display()))?;
    Ok(())
}

/// Paths the shadow's clean must leave alone, as a matcher.
pub fn preserve_matcher(cfg: &Config) -> Result<globset::GlobSet> {
    build_globset(&cfg.shadow_preserve, "shadow.preserve")
}

/// The agent's work that never made it into the real tree, as one patch.
///
/// Written to `archive/<timestamp>.patch` before `done` and `abort` sync the
/// shadow away — that sync is the only thing standing between a skipped hunk and
/// oblivion, so the residue is captured first (DESIGN.md §1).
pub fn residue_diff(project: &ProjectPaths, cfg: &Config) -> Result<Vec<u8>> {
    let real = &project.repo_root;
    let shadow = &project.shadow_dir;
    if !shadow.join(".git").exists() {
        return Ok(Vec::new());
    }

    let mut candidates = git::status_paths(shadow)?;
    candidates.extend(git::status_paths(real)?);
    candidates.sort();
    candidates.dedup();

    let devnull = std::path::PathBuf::from("/dev/null");
    let mut out = Vec::new();
    for rel in candidates {
        let real_path = real.join(&rel);
        let shadow_path = shadow.join(&rel);
        let real_bytes = std::fs::read(&real_path).ok();
        let shadow_bytes = std::fs::read(&shadow_path).ok();
        if real_bytes == shadow_bytes {
            continue;
        }
        let a = if real_bytes.is_some() {
            &real_path
        } else {
            &devnull
        };
        let b = if shadow_bytes.is_some() {
            &shadow_path
        } else {
            &devnull
        };
        let raw = git::diff_no_index(a, b, crate::hunks::CONTEXT_LINES)?;
        // Repo-relative headers, so the archived patch can actually be applied.
        out.extend(git::relativize_no_index_diff(&raw, &rel.to_string_lossy()));
    }
    let _ = cfg;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn digest_is_empty_for_empty_diff() {
        assert_eq!(digest_of(b""), "");
    }

    #[test]
    fn digest_is_stable_and_sensitive() {
        let a = digest_of(b"diff --git a/x b/x\n");
        let b = digest_of(b"diff --git a/x b/x\n");
        let c = digest_of(b"diff --git a/y b/y\n");
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert_eq!(a.len(), 64);
    }
}
