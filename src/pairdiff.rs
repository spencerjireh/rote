//! Two trees, compared file by file.
//!
//! Three callers need the same walk — the hunk queue (real → shadow), the
//! archived residue (real → shadow), and the reviewer's diff (baseline → real) —
//! and each needs it slightly differently. What they share is the part that is
//! easy to get subtly wrong: which paths are candidates, when a file is absent
//! rather than empty, and what a `--no-index` diff's headers have to say for the
//! result to be applicable. All of that lives here; `git.rs` stays a thin
//! adapter over the subprocess.

use crate::git;
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

/// One file that differs between the trees.
///
/// `None` on a side means the file is absent there, which is what makes an add
/// or a delete distinguishable from an empty file. Both sides' bytes are carried
/// because every caller wants them: to sniff for binary, to compare, or to skip.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Changed {
    pub rel: PathBuf,
    pub left: Option<Vec<u8>>,
    pub right: Option<Vec<u8>>,
}

impl Changed {
    pub fn left_exists(&self) -> bool {
        self.left.is_some()
    }

    pub fn right_exists(&self) -> bool {
        self.right.is_some()
    }
}

/// Two trees to compare, and where the candidate paths come from.
pub struct Pair<'a> {
    pub left: &'a Path,
    pub right: &'a Path,
    /// Trees whose `git status` supplies candidates. Usually both sides; the
    /// reviewer's baseline worktree is a throwaway checkout with no interesting
    /// status of its own, so it names the real tree and the shadow instead.
    pub status_from: &'a [&'a Path],
    /// Considered regardless of what status reports.
    pub extra: &'a [PathBuf],
}

impl Pair<'_> {
    /// Every file that differs between the trees, with both sides' bytes.
    ///
    /// Enumeration is `status --porcelain=v1 -z` in each of `status_from`,
    /// unioned: one tree's status covers the agent's work, the other's covers
    /// anything the user changed or created since. Identical files are then
    /// dropped by a byte comparison, so a generous candidate set costs only a
    /// read.
    pub fn changed(&self) -> Result<Vec<Changed>> {
        let mut candidates: Vec<PathBuf> = Vec::new();
        for tree in self.status_from {
            candidates.extend(git::status_paths(tree)?);
        }
        candidates.extend(self.extra.iter().cloned());
        candidates.sort();
        candidates.dedup();

        let mut out = Vec::new();
        for rel in candidates {
            let left = read_opt(&self.left.join(&rel))?;
            let right = read_opt(&self.right.join(&rel))?;
            if left == right {
                continue; // identical, or absent from both
            }
            out.push(Changed { rel, left, right });
        }
        Ok(out)
    }

    /// One file's diff: `/dev/null` for whichever side is absent, and headers
    /// rewritten to `rel` so the result applies at a repo root.
    pub fn diff_one(&self, c: &Changed, context: usize) -> Result<Vec<u8>> {
        let devnull = PathBuf::from("/dev/null");
        let left = self.left.join(&c.rel);
        let right = self.right.join(&c.rel);
        let a = if c.left_exists() { &left } else { &devnull };
        let b = if c.right_exists() { &right } else { &devnull };
        let raw = git::diff_no_index(a, b, context)?;
        Ok(git::relativize_no_index_diff(
            &raw,
            &c.rel.to_string_lossy(),
        ))
    }

    /// Every differing file, concatenated into one patch.
    pub fn diff(&self, context: usize) -> Result<Vec<u8>> {
        let mut out = Vec::new();
        for c in self.changed()? {
            out.extend(self.diff_one(&c, context)?);
        }
        Ok(out)
    }
}

