//! SessionEngine: the manifest, the state machine, and recompute.
//!
//! The manifest on disk is the single source of truth. Every command loads it,
//! validates state, acts, and atomically rewrites it. DESIGN.md §2, §4, §5.

use crate::config::Config;
use crate::git;
use crate::hunks::{compute_hunks, Divergence, Hunk, Status};
use crate::paths::{assert_not_in_real_tree, write_atomic, ProjectPaths};
use crate::shadow::{self, Baseline};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

/// Bumped to 2 for the v2 manifest: `generation`, and the per-hunk `key`,
/// `pending_divergence`, `curator_note`, `curator_rank` and `input` fields.
/// A v1 session cannot be read by this build — `load` refuses it with advice.
pub const MANIFEST_VERSION: u32 = 2;

/// The live states. There is no `done` or `aborted` state: both return the
/// session to `idle`, and how it ended is recorded on the archived copy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum State {
    Idle,
    Working,
    Transcribing,
}

impl std::fmt::Display for State {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            State::Idle => "idle",
            State::Working => "working",
            State::Transcribing => "transcribing",
        })
    }
}

/// How an archived session ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Terminal {
    Done,
    Aborted,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BaselineRecord {
    pub head: String,
    pub uncommitted_digest: String,
    pub synced_at: String,
}

