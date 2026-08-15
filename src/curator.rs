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
    /// The session this belongs to, from `Manifest::session_stamp`.
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

// ------------------------------------------------------- asking, and reading

/// The curator's instructions. A constant so the payload is reproducible.
///
/// Two things it is deliberately *not* asked for. Not a summary of the change —
/// the user is about to read the hunk line by line and type it, so describing it
/// first would be the thing they read instead of the code. Not typing hazards —
/// noticing those is the exercise. What is left is the one judgement a reader
/// cannot make until they have seen everything: what has to exist before this
/// makes sense.
pub const CURATOR_PROMPT: &str = "You are ordering a set of code changes for \
someone who is about to type every line of them by hand, in the order you give, \
to learn them. Order them so that each hunk makes sense to a reader who has seen \
everything above it and nothing below it: definitions before uses, a data model \
before the code that reads it, the core change before the knock-on edits, tests \
and generated files last. For each hunk write one short line of ORDERING \
RATIONALE — what it depends on, or what depends on it (\"the Tag model \
everything below refers to\", \"uses Tag; type this after the model exists\"). Do \
not summarize what the hunk does; the reader is about to read it. Do not warn \
about typos or tricky syntax. Reply with JSON only, no prose and no code fence, \
in exactly this shape: {\"order\":[{\"hunk\":3,\"note\":\"...\"},...]}, listing \
every hunk number exactly once, best first.";

/// How long the curator gets. Shorter than the reviewer's five minutes: this is
/// a ranking task over material already summarized, and it runs while the user
/// waits to see a queue.
pub const CURATOR_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

/// How many hunks are offered in one payload.
///
/// Past this the ordering task stops being one a model does well and the payload
/// stops being cheap. Everything beyond the cap is still *considered* — it gets
/// a tombstone so typing does not re-fire the pass — and simply keeps file
/// order, which is the documented fallback.
pub const MAX_HUNKS: usize = 60;

/// Lines of each side of a hunk that go into the payload.
///
/// Ordering is a question about what a hunk touches, not about its every line,
/// and a single 400-line hunk must not crowd out the other fifty.
pub const MAX_LINES_PER_HUNK: usize = 30;

/// The longest note worth rendering above a hunk. Anything more is a paragraph,
/// and a paragraph is the summary the prompt asked it not to write.
pub const MAX_NOTE_CHARS: usize = 160;

/// One hunk as the curator sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub key: String,
    pub file: String,
    pub anchor_hint: usize,
    pub op: crate::hunks::Op,
    /// The innermost line of context above, as a "which symbol is this in" hint.
    pub in_symbol: Option<String>,
    pub old_lines: Vec<String>,
    pub new_lines: Vec<String>,
    /// The mechanical note (`possible rename from x`, or why a file is
    /// untypeable), which the model cannot reconstruct.
    pub note: Option<String>,
    /// What a previous pass already said, offered back as a hint.
    pub cached_note: Option<String>,
}

/// The pending hunks worth curating, in queue order, one per distinct key.
///
/// Deduplicated because two hunks in one file with the same `old -> new` pair
/// share a key by design (`disambiguate_ids` splits ids, not keys) — offering
/// the same change twice would only invite the model to rank it inconsistently
/// with itself.
pub fn candidates(manifest: &Manifest, cache: &Cache) -> Vec<Candidate> {
    let mut seen = std::collections::HashSet::new();
    manifest
        .queue_view()
        .iter()
        .filter(|h| is_rankable(h))
        .filter(|h| seen.insert(h.key.clone()))
        .map(|h| Candidate {
            key: h.key.clone(),
            file: h.file.clone(),
            anchor_hint: h.anchor_hint,
            op: h.op,
            in_symbol: h
                .context_before
                .iter()
                .rev()
                .find(|l| !l.trim().is_empty())
                .cloned(),
            old_lines: h.old_lines.clone(),
            new_lines: h.new_lines.clone(),
            note: h.note.clone(),
            cached_note: cache.entries.get(&h.key).and_then(|e| e.note.clone()),
        })
        .collect()
}

