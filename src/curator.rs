//! The curator: a headless pass that puts the queue in teaching order.
//!
//! The deterministic queue is sorted by file path and then by line number, which
//! is the order a filesystem happens to be in and not the order a change makes
//! sense in. Left alone it hands you a caller before the thing it calls, and a
//! test for something you have not written yet. The curator ranks the pending
//! hunks so definitions come before uses, and writes one line per hunk saying
//! why it sits where it does.
//!
//! Everything here is advisory. A curator that cannot run leaves the
//! deterministic order exactly as it was, which is the behaviour rote had
//! before this module existed.

use crate::hunks::Hunk;
use crate::paths::{self, ProjectPaths};
use crate::session::Manifest;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::Path;

/// Bumped when the cache's shape changes. An older or newer file reads as empty
/// rather than being coerced — a wrong teaching order is worse than none.
pub const CACHE_VERSION: u32 = 1;

/// What the curator decided about one hunk.
///
/// Both fields optional, and that is the important part: an entry with neither
/// is a **tombstone**, meaning "this key was offered to the model and nothing
/// came back". Without it a failed pass would be retried on every keystroke —
/// the uncached set would still equal the pending set, typing a hunk would
/// shrink it, and the trigger would fire again. See `should_curate`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    /// Position in the teaching order. Lower leads; `None` sorts after every
    /// ranked hunk, which is `queue_view`'s documented fallback.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rank: Option<u32>,
    /// One line of ordering rationale, rendered above the hunk.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// The effective teaching order, keyed by hunk `key`.
///
/// Note "effective", not "what the model said": the pin that keeps the hunk you
/// are typing at the head is baked in here rather than applied at write time.
/// It has to be. This cache is re-applied after every `reconcile`, so a pin held
/// anywhere else would be overwritten by the next recompute — at most a second
/// and a half later — and the screen would jump after all.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Cache {
    pub version: u32,
    /// The session this belongs to, copied from `Manifest::created_at`.
    ///
    /// Load-bearing. Clearing the file at `done`/`abort` is not enough: an
    /// `abort` reaps the daemon best-effort, and a `rote watch --local` pane
    /// publishes no `daemon.json` to be reaped at all, so that pane's engine can
    /// write here *after* the teardown cleared it. The next session would then
    /// inherit ranks assigned against a different set of hunks — and because
    /// every key would look considered, the curator would never fire to correct
    /// it. Stamping makes a stale file unbelievable instead of merely unlikely.
    pub session: String,
    pub entries: BTreeMap<String, Entry>,
}

impl Cache {
    pub fn new(session: &str) -> Self {
        Self {
            version: CACHE_VERSION,
            session: session.to_string(),
            entries: BTreeMap::new(),
        }
    }

    /// Read the cache for this session, or an empty one.
    ///
    /// Never fails. Missing, unreadable, half-written, from another version or
    /// from a previous session all mean the same thing to every caller: there is
    /// nothing to apply. The same posture as `Endpoint::read`.
    pub fn load(project: &ProjectPaths, session: &str) -> Self {
        std::fs::read(project.curator_json())
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Self>(&bytes).ok())
            .filter(|c| c.version == CACHE_VERSION && c.session == session)
            .unwrap_or_else(|| Self::new(session))
    }

    pub fn save(&self, project: &ProjectPaths) -> Result<()> {
        let path = project.curator_json();
        paths::assert_not_in_real_tree(&path, &project.repo_root);
        let mut bytes = serde_json::to_vec_pretty(self).context("cannot serialize the curation")?;
        bytes.push(b'\n');
        paths::write_atomic(&path, &bytes)
    }

    pub fn remove(project: &ProjectPaths) {
        // Best effort, and deliberately so: the `session` stamp is what makes a
        // leftover harmless, not this.
        let _ = std::fs::remove_file(project.curator_json());
    }

    /// File the cache beside the archived manifest. Hygiene, not correctness.
    pub fn archive_to(&self, dir: &Path, slug: &str) -> Result<()> {
        std::fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
        let mut bytes = serde_json::to_vec_pretty(self).context("cannot serialize the curation")?;
        bytes.push(b'\n');
        paths::write_atomic(&dir.join(format!("{slug}.curator.json")), &bytes)
    }

    /// Has this key been offered to the model? Presence, not note-ness.
    ///
    /// A tombstone counts. That is the whole point of one.
    pub fn considered(&self, key: &str) -> bool {
        self.entries.contains_key(key)
    }

    /// Project the cache onto a manifest. `true` if anything actually moved.
    ///
    /// Walks every hunk rather than only the pending ones, so a hunk that has
    /// gone terminal keeps its note into the archive — the archive is the record
    /// of what the user was actually shown. Never *clears* a field: a hunk with
    /// no entry keeps whatever it already had.
    ///
    /// The `bool` matters. Stage 2 stopped bumping `generation` for a cycle that
    /// wrote nothing, and this runs on every recompute; reporting "changed"
    /// unconditionally would invalidate every client's snapshot several times a
    /// minute for no reason.
    pub fn apply(&self, manifest: &mut Manifest) -> bool {
        let mut moved = false;
        for h in manifest.hunks.iter_mut() {
            let Some(entry) = self.entries.get(&h.key) else {
                continue;
            };
            moved |= set_if_changed(&mut h.curator_rank, entry.rank);
            moved |= set_if_changed(&mut h.curator_note, entry.note.clone());
        }
        moved
    }
}

