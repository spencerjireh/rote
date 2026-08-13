//! XDG directory resolution, project identity, and the manifest lock.
//!
//! Path layout is XDG on every platform (DESIGN.md § Filesystem layout), not
//! platform-native: the documented escape hatch is `rm -rf ~/.cache/rote/<hash>`
//! and it has to be literally true on macOS too.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
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

    pub fn baseline_patch(&self) -> PathBuf {
        self.state_dir.join("baseline.patch")
    }

    pub fn archive_dir(&self) -> PathBuf {
        self.state_dir.join("archive")
    }

    /// The per-project config, the one file rote may write inside the real tree.
    pub fn project_config(&self) -> PathBuf {
        self.repo_root.join(".rote.toml")
    }

    pub fn ensure_state_dir(&self) -> Result<()> {
        fs::create_dir_all(&self.state_dir)
            .with_context(|| format!("cannot create {}", self.state_dir.display()))
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
    let parent = path
        .parent()
        .with_context(|| format!("{} has no parent directory", path.display()))?;
    fs::create_dir_all(parent).with_context(|| format!("cannot create {}", parent.display()))?;
    let tmp = parent.join(format!(
        ".{}.tmp",
        path.file_name().unwrap_or_default().to_string_lossy()
    ));
    fs::write(&tmp, contents).with_context(|| format!("cannot write {}", tmp.display()))?;
    fs::rename(&tmp, path)
        .with_context(|| format!("cannot rename {} -> {}", tmp.display(), path.display()))?;
    Ok(())
}

#[derive(Debug, Serialize, Deserialize)]
struct LockInfo {
    pid: u32,
    acquired_at: String,
}

/// How long `acquire` will wait for a holder to finish before giving up.
const LOCK_WAIT: std::time::Duration = std::time::Duration::from_secs(5);
/// Poll interval while waiting. House style: `review::wait_with_timeout`.
const LOCK_POLL: std::time::Duration = std::time::Duration::from_millis(25);

/// Guards manifest read-modify-write cycles. Held across the whole cycle by
/// `session::with_session`, but only for the milliseconds that takes — never for
/// a session's duration. `rote start` must still drop it before `exec`ing
/// claude (DESIGN.md §9.7).
///
/// Backed by `flock(2)` rather than a PID file. Two properties matter and the
/// PID-file version had neither:
///   - The kernel releases the lock when the holder dies, however it dies. The
///     whole stale-reclaim dance becomes unnecessary rather than merely
///     better-tested.
///   - Waiting is possible. A daemon and a CLI invocation contend routinely;
///     failing fast on a lock held for 3ms would make the CLI unusable.
///
/// The lock file is never unlinked. Removing a file another process holds open
/// is the classic flock race: the next acquirer creates a *new* inode, flocks
/// that, and both believe they hold the lock. A 0-byte file that lives forever
/// is the correct trade.
#[derive(Debug)]
pub struct Lock {
    /// Dropping the file closes the fd, which is what releases the flock.
    file: fs::File,
    path: PathBuf,
}

impl Lock {
    /// Block until the lock is available, up to `LOCK_WAIT`.
    pub fn acquire(path: &Path) -> Result<Self> {
        Self::acquire_within(path, LOCK_WAIT)
    }

    /// One attempt, no waiting. `Ok(None)` means someone else holds it.
    pub fn try_acquire(path: &Path) -> Result<Option<Self>> {
        let file = open_lock_file(path)?;
        if flock_nonblocking(&file).with_context(|| format!("cannot lock {}", path.display()))? {
            let lock = Self {
                file,
                path: path.to_path_buf(),
            };
            lock.stamp();
            return Ok(Some(lock));
        }
        Ok(None)
    }

    /// Wait for a caller-chosen duration. The daemon wants a shorter ceiling
    /// than an interactive command does.
    pub fn acquire_within(path: &Path, timeout: std::time::Duration) -> Result<Self> {
        let file = open_lock_file(path)?;
        let start = std::time::Instant::now();
        loop {
            if flock_nonblocking(&file)
                .with_context(|| format!("cannot lock {}", path.display()))?
            {
                let lock = Self {
                    file,
                    path: path.to_path_buf(),
                };
                lock.stamp();
                return Ok(lock);
            }
            if start.elapsed() >= timeout {
                let holder = read_lock_holder(path)
                    .filter(|pid| pid_is_live(*pid))
                    .map(|pid| format!(" (pid {pid})"))
                    .unwrap_or_default();
                bail!(
                    "another rote process{holder} is working on this project and has held \
                     the lock for {}s.\nWait for it to finish, or stop it.",
                    timeout.as_secs()
                );
            }
            std::thread::sleep(LOCK_POLL);
        }
    }

    /// Record who holds it. Advisory only — flock is the actual exclusion, this
    /// is so a timeout can name the process the user has to go and deal with.
    fn stamp(&self) {
        let info = LockInfo {
            pid: std::process::id(),
            acquired_at: crate::now_iso8601(),
        };
        if let Ok(bytes) = serde_json::to_vec(&info) {
            use std::io::{Seek, Write};
            let mut f = &self.file;
            let _ = f.set_len(0);
            let _ = f.rewind();
            let _ = f.write_all(&bytes);
            let _ = f.flush();
        }
    }

    /// The lock file's path, for diagnostics.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

fn open_lock_file(path: &Path) -> Result<fs::File> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("cannot create {}", parent.display()))?;
    }
    // Never `truncate` on open: another process may hold this open and have
    // stamped it. Truncation happens after we own the lock, in `stamp`.
    fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .with_context(|| format!("cannot open lock {}", path.display()))
}