impl From<Baseline> for BaselineRecord {
    fn from(b: Baseline) -> Self {
        Self {
            head: b.head,
            uncommitted_digest: b.uncommitted_digest,
            synced_at: b.synced_at,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    pub version: u32,
    pub state: State,
    pub task: String,
    pub created_at: String,
    pub project_root: String,
    pub shadow_dir: String,
    pub baseline: BaselineRecord,
    pub hunks: Vec<Hunk>,
    /// Monotonic, bumped by every `with_session` write.
    ///
    /// The anti-staleness token: a front end sends the generation it was looking
    /// at with each verb, and a mismatch means the world moved underneath it. It
    /// is also the cheap "has anything changed" signal for `/health`.
    #[serde(default)]
    pub generation: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_presented: Option<String>,
    /// Set only on archived manifests.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal: Option<Terminal>,
}

impl Manifest {
    pub fn new(project: &ProjectPaths, task: String, baseline: Baseline) -> Self {
        Self {
            version: MANIFEST_VERSION,
            state: State::Working,
            task,
            created_at: crate::now_iso8601(),
            project_root: project.repo_root.to_string_lossy().into_owned(),
            shadow_dir: project.shadow_dir.to_string_lossy().into_owned(),
            baseline: baseline.into(),
            hunks: Vec::new(),
            generation: 0,
            last_presented: None,
            terminal: None,
        }
    }

    /// Load the active session, or `None` when idle.
    pub fn load(project: &ProjectPaths) -> Result<Option<Self>> {
        let path = project.session_json();
        let bytes = match std::fs::read(&path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e).with_context(|| format!("cannot read {}", path.display())),
        };
        let m: Manifest = serde_json::from_slice(&bytes).with_context(|| {
            format!(
                "cannot parse {}.\n\
                 If it is damaged, `rote abort` clears the session.",
                path.display()
            )
        })?;
        if m.version != MANIFEST_VERSION {
            bail!(
                "{} was written by a different version of rote (manifest version {}, this build expects {}).\n\
                 Finish that session with the matching version, or delete the file to start over.",
                path.display(),
                m.version,
                MANIFEST_VERSION
            );
        }
        Ok(Some(m))
    }

    /// Load and require an active session.
    pub fn require(project: &ProjectPaths) -> Result<Self> {
        Self::load(project)?.context(
            "no active session.\nRun `rote start \"what you're working on\"` to begin one.",
        )
    }

    pub fn save(&self, project: &ProjectPaths) -> Result<()> {
        let path = project.session_json();
        assert_not_in_real_tree(&path, &project.repo_root);
        let mut bytes = serde_json::to_vec_pretty(self).context("cannot serialize manifest")?;
        bytes.push(b'\n');
        write_atomic(&path, &bytes)
    }

    pub fn pending(&self) -> impl Iterator<Item = &Hunk> {
        self.hunks.iter().filter(|h| h.status == Status::Pending)
    }

    /// The head of the *ordered* queue. Delegates to `active` so that every
    /// caller agrees with `queue_position` about what "next" means.
    pub fn head_of_queue(&self) -> Option<&Hunk> {
        self.active()
    }

    pub fn count(&self, status: Status) -> usize {
        self.hunks.iter().filter(|h| h.status == status).count()
    }

    pub fn find(&self, id: &str) -> Option<&Hunk> {
        self.hunks.iter().find(|h| h.id == id)
    }

    pub fn find_mut(&mut self, id: &str) -> Option<&mut Hunk> {
        self.hunks.iter_mut().find(|h| h.id == id)
    }

    /// Position of a hunk in the queue, 1-based, for progress display.
    pub fn queue_position(&self, id: &str) -> Option<usize> {
        self.queue_view()
            .iter()
            .position(|h| h.id == id)
            .map(|p| p + 1)
    }

    /// The pending queue in the order it should be transcribed.
    ///
    /// Ordering is a *read-time view*, never storage order. `reconcile` appends
    /// new hunks to the tail (rule 6), so the vector drifts out of any sort as
    /// the session runs; and its positional invariants are what the reconcile
    /// tests assert against. Sorting here leaves all of that untouched.
    ///
    /// Curator rank first where it exists, then a deterministic fallback: file
    /// path, then position within the file. The fallback is the entire ordering
    /// when no curator has run, and the tie-break when one has. `id` last so the
    /// sort is total — two hunks can share a file and an anchor.
    pub fn queue_view(&self) -> Vec<&Hunk> {
        let mut q: Vec<&Hunk> = self.pending().collect();
        q.sort_by(|a, b| {
            a.curator_rank
                .unwrap_or(u32::MAX)
                .cmp(&b.curator_rank.unwrap_or(u32::MAX))
                .then_with(|| a.file.cmp(&b.file))
                .then_with(|| a.anchor_hint.cmp(&b.anchor_hint))
                .then_with(|| a.id.cmp(&b.id))
        });
        q
    }

    /// The hunk to work on next.
    pub fn active(&self) -> Option<&Hunk> {
        self.queue_view().into_iter().next()
    }
}

/// Run a read-modify-write cycle against the manifest under the lock.
///
/// This is the only correct way to mutate a session. The pattern it replaces —
/// load unlocked, mutate in memory, then lock just long enough to write — is a
/// TOCTOU: two processes both load, both mutate different hunks, both save, and
/// the second silently discards the first's work. Nothing detected it, because
/// nothing could.
///
/// Holding the lock across the whole cycle costs nothing here: a cycle is a
/// small read, an in-memory edit, and an atomic write. Expensive work (recompute,
/// checks, the reviewer) stays outside.
///
/// The daemon uses this exactly as the CLI does. Neither is privileged.
pub fn with_session<T>(
    project: &ProjectPaths,
    f: impl FnOnce(&mut Manifest) -> Result<T>,
) -> Result<T> {
    project.ensure_state_dir()?;
    let _lock = crate::paths::Lock::acquire(&project.lock_path())?;

    // Loaded *inside* the lock. Loading outside it is the bug this exists to fix.
    let mut manifest = Manifest::require(project)?;
    let out = f(&mut manifest)?;
    manifest.generation += 1;
    manifest.save(project)?;
    Ok(out)
}

/// `with_session`, with a recompute between the load and the closure.
///
/// The recompute runs inside the lock even though it is the expensive part of
/// the cycle. It has to: `recompute` mutates the manifest, so doing it outside
/// and assigning the result back in would reintroduce exactly the lost-update
/// bug `with_session` exists to prevent. A few hundred milliseconds of lock is
/// the price of correctness on the CLI path; the daemon avoids paying it by
/// classifying against pending hunks without shelling out at all.
pub fn with_session_recomputed<T>(
    project: &ProjectPaths,
    cfg: &Config,
    f: impl FnOnce(&mut Manifest, &RecomputeReport) -> Result<T>,
) -> Result<T> {
    project.ensure_state_dir()?;
    let _lock = crate::paths::Lock::acquire(&project.lock_path())?;

    let mut manifest = Manifest::require(project)?;
    let report = recompute(&mut manifest, project, cfg)?;
    let out = f(&mut manifest, &report)?;
    manifest.generation += 1;
    manifest.save(project)?;
    Ok(out)
}

/// What a recompute observed and had to warn about.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct RecomputeReport {
    pub warnings: Vec<String>,
    /// Real `HEAD` no longer matches the session baseline (DESIGN.md §9.2).
    pub drift: bool,
    pub added: usize,
    pub dropped: usize,
}

/// Re-diff the trees and reconcile the queue. DESIGN.md §5, all seven rules.
///
/// The heart of the tool: it is what lets the user flip back to the agent
/// mid-transcription, ask for rework, and carry on.
pub fn recompute(
    manifest: &mut Manifest,
    project: &ProjectPaths,
    cfg: &Config,
) -> Result<RecomputeReport> {
    // Drift is HEAD-only. The uncommitted digest cannot serve here: it changes
    // the instant the user types their first character, so it would flag every
    // normal transcription as drift.
    let current_head = git::head_commit(&project.repo_root)?;
    let drift = current_head != manifest.baseline.head;

    // Rule 1: the fresh truth from the trees.
    let fresh = compute_hunks(project, cfg)?;
    let mut report = reconcile(manifest, fresh, cfg.strict_whitespace);
    promote_state(manifest);

    report.drift = drift;
    if drift {
        report.warnings.insert(0, format!(
            "baseline drift: the repository moved from {} to {} during this session.\n\
             Continuing against current reality; hunks are recomputed from the trees as they are now.",
            short(&manifest.baseline.head),
            short(&current_head)
        ));
    }
    Ok(report)
}