fn set_if_changed<T: PartialEq>(slot: &mut Option<T>, value: Option<T>) -> bool {
    if value.is_none() || *slot == value {
        return false;
    }
    *slot = value;
    true
}

/// Identify a set of hunks by their keys, order-independently.
///
/// The trigger compares this against the last attempt, so it must not move when
/// the queue is merely re-sorted or when the same key appears twice — otherwise
/// the curator would re-fire on its own output.
pub fn fingerprint(keys: &[String]) -> String {
    let mut sorted: Vec<&str> = keys.iter().map(String::as_str).collect();
    sorted.sort_unstable();
    sorted.dedup();

    let mut h = Sha256::new();
    for key in sorted {
        h.update(key.as_bytes());
        h.update([0u8]);
    }
    let digest = h.finalize();
    digest.iter().take(8).map(|b| format!("{b:02x}")).collect()
}

/// The keys of every pending hunk, deduplicated, in queue order.
pub fn pending_keys(manifest: &Manifest) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    manifest
        .queue_view()
        .iter()
        .map(|h| h.key.clone())
        .filter(|k| seen.insert(k.clone()))
        .collect()
}

/// Whether a hunk is one the curator has anything to say about.
pub fn is_rankable(h: &Hunk) -> bool {
    !h.key.is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hunks::{Hunk, Op, Status};
    use crate::session::{BaselineRecord, State, MANIFEST_VERSION};

    fn hunk(key: &str, file: &str) -> Hunk {
        Hunk {
            id: format!("h-{key}"),
            key: key.to_string(),
            file: file.to_string(),
            op: Op::Replace,
            context_before: vec![],
            old_lines: vec![],
            new_lines: vec!["work();".into()],
            context_after: vec![],
            anchor_hint: 1,
            status: Status::Pending,
            divergence: None,
            note: None,
            pending_divergence: None,
            curator_note: None,
            curator_rank: None,
            input: Default::default(),
        }
    }

    fn manifest(hunks: Vec<Hunk>) -> Manifest {
        Manifest {
            version: MANIFEST_VERSION,
            generation: 0,
            task: "t".into(),
            created_at: "2026-01-01T00:00:00Z".into(),
            state: State::Transcribing,
            project_root: "/repo".into(),
            shadow_dir: "/shadow".into(),
            baseline: BaselineRecord {
                head: "abc".into(),
                uncommitted_digest: "d".into(),
                synced_at: "2026-01-01T00:00:00Z".into(),
            },
            hunks,
            last_presented: None,
            terminal: None,
        }
    }

    fn project() -> (tempfile::TempDir, ProjectPaths) {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        let state = dir.path().join("state");
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::create_dir_all(&state).unwrap();
        let project = ProjectPaths {
            repo_root: repo,
            hash: "hash".into(),
            shadow_dir: dir.path().join("shadow"),
            state_dir: state,
        };
        (dir, project)
    }

    #[test]
    fn a_missing_cache_reads_as_an_empty_one() {
        let (_d, project) = project();
        let cache = Cache::load(&project, "2026-01-01T00:00:00Z");
        assert!(cache.entries.is_empty());
        assert_eq!(cache.version, CACHE_VERSION);
    }

    #[test]
    fn a_corrupt_cache_reads_as_an_empty_one_rather_than_failing() {
        // Half a file is what a crash mid-write leaves behind, and there is
        // nothing a caller could usefully do about it that "no curation yet"
        // does not already do.
        let (_d, project) = project();
        std::fs::write(project.curator_json(), b"{\"version\": 1, \"entr").unwrap();
        assert!(Cache::load(&project, "2026-01-01T00:00:00Z")
            .entries
            .is_empty());
    }

    #[test]
    fn a_cache_left_by_a_previous_session_is_not_believed() {
        // The silent-wrong-order case. A `--local` pane publishes no daemon.json
        // to reap, so its engine can write here after a teardown cleared the
        // file — and every key would then look considered, so the curator would
        // never fire to correct the order it inherited.
        let (_d, project) = project();
        let mut old = Cache::new("2026-01-01T00:00:00Z");
        old.entries.insert(
            "k-1".into(),
            Entry {
                rank: Some(1),
                note: Some("stale".into()),
            },
        );
        old.save(&project).unwrap();

        let fresh = Cache::load(&project, "2026-06-01T00:00:00Z");
        assert!(
            fresh.entries.is_empty(),
            "a cache stamped for another session carries no authority"
        );
        assert!(!fresh.considered("k-1"));
    }

    #[test]
    fn a_cache_written_by_another_version_is_ignored() {
        let (_d, project) = project();
        std::fs::write(
            project.curator_json(),
            br#"{"version": 99, "session": "2026-01-01T00:00:00Z", "entries": {"k-1": {}}}"#,
        )
        .unwrap();
        assert!(Cache::load(&project, "2026-01-01T00:00:00Z")
            .entries
            .is_empty());
    }

    #[test]
    fn an_entry_with_no_note_still_counts_as_considered() {
        // The tombstone contract, stated as a test because the whole cost bound
        // rests on it.
        let mut cache = Cache::new("s");
        cache.entries.insert("k-1".into(), Entry::default());
        assert!(cache.considered("k-1"));
        assert!(!cache.considered("k-2"));
    }

    #[test]
    fn a_cache_round_trips_through_the_state_dir() {
        let (_d, project) = project();
        let mut cache = Cache::new("s");
        cache.entries.insert(
            "k-1".into(),
            Entry {
                rank: Some(3),
                note: Some("the model everything below refers to".into()),
            },
        );
        cache.save(&project).unwrap();

        let back = Cache::load(&project, "s");
        assert_eq!(back.entries, cache.entries);
    }

    #[test]
    fn the_fingerprint_ignores_order_and_duplicates() {
        let a = fingerprint(&["k-2".into(), "k-1".into()]);
        let b = fingerprint(&["k-1".into(), "k-2".into(), "k-1".into()]);
        assert_eq!(a, b, "a re-sorted queue is the same queue");
        assert_ne!(a, fingerprint(&["k-1".into()]));
        assert_ne!(a, fingerprint(&[]));
    }

    #[test]
    fn applying_a_cache_that_changes_nothing_reports_no_change() {
        // Reported "changed" would bump the generation on every recompute and
        // invalidate every subscriber's snapshot several times a minute.
        let mut m = manifest(vec![hunk("k-1", "a.rs")]);
        let mut cache = Cache::new("s");
        cache.entries.insert(
            "k-1".into(),
            Entry {
                rank: Some(1),
                note: Some("first".into()),
            },
        );

        assert!(
            cache.apply(&mut m),
            "the first application moves both fields"
        );
        assert!(!cache.apply(&mut m), "the second changes nothing");
    }

    #[test]
    fn applying_a_cache_reaches_terminal_hunks_too_so_the_archive_keeps_the_note() {
        let mut typed = hunk("k-1", "a.rs");
        typed.status = Status::Typed;
        let mut m = manifest(vec![typed]);

        let mut cache = Cache::new("s");
        cache.entries.insert(
            "k-1".into(),
            Entry {
                rank: Some(1),
                note: Some("why it came first".into()),
            },
        );
        assert!(cache.apply(&mut m));
        assert_eq!(
            m.hunks[0].curator_note.as_deref(),
            Some("why it came first")
        );
    }

    #[test]
    fn a_hunk_with_no_entry_keeps_what_it_already_had() {
        // `apply` runs on every recompute against a cache that may not mention
        // this hunk yet. Clearing here would erase a note a moment after writing
        // it.
        let mut h = hunk("k-9", "a.rs");
        h.curator_note = Some("kept".into());
        h.curator_rank = Some(4);
        let mut m = manifest(vec![h]);

        assert!(!Cache::new("s").apply(&mut m));
        assert_eq!(m.hunks[0].curator_note.as_deref(), Some("kept"));
        assert_eq!(m.hunks[0].curator_rank, Some(4));
    }

    #[test]
    fn a_tombstone_leaves_a_hunk_unranked() {
        let mut m = manifest(vec![hunk("k-1", "a.rs")]);
        let mut cache = Cache::new("s");
        cache.entries.insert("k-1".into(), Entry::default());

        assert!(!cache.apply(&mut m), "nothing to write");
        assert_eq!(
            m.hunks[0].curator_rank, None,
            "so it sorts by file, as before"
        );
    }

    #[test]
    fn two_hunks_that_share_a_key_share_an_entry() {
        // `disambiguate_ids` splits ids but deliberately leaves keys shared: the
        // same old -> new pair in one file is the same change, and deserves the
        // same note.
        let mut m = manifest(vec![hunk("k-1", "a.rs"), hunk("k-1", "a.rs")]);
        m.hunks[1].id = "h-k-1.2".into();

        let mut cache = Cache::new("s");
        cache.entries.insert(
            "k-1".into(),
            Entry {
                rank: Some(2),
                note: Some("both".into()),
            },
        );
        cache.apply(&mut m);
        assert_eq!(m.hunks[0].curator_rank, Some(2));
        assert_eq!(m.hunks[1].curator_rank, Some(2));
    }

    #[test]
    fn pending_keys_are_deduplicated_and_skip_terminal_hunks() {
        let mut typed = hunk("k-9", "z.rs");
        typed.status = Status::Typed;
        let m = manifest(vec![hunk("k-1", "a.rs"), hunk("k-1", "a.rs"), typed]);
        assert_eq!(pending_keys(&m), vec!["k-1".to_string()]);
    }
}
