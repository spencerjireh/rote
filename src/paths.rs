//! XDG directory resolution, project identity, and the guarded writers.
//!
//! Path layout is XDG on every platform (DESIGN.md § Filesystem layout), not
//! platform-native: the documented escape hatch is `rm -rf ~/.cache/rote/<hash>`
//! and it has to be literally true on macOS too.

use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};
use std::fs;
use std::path::{Path, PathBuf};

/// Walk up from `start` looking for a `.git` entry.
pub fn discover_repo_root(start: &Path) -> Result<PathBuf> {
    let start = start
        .canonicalize()
        .with_context(|| format!("cannot resolve path {}", start.display()))?;
    for dir in start.ancestors() {
        if dir.join(".git").exists() {
            return Ok(dir.to_path_buf());
        }
    }
    bail!(
        "not inside a git repository (looked upward from {}).\n\
         rote works on git repos only — run `git init` first, or pass --project <path>.",
        start.display()
    )
}

/// Project identity: first 12 hex of SHA-256 over the canonicalized root path.
pub fn project_hash(repo_root: &Path) -> Result<String> {
    let canonical = repo_root
        .canonicalize()
        .with_context(|| format!("cannot resolve repo root {}", repo_root.display()))?;
    let mut hasher = Sha256::new();
    hasher.update(canonical.as_os_str().as_encoded_bytes());
    Ok(hex12(&hasher.finalize()))
}

fn hex12(bytes: &[u8]) -> String {
    bytes.iter().take(6).map(|b| format!("{b:02x}")).collect()
}

fn home_dir() -> Result<PathBuf> {
    directories::BaseDirs::new()
        .map(|d| d.home_dir().to_path_buf())
        .context("cannot determine home directory")
}

fn xdg_dir(var: &str, fallback: &str) -> Result<PathBuf> {
    match std::env::var_os(var) {
        Some(v) if !v.is_empty() => Ok(PathBuf::from(v)),
        _ => Ok(home_dir()?.join(fallback)),
    }
}

pub fn config_home() -> Result<PathBuf> {
    xdg_dir("XDG_CONFIG_HOME", ".config")
}

pub fn cache_home() -> Result<PathBuf> {
    xdg_dir("XDG_CACHE_HOME", ".cache")
}

pub fn data_home() -> Result<PathBuf> {
    xdg_dir("XDG_DATA_HOME", ".local/share")
}

/// Path to the global config file, whether or not it exists.
pub fn global_config_path() -> Result<PathBuf> {
    Ok(config_home()?.join("rote").join("config.toml"))
}

/// Every rote-owned path for one project.
#[derive(Debug, Clone)]
pub struct ProjectPaths {
    pub repo_root: PathBuf,
    pub hash: String,
    pub shadow_dir: PathBuf,
    pub state_dir: PathBuf,
}

impl ProjectPaths {
    pub fn resolve(repo_root: &Path) -> Result<Self> {
        Self::resolve_in(repo_root, &cache_home()?, &data_home()?)
    }

    /// Resolve against explicit roots instead of the ambient XDG environment.
    ///
    /// Tests use this to get isolated state directories without mutating
    /// process-global env, which would force them to run single-threaded.
    pub fn resolve_in(repo_root: &Path, cache_root: &Path, data_root: &Path) -> Result<Self> {
        let repo_root = repo_root
            .canonicalize()
            .with_context(|| format!("cannot resolve repo root {}", repo_root.display()))?;
        let hash = project_hash(&repo_root)?;

        let shadow_dir = cache_root.join("rote").join(&hash).join("shadow");
        let state_dir = data_root.join("rote").join(&hash);

        // Principle 2, enforced at runtime rather than only in debug builds.
        //
        // `cargo install` builds in release, where `debug_assert!` compiles to
        // nothing — so the guarded writers alone would leave the installed
        // binary unprotected. This is not a hypothetical: when $HOME is itself a
        // git repo (a dotfiles repo), ~/.cache/rote/<hash>/shadow is inside the
        // real tree by definition, and rote would clone the home directory into
        // a subdirectory of itself.
        ensure_outside_repo(&shadow_dir, &repo_root, "shadow", "XDG_CACHE_HOME")?;
        ensure_outside_repo(&state_dir, &repo_root, "state", "XDG_DATA_HOME")?;

        Ok(Self {
            shadow_dir,
            state_dir,
            hash,
            repo_root,
        })
    }