/// A session with a queue is a session being transcribed.
///
/// This used to live in `rote next`, which made the state machine a property of
/// one command rather than of the session — under a watcher nobody runs `next`,
/// and a session would have sat in `working` while the user typed their way
/// through it. Recompute is the right home: it is the only thing that can
/// observe a queue coming into existence.
pub fn promote_state(manifest: &mut Manifest) {
    if manifest.state == State::Working && manifest.pending().next().is_some() {
        manifest.state = State::Transcribing;
    }
}

/// Rules 2–7 of DESIGN.md §5, as a pure function over (manifest, fresh).
///
/// Separated from `recompute` so the rules are tested directly rather than
/// through a fixture repository — and so the tests exercise this code rather
/// than a reimplementation of it.
pub fn reconcile(
    manifest: &mut Manifest,
    fresh: Vec<Hunk>,
    strict_whitespace: bool,
) -> RecomputeReport {
    let mut report = RecomputeReport::default();
    let fresh_ids: HashSet<&str> = fresh.iter().map(|h| h.id.as_str()).collect();

    // Rule 5: pending hunks the agent reworked away simply go.
    let before = manifest.hunks.len();
    manifest.hunks.retain(|h| {
        if h.status == Status::Pending {
            // An unanswered divergence question is the exception. Its own text
            // is already in the real tree, so the next diff of that region is
            // (yours → proposal) under a *different* id, and this retain would
            // otherwise drop the question the user is still deciding about.
            //
            // Safe against stranding: a question is only cleared by answering
            // it or by the region reaching `Typed`, and the engine commits a
            // terminal status *before* it recomputes — so by the time a fixed
            // region produces no fresh hunk, this entry is no longer pending.
            h.pending_divergence.is_some() || fresh_ids.contains(h.id.as_str())
        } else {
            // Rules 2 and 4: every terminal status stays, whether or not it is
            // still outstanding in the trees. This is what makes `skip` durable
            // and `keep mine` final.
            true
        }
    });
    report.dropped = before - manifest.hunks.len();

    // Rule 3: a *typed* hunk that reappears means the user's work was
    // overwritten or never landed. This applies to `typed` alone — applying it
    // to every terminal status would resurrect skipped and diverged hunks on
    // every single recompute.
    for h in manifest.hunks.iter_mut() {
        if h.status == Status::Typed && fresh_ids.contains(h.id.as_str()) {
            h.status = Status::Pending;
            report.warnings.push(format!(
                "{} (hunk {}) differs from the shadow again — it was typed earlier this session. \
                 Back in the queue.",
                h.file, h.id
            ));
        }
    }

    // Rule 6: anything the trees show that the manifest has never seen. Note a
    // reworked region arrives here rather than being matched: new content means
    // a new ID, and a stale skipped entry for the old version does not suppress
    // the new proposal.
    //
    // Divergence needs more than ID matching, and this is the subtle part.
    // Keeping your own version *edits the real tree*, so the next diff of that
    // region is no longer (nothing → proposal) but (what you typed → proposal).
    // Different content means a different ID, so rule 4 never sees it and the
    // rejected proposal comes back as a brand new hunk, every recompute,
    // forever. A resolved divergence is therefore matched by its content: a
    // fresh hunk that wants to replace exactly what you typed with exactly what
    // you already turned down is the same decision, already made.
    let known: HashSet<String> = manifest.hunks.iter().map(|h| h.id.clone()).collect();
    // Cloned rather than borrowed: the loop below pushes into `manifest.hunks`.
    let resolved: Vec<(String, Divergence)> = manifest
        .hunks
        .iter()
        .filter(|h| h.status == Status::Diverged)
        .filter_map(|h| h.divergence.clone().map(|d| (h.file.clone(), d)))
        .collect();

    // A question the user has been asked but has not answered. Exactly the same
    // predicate as `resolved` over a different status, and that symmetry is the
    // argument for it: answering `keep` turns `pending_divergence` into
    // `divergence` and `Pending` into `Diverged`, so an open question and a
    // closed one describe the same region the same way. Without this the fresh
    // hunk restating the question is appended beside the entry that already
    // carries it, and the user is asked twice about one disagreement.
    let asked: Vec<(String, Divergence)> = manifest
        .hunks
        .iter()
        .filter(|h| h.status == Status::Pending)
        .filter_map(|h| h.pending_divergence.clone().map(|d| (h.file.clone(), d)))
        .collect();

    // What each typed hunk put into the real tree. Used to recognise the
    // whitespace phantom below.
    let typed_regions: Vec<(String, Vec<String>)> = manifest
        .hunks
        .iter()
        .filter(|h| h.status == Status::Typed)
        .map(|h| (h.file.clone(), h.new_lines.clone()))
        .collect();

    for f in fresh {
        if known.contains(&f.id) {
            continue;
        }
        let already_decided = resolved.iter().any(|(file, d)| {
            *file == f.file && d.actual == f.old_lines && d.proposed == f.new_lines
        });
        if already_decided {
            continue;
        }
        let already_asked = asked.iter().any(|(file, d)| {
            *file == f.file && d.actual == f.old_lines && d.proposed == f.new_lines
        });
        if already_asked {
            continue;
        }
        if is_whitespace_phantom(&f, &typed_regions, strict_whitespace) {
            continue;
        }
        manifest.hunks.push(f);
        report.added += 1;
    }

    report
}