/// A file's bytes, or `None` if it is not there.
///
/// The one read path for tree comparison, and deliberately strict about
/// everything except `NotFound`: a file rote cannot read is a fact worth
/// stopping for. Swallowing it would drop a hunk silently from the queue, or —
/// worse — quietly shorten the residue patch that DESIGN.md §1 makes the last
/// thing standing between a skipped hunk and oblivion.
fn read_opt(path: &Path) -> Result<Option<Vec<u8>>> {
    match std::fs::read(path) {
        Ok(b) => Ok(Some(b)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("cannot read {}", path.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(root: &Path, rel: &str, body: &str) {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, body).unwrap();
    }

    /// Two directories that are not repositories at all.
    ///
    /// `changed` only consults `status_from`, so a pair with none of those is a
    /// pure `extra`-driven comparison — which is exactly what isolates the
    /// absence rules from git.
    fn pair<'a>(left: &'a Path, right: &'a Path, extra: &'a [PathBuf]) -> Pair<'a> {
        Pair {
            left,
            right,
            status_from: &[],
            extra,
        }
    }

    #[test]
    fn identical_files_are_not_changed() {
        let dir = tempfile::tempdir().unwrap();
        let (l, r) = (dir.path().join("l"), dir.path().join("r"));
        write(&l, "a.rs", "same\n");
        write(&r, "a.rs", "same\n");

        let extra = vec![PathBuf::from("a.rs")];
        assert!(pair(&l, &r, &extra).changed().unwrap().is_empty());
    }

    #[test]
    fn an_absent_side_is_none_not_empty() {
        let dir = tempfile::tempdir().unwrap();
        let (l, r) = (dir.path().join("l"), dir.path().join("r"));
        write(&l, "gone.rs", "");
        write(&r, "added.rs", "new\n");

        let extra = vec![PathBuf::from("gone.rs"), PathBuf::from("added.rs")];
        let changed = pair(&l, &r, &extra).changed().unwrap();

        // An empty file on the left and no file on the right still differ: the
        // whole add/delete distinction rests on that not collapsing.
        let gone = changed
            .iter()
            .find(|c| c.rel == Path::new("gone.rs"))
            .unwrap();
        assert!(gone.left_exists() && !gone.right_exists());
        assert_eq!(gone.left.as_deref(), Some(&b""[..]));

        let added = changed
            .iter()
            .find(|c| c.rel == Path::new("added.rs"))
            .unwrap();
        assert!(!added.left_exists() && added.right_exists());
    }

    #[test]
    fn a_missing_file_on_both_sides_is_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let (l, r) = (dir.path().join("l"), dir.path().join("r"));
        std::fs::create_dir_all(&l).unwrap();
        std::fs::create_dir_all(&r).unwrap();

        let extra = vec![PathBuf::from("nowhere.rs")];
        assert!(pair(&l, &r, &extra).changed().unwrap().is_empty());
    }

    #[test]
    fn headers_are_repo_relative_so_the_patch_applies() {
        let dir = tempfile::tempdir().unwrap();
        let (l, r) = (dir.path().join("l"), dir.path().join("r"));
        write(&l, "src/a.rs", "one\n");
        write(&r, "src/a.rs", "two\n");

        let extra = vec![PathBuf::from("src/a.rs")];
        let patch = pair(&l, &r, &extra).diff(3).unwrap();
        let text = String::from_utf8(patch).unwrap();

        assert!(text.contains("diff --git a/src/a.rs b/src/a.rs"), "{text}");
        assert!(text.contains("--- a/src/a.rs"), "{text}");
        assert!(text.contains("+++ b/src/a.rs"), "{text}");
        assert!(!text.contains(dir.path().to_str().unwrap()), "{text}");
    }

    #[test]
    fn an_added_file_keeps_its_devnull_side() {
        let dir = tempfile::tempdir().unwrap();
        let (l, r) = (dir.path().join("l"), dir.path().join("r"));
        std::fs::create_dir_all(&l).unwrap();
        write(&r, "new.rs", "hello\n");

        let extra = vec![PathBuf::from("new.rs")];
        let text = String::from_utf8(pair(&l, &r, &extra).diff(3).unwrap()).unwrap();
        assert!(text.contains("--- /dev/null"), "{text}");
        assert!(text.contains("+++ b/new.rs"), "{text}");
    }
}