    pub fn session_json(&self) -> PathBuf {
        self.state_dir.join("session.json")
    }

    pub fn lock_path(&self) -> PathBuf {
        self.state_dir.join("session.json.lock")
    }

    /// **The engine token.** Whoever holds this flock owns the watch engine for
    /// this project, and holds it unbroken for that engine's whole lifetime.
    ///
    /// This is the invariant the whole design rests on, because two engines do
    /// not merely duplicate work — they corrupt each other. An engine's
    /// authority lives in private in-process state (its baselines, its watchdog,
    /// its recompute schedule) that no other process can see or invalidate. Two
    /// of them classify the same keystroke, one wins the compare-and-swap, and
    /// the loser never retakes its baseline — so it goes on to raise a
    /// divergence question about a hunk the user never touched, and that
    /// question survives every recompute.
    ///
    /// So it is also the routing decision for every mutation: acquire it and
    /// there is no engine to desynchronize, so mutate directly; fail to acquire
    /// it and an engine exists, so send it a verb instead. The check *is* the
    /// exclusion, which is why it beats asking `daemon.json` — that file is an
    /// address book and can be stale, while the kernel maintains this.
    ///
    /// Necessarily a different file from `lock_path`, which is taken and
    /// released around each manifest write and which the engine uses constantly.
    pub fn watch_lock_path(&self) -> PathBuf {
        self.state_dir.join("watch.lock")
    }

    /// Where a running daemon publishes its port and token. An address book,
    /// never an authority: it outlives a `kill -9`, and a `--local` pane owns
    /// the engine without writing one at all.
    pub fn daemon_json(&self) -> PathBuf {
        self.state_dir.join("daemon.json")
    }

    /// The detached daemon's stdout and stderr.
    ///
    /// Without it a daemon that dies during startup leaves nothing to debug:
    /// its parent has already `exec`ed into claude.
    pub fn daemon_log(&self) -> PathBuf {
        self.state_dir.join("daemon.log")
    }

    pub fn baseline_patch(&self) -> PathBuf {
        self.state_dir.join("baseline.patch")
    }

    /// The curator's teaching order and notes, keyed by hunk `key`.
    ///
    /// Not a convenience: it is where curator state actually lives. `reconcile`
    /// copies nothing onto a fresh hunk, and a hunk's `id` folds in its
    /// surrounding context — so typing within three lines of a hunk re-identifies
    /// it and the manifest's copy of a note is gone. `key` ignores context and
    /// survives exactly that churn, which is why this file is the source of truth
    /// and the fields on `Hunk` are a projection of it.
    pub fn curator_json(&self) -> PathBuf {
        self.state_dir.join("curator.json")
    }

    pub fn archive_dir(&self) -> PathBuf {
        self.state_dir.join("archive")
    }

    /// The per-project config, the one file rote may write inside the real tree.
    pub fn project_config(&self) -> PathBuf {
        self.repo_root.join(".rote.toml")
    }

    /// Create the state directory, owner-only.
    ///
    /// 0700 because a bearer token lives in here. Set unconditionally rather
    /// than only at creation: the directory predates that requirement on any
    /// machine that ran an earlier rote.
    pub fn ensure_state_dir(&self) -> Result<()> {
        fs::create_dir_all(&self.state_dir)
            .with_context(|| format!("cannot create {}", self.state_dir.display()))?;
        fs::set_permissions(
            &self.state_dir,
            std::os::unix::fs::PermissionsExt::from_mode(0o700),
        )
        .with_context(|| format!("cannot secure {}", self.state_dir.display()))
    }
}