/// A fresh hunk that exists only because the user's typing was accepted under a
/// whitespace tolerance the tree comparison does not share.
///
/// `classify` calls a hunk typed when the region matches the proposal modulo
/// trailing whitespace, but `compute_hunks` compares the trees byte for byte —
/// so a tolerated trailing space leaves the region still differing, and the next
/// recompute produces a hunk whose `old_lines` are the user's version and whose
/// `new_lines` are the proposal. Its id is new, so rule 3 cannot recognise it
/// and rule 6 appends it: a permanent pending hunk that says nothing but "you
/// left a trailing space", and that typing cannot clear, because typing it
/// exactly produces yet another state.
///
/// Narrow on purpose. It fires only when the two sides differ by nothing but
/// trailing whitespace *and* some typed hunk in the same file already put that
/// proposal into the tree. A genuinely new proposal for a region the user has
/// touched still reaches the queue — the property
/// `a_genuinely_new_proposal_for_a_diverged_region_is_still_offered` guards.
fn is_whitespace_phantom(
    f: &Hunk,
    typed_regions: &[(String, Vec<String>)],
    strict_whitespace: bool,
) -> bool {
    if strict_whitespace {
        // The user asked for whitespace to count. Then it is a real difference.
        return false;
    }
    if f.old_lines.is_empty() || f.new_lines.is_empty() {
        return false;
    }
    if !crate::present::lines_eq(&f.old_lines, &f.new_lines, false) {
        return false;
    }
    // `is_subrun`, not equality: git diffs only the lines that differ, so the
    // phantom covers a subset of what the typed hunk proposed.
    typed_regions.iter().any(|(file, new_lines)| {
        *file == f.file && crate::present::is_subrun(new_lines, &f.new_lines, false)
    })
}

fn short(sha: &str) -> String {
    sha.chars().take(8).collect()
}

/// Archive the manifest and the agent's unabsorbed work, then clear the session.
///
/// Ordering is load-bearing: the residue patch is written before the caller
/// syncs, because that sync is what destroys the shadow's only copy of any
/// skipped or diverged work.
pub fn archive_and_clear(
    manifest: &mut Manifest,
    project: &ProjectPaths,
    cfg: &Config,
    terminal: Terminal,
) -> Result<ArchivePaths> {
    let slug = crate::now_timestamp_slug()?;
    let dir = project.archive_dir();
    assert_not_in_real_tree(&dir, &project.repo_root);
    std::fs::create_dir_all(&dir).with_context(|| format!("cannot create {}", dir.display()))?;

    let patch_path = dir.join(format!("{slug}.patch"));
    let residue = shadow::residue_diff(project, cfg)?;
    write_atomic(&patch_path, &residue)?;

    manifest.state = State::Idle;
    manifest.terminal = Some(terminal);
    let manifest_path = dir.join(format!("{slug}.json"));
    let mut bytes = serde_json::to_vec_pretty(&manifest).context("cannot serialize manifest")?;
    bytes.push(b'\n');
    write_atomic(&manifest_path, &bytes)?;

    // Clearing session.json is what returns the project to idle.
    let live = project.session_json();
    match std::fs::remove_file(&live) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e).with_context(|| format!("cannot remove {}", live.display())),
    }

    Ok(ArchivePaths {
        manifest: manifest_path,
        patch: patch_path,
        residue_bytes: residue.len(),
    })
}

