//! Advisory file locking, and the liveness probe that names a holder.
//!
//! Split out of `paths.rs`, which is about where rote's files live. This is
//! about who is allowed to touch them, which is a different subject and the one
//! the concurrency design actually rests on. See DESIGN.md §9.7.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Serialize, Deserialize)]
struct LockInfo {
    pid: u32,
    acquired_at: String,
}

/// How long `acquire` will wait for a holder to finish before giving up.
const LOCK_WAIT: std::time::Duration = std::time::Duration::from_secs(5);
/// Poll interval while waiting. House style: `model::run`.
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
}