/// Normalize a path that may not exist yet.
///
/// `canonicalize` fails on a missing directory, and rote's state dirs routinely
/// do not exist on first run — but a raw string comparison would miss macOS's
/// `/var` → `/private/var` symlink, the exact mismatch that already caught the
/// test fixtures out. So: canonicalize the deepest ancestor that does exist, and
/// re-attach the rest.
fn normalize_possibly_missing(path: &Path) -> PathBuf {
    let mut tail = Vec::new();
    let mut cursor = path;
    loop {
        if let Ok(real) = cursor.canonicalize() {
            let mut out = real;
            for part in tail.iter().rev() {
                out.push(part);
            }
            return out;
        }
        match (cursor.file_name(), cursor.parent()) {
            (Some(name), Some(parent)) => {
                tail.push(name.to_os_string());
                cursor = parent;
            }
            // Nothing along the path exists; the best we can do is the input.
            _ => return path.to_path_buf(),
        }
    }
}

/// Refuse a rote-owned directory that would sit inside the user's repository.
fn ensure_outside_repo(
    candidate: &Path,
    repo_root: &Path,
    what: &str,
    env_var: &str,
) -> Result<()> {
    let normalized = normalize_possibly_missing(candidate);
    if !normalized.starts_with(repo_root) {
        return Ok(());
    }
    bail!(
        "rote's {what} directory would sit inside the repository it is shadowing.\n\
         \n  {what} directory: {}\n  repository:      {}\n\n\
         rote never writes into your real tree, so it will not proceed. This usually \
         means the repository is your home directory (a dotfiles repo), or that \
         {env_var} points inside the project.\n\
         Set {env_var} to a location outside {}, then try again.",
        normalized.display(),
        repo_root.display(),
        repo_root.display()
    )
}

/// Principle 2, mechanically enforced: no rote code path writes source into the
/// real tree. `rote init` writing `.rote.toml` is the single documented
/// exception and does not route through the guarded writers.
///
/// A second layer, not the primary defence. `ProjectPaths::resolve_in` refuses
/// at runtime, in every build, when rote's directories would land inside the
/// repository — this catches the narrower case of a writer that constructs a
/// target without going through `ProjectPaths` at all. Debug-only is adequate
/// for that: it is a coding mistake, caught in development and tests.
pub fn assert_not_in_real_tree(path: &Path, repo_root: &Path) {
    debug_assert!(
        !path.starts_with(repo_root),
        "rote attempted to write inside the real tree: {} (repo root {}). \
         The real tree is written only by the user's keystrokes.",
        path.display(),
        repo_root.display()
    );
}

/// Atomic write: temp file in the same directory, then rename.
pub fn write_atomic(path: &Path, contents: &[u8]) -> Result<()> {
    write_atomic_mode(path, contents, 0o644)
}

/// `create_dir_all`, owner-only.
///
/// Every directory rote makes for itself is one of its own — a project's state
/// directory, its `archive/`, or `~/.config/rote` — and the first of those holds
/// a bearer token. `ensure_state_dir` sets 0700 on the state directory, but only
/// when it is what runs first, and it is not: `Lock::try_acquire` on `watch.lock`
/// is the routing check every mutating verb makes (DESIGN.md §13), and a manifest
/// write can equally get there first. A guarantee that depends on which caller
/// arrives first is not one, so it is made here, where the directories are
/// actually created.
///
/// `DirBuilder::mode` is masked *down* by umask and never up, so this can only
/// ever be tighter than what the bare call produced. Nothing rote writes through
/// these helpers lands in the real tree — `.rote.toml` is written directly, and
/// `assert_not_in_real_tree` guards the rest.
pub(crate) fn create_dir_all_owner_only(dir: &Path) -> Result<()> {
    use std::os::unix::fs::DirBuilderExt as _;
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .with_context(|| format!("cannot create {}", dir.display()))
}