#[derive(Debug, Clone)]
pub struct ArchivePaths {
    pub manifest: std::path::PathBuf,
    pub patch: std::path::PathBuf,
    pub residue_bytes: usize,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hunks::Op;

    fn hunk(id_seed: &str, status: Status) -> Hunk {
        let mut h = Hunk {
            id: String::new(),
            key: String::new(),
            file: format!("{id_seed}.rs"),
            op: Op::Replace,
            context_before: vec!["ctx".into()],
            old_lines: vec![format!("old {id_seed}")],
            new_lines: vec![format!("new {id_seed}")],
            context_after: vec![],
            anchor_hint: 1,
            status,
            divergence: None,
            note: None,
            pending_divergence: None,
            curator_note: None,
            curator_rank: None,
            input: Default::default(),
        };
        h.id = Hunk::compute_id(
            &h.file,
            &h.old_lines,
            &h.new_lines,
            &h.context_before,
            &h.context_after,
        );
        h
    }

    /// A hunk with explicit content, for the rules that turn on what the lines
    /// actually say rather than merely on identity.
    fn hunk_with(file: &str, old: &[&str], new: &[&str], status: Status) -> Hunk {
        let mut h = hunk("seed", status);
        h.file = file.into();
        h.old_lines = old.iter().map(|s| s.to_string()).collect();
        h.new_lines = new.iter().map(|s| s.to_string()).collect();
        h.id = Hunk::compute_id(
            &h.file,
            &h.old_lines,
            &h.new_lines,
            &h.context_before,
            &h.context_after,
        );
        h.key = Hunk::compute_key(&h.file, &h.old_lines, &h.new_lines);
        h
    }

    fn manifest_with(hunks: Vec<Hunk>) -> Manifest {
        Manifest {
            version: MANIFEST_VERSION,
            state: State::Transcribing,
            task: "t".into(),
            created_at: "2026-01-01T00:00:00Z".into(),
            project_root: "/tmp/p".into(),
            shadow_dir: "/tmp/s".into(),
            baseline: BaselineRecord {
                head: "abc".into(),
                uncommitted_digest: String::new(),
                synced_at: "2026-01-01T00:00:00Z".into(),
            },
            hunks,
            generation: 0,
            last_presented: None,
            terminal: None,
        }
    }

    #[test]
    fn manifest_serializes_state_lowercase() {
        let m = manifest_with(vec![]);
        let json = serde_json::to_string(&m).unwrap();
        assert!(json.contains("\"state\":\"transcribing\""), "{json}");
        assert!(
            !json.contains("terminal"),
            "live manifests carry no terminal"
        );
        let back: Manifest = serde_json::from_str(&json).unwrap();
        assert_eq!(m, back);
    }

    #[test]
    fn archived_manifest_records_how_it_ended() {
        let mut m = manifest_with(vec![]);
        m.state = State::Idle;
        m.terminal = Some(Terminal::Aborted);
        let json = serde_json::to_string(&m).unwrap();
        assert!(json.contains("\"terminal\":\"aborted\""), "{json}");
    }

    // (a) A typed hunk vanishes from the fresh set; its status is preserved.
    #[test]
    fn typed_hunk_absent_from_fresh_keeps_its_status() {
        let mut m = manifest_with(vec![hunk("a", Status::Typed)]);
        let report = reconcile(&mut m, vec![], false);
        assert_eq!(m.hunks.len(), 1);
        assert_eq!(m.hunks[0].status, Status::Typed);
        assert_eq!(m.pending().count(), 0);
        assert!(report.warnings.is_empty());
    }

    // (b) The agent reworks a pending hunk: old one dropped, new one appended.
    #[test]
    fn reworked_pending_hunk_is_replaced() {
        let old = hunk("a", Status::Pending);
        let mut m = manifest_with(vec![old.clone()]);
        let new = hunk("b", Status::Pending);
        let report = reconcile(&mut m, vec![new.clone()], false);
        assert_eq!(report.dropped, 1);
        assert_eq!(report.added, 1);
        assert_eq!(m.hunks.len(), 1);
        assert_eq!(m.hunks[0].id, new.id);
    }

    // (c) A typed hunk unexpectedly reappears: reset to pending, with a warning.
    #[test]
    fn typed_hunk_that_reappears_is_reset_with_a_warning() {
        let h = hunk("a", Status::Typed);
        let mut m = manifest_with(vec![h.clone()]);
        let report = reconcile(&mut m, vec![hunk("a", Status::Pending)], false);
        assert_eq!(m.hunks[0].status, Status::Pending);
        assert_eq!(report.warnings.len(), 1);
    }

