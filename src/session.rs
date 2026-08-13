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

pub const MANIFEST_VERSION: u32 = 1;

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

    pub fn head_of_queue(&self) -> Option<&Hunk> {
        self.hunks.iter().find(|h| h.status == Status::Pending)
    }

    pub fn count(&self, status: Status) -> usize {
        self.hunks.iter().filter(|h| h.status == status).count()
    }

    pub fn find_mut(&mut self, id: &str) -> Option<&mut Hunk> {
        self.hunks.iter_mut().find(|h| h.id == id)
    }

    /// Position of a hunk in the queue, 1-based, for progress display.
    pub fn queue_position(&self, id: &str) -> Option<usize> {
        self.hunks
            .iter()
            .filter(|h| h.status == Status::Pending)
            .position(|h| h.id == id)
            .map(|p| p + 1)
    }
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
    let mut report = reconcile(manifest, fresh);

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

/// Rules 2–7 of DESIGN.md §5, as a pure function over (manifest, fresh).
///
/// Separated from `recompute` so the rules are tested directly rather than
/// through a fixture repository — and so the tests exercise this code rather
/// than a reimplementation of it.
pub fn reconcile(manifest: &mut Manifest, fresh: Vec<Hunk>) -> RecomputeReport {
    let mut report = RecomputeReport::default();
    let fresh_ids: HashSet<&str> = fresh.iter().map(|h| h.id.as_str()).collect();

    // Rule 5: pending hunks the agent reworked away simply go.
    let before = manifest.hunks.len();
    manifest.hunks.retain(|h| {
        if h.status == Status::Pending {
            fresh_ids.contains(h.id.as_str())
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
        manifest.hunks.push(f);
        report.added += 1;
    }

    report
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
        let report = reconcile(&mut m, vec![]);
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
        let report = reconcile(&mut m, vec![new.clone()]);
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
        let report = reconcile(&mut m, vec![hunk("a", Status::Pending)]);
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
            let report = reconcile(&mut m, vec![hunk("a", Status::Pending)]);
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
            reconcile(&mut m, vec![hunk("a", Status::Pending)]);
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

        let report = reconcile(&mut m, vec![reworked.clone()]);

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
            let report = reconcile(&mut m, vec![fresh.clone()]);
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

        let report = reconcile(&mut m, vec![fresh]);
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
            let report = reconcile(&mut m, fresh.clone());
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
        let ids: Vec<_> = m.pending().map(|h| h.id.clone()).collect();
        assert_eq!(m.queue_position(&ids[0]), Some(1));
        assert_eq!(m.queue_position(&ids[1]), Some(2));
        assert_eq!(m.count(Status::Typed), 1);
    }
}
