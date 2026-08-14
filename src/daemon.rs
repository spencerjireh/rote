//! The daemon: finding one, addressing one, and (later) being one.
//!
//! Exactly one process may own the watch engine for a project, and it proves
//! that by holding the `watch.lock` flock for the engine's whole lifetime
//! (`ProjectPaths::watch_lock_path`). Everything in this module is downstream
//! of that invariant.
//!
//! `daemon.json` is the address book: where a running daemon listens and the
//! token to speak to it. It is deliberately **not** the authority on whether a
//! daemon exists — it outlives a `kill -9`, and a `rote watch --local` pane
//! owns the engine without writing one at all. The flock is the authority,
//! because the kernel maintains it and a file cannot.

use crate::paths::{write_atomic_mode, ProjectPaths};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::Path;

/// Bytes of entropy in a session token, before hex encoding.
const TOKEN_BYTES: usize = 32;

/// Where a daemon is listening, and how to prove you may talk to it.
///
/// Written as one file rather than the conventional separate pid and port
/// files: three files can be read while only some of them are current, and a
/// client that pairs this run's port with the last run's token gets a
/// confusing 401 instead of an honest "nothing is running".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Endpoint {
    pub pid: u32,
    pub port: u16,
    pub token: String,
    /// Which project this daemon serves. Checked before anything is believed —
    /// or signalled. PIDs are recycled, and a stale file that outlived a reboot
    /// must never get an unrelated process killed.
    pub project_hash: String,
    pub repo_root: String,
    pub wire_version: u32,
    pub rote_version: String,
    pub started_at: String,
}

impl Endpoint {
    pub fn new(project: &ProjectPaths, pid: u32, port: u16, token: String) -> Self {
        Self {
            pid,
            port,
            token,
            project_hash: project.hash.clone(),
            repo_root: project.repo_root.to_string_lossy().into_owned(),
            wire_version: crate::state::WIRE_VERSION,
            rote_version: env!("CARGO_PKG_VERSION").to_string(),
            started_at: crate::now_iso8601(),
        }
    }

    /// `None` when nothing is published, or when what is published is
    /// unreadable. A malformed address book is the same as no address book;
    /// there is nothing a caller could usefully do differently.
    pub fn read(project: &ProjectPaths) -> Option<Self> {
        let bytes = std::fs::read(project.daemon_json()).ok()?;
        serde_json::from_slice(&bytes).ok()
    }

    /// Owner-readable only: this file holds a bearer token.
    pub fn write(&self, project: &ProjectPaths) -> Result<()> {
        project.ensure_state_dir()?;
        let mut bytes = serde_json::to_vec_pretty(self).context("cannot serialize the endpoint")?;
        bytes.push(b'\n');
        write_atomic_mode(&project.daemon_json(), &bytes, 0o600)
    }

    pub fn remove(project: &ProjectPaths) {
        // Best effort. A stale file is harmless — `matches` and the health
        // check are what make it safe to ignore one.
        let _ = std::fs::remove_file(project.daemon_json());
    }

    /// Does this endpoint describe a daemon for *this* project?
    pub fn matches(&self, project: &ProjectPaths) -> bool {
        self.project_hash == project.hash
    }

    pub fn base_url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }
}

/// A fresh session token: 32 bytes of kernel entropy, hex encoded.
///
/// `/dev/urandom` through ordinary file I/O rather than `getrandom`/`rand`
/// (a dependency for one call) or `libc::getentropy` (an `unsafe` block, and a
/// `// Safety:` line, to buy nothing over a read that cannot fail on any
/// platform rote supports).
pub fn mint_token() -> Result<String> {
    let mut buf = [0u8; TOKEN_BYTES];
    read_exact(Path::new("/dev/urandom"), &mut buf)
        .context("cannot read entropy from /dev/urandom")?;
    Ok(buf.iter().map(|b| format!("{b:02x}")).collect())
}

fn read_exact(path: &Path, buf: &mut [u8]) -> std::io::Result<()> {
    use std::io::Read as _;
    std::fs::File::open(path)?.read_exact(buf)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::watcher::Filter;

    fn project(dir: &Path) -> ProjectPaths {
        let repo = dir.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        ProjectPaths::resolve_in(&repo, &dir.join("cache"), &dir.join("data")).unwrap()
    }

    #[test]
    fn a_token_is_sixty_four_hex_characters_and_never_repeats() {
        let a = mint_token().unwrap();
        let b = mint_token().unwrap();
        assert_eq!(a.len(), TOKEN_BYTES * 2);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b, "two mints must not collide");
    }

    #[test]
    fn the_endpoint_file_is_owner_readable_only() {
        // It holds a bearer token. 0644 under a normal umask would hand it to
        // every account on a shared machine.
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        let p = project(dir.path());
        Endpoint::new(&p, 42, 5000, "deadbeef".into())
            .write(&p)
            .unwrap();

        let mode = std::fs::metadata(p.daemon_json())
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600, "got {:o}", mode & 0o777);
        let dir_mode = std::fs::metadata(&p.state_dir)
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(dir_mode & 0o777, 0o700, "got {:o}", dir_mode & 0o777);
    }

    #[test]
    fn an_endpoint_round_trips_and_knows_its_project() {
        let dir = tempfile::tempdir().unwrap();
        let p = project(dir.path());
        let ep = Endpoint::new(&p, 42, 5000, "deadbeef".into());
        ep.write(&p).unwrap();

        let back = Endpoint::read(&p).expect("readable");
        assert_eq!(back, ep);
        assert!(back.matches(&p));
        assert_eq!(back.base_url(), "http://127.0.0.1:5000");
    }

    #[test]
    fn an_endpoint_from_another_project_is_not_ours() {
        // The guard that stops a stale file getting an unrelated pid killed.
        let dir = tempfile::tempdir().unwrap();
        let p = project(dir.path());
        let mut ep = Endpoint::new(&p, 42, 5000, "deadbeef".into());
        ep.project_hash = "0123456789ab".into();
        assert!(!ep.matches(&p));
    }

    #[test]
    fn a_missing_or_corrupt_endpoint_reads_as_nothing_running() {
        let dir = tempfile::tempdir().unwrap();
        let p = project(dir.path());
        assert!(Endpoint::read(&p).is_none(), "nothing published yet");

        p.ensure_state_dir().unwrap();
        std::fs::write(p.daemon_json(), b"{ truncated by kill -9").unwrap();
        assert!(
            Endpoint::read(&p).is_none(),
            "unreadable is the same as absent"
        );
    }

    #[test]
    fn the_endpoint_file_is_invisible_to_the_watcher() {
        // Otherwise the daemon publishing its own address would wake its own
        // engine, once per write, forever.
        let dir = tempfile::tempdir().unwrap();
        let p = project(dir.path());
        let f = Filter::new(&p, &crate::config::Config::default()).unwrap();
        assert_eq!(f.classify(&p.daemon_json()), None);
        assert_eq!(f.classify(&p.state_dir.join(".daemon.json.tmp")), None);
        assert_eq!(f.classify(&p.daemon_log()), None);
    }
}