    // (d) New agent work mid-session is appended.
    #[test]
    fn new_agent_work_is_appended() {
        let existing = hunk("a", Status::Pending);
        let mut m = manifest_with(vec![existing.clone()]);
        let report = reconcile(
            &mut m,
            vec![hunk("a", Status::Pending), hunk("b", Status::Pending)],
            false,
        );
        assert_eq!(report.added, 1);
        assert_eq!(m.hunks.len(), 2);
        assert_eq!(m.hunks[0].id, existing.id, "existing order preserved");
    }

    // (f) A skipped hunk stays skipped, recompute after recompute.
    #[test]
    fn skipped_hunk_survives_repeated_recomputes() {
        let h = hunk("a", Status::Skipped);
        let mut m = manifest_with(vec![h.clone()]);
        // Its region still differs, so it is in every fresh set, forever.
        for round in 0..10 {
            let report = reconcile(&mut m, vec![hunk("a", Status::Pending)], false);
            assert_eq!(
                m.hunks.len(),
                1,
                "round {round}: skipped hunk must not be duplicated"
            );
            assert_eq!(
                m.hunks[0].status,
                Status::Skipped,
                "round {round}: skip must be durable"
            );
            assert_eq!(
                m.pending().count(),
                0,
                "round {round}: stays out of the queue"
            );
            assert_eq!(report.added, 0);
        }
    }

    // (g) Same for a divergence the user chose to keep.
    #[test]
    fn diverged_hunk_survives_repeated_recomputes() {
        let mut h = hunk("a", Status::Diverged);
        h.divergence = Some(Divergence {
            proposed: vec!["new a".into()],
            actual: vec!["my version".into()],
        });
        let mut m = manifest_with(vec![h]);
        for round in 0..10 {
            reconcile(&mut m, vec![hunk("a", Status::Pending)], false);
            assert_eq!(m.hunks.len(), 1, "round {round}");
            assert_eq!(m.hunks[0].status, Status::Diverged, "round {round}");
            assert!(
                m.hunks[0].divergence.is_some(),
                "round {round}: both versions kept"
            );
            assert_eq!(m.pending().count(), 0, "round {round}");
        }
    }

    // (h) Skipped, then reworked by the agent: a new pending hunk appears while
    //     the old skipped entry stays terminal in the history.
    #[test]
    fn reworking_a_skipped_region_yields_a_new_pending_hunk() {
        let skipped = hunk("a", Status::Skipped);
        let mut m = manifest_with(vec![skipped.clone()]);
        let reworked = hunk("b", Status::Pending);

        let report = reconcile(&mut m, vec![reworked.clone()], false);

        assert_eq!(report.added, 1);
        assert_eq!(m.hunks.len(), 2);
        let old = m.hunks.iter().find(|h| h.id == skipped.id).unwrap();
        assert_eq!(old.status, Status::Skipped, "history is kept");
        let new = m.hunks.iter().find(|h| h.id == reworked.id).unwrap();
        assert_eq!(new.status, Status::Pending, "the new proposal is offered");
    }

    #[test]
    fn a_kept_divergence_is_not_re_proposed_under_a_new_id() {
        // Keeping your version rewrites the real tree, so the next diff runs
        // (yours -> proposal) and hashes to a different ID than the original
        // hunk. Without content matching this reappears on every recompute.
        let mut original = hunk("a", Status::Diverged);
        original.divergence = Some(Divergence {
            proposed: vec!["new a".into()],
            actual: vec!["my version".into()],
        });
        let mut m = manifest_with(vec![original]);

        // What the trees now show: replace what the user typed with the proposal.
        let mut fresh = hunk("a", Status::Pending);
        fresh.old_lines = vec!["my version".into()];
        fresh.new_lines = vec!["new a".into()];
        fresh.id = Hunk::compute_id(
            &fresh.file,
            &fresh.old_lines,
            &fresh.new_lines,
            &fresh.context_before,
            &fresh.context_after,
        );
        assert_ne!(fresh.id, m.hunks[0].id, "the fixture must have a new ID");

        for round in 0..5 {
            let report = reconcile(&mut m, vec![fresh.clone()], false);
            assert_eq!(
                report.added, 0,
                "round {round}: the decision was already made"
            );
            assert_eq!(m.hunks.len(), 1, "round {round}");
            assert_eq!(m.pending().count(), 0, "round {round}");
        }
    }

    #[test]
    fn a_genuinely_new_proposal_for_a_diverged_region_is_still_offered() {
        // Suppression is narrow: it matches only the exact decision already
        // made. If the agent proposes something *different*, that is new work.
        let mut original = hunk("a", Status::Diverged);
        original.divergence = Some(Divergence {
            proposed: vec!["new a".into()],
            actual: vec!["my version".into()],
        });
        let mut m = manifest_with(vec![original]);

        let mut fresh = hunk("a", Status::Pending);
        fresh.old_lines = vec!["my version".into()];
        fresh.new_lines = vec!["a third idea".into()];
        fresh.id = Hunk::compute_id(
            &fresh.file,
            &fresh.old_lines,
            &fresh.new_lines,
            &fresh.context_before,
            &fresh.context_after,
        );

        let report = reconcile(&mut m, vec![fresh], false);
        assert_eq!(report.added, 1, "a different proposal is genuinely new");
        assert_eq!(m.pending().count(), 1);
    }