/// Assemble the payload, in the `== SECTION ==` shape DESIGN §8 established.
///
/// Hunks are labelled with small integers rather than their keys, and that is
/// not cosmetic: a model asked to echo `k-3f9a1c...` will eventually mangle one,
/// and a mangled key is a silently misplaced hunk. Index to key is a mapping we
/// own and can check.
pub fn build_payload(task: &str, candidates: &[Candidate]) -> String {
    let mut out = String::new();
    out.push_str("== TASK ==\n");
    out.push_str(if task.is_empty() {
        "(unspecified)"
    } else {
        task
    });

    out.push_str("\n\n== HOW TO ANSWER ==\n");
    out.push_str("{\"order\":[{\"hunk\":3,\"note\":\"...\"},...]} — every hunk number below\n");
    out.push_str("exactly once, best first. JSON only.\n");

    out.push_str("\n== HUNKS ==\n");
    for (i, c) in candidates.iter().take(MAX_HUNKS).enumerate() {
        out.push_str(&format!(
            "[{}] {}:{}  {}\n",
            i + 1,
            c.file,
            c.anchor_hint,
            op_label(c.op)
        ));
        if let Some(sym) = &c.in_symbol {
            out.push_str(&format!("    in: {}\n", sym.trim_end()));
        }
        if let Some(note) = &c.note {
            out.push_str(&format!("    note: {note}\n"));
        }
        if let Some(prev) = &c.cached_note {
            // Offered back so an unchanged hunk keeps a stable line rather than
            // being re-described slightly differently on every pass.
            out.push_str(&format!("    previously: {prev}\n"));
        }
        for l in c.old_lines.iter().take(MAX_LINES_PER_HUNK) {
            out.push_str(&format!("    - {l}\n"));
        }
        if c.old_lines.len() > MAX_LINES_PER_HUNK {
            out.push_str(&format!(
                "    - … {} more removed lines\n",
                c.old_lines.len() - MAX_LINES_PER_HUNK
            ));
        }
        for l in c.new_lines.iter().take(MAX_LINES_PER_HUNK) {
            out.push_str(&format!("    + {l}\n"));
        }
        if c.new_lines.len() > MAX_LINES_PER_HUNK {
            out.push_str(&format!(
                "    + … {} more added lines\n",
                c.new_lines.len() - MAX_LINES_PER_HUNK
            ));
        }
        if c.old_lines.is_empty() && c.new_lines.is_empty() {
            // An untypeable file — presented whole and byte-compared rather than
            // typed. It still belongs somewhere in the order, usually last.
            out.push_str("    (no lines to type; verified by byte comparison)\n");
        }
    }
    out
}

fn op_label(op: crate::hunks::Op) -> &'static str {
    use crate::hunks::Op;
    match op {
        Op::Insert => "insert",
        Op::Replace => "replace",
        Op::Delete => "delete",
        Op::CreateFile => "new file",
        Op::DeleteFile => "delete file",
    }
}

/// One entry of the model's answer, already range-checked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ranked {
    /// Zero-based index into the candidates that were offered.
    pub index: usize,
    pub note: Option<String>,
}

#[derive(Deserialize)]
struct RawOrder {
    order: Vec<RawRanked>,
}

#[derive(Deserialize)]
struct RawRanked {
    hunk: i64,
    #[serde(default)]
    note: Option<String>,
}