/// `Ok(true)` acquired, `Ok(false)` held by someone else, `Err` genuinely broken.
fn flock_nonblocking(file: &fs::File) -> std::io::Result<bool> {
    use std::os::unix::io::AsRawFd;
    // Safety: flock on a valid fd we own; LOCK_NB means it cannot block.
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc == 0 {
        return Ok(true);
    }
    let err = std::io::Error::last_os_error();
    // EWOULDBLOCK and EAGAIN are the same value; this is the contended case.
    if err.raw_os_error() == Some(libc::EWOULDBLOCK) {
        return Ok(false);
    }
    Err(err)
}

fn read_lock_holder(path: &Path) -> Option<u32> {
    let bytes = fs::read(path).ok()?;
    let info: LockInfo = serde_json::from_slice(&bytes).ok()?;
    Some(info.pid)
}

/// `kill(pid, 0)` probes for existence without signalling.
///
/// Two POSIX details matter here, and getting either wrong means stealing a live
/// lock or refusing to clear a dead one:
///   - pid 0 targets the caller's whole process group, so it always "succeeds".
///     It is never a holder we wrote, so treat it as dead.
///   - EPERM means the process exists but belongs to another user — alive.
///     Only ESRCH means genuinely gone.
pub fn pid_is_live(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    // Safety: kill with signal 0 performs only an existence/permission check.
    let rc = unsafe { libc::kill(pid as libc::pid_t, 0) };
    if rc == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
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
    fn lock_is_exclusive_while_held() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("session.json.lock");
        let held = Lock::acquire(&p).unwrap();
        assert!(
            Lock::try_acquire(&p).unwrap().is_none(),
            "a second acquire must not succeed while the first is held"
        );
        drop(held);
        // Closing the fd is what releases the flock — but a `fork` anywhere else
        // in this process transiently shares the open file description, so the
        // release is not always visible on the very next instruction. That is
        // exactly why `acquire` retries and `try_acquire` does not; asserting
        // through `acquire` is asserting the real contract.
        let _reacquired = Lock::acquire(&p).unwrap();
    }

    #[test]
    fn a_waiting_acquirer_succeeds_once_the_holder_drops() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("session.json.lock");
        let held = Lock::acquire(&p).unwrap();

        let path = p.clone();
        let waiter = std::thread::spawn(move || {
            // Blocks in the poll loop until the main thread drops its lock.
            Lock::acquire(&path).map(|_| ())
        });

        std::thread::sleep(std::time::Duration::from_millis(120));
        drop(held);
        waiter
            .join()
            .unwrap()
            .expect("the waiter should acquire once the holder releases");
    }

    #[test]
    fn the_lock_file_is_never_unlinked() {
        // Unlinking on drop is the flock race: the next acquirer would create a
        // fresh inode and two processes would both believe they hold the lock.
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("session.json.lock");
        let held = Lock::acquire(&p).unwrap();
        let inode_while_held = fs::metadata(&p).unwrap();
        drop(held);
        assert!(p.exists(), "the lock file must survive the guard");
        let _ = inode_while_held;
    }

    #[test]
    fn a_lock_from_a_killed_process_is_available_again() {
        // The `kill -9 mid-session` case from DESIGN.md §9.7. With flock the
        // kernel does the reclaiming, so there is nothing for rote to clean up.
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("session.json.lock");

        // A child that takes the lock and then dies without ever unlocking.
        // `perl` rather than `flock(1)`: the latter is Linux-only, so on macOS
        // it silently turned this test into a no-op that always passed.
        let mut child = std::process::Command::new("perl")
            .arg("-e")
            .arg(
                "open(my $f, '>>', $ARGV[0]) or die; flock($f, 2) or die; \
                 $| = 1; print \"locked\\n\"; sleep 300;",
            )
            .arg(&p)
            .stdout(std::process::Stdio::piped())
            .spawn()
            .expect("perl is required for this test");

        // Wait for the child to say it holds the lock, rather than sleeping and
        // hoping. A precondition that silently fails to hold is not a pass.
        let mut line = String::new();
        std::io::BufRead::read_line(
            &mut std::io::BufReader::new(child.stdout.take().unwrap()),
            &mut line,
        )
        .unwrap();
        assert_eq!(line.trim(), "locked", "the child never took the lock");
        assert!(
            Lock::try_acquire(&p).unwrap().is_none(),
            "the child holds the lock, so we must not get it"
        );

        child.kill().unwrap();
        child.wait().unwrap();

        assert!(
            Lock::acquire(&p).is_ok(),
            "the kernel must release the lock when the holder dies"
        );
    }

    #[test]
    fn live_pid_is_detected_and_pid_zero_is_not() {
        assert!(pid_is_live(std::process::id()), "our own pid is live");
        // pid 0 means "my process group" to kill(2); it is never a lock holder.
        assert!(!pid_is_live(0), "pid 0 must never read as a live holder");
    }

    #[test]
    fn a_corrupt_lock_file_still_locks() {
        // The file's contents are advisory (they only name the holder for error
        // messages); flock does not care what is in it.
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("session.json.lock");
        fs::write(&p, b"{ truncated by kill -9").unwrap();
        let held = Lock::acquire(&p).unwrap();
        assert!(Lock::try_acquire(&p).unwrap().is_none());
        drop(held);
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