    #[test]
    fn mixed_queue_reaches_terminal_and_stays_there() {
        // A session holding one of everything must converge, not loop.
        let mut m = manifest_with(vec![
            hunk("typed", Status::Typed),
            hunk("skipped", Status::Skipped),
            hunk("diverged", Status::Diverged),
        ]);
        // The skipped and diverged regions still differ; the typed one does not.
        let fresh = vec![
            hunk("skipped", Status::Pending),
            hunk("diverged", Status::Pending),
        ];
        for _ in 0..5 {
            let report = reconcile(&mut m, fresh.clone(), false);
            assert_eq!(report.added, 0, "nothing new should ever be added");
            assert_eq!(m.pending().count(), 0, "the queue must stay empty");
            assert_eq!(m.hunks.len(), 3);
        }
    }

    #[test]
    fn queue_position_counts_only_pending() {
        let m = manifest_with(vec![
            hunk("done", Status::Typed),
            hunk("next", Status::Pending),
            hunk("later", Status::Pending),
        ]);
        let ids: Vec<_> = m.queue_view().iter().map(|h| h.id.clone()).collect();
        assert_eq!(ids.len(), 2, "terminal hunks are not in the queue");
        assert_eq!(m.queue_position(&ids[0]), Some(1));
        assert_eq!(m.queue_position(&ids[1]), Some(2));
        assert_eq!(m.count(Status::Typed), 1);
    }

    #[test]
    fn the_queue_is_ordered_by_file_not_by_insertion() {
        // The fixture names files after the seed, so "later.rs" sorts before
        // "next.rs" despite being appended second. Storage order is arrival
        // order; the queue is a sorted view over it.
        let m = manifest_with(vec![
            hunk("next", Status::Pending),
            hunk("later", Status::Pending),
        ]);
        let files: Vec<_> = m.queue_view().iter().map(|h| h.file.clone()).collect();
        assert_eq!(files, vec!["later.rs", "next.rs"]);
        assert_eq!(
            m.hunks.iter().map(|h| h.file.clone()).collect::<Vec<_>>(),
            vec!["next.rs", "later.rs"],
            "storage order is untouched"
        );
        assert_eq!(m.head_of_queue().unwrap().file, "later.rs");
    }

    #[test]
    fn curator_rank_outranks_the_deterministic_sort() {
        let mut m = manifest_with(vec![
            hunk("aaa", Status::Pending),
            hunk("zzz", Status::Pending),
        ]);
        // Unranked, "aaa.rs" leads on file order.
        assert_eq!(m.head_of_queue().unwrap().file, "aaa.rs");
        // Ranked, the curator decides.
        m.hunks[1].curator_rank = Some(1);
        assert_eq!(m.head_of_queue().unwrap().file, "zzz.rs");
        // And an unranked hunk sorts after every ranked one.
        let order: Vec<_> = m.queue_view().iter().map(|h| h.file.clone()).collect();
        assert_eq!(order, vec!["zzz.rs", "aaa.rs"]);
    }

    // ------------------------------------------------ unanswered divergence

    /// A pending hunk carrying an unanswered question, plus the fresh hunk the
    /// trees now produce for that same region: (what they typed → proposal).
    fn open_question() -> (Hunk, Hunk) {
        let mut asked = hunk_with("m.py", &["    was()"], &["    proposal()"], Status::Pending);
        asked.pending_divergence = Some(Divergence {
            proposed: vec!["    proposal()".into()],
            actual: vec!["    mine()".into()],
        });
        // Their text is already in the tree, so the diff has moved on.
        let fresh = hunk_with(
            "m.py",
            &["    mine()"],
            &["    proposal()"],
            Status::Pending,
        );
        assert_ne!(asked.id, fresh.id, "the region re-ids once they type");
        (asked, fresh)
    }

    #[test]
    fn an_unanswered_divergence_survives_a_recompute_that_no_longer_lists_it() {
        // Rule 5 would drop it: its id is absent from `fresh` the moment the
        // user's own text lands in the real tree. Dropping a question the user
        // is still deciding about loses their work silently.
        let (asked, _) = open_question();
        let mut m = manifest_with(vec![asked.clone()]);

        let report = reconcile(&mut m, vec![], false);

        assert_eq!(report.dropped, 0, "the question must not be dropped");
        assert_eq!(m.hunks.len(), 1);
        assert!(m.hunks[0].pending_divergence.is_some());
    }