/// Read the model's answer, forgivingly.
///
/// Forgiving in specific, bounded ways, because the failure mode of strictness
/// here is discarding a perfectly good ordering over a code fence. It accepts a
/// fenced reply, a reply wrapped in prose, a bare top-level array, and unknown
/// keys. It does not accept an answer it cannot make sense of at all — that
/// returns `Err` so the caller can tombstone the batch and warn once, rather
/// than silently producing an empty ordering that looks like success.
pub fn parse_response(text: &str, n: usize) -> Result<Vec<Ranked>> {
    let body = extract_json(text).context("no JSON object or array in the reply")?;

    let raw: Vec<RawRanked> = if body.starts_with('[') {
        serde_json::from_str(&body).context("the reply is not a list of hunks")?
    } else {
        serde_json::from_str::<RawOrder>(&body)
            .context("the reply has no usable `order`")?
            .order
    };

    let mut seen = std::collections::HashSet::new();
    let out: Vec<Ranked> = raw
        .into_iter()
        .filter_map(|r| {
            // 1-based on the wire, because that is what the payload showed.
            let index = usize::try_from(r.hunk).ok()?.checked_sub(1)?;
            if index >= n || !seen.insert(index) {
                // Out of range, or named twice: the first position wins. A model
                // that repeats itself has still told us where it wanted the hunk
                // the first time.
                return None;
            }
            Some(Ranked {
                index,
                note: r.note.and_then(|n| tidy_note(&n)),
            })
        })
        .collect();

    if out.is_empty() {
        anyhow::bail!("the reply named no hunk that exists");
    }
    Ok(out)
}

/// One line, trimmed, bounded. `None` for a note that says nothing.
fn tidy_note(raw: &str) -> Option<String> {
    let line = raw.lines().find(|l| !l.trim().is_empty())?.trim();
    if line.is_empty() {
        return None;
    }
    let mut out: String = line.chars().take(MAX_NOTE_CHARS).collect();
    if line.chars().count() > MAX_NOTE_CHARS {
        out.push('…');
    }
    Some(out)
}