/// `write_atomic`, with the permissions set before the file has any content.
///
/// The mode is applied at creation rather than afterwards, so the contents are
/// never briefly readable by anyone who should not see them. `write_atomic`
/// creates 0644 under a normal umask, which is right for a manifest and wrong
/// for a file holding a bearer token on a shared machine.
pub fn write_atomic_mode(path: &Path, contents: &[u8], mode: u32) -> Result<()> {
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt as _;

    let parent = path
        .parent()
        .with_context(|| format!("{} has no parent directory", path.display()))?;
    create_dir_all_owner_only(parent)?;
    let tmp = parent.join(format!(
        ".{}.tmp",
        path.file_name().unwrap_or_default().to_string_lossy()
    ));
    {
        let mut f = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(mode)
            .open(&tmp)
            .with_context(|| format!("cannot write {}", tmp.display()))?;
        f.write_all(contents)
            .with_context(|| format!("cannot write {}", tmp.display()))?;
    }
    // An existing target keeps its own mode through a rename, so set it again.
    fs::set_permissions(&tmp, std::os::unix::fs::PermissionsExt::from_mode(mode))
        .with_context(|| format!("cannot set permissions on {}", tmp.display()))?;
    fs::rename(&tmp, path)
        .with_context(|| format!("cannot rename {} -> {}", tmp.display(), path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_hash_is_stable_and_12_hex() {
        let dir = tempfile::tempdir().unwrap();
        let a = project_hash(dir.path()).unwrap();
        let b = project_hash(dir.path()).unwrap();
        assert_eq!(a, b);
        assert_eq!(a.len(), 12);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn distinct_roots_hash_differently() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        assert_ne!(
            project_hash(a.path()).unwrap(),
            project_hash(b.path()).unwrap()
        );
    }

    #[test]
    fn discover_repo_root_walks_up() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        fs::create_dir_all(root.join(".git")).unwrap();
        let nested = root.join("a/b/c");
        fs::create_dir_all(&nested).unwrap();
        assert_eq!(discover_repo_root(&nested).unwrap(), root);
    }

    #[test]
    fn discover_repo_root_errors_outside_a_repo() {
        let dir = tempfile::tempdir().unwrap();
        let err = discover_repo_root(dir.path()).unwrap_err().to_string();
        assert!(err.contains("not inside a git repository"), "got: {err}");
    }

    #[test]
    fn shadow_inside_the_repo_is_refused() {
        // The dotfiles-repo case: the repository *is* the home directory, so the
        // XDG cache root lives underneath it.
        let home = tempfile::tempdir().unwrap();
        let home = home.path().canonicalize().unwrap();
        fs::create_dir_all(home.join(".git")).unwrap();
        let cache = home.join(".cache");
        let data = home.join(".local/share");

        let err = ProjectPaths::resolve_in(&home, &cache, &data)
            .unwrap_err()
            .to_string();
        assert!(err.contains("inside the repository"), "got: {err}");
        assert!(err.contains("XDG_CACHE_HOME"), "names the fix: {err}");
    }

    #[test]
    fn state_dir_inside_the_repo_is_refused_too() {
        // Cache outside, data inside — the guard must check both.
        let base = tempfile::tempdir().unwrap();
        let base = base.path().canonicalize().unwrap();
        let repo = base.join("repo");
        fs::create_dir_all(repo.join(".git")).unwrap();

        let err = ProjectPaths::resolve_in(&repo, &base.join("cache"), &repo.join("state"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("XDG_DATA_HOME"), "got: {err}");
    }

    #[test]
    fn roots_outside_the_repo_resolve_normally() {
        let base = tempfile::tempdir().unwrap();
        let base = base.path().canonicalize().unwrap();
        let repo = base.join("repo");
        fs::create_dir_all(repo.join(".git")).unwrap();

        let p = ProjectPaths::resolve_in(&repo, &base.join("cache"), &base.join("data")).unwrap();
        assert!(p.shadow_dir.starts_with(base.join("cache")));
        assert!(p.state_dir.starts_with(base.join("data")));
        assert!(!p.shadow_dir.starts_with(&repo));
    }

    #[test]
    fn a_sibling_directory_sharing_a_name_prefix_is_not_inside() {
        // `/x/repo-cache` must not read as inside `/x/repo`. starts_with works
        // on components, but this is the classic way to get it wrong.
        let base = tempfile::tempdir().unwrap();
        let base = base.path().canonicalize().unwrap();
        let repo = base.join("repo");
        fs::create_dir_all(repo.join(".git")).unwrap();
        assert!(
            ProjectPaths::resolve_in(&repo, &base.join("repo-cache"), &base.join("repo-data"))
                .is_ok()
        );
    }

    #[test]
    fn normalization_survives_a_missing_tail() {
        // The state dir usually does not exist on first run, but the symlinked
        // prefix still has to be resolved or the comparison is meaningless.
        let base = tempfile::tempdir().unwrap();
        let real = base.path().canonicalize().unwrap();
        let missing = base.path().join("not/created/yet");
        let normalized = normalize_possibly_missing(&missing);
        assert!(normalized.starts_with(&real), "{normalized:?} vs {real:?}");
        assert!(normalized.ends_with("not/created/yet"));
    }

    #[test]
    fn write_atomic_mode_sets_permissions_before_any_content_lands() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("secret");

        write_atomic_mode(&p, b"a bearer token", 0o600).unwrap();
        assert_eq!(
            fs::metadata(&p).unwrap().permissions().mode() & 0o777,
            0o600
        );

        // Overwriting an existing file keeps the requested mode: a rename
        // carries the temp file's permissions, not the target's, but a target
        // that already exists with a laxer mode must not survive.
        fs::set_permissions(&p, fs::Permissions::from_mode(0o644)).unwrap();
        write_atomic_mode(&p, b"a new token", 0o600).unwrap();
        assert_eq!(
            fs::metadata(&p).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(fs::read(&p).unwrap(), b"a new token");

        // And the default wrapper is unchanged for everything else.
        let q = dir.path().join("ordinary");
        write_atomic(&q, b"x").unwrap();
        assert_eq!(
            fs::metadata(&q).unwrap().permissions().mode() & 0o777,
            0o644
        );
    }

    #[test]
    fn a_written_file_gets_an_owner_only_parent_at_every_level() {
        // The mode must not depend on whether `ensure_state_dir` happened to run
        // first. A 0644 manifest is enough to create the directory a 0600 token
        // lands in later, so the directory is what has to be right here.
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        let deep = dir.path().join("state/archive");

        write_atomic(&deep.join("1970.json"), b"{}").unwrap();

        for level in [dir.path().join("state"), deep] {
            assert_eq!(
                fs::metadata(&level).unwrap().permissions().mode() & 0o777,
                0o700,
                "{} must be owner-only",
                level.display()
            );
        }
    }

    #[test]
    fn write_atomic_replaces_contents() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("nested/session.json");
        write_atomic(&p, b"first").unwrap();
        write_atomic(&p, b"second").unwrap();
        assert_eq!(fs::read(&p).unwrap(), b"second");
        // No temp files left behind.
        let leftovers: Vec<_> = fs::read_dir(p.parent().unwrap())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty());
    }

    #[test]
    fn xdg_env_overrides_home_fallback() {
        // Uses the documented env var rather than mutating process state broadly.
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("XDG_CACHE_HOME", dir.path());
        assert_eq!(cache_home().unwrap(), dir.path());
        std::env::remove_var("XDG_CACHE_HOME");
    }
}