    #[test]
    fn the_same_question_is_not_appended_a_second_time() {
        let (asked, fresh) = open_question();
        let mut m = manifest_with(vec![asked.clone()]);

        for round in 0..5 {
            let report = reconcile(&mut m, vec![fresh.clone()], false);
            assert_eq!(report.added, 0, "round {round}: one question, one entry");
            assert_eq!(m.hunks.len(), 1, "round {round}");
            assert_eq!(
                m.hunks[0].id, asked.id,
                "round {round}: the entry the user was told to resolve survives"
            );
        }
    }

    #[test]
    fn a_genuinely_new_proposal_while_a_question_is_open_is_still_appended() {
        // The narrowness guard. Suppression keys on the question's content, so
        // the agent reworking that region must still reach the queue.
        let (asked, _) = open_question();
        let mut m = manifest_with(vec![asked]);
        let reworked = hunk_with(
            "m.py",
            &["    mine()"],
            &["    something_else()"],
            Status::Pending,
        );

        let report = reconcile(&mut m, vec![reworked], false);

        assert_eq!(
            report.added, 1,
            "a different proposal is a different question"
        );
        assert_eq!(m.hunks.len(), 2);
    }

    #[test]
    fn fixing_the_text_clears_the_question_before_the_retain_can_strand_it() {
        // The third touch point. If the user types the proposal correctly
        // instead of answering, the region matches the shadow and `fresh` is
        // empty — so the retain guard above would keep a pending entry alive
        // forever. The engine's ordering is what prevents it: the terminal
        // status is committed first, and a typed hunk is not pending.
        let (mut asked, _) = open_question();
        asked.status = Status::Typed;
        asked.pending_divergence = None;
        let mut m = manifest_with(vec![asked]);

        let report = reconcile(&mut m, vec![], false);

        assert_eq!(report.added, 0);
        assert_eq!(m.pending().count(), 0, "nothing is stranded in the queue");
        assert_eq!(m.count(Status::Typed), 1, "history is kept");
    }

    // ------------------------------------------------ whitespace phantom

    /// A typed hunk, and the phantom the trees then produce because the user's
    /// line carries a trailing space the byte comparison will not forgive.
    fn phantom() -> (Hunk, Hunk) {
        let typed = hunk_with("m.py", &["    was()"], &["    two();"], Status::Typed);
        let fresh = hunk_with("m.py", &["    two();   "], &["    two();"], Status::Pending);
        (typed, fresh)
    }

    #[test]
    fn a_trailing_whitespace_phantom_does_not_re_enter_the_queue() {
        let (typed, fresh) = phantom();
        let mut m = manifest_with(vec![typed]);

        for round in 0..5 {
            let report = reconcile(&mut m, vec![fresh.clone()], false);
            assert_eq!(
                report.added, 0,
                "round {round}: a trailing space is not a hunk"
            );
            assert_eq!(m.pending().count(), 0, "round {round}");
        }
    }

    #[test]
    fn a_whitespace_phantom_is_still_offered_under_strict_whitespace() {
        // Asking for strictness means asking for exactly this hunk.
        let (typed, fresh) = phantom();
        let mut m = manifest_with(vec![typed]);

        let report = reconcile(&mut m, vec![fresh], true);

        assert_eq!(report.added, 1);
        assert_eq!(m.pending().count(), 1);
    }

    #[test]
    fn a_real_edit_to_a_typed_region_is_not_mistaken_for_a_phantom() {
        // The phantom rule must not swallow the agent genuinely changing its
        // mind about a region the user already typed.
        let (typed, _) = phantom();
        let mut m = manifest_with(vec![typed]);
        let real = hunk_with("m.py", &["    two();"], &["    three();"], Status::Pending);

        let report = reconcile(&mut m, vec![real], false);

        assert_eq!(
            report.added, 1,
            "old and new differ by more than whitespace"
        );
    }

    // ------------------------------------------------ state promotion

    #[test]
    fn a_working_session_becomes_transcribing_once_the_queue_is_not_empty() {
        let mut m = manifest_with(vec![]);
        m.state = State::Working;
        promote_state(&mut m);
        assert_eq!(
            m.state,
            State::Working,
            "an empty queue is still just working"
        );

        m.hunks.push(hunk("a", Status::Pending));
        promote_state(&mut m);
        assert_eq!(m.state, State::Transcribing);

        // It never travels backwards once the queue drains.
        m.hunks[0].status = Status::Typed;
        promote_state(&mut m);
        assert_eq!(m.state, State::Transcribing);
    }
}