/// The first balanced JSON value in a blob of text.
///
/// Brace matching rather than "find the last `}`", and it tracks string literals
/// and their escapes, because a note containing a brace or a quote is entirely
/// likely in this of all applications — the material being described is code.
fn extract_json(text: &str) -> Option<String> {
    let chars: Vec<char> = text.chars().collect();
    let start = chars.iter().position(|c| *c == '{' || *c == '[')?;
    let (open, close) = if chars[start] == '{' {
        ('{', '}')
    } else {
        ('[', ']')
    };

    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    for (i, c) in chars.iter().enumerate().skip(start) {
        if in_string {
            if escaped {
                escaped = false;
            } else if *c == '\\' {
                escaped = true;
            } else if *c == '"' {
                in_string = false;
            }
            continue;
        }
        match *c {
            '"' => in_string = true,
            c if c == open => depth += 1,
            c if c == close => {
                depth -= 1;
                if depth == 0 {
                    return Some(chars[start..=i].iter().collect());
                }
            }
            _ => {}
        }
    }
    None
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
            session_id: "test-session".into(),
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

    // ------------------------------------------------------------- payload

    #[test]
    fn the_payload_labels_hunks_with_small_integers_not_keys() {
        // A model asked to echo `k-3f9a1c...` will eventually mangle one, and a
        // mangled key is a hunk silently placed somewhere it does not belong.
        let m = manifest(vec![hunk("k-aaaa1111", "a.rs"), hunk("k-bbbb2222", "b.rs")]);
        let payload = build_payload("add tagging", &candidates(&m, &Cache::new("s")));

        assert!(payload.contains("[1] a.rs:1"), "{payload}");
        assert!(payload.contains("[2] b.rs:1"), "{payload}");
        assert!(
            !payload.contains("k-aaaa1111"),
            "no key reaches the model: {payload}"
        );
        assert!(payload.contains("== TASK ==\nadd tagging"), "{payload}");
    }

    #[test]
    fn two_hunks_that_share_a_key_are_offered_to_the_model_once() {
        let mut m = manifest(vec![hunk("k-1", "a.rs"), hunk("k-1", "a.rs")]);
        m.hunks[1].id = "h-k-1.2".into();
        assert_eq!(candidates(&m, &Cache::new("s")).len(), 1);
    }

    #[test]
    fn the_payload_carries_the_note_a_hunk_already_has() {
        let mut m = manifest(vec![hunk("k-1", "a.rs")]);
        m.hunks[0].note = Some("possible rename from old_name".into());
        m.hunks[0].context_before = vec!["".into(), "impl Tag {".into()];

        let mut cache = Cache::new("s");
        cache.entries.insert(
            "k-1".into(),
            Entry {
                rank: Some(1),
                note: Some("the model everything refers to".into()),
            },
        );

        let payload = build_payload("t", &candidates(&m, &cache));
        assert!(
            payload.contains("note: possible rename from old_name"),
            "{payload}"
        );
        assert!(
            payload.contains("previously: the model everything refers to"),
            "an unchanged hunk should keep a stable line: {payload}"
        );
        assert!(
            payload.contains("in: impl Tag {"),
            "the innermost non-blank context line: {payload}"
        );
    }

    #[test]
    fn the_payload_stops_at_the_cap() {
        let hunks: Vec<Hunk> = (0..MAX_HUNKS + 5)
            .map(|i| hunk(&format!("k-{i:03}"), &format!("f{i:03}.rs")))
            .collect();
        let m = manifest(hunks);
        let all = candidates(&m, &Cache::new("s"));
        assert_eq!(all.len(), MAX_HUNKS + 5, "the caller still sees the rest");

        let payload = build_payload("t", &all);
        assert!(payload.contains(&format!("[{MAX_HUNKS}] ")), "{payload}");
        assert!(
            !payload.contains(&format!("[{}] ", MAX_HUNKS + 1)),
            "and stops there"
        );
    }

    #[test]
    fn a_long_hunk_does_not_crowd_out_the_others() {
        let mut h = hunk("k-1", "a.rs");
        h.new_lines = (0..MAX_LINES_PER_HUNK + 20)
            .map(|i| format!("line {i}"))
            .collect();
        let m = manifest(vec![h]);

        let payload = build_payload("t", &candidates(&m, &Cache::new("s")));
        assert!(payload.contains(&format!("+ line {}", MAX_LINES_PER_HUNK - 1)));
        assert!(!payload.contains(&format!("+ line {MAX_LINES_PER_HUNK}\n")));
        assert!(payload.contains("20 more added lines"), "{payload}");
    }

    #[test]
    fn an_untypeable_hunk_still_goes_into_the_order() {
        // A lockfile is verified by byte comparison rather than typed, but it
        // still belongs somewhere in the sequence — usually last.
        let mut h = hunk("k-1", "Cargo.lock");
        h.new_lines = vec![];
        h.note = Some("generated; run the command that produces it".into());
        let m = manifest(vec![h]);

        let payload = build_payload("t", &candidates(&m, &Cache::new("s")));
        assert!(payload.contains("[1] Cargo.lock:1"), "{payload}");
        assert!(payload.contains("no lines to type"), "{payload}");
    }

    #[test]
    fn the_curator_prompt_asks_for_ordering_rationale_not_a_summary() {
        // The one thing that separates a useful note from a distraction. If this
        // wording drifts, the notes quietly become descriptions of code the user
        // is about to read anyway.
        assert!(CURATOR_PROMPT.contains("ORDERING RATIONALE"));
        assert!(CURATOR_PROMPT.contains("Do not summarize what the hunk does"));
        assert!(CURATOR_PROMPT.contains("Do not warn about typos"));
        assert!(CURATOR_PROMPT.contains("definitions before uses"));
    }

    // -------------------------------------------------------------- parsing

    #[test]
    fn a_plain_reply_is_read() {
        let got = parse_response(r#"{"order":[{"hunk":2,"note":"first"},{"hunk":1}]}"#, 2).unwrap();
        assert_eq!(
            got,
            vec![
                Ranked {
                    index: 1,
                    note: Some("first".into())
                },
                Ranked {
                    index: 0,
                    note: None
                },
            ]
        );
    }

    #[test]
    fn a_fenced_json_reply_is_read_anyway() {
        let text = "```json\n{\"order\":[{\"hunk\":1,\"note\":\"a\"}]}\n```";
        assert_eq!(parse_response(text, 1).unwrap().len(), 1);
    }

    #[test]
    fn a_reply_wrapped_in_prose_is_read_anyway() {
        // Discarding a good ordering because the model said hello first would be
        // a silly way to lose a token spend.
        let text = "Here is the order:\n{\"order\":[{\"hunk\":1}]}\nHope that helps!";
        assert_eq!(parse_response(text, 1).unwrap().len(), 1);
    }

    #[test]
    fn a_bare_array_is_read_too() {
        assert_eq!(
            parse_response(r#"[{"hunk":1,"note":"x"}]"#, 1)
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn a_note_containing_a_brace_does_not_truncate_the_object() {
        // The material being described is code, so a brace or a quote inside a
        // note is not an edge case. Matching on the last `}` would cut here.
        let text = r#"{"order":[{"hunk":1,"note":"the impl Tag { } block"},{"hunk":2}]}"#;
        let got = parse_response(text, 2).unwrap();
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].note.as_deref(), Some("the impl Tag { } block"));
    }

    #[test]
    fn a_note_containing_an_escaped_quote_survives() {
        let text = r#"{"order":[{"hunk":1,"note":"sets name=\"tag\" first"}]}"#;
        let got = parse_response(text, 1).unwrap();
        assert_eq!(got[0].note.as_deref(), Some(r#"sets name="tag" first"#));
    }

    #[test]
    fn a_reply_naming_a_hunk_that_does_not_exist_is_ignored() {
        let got = parse_response(r#"{"order":[{"hunk":99},{"hunk":0},{"hunk":1}]}"#, 2).unwrap();
        assert_eq!(
            got,
            vec![Ranked {
                index: 0,
                note: None
            }]
        );
    }

    #[test]
    fn a_reply_that_names_a_hunk_twice_keeps_the_first_position() {
        let text = r#"{"order":[{"hunk":2,"note":"here"},{"hunk":1},{"hunk":2,"note":"no"}]}"#;
        let got = parse_response(text, 2).unwrap();
        assert_eq!(got.len(), 2);
        assert_eq!(
            got[0],
            Ranked {
                index: 1,
                note: Some("here".into())
            }
        );
    }

    #[test]
    fn a_multi_line_note_is_reduced_to_one_line() {
        let text = "{\"order\":[{\"hunk\":1,\"note\":\"\\n  first line\\nsecond line\"}]}";
        let got = parse_response(text, 1).unwrap();
        assert_eq!(got[0].note.as_deref(), Some("first line"));
    }

    #[test]
    fn an_essay_of_a_note_is_cut_to_something_that_fits_above_a_hunk() {
        let long = "x".repeat(MAX_NOTE_CHARS + 50);
        let text = format!(r#"{{"order":[{{"hunk":1,"note":"{long}"}}]}}"#);
        let note = parse_response(&text, 1).unwrap()[0].note.clone().unwrap();
        assert_eq!(
            note.chars().count(),
            MAX_NOTE_CHARS + 1,
            "plus the ellipsis"
        );
        assert!(note.ends_with('…'));
    }

    #[test]
    fn an_empty_note_is_dropped_rather_than_rendered_blank() {
        let got = parse_response(r#"{"order":[{"hunk":1,"note":"   "}]}"#, 1).unwrap();
        assert_eq!(got[0].note, None);
    }

    #[test]
    fn a_reply_with_nothing_usable_in_it_is_an_error() {
        // An error, not an empty ordering: the caller has to be able to tell
        // "the model declined" from "the model put them in this order", because
        // the first one deserves a warning and a tombstone.
        assert!(parse_response("I could not do that.", 3).is_err());
        assert!(parse_response(r#"{"order":[]}"#, 3).is_err());
        assert!(parse_response(r#"{"order":[{"hunk":9}]}"#, 3).is_err());
        assert!(parse_response("", 3).is_err());
    }

    #[test]
    fn pending_keys_are_deduplicated_and_skip_terminal_hunks() {
        let mut typed = hunk("k-9", "z.rs");
        typed.status = Status::Typed;
        let m = manifest(vec![hunk("k-1", "a.rs"), hunk("k-1", "a.rs"), typed]);
        assert_eq!(pending_keys(&m), vec!["k-1".to_string()]);
    }
}
