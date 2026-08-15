//! The Hunk model, content-addressed IDs, splitting, and tree → hunk computation.
//!
//! The `Hunk` struct is a public contract: it crosses the `--json` seam that a
//! future nvim plugin will consume (DESIGN.md §4). Enum values serialize
//! lowercase and must stay that way.

use crate::config::Config;
use crate::diffparse::{self, DiffLine, ParsedDiff, RawHunk};
use crate::git;
use crate::paths::ProjectPaths;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Context lines carried on each side of a hunk.
pub const CONTEXT_LINES: usize = 3;

/// Minimum context a synthesized sub-hunk boundary gets (DESIGN.md §5).
pub const MIN_SYNTHESIZED_CONTEXT: usize = 2;

/// Bytes inspected when deciding whether a file is binary.
const BINARY_SNIFF_BYTES: usize = 8192;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Op {
    Insert,
    Replace,
    Delete,
    CreateFile,
    DeleteFile,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Pending,
    Typed,
    Diverged,
    Skipped,
}

impl Status {
    /// Terminal statuses leave the queue and, per DESIGN.md §5, are sticky.
    pub fn is_terminal(self) -> bool {
        !matches!(self, Status::Pending)
    }
}

impl std::fmt::Display for Status {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            Status::Pending => "pending",
            Status::Typed => "typed",
            Status::Diverged => "diverged",
            Status::Skipped => "skipped",
        };
        f.write_str(s)
    }
}

impl std::str::FromStr for Status {
    type Err = anyhow::Error;
    fn from_str(s: &str) -> Result<Self> {
        match s {
            "pending" => Ok(Status::Pending),
            "typed" => Ok(Status::Typed),
            "diverged" => Ok(Status::Diverged),
            "skipped" => Ok(Status::Skipped),
            other => {
                anyhow::bail!("unknown status {other:?} (want pending/typed/diverged/skipped)")
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Divergence {
    pub proposed: Vec<String>,
    pub actual: Vec<String>,
}

/// How the content of a hunk arrived in the real tree.
///
/// Two producers, and the difference between them is the whole design.
///
/// The **engine** infers, from the save history rather than from the bytes. A
/// hunk observed `InProgress` — the region a line-wise prefix of the proposal —
/// had a human in the loop at that moment, and one paste of a whole hunk cannot
/// produce that: it classifies `Typed` on the first save and never passes
/// through the prefix rule. This is deliberately weaker than "every character
/// was typed" — pasting the first half and then the second is recorded as
/// `typed`, and so is accepting a completion over a typed prefix. What the
/// engine will never do is assert `Pasted`, because a careful typist who writes
/// a whole hunk into the buffer and saves once is byte-identical to a paste.
/// That is why `Unknown` is a first-class answer rather than a null.
///
/// A **front end** reports, from something the filesystem cannot see: nvim knows
/// a bracketed paste from `TextChangedI`. A report always wins; an inference
/// only ever fills in `Unknown`. So a wrong report can be corrected by another
/// one, and an inference can never quietly overwrite what a front end saw.
///
/// Advisory throughout, and surfaced rather than enforced.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Input {
    #[default]
    Unknown,
    Typed,
    Pasted,
}

impl Input {
    /// Omitted from the wire when unset, like every other optional hunk field.
    pub fn is_unknown(&self) -> bool {
        *self == Input::Unknown
    }

    /// The engine's write: only ever into `Unknown`.
    ///
    /// An inference must not overwrite what a front end reported — you paste a
    /// hunk, the plugin says so, and then you fix a character in it, which the
    /// engine sees as typing. Without this guard that correction would erase the
    /// report.
    pub fn fill(&mut self, value: Input) -> bool {
        if !self.is_unknown() || value == *self {
            return false;
        }
        *self = value;
        true
    }

    /// A front end's write: always lands.
    ///
    /// Last write wins, which is what makes `rote report … typed` able to
    /// correct a heuristic that misfired. Two front ends disagreeing is
    /// therefore order-dependent; that is rarer than a heuristic being wrong,
    /// and correctable by hand either way.
    ///
    /// Returns whether anything moved, so a repeated report writes nothing and
    /// does not move the generation.
    pub fn set(&mut self, value: Input) -> bool {
        if *self == value {
            return false;
        }
        *self = value;
        true
    }
}

impl std::fmt::Display for Input {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Input::Unknown => "unknown",
            Input::Typed => "typed",
            Input::Pasted => "pasted",
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hunk {
    pub id: String,
    /// Content identity *without* context — see `compute_key`. Stable under the
    /// edits that churn `id`, so it is what caches key off.
    #[serde(default)]
    pub key: String,
    pub file: String,
    pub op: Op,
    pub context_before: Vec<String>,
    pub old_lines: Vec<String>,
    pub new_lines: Vec<String>,
    pub context_after: Vec<String>,
    /// Line in the REAL file at last recompute. Advisory only — the presenter
    /// re-locates by context and falls back to this (DESIGN.md §6).
    pub anchor_hint: usize,
    pub status: Status,
    pub divergence: Option<Divergence>,
    pub note: Option<String>,
    /// A divergence the user has not yet answered. The hunk stays `Pending`;
    /// `divergence` plus `Status::Diverged` is the answered, terminal form.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_divergence: Option<Divergence>,
    /// One line of "why", written by the curator pass. Kept apart from `note`,
    /// which carries mechanical facts ("possible rename from x") that a model
    /// cannot reconstruct and must not overwrite.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub curator_note: Option<String>,
    /// Curator-assigned teaching order. `None` sorts after everything ranked.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub curator_rank: Option<u32>,
    /// How this hunk's content arrived. See `Input`.
    #[serde(default, skip_serializing_if = "Input::is_unknown")]
    pub input: Input,
}

/// The four line arrays of a hunk, which always travel together.
#[derive(Debug, Clone, Default)]
struct Lines {
    context_before: Vec<String>,
    old: Vec<String>,
    new: Vec<String>,
    context_after: Vec<String>,
}

impl Hunk {
    /// Content-addressed: an unchanged proposal keeps its ID across recomputes,
    /// a reworked one becomes a different hunk. Reconciliation depends on this.
    pub fn compute_id(
        file: &str,
        old_lines: &[String],
        new_lines: &[String],
        context_before: &[String],
        context_after: &[String],
    ) -> String {
        let mut h = Sha256::new();
        // Field- and group-separated with bytes that cannot appear in a line, so
        // ["a","b"] and ["a\nb"] cannot collide, and neither can a boundary shift
        // between old_lines and new_lines.
        h.update(file.as_bytes());
        h.update([0u8]);
        for group in [old_lines, new_lines, context_before, context_after] {
            for line in group {
                h.update(line.as_bytes());
                h.update([0u8]);
            }
            h.update([1u8]);
        }
        let digest = h.finalize();
        // 8 bytes, not 4. At 32 bits a session hits a ~1% birthday collision
        // around nine thousand hunks, and every lookup in the manifest is by id.
        let hex: String = digest.iter().take(8).map(|b| format!("{b:02x}")).collect();
        format!("h-{hex}")
    }

    /// Content identity ignoring surrounding context.
    ///
    /// `compute_id` folds in up to `CONTEXT_LINES` lines each side, which is what
    /// makes it a precise reconciliation key — and what makes it useless as a
    /// cache key. Typing anywhere within three lines of a hunk changes its
    /// neighbour's context and therefore its id, so anything stored against an id
    /// evaporates as the user works. The key survives exactly that churn.
    ///
    /// Two hunks in one file with the same `old -> new` pair share a key. That is
    /// correct: they are the same change, and they deserve the same note.
    pub fn compute_key(file: &str, old_lines: &[String], new_lines: &[String]) -> String {
        let mut h = Sha256::new();
        h.update(file.as_bytes());
        h.update([0u8]);
        for group in [old_lines, new_lines] {
            for line in group {
                h.update(line.as_bytes());
                h.update([0u8]);
            }
            h.update([1u8]);
        }
        let digest = h.finalize();
        let hex: String = digest.iter().take(8).map(|b| format!("{b:02x}")).collect();
        format!("k-{hex}")
    }

    fn new(file: &str, op: Op, lines: Lines, anchor_hint: usize, note: Option<String>) -> Self {
        let id = Self::compute_id(
            file,
            &lines.old,
            &lines.new,
            &lines.context_before,
            &lines.context_after,
        );
        let key = Self::compute_key(file, &lines.old, &lines.new);
        Self {
            id,
            key,
            file: file.to_string(),
            op,
            context_before: lines.context_before,
            old_lines: lines.old,
            new_lines: lines.new,
            context_after: lines.context_after,
            anchor_hint,
            status: Status::Pending,
            divergence: None,
            note,
            pending_divergence: None,
            curator_note: None,
            curator_rank: None,
            input: Input::default(),
        }
    }

    /// A hunk that cannot be typed: binary content, or a generated file the user
    /// should regenerate. Classified by byte comparison (DESIGN.md §5).
    pub fn is_untypeable(&self) -> bool {
        self.old_lines.is_empty() && self.new_lines.is_empty()
    }
}

/// Derive the line-level op. Whole-file ops are decided by the caller.
fn derive_op(old_lines: &[String], new_lines: &[String]) -> Op {
    match (old_lines.is_empty(), new_lines.is_empty()) {
        (true, false) => Op::Insert,
        (false, true) => Op::Delete,
        _ => Op::Replace,
    }
}

/// Leading-whitespace width, used as a crude block-boundary signal.
fn indent_of(line: &str) -> usize {
    line.len() - line.trim_start().len()
}

/// How many lines the first sub-hunk should take from `lines`.
///
/// Preference order per DESIGN.md §5: a blank line, then a line less indented
/// than its successor (which opens a block, so cut before it), then a hard cut.
pub fn find_split(lines: &[String], max: usize) -> usize {
    if lines.len() <= max {
        return lines.len();
    }
    // Cut just after the last blank line inside the window.
    for i in (0..max).rev() {
        if lines[i].trim().is_empty() {
            return i + 1;
        }
    }
    // Cut before a line that opens a block.
    for i in (1..max).rev() {
        if indent_of(&lines[i]) < indent_of(&lines[i + 1]) {
            return i;
        }
    }
    max
}

/// One contiguous edit inside a raw hunk, before size splitting.
#[derive(Debug, Clone)]
struct ChangeGroup {
    context_before: Vec<String>,
    old_lines: Vec<String>,
    new_lines: Vec<String>,
    context_after: Vec<String>,
    anchor: usize,
}

/// Split a raw hunk into its independent edits.
///
/// With `--unified=3`, git merges nearby edits into one hunk separated by
/// context. Those are separate things to type, so they become separate hunks
/// rather than one hunk whose line arrays span unrelated regions.
fn change_groups(h: &RawHunk) -> Vec<ChangeGroup> {
    // Line numbers in each file at every position in the hunk.
    // Only the old-side number matters: anchors point into the REAL file.
    let mut old_at = Vec::with_capacity(h.lines.len());
    let mut o = h.old_start.max(1);
    for line in &h.lines {
        old_at.push(o);
        if matches!(line, DiffLine::Context(_) | DiffLine::Removed(_)) {
            o += 1;
        }
    }

    let mut groups = Vec::new();
    let mut i = 0;
    while i < h.lines.len() {
        if !h.lines[i].is_change() {
            i += 1;
            continue;
        }
        let start = i;
        while i < h.lines.len() && h.lines[i].is_change() {
            i += 1;
        }
        let end = i;

        let mut before: Vec<String> = Vec::new();
        let mut k = start;
        while k > 0 && !h.lines[k - 1].is_change() && before.len() < CONTEXT_LINES {
            before.push(h.lines[k - 1].text().to_string());
            k -= 1;
        }
        before.reverse();

        let after: Vec<String> = h.lines[end..]
            .iter()
            .take_while(|l| !l.is_change())
            .take(CONTEXT_LINES)
            .map(|l| l.text().to_string())
            .collect();

        groups.push(ChangeGroup {
            context_before: before,
            old_lines: h.lines[start..end]
                .iter()
                .filter_map(|l| match l {
                    DiffLine::Removed(s) => Some(s.clone()),
                    _ => None,
                })
                .collect(),
            new_lines: h.lines[start..end]
                .iter()
                .filter_map(|l| match l {
                    DiffLine::Added(s) => Some(s.clone()),
                    _ => None,
                })
                .collect(),
            context_after: after,
            anchor: old_at[start],
        });
    }
    groups
}

/// Turn one change group into one or more hunks, splitting at `max`.
///
/// Splitting counts `max(old, new)`: counting only `new_lines` would leave every
/// large deletion unsplittable, since a pure removal has no new lines at all.
fn split_group(file: &str, g: &ChangeGroup, max: usize, whole_file: Option<Op>) -> Vec<Hunk> {
    let unit = g.old_lines.len().max(g.new_lines.len());
    let op_for = |old: &[String], new: &[String], first: bool| -> Op {
        match whole_file {
            // Only the first sub-hunk creates the file; later ones append to it.
            Some(Op::CreateFile) if first => Op::CreateFile,
            Some(Op::DeleteFile) => Op::DeleteFile,
            Some(Op::CreateFile) => Op::Insert,
            _ => derive_op(old, new),
        }
    };

    if unit <= max {
        return vec![Hunk::new(
            file,
            op_for(&g.old_lines, &g.new_lines, true),
            Lines {
                context_before: g.context_before.clone(),
                old: g.old_lines.clone(),
                new: g.new_lines.clone(),
                context_after: g.context_after.clone(),
            },
            g.anchor,
            None,
        )];
    }

    // Split whichever side is long. For a replace, the deletion happens once (on
    // the first sub-hunk) and the new content is then typed in pieces.
    let split_new = g.new_lines.len() >= g.old_lines.len();
    let source = if split_new {
        &g.new_lines
    } else {
        &g.old_lines
    };

    let mut chunks: Vec<Vec<String>> = Vec::new();
    let mut rest = source.as_slice();
    while !rest.is_empty() {
        let take = find_split(rest, max);
        chunks.push(rest[..take].to_vec());
        rest = &rest[take..];
    }

    let last = chunks.len() - 1;
    let mut out = Vec::with_capacity(chunks.len());
    let mut consumed_old = 0usize;

    for (idx, chunk) in chunks.iter().enumerate() {
        // Context is synthesized from the neighbouring chunk where the original
        // is out of reach, so every sub-hunk still anchors on something real.
        let context_before = if idx == 0 {
            g.context_before.clone()
        } else {
            tail(&chunks[idx - 1], MIN_SYNTHESIZED_CONTEXT)
        };
        let context_after = if idx == last {
            g.context_after.clone()
        } else {
            head(&chunks[idx + 1], MIN_SYNTHESIZED_CONTEXT)
        };

        let (old_lines, new_lines) = if split_new {
            // All removals belong to the first sub-hunk.
            let old = if idx == 0 {
                g.old_lines.clone()
            } else {
                Vec::new()
            };
            (old, chunk.clone())
        } else {
            let new = if idx == 0 {
                g.new_lines.clone()
            } else {
                Vec::new()
            };
            (chunk.clone(), new)
        };

        let anchor = if split_new {
            // Everything old is removed by sub-hunk 0; later ones follow it.
            if idx == 0 {
                g.anchor
            } else {
                g.anchor + g.old_lines.len()
            }
        } else {
            g.anchor + consumed_old
        };
        consumed_old += if split_new { 0 } else { chunk.len() };

        out.push(Hunk::new(
            file,
            op_for(&old_lines, &new_lines, idx == 0),
            Lines {
                context_before,
                old: old_lines,
                new: new_lines,
                context_after,
            },
            anchor,
            None,
        ));
    }
    out
}

fn tail(v: &[String], n: usize) -> Vec<String> {
    v[v.len().saturating_sub(n)..].to_vec()
}

fn head(v: &[String], n: usize) -> Vec<String> {
    v[..n.min(v.len())].to_vec()
}

/// Build hunks for one file from its parsed diff.
pub fn hunks_for_file(file: &str, parsed: &ParsedDiff, max: usize) -> Vec<Hunk> {
    let whole_file = if parsed.old_is_devnull {
        Some(Op::CreateFile)
    } else if parsed.new_is_devnull {
        Some(Op::DeleteFile)
    } else {
        None
    };

    let mut out = Vec::new();
    for raw in &parsed.hunks {
        for group in change_groups(raw) {
            out.extend(split_group(file, &group, max, whole_file));
        }
    }
    if let Some(first) = out.first_mut() {
        if whole_file == Some(Op::CreateFile) {
            first.note = Some("file is new".into());
        } else if whole_file == Some(Op::DeleteFile) {
            first.note = Some("delete this file".into());
        }
    }
    out
}

fn looks_binary(bytes: &[u8]) -> bool {
    bytes.iter().take(BINARY_SNIFF_BYTES).any(|b| *b == 0)
}

fn read_opt(path: &Path) -> Result<Option<Vec<u8>>> {
    match std::fs::read(path) {
        Ok(b) => Ok(Some(b)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("cannot read {}", path.display())),
    }
}

/// An untypeable hunk: shown as a path and a note, gated on byte equality.
fn untypeable_hunk(file: &str, real_exists: bool, shadow_exists: bool, note: &str) -> Hunk {
    let op = match (real_exists, shadow_exists) {
        (false, true) => Op::CreateFile,
        (true, false) => Op::DeleteFile,
        _ => Op::Replace,
    };
    Hunk::new(file, op, Lines::default(), 1, Some(note.to_string()))
}

/// Every file that differs between the trees, as an ordered hunk queue.
///
/// Enumeration is `status --porcelain=v1 -z` in both trees, unioned: the shadow's
/// status covers the agent's work, and the real tree's covers anything the user
/// changed or created since the sync. Identical files are then skipped by a byte
/// comparison, so a generous candidate set costs only a read.
pub fn compute_hunks(project: &ProjectPaths, cfg: &Config) -> Result<Vec<Hunk>> {
    let real = &project.repo_root;
    let shadow = &project.shadow_dir;
    anyhow::ensure!(
        shadow.join(".git").exists(),
        "the shadow at {} is missing or corrupted.\nRun `rote abort`, then `rote start` again.",
        shadow.display()
    );

    let mut candidates: Vec<PathBuf> = git::status_paths(shadow)?;
    candidates.extend(git::status_paths(real)?);
    candidates.sort();
    candidates.dedup();

    let verbatim = cfg.verbatim_set()?;
    let devnull = PathBuf::from("/dev/null");
    let mut out = Vec::new();

    for rel in candidates {
        let rel_str = rel.to_string_lossy().to_string();
        let real_path = real.join(&rel);
        let shadow_path = shadow.join(&rel);

        let real_bytes = read_opt(&real_path)?;
        let shadow_bytes = read_opt(&shadow_path)?;
        if real_bytes == shadow_bytes {
            continue; // identical, or absent from both
        }

        let real_exists = real_bytes.is_some();
        let shadow_exists = shadow_bytes.is_some();
        let is_binary = real_bytes.as_deref().map(looks_binary).unwrap_or(false)
            || shadow_bytes.as_deref().map(looks_binary).unwrap_or(false);

        if is_binary {
            out.push(untypeable_hunk(
                &rel_str,
                real_exists,
                shadow_exists,
                "binary — copy it across yourself",
            ));
            continue;
        }
        if verbatim.is_match(&rel) {
            out.push(untypeable_hunk(
                &rel_str,
                real_exists,
                shadow_exists,
                "generated file — run the generating command instead",
            ));
            continue;
        }

        let a = if real_exists { &real_path } else { &devnull };
        let b = if shadow_exists {
            &shadow_path
        } else {
            &devnull
        };
        let diff = git::diff_no_index(a, b, CONTEXT_LINES)?;
        let mut parsed = diffparse::parse(&diff)?;
        // `--no-index` against /dev/null reports a path, not a null side.
        parsed.old_is_devnull |= !real_exists;
        parsed.new_is_devnull |= !shadow_exists;

        if parsed.is_binary {
            out.push(untypeable_hunk(
                &rel_str,
                real_exists,
                shadow_exists,
                "binary — copy it across yourself",
            ));
            continue;
        }

        out.extend(hunks_for_file(&rel_str, &parsed, cfg.max_hunk_lines));
    }

    annotate_possible_renames(&mut out);
    disambiguate_ids(&mut out);
    Ok(out)
}

/// Give exact-duplicate hunks distinct ids within one run.
///
/// Two identical edits in one file, with identical context, hash identically —
/// deterministically, not by birthday. Repetitive code makes that ordinary. The
/// manifest looks hunks up by id (`find_mut`, `queue_position`), and both return
/// the *first* match, so without this the second duplicate is unaddressable:
/// typing it mutates the first one's entry instead.
///
/// The suffix is positional within a run, and the run order is deterministic
/// (`candidates.sort()`, then diff order within a file), so ids stay stable
/// across recomputes for as long as the content is stable.
fn disambiguate_ids(hunks: &mut [Hunk]) {
    let mut seen: HashMap<String, usize> = HashMap::new();
    for h in hunks.iter_mut() {
        let n = seen.entry(h.id.clone()).or_insert(0);
        *n += 1;
        if *n > 1 {
            h.id = format!("{}.{}", h.id, n);
        }
    }
}

/// Fraction of lines the two sides share, ignoring order.
fn line_similarity(a: &[String], b: &[String]) -> f64 {
    if a.is_empty() && b.is_empty() {
        return 1.0;
    }
    let mut pool: Vec<&String> = b.iter().collect();
    let mut shared = 0usize;
    for line in a {
        if let Some(pos) = pool.iter().position(|c| *c == line) {
            pool.remove(pos);
            shared += 1;
        }
    }
    shared as f64 / a.len().max(b.len()) as f64
}

/// Note likely renames. DESIGN.md §9.5.
///
/// `--no-index` has no rename detection, so a move arrives as a delete plus a
/// create. rote does not try to turn that back into a rename — the user still
/// types the new file and removes the old one — but saying *where* the content
/// came from turns two mysterious hunks into one obvious operation.
const RENAME_SIMILARITY: f64 = 0.8;

fn annotate_possible_renames(hunks: &mut [Hunk]) {
    let created: Vec<(usize, Vec<String>)> = hunks
        .iter()
        .enumerate()
        .filter(|(_, h)| h.op == Op::CreateFile && !h.new_lines.is_empty())
        .map(|(i, h)| (i, h.new_lines.clone()))
        .collect();
    let deleted: Vec<(String, Vec<String>)> = hunks
        .iter()
        .filter(|h| h.op == Op::DeleteFile && !h.old_lines.is_empty())
        .map(|h| (h.file.clone(), h.old_lines.clone()))
        .collect();

    for (idx, new_lines) in created {
        let best = deleted
            .iter()
            .map(|(file, old)| (file, line_similarity(&new_lines, old)))
            .filter(|(_, score)| *score >= RENAME_SIMILARITY)
            .max_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
        if let Some((from, _)) = best {
            hunks[idx].note = Some(format!("possible rename from {from}"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    fn parse_and_build(diff: &str, max: usize) -> Vec<Hunk> {
        let parsed = diffparse::parse(diff.as_bytes()).unwrap();
        hunks_for_file("f.rs", &parsed, max)
    }

    #[test]
    fn id_is_stable_across_runs() {
        let a = Hunk::compute_id("f.rs", &lines(&["a"]), &lines(&["b"]), &[], &[]);
        let b = Hunk::compute_id("f.rs", &lines(&["a"]), &lines(&["b"]), &[], &[]);
        assert_eq!(a, b);
        assert!(a.starts_with("h-"));
        // 8 bytes of digest: "h-" plus 16 hex.
        assert_eq!(a.len(), 18);
    }

    #[test]
    fn the_key_survives_the_context_churn_that_changes_the_id() {
        // The user types a line three above this hunk. Its `context_before`
        // changes, so its id changes and reconcile treats it as a new hunk —
        // but it is the same change, and anything cached against it must hold.
        let old = lines(&["    tags = Manager()"]);
        let new = lines(&["    tags = TagManager()"]);

        let id_before = Hunk::compute_id("m.py", &old, &new, &lines(&["class Post:"]), &[]);
        let id_after = Hunk::compute_id("m.py", &old, &new, &lines(&["class Post(Base):"]), &[]);
        assert_ne!(id_before, id_after, "context is part of the id");

        let key_before = Hunk::compute_key("m.py", &old, &new);
        let key_after = Hunk::compute_key("m.py", &old, &new);
        assert_eq!(key_before, key_after, "context is not part of the key");
        assert!(key_before.starts_with("k-"));
        assert_eq!(key_before.len(), 18);

        // The key still separates genuinely different changes.
        assert_ne!(
            key_before,
            Hunk::compute_key("m.py", &old, &lines(&["    tags = Other()"]))
        );
        assert_ne!(key_before, Hunk::compute_key("other.py", &old, &new));
    }

    #[test]
    fn identical_hunks_in_one_file_get_distinct_ids() {
        // Two identical edits with identical context hash identically — not by
        // birthday, by construction. Repetitive code makes it ordinary. Without
        // disambiguation the second is unaddressable: `find_mut` returns the
        // first, so typing the second mutates the first one's entry.
        let mut hs = vec![
            Hunk::new(
                "f.rs",
                Op::Replace,
                Lines {
                    context_before: lines(&["ctx"]),
                    old: lines(&["a"]),
                    new: lines(&["b"]),
                    context_after: vec![],
                },
                10,
                None,
            ),
            Hunk::new(
                "f.rs",
                Op::Replace,
                Lines {
                    context_before: lines(&["ctx"]),
                    old: lines(&["a"]),
                    new: lines(&["b"]),
                    context_after: vec![],
                },
                40,
                None,
            ),
        ];
        assert_eq!(hs[0].id, hs[1].id, "identical content collides by design");

        disambiguate_ids(&mut hs);
        assert_ne!(hs[0].id, hs[1].id, "but the queue entries must be distinct");
        assert_eq!(hs[1].id, format!("{}.2", hs[0].id));
        // The key is content identity, so it is deliberately still shared.
        assert_eq!(hs[0].key, hs[1].key);
    }

    #[test]
    fn id_changes_with_any_field() {
        let base = Hunk::compute_id("f.rs", &lines(&["a"]), &lines(&["b"]), &[], &[]);
        assert_ne!(
            base,
            Hunk::compute_id("g.rs", &lines(&["a"]), &lines(&["b"]), &[], &[])
        );
        assert_ne!(
            base,
            Hunk::compute_id("f.rs", &lines(&["z"]), &lines(&["b"]), &[], &[])
        );
        assert_ne!(
            base,
            Hunk::compute_id("f.rs", &lines(&["a"]), &lines(&["z"]), &[], &[])
        );
        assert_ne!(
            base,
            Hunk::compute_id("f.rs", &lines(&["a"]), &lines(&["b"]), &lines(&["c"]), &[])
        );
    }

    #[test]
    fn id_separates_fields_unambiguously() {
        // ["a","b"] must not collide with ["a\nb"] or with a shifted boundary.
        let two = Hunk::compute_id("f", &lines(&["a", "b"]), &[], &[], &[]);
        let one = Hunk::compute_id("f", &lines(&["a\nb"]), &[], &[], &[]);
        let shifted = Hunk::compute_id("f", &lines(&["a"]), &lines(&["b"]), &[], &[]);
        assert_ne!(two, one);
        assert_ne!(two, shifted);
    }

    #[test]
    fn serde_round_trips_with_lowercase_enums() {
        let h = Hunk::new(
            "src/models.py",
            Op::Replace,
            Lines {
                context_before: lines(&["class Post:"]),
                old: lines(&["    body = TextField()"]),
                new: lines(&["    body = TextField()", "    tags = Manager()"]),
                context_after: lines(&["", "    def __str__(self):"]),
            },
            42,
            None,
        );
        let json = serde_json::to_string(&h).unwrap();
        assert!(json.contains("\"status\":\"pending\""), "{json}");
        assert!(json.contains("\"op\":\"replace\""), "{json}");
        let back: Hunk = serde_json::from_str(&json).unwrap();
        assert_eq!(h, back);
    }

    #[test]
    fn whole_file_ops_serialize_snake_case() {
        assert_eq!(
            serde_json::to_string(&Op::CreateFile).unwrap(),
            "\"create_file\""
        );
        assert_eq!(
            serde_json::to_string(&Op::DeleteFile).unwrap(),
            "\"delete_file\""
        );
    }

    #[test]
    fn op_is_derived_from_the_line_arrays() {
        assert_eq!(derive_op(&[], &lines(&["a"])), Op::Insert);
        assert_eq!(derive_op(&lines(&["a"]), &[]), Op::Delete);
        assert_eq!(derive_op(&lines(&["a"]), &lines(&["b"])), Op::Replace);
    }

    #[test]
    fn separate_edits_in_one_raw_hunk_become_separate_hunks() {
        // Two edits three context lines apart: git emits one hunk, we emit two.
        let diff = "--- a/f.rs\n+++ b/f.rs\n@@ -1,9 +1,9 @@\n a\n-b\n+B\n c\n d\n e\n-f\n+F\n g\n";
        let hs = parse_and_build(diff, 20);
        assert_eq!(hs.len(), 2, "one hunk per edit, not per @@ block");
        assert_eq!(hs[0].old_lines, lines(&["b"]));
        assert_eq!(hs[0].new_lines, lines(&["B"]));
        assert_eq!(hs[1].old_lines, lines(&["f"]));
        assert_eq!(hs[1].new_lines, lines(&["F"]));
        // Anchors are real-file line numbers.
        assert_eq!(hs[0].anchor_hint, 2);
        assert_eq!(hs[1].anchor_hint, 6);
    }

    #[test]
    fn context_is_captured_on_both_sides() {
        let diff =
            "--- a/f.rs\n+++ b/f.rs\n@@ -1,5 +1,5 @@\n one\n two\n-three\n+THREE\n four\n five\n";
        let hs = parse_and_build(diff, 20);
        assert_eq!(hs[0].context_before, lines(&["one", "two"]));
        assert_eq!(hs[0].context_after, lines(&["four", "five"]));
    }

    #[test]
    fn hunk_of_exactly_max_does_not_split() {
        let body: String = (0..20).map(|i| format!("+line{i}\n")).collect();
        let diff = format!("--- a/f.rs\n+++ b/f.rs\n@@ -1,0 +1,20 @@\n{body}");
        let hs = parse_and_build(&diff, 20);
        assert_eq!(hs.len(), 1);
        assert_eq!(hs[0].new_lines.len(), 20);
    }

    #[test]
    fn hunk_of_max_plus_one_splits() {
        let body: String = (0..21).map(|i| format!("+line{i}\n")).collect();
        let diff = format!("--- a/f.rs\n+++ b/f.rs\n@@ -1,0 +1,21 @@\n{body}");
        let hs = parse_and_build(&diff, 20);
        assert_eq!(hs.len(), 2);
        assert_eq!(hs[0].new_lines.len() + hs[1].new_lines.len(), 21);
        // No line is lost or duplicated across the split.
        let mut all: Vec<String> = hs[0].new_lines.clone();
        all.extend(hs[1].new_lines.clone());
        assert_eq!(all, (0..21).map(|i| format!("line{i}")).collect::<Vec<_>>());
    }

    #[test]
    fn a_pure_deletion_over_max_splits() {
        // The case that counting only `new_lines` would miss entirely.
        let body: String = (0..45).map(|i| format!("-gone{i}\n")).collect();
        let diff = format!("--- a/f.rs\n+++ b/f.rs\n@@ -1,45 +1,0 @@\n{body}");
        let hs = parse_and_build(&diff, 20);
        assert!(
            hs.len() >= 3,
            "45 deleted lines must split, got {}",
            hs.len()
        );
        assert!(hs.iter().all(|h| h.new_lines.is_empty()));
        let total: usize = hs.iter().map(|h| h.old_lines.len()).sum();
        assert_eq!(total, 45);
        assert!(hs.iter().all(|h| h.old_lines.len() <= 20));
    }

    #[test]
    fn giant_block_with_no_blank_lines_hard_cuts() {
        let body: String = (0..50).map(|i| format!("+x{i}\n")).collect();
        let diff = format!("--- a/f.rs\n+++ b/f.rs\n@@ -1,0 +1,50 @@\n{body}");
        let hs = parse_and_build(&diff, 20);
        assert_eq!(hs.len(), 3);
        assert_eq!(hs[0].new_lines.len(), 20);
        assert_eq!(hs[1].new_lines.len(), 20);
        assert_eq!(hs[2].new_lines.len(), 10);
    }

    #[test]
    fn split_prefers_blank_lines() {
        let mut body = String::new();
        for i in 0..8 {
            body.push_str(&format!("+a{i}\n"));
        }
        body.push_str("+\n"); // blank line at index 8
        for i in 0..12 {
            body.push_str(&format!("+b{i}\n"));
        }
        let diff = format!("--- a/f.rs\n+++ b/f.rs\n@@ -1,0 +1,21 @@\n{body}");
        let hs = parse_and_build(&diff, 20);
        assert_eq!(hs.len(), 2);
        // Cut just after the blank rather than at the 20-line limit.
        assert_eq!(hs[0].new_lines.len(), 9);
        assert_eq!(hs[0].new_lines.last().unwrap(), "");
    }

    #[test]
    fn split_falls_back_to_indentation_boundary() {
        // No blank lines; a dedented line at index 15 opens the next block.
        let mut v: Vec<String> = (0..15).map(|i| format!("    inner{i}")).collect();
        v.push("fn next() {".into());
        v.extend((0..8).map(|i| format!("    tail{i}")));
        let body: String = v.iter().map(|l| format!("+{l}\n")).collect();
        let diff = format!("--- a/f.rs\n+++ b/f.rs\n@@ -1,0 +1,24 @@\n{body}");
        let hs = parse_and_build(&diff, 20);
        assert_eq!(hs[0].new_lines.len(), 15, "cut before the dedented line");
        assert_eq!(hs[1].new_lines[0], "fn next() {");
    }

    #[test]
    fn sub_hunks_get_synthesized_context() {
        let body: String = (0..50).map(|i| format!("+x{i}\n")).collect();
        let diff = format!("--- a/f.rs\n+++ b/f.rs\n@@ -1,0 +1,50 @@\n{body}");
        let hs = parse_and_build(&diff, 20);
        // Later sub-hunks anchor on the tail of the previous chunk.
        assert_eq!(hs[1].context_before, lines(&["x18", "x19"]));
        assert_eq!(hs[0].context_after, lines(&["x20", "x21"]));
    }

    #[test]
    fn split_replace_deletes_once_then_inserts() {
        let mut body = String::new();
        body.push_str("-old0\n-old1\n");
        for i in 0..30 {
            body.push_str(&format!("+new{i}\n"));
        }
        let diff = format!("--- a/f.rs\n+++ b/f.rs\n@@ -1,2 +1,30 @@\n{body}");
        let hs = parse_and_build(&diff, 20);
        assert_eq!(hs.len(), 2);
        assert_eq!(hs[0].op, Op::Replace);
        assert_eq!(hs[0].old_lines, lines(&["old0", "old1"]));
        // The removal happens once; the rest is pure typing.
        assert_eq!(hs[1].op, Op::Insert);
        assert!(hs[1].old_lines.is_empty());
    }

    #[test]
    fn new_file_marks_only_the_first_sub_hunk_as_create() {
        let body: String = (0..30).map(|i| format!("+l{i}\n")).collect();
        let diff = format!("--- /dev/null\n+++ b/f.rs\n@@ -0,0 +1,30 @@\n{body}");
        let hs = parse_and_build(&diff, 20);
        assert_eq!(hs.len(), 2);
        assert_eq!(hs[0].op, Op::CreateFile);
        assert_eq!(hs[0].note.as_deref(), Some("file is new"));
        assert_eq!(hs[1].op, Op::Insert, "the file exists by the second hunk");
    }

    #[test]
    fn deleted_file_is_marked() {
        let diff = "--- a/f.rs\n+++ /dev/null\n@@ -1,2 +0,0 @@\n-a\n-b\n";
        let hs = parse_and_build(diff, 20);
        assert_eq!(hs[0].op, Op::DeleteFile);
        assert_eq!(hs[0].note.as_deref(), Some("delete this file"));
    }

    #[test]
    fn find_split_respects_the_limit() {
        let v: Vec<String> = (0..100).map(|i| format!("line{i}")).collect();
        assert_eq!(find_split(&v, 20), 20);
        assert_eq!(find_split(&v[..5], 20), 5);
    }

    #[test]
    fn untypeable_hunks_are_flagged() {
        let h = untypeable_hunk("Cargo.lock", true, true, "generated file");
        assert!(h.is_untypeable());
        assert!(h.new_lines.is_empty() && h.old_lines.is_empty());
    }

    #[test]
    fn similarity_scores_shared_lines() {
        let a = lines(&["one", "two", "three", "four"]);
        assert_eq!(line_similarity(&a, &a), 1.0);
        assert_eq!(
            line_similarity(&a, &lines(&["one", "two", "three", "CHANGED"])),
            0.75
        );
        assert_eq!(line_similarity(&a, &lines(&["w", "x", "y", "z"])), 0.0);
        // A longer target dilutes the score, so a small file inside a big one
        // does not read as a rename.
        assert!(line_similarity(&lines(&["one"]), &a) < 0.3);
    }

    #[test]
    fn a_moved_file_is_noted_as_a_possible_rename() {
        let body = lines(&["fn moved() {", "    work();", "}"]);
        let mut hunks = vec![
            Hunk::new(
                "new/place.rs",
                Op::CreateFile,
                Lines {
                    new: body.clone(),
                    ..Default::default()
                },
                1,
                Some("file is new".into()),
            ),
            Hunk::new(
                "old/place.rs",
                Op::DeleteFile,
                Lines {
                    old: body,
                    ..Default::default()
                },
                1,
                Some("delete this file".into()),
            ),
        ];
        annotate_possible_renames(&mut hunks);
        assert_eq!(
            hunks[0].note.as_deref(),
            Some("possible rename from old/place.rs")
        );
    }

    #[test]
    fn an_unrelated_new_file_is_not_called_a_rename() {
        let mut hunks = vec![
            Hunk::new(
                "new.rs",
                Op::CreateFile,
                Lines {
                    new: lines(&["totally", "different", "content"]),
                    ..Default::default()
                },
                1,
                Some("file is new".into()),
            ),
            Hunk::new(
                "gone.rs",
                Op::DeleteFile,
                Lines {
                    old: lines(&["nothing", "in", "common"]),
                    ..Default::default()
                },
                1,
                None,
            ),
        ];
        annotate_possible_renames(&mut hunks);
        assert_eq!(hunks[0].note.as_deref(), Some("file is new"));
    }

    #[test]
    fn binary_sniffing_uses_nul_bytes() {
        assert!(looks_binary(b"abc\0def"));
        assert!(!looks_binary(b"plain text\nwith newlines\n"));
    }

    // ------------------------------------------------------------ how it came

    #[test]
    fn an_unknown_input_is_omitted_from_the_wire() {
        // Every optional hunk field is absent when unset, and this one was the
        // exception — it serialized `"input":"unknown"` into every manifest and
        // every snapshot, contradicting the schema in DESIGN §4.
        let h = Hunk::new(
            "a.rs",
            Op::Insert,
            Lines {
                new: lines(&["work();"]),
                ..Default::default()
            },
            1,
            None,
        );
        let json = serde_json::to_string(&h).unwrap();
        assert!(!json.contains("input"), "{json}");

        let mut typed = h.clone();
        typed.input = Input::Typed;
        assert!(serde_json::to_string(&typed)
            .unwrap()
            .contains("\"input\":\"typed\""));
    }

    #[test]
    fn an_inference_only_ever_fills_in_an_unknown() {
        let mut i = Input::Unknown;
        assert!(i.fill(Input::Typed));
        assert_eq!(i, Input::Typed);
        // Already decided: a second inference changes nothing and says so, so
        // it cannot move the generation.
        assert!(!i.fill(Input::Typed));

        // And the case the guard exists for: you paste a hunk, the plugin says
        // so, then you fix a character in it. The engine must not erase that.
        let mut reported = Input::Pasted;
        assert!(!reported.fill(Input::Typed));
        assert_eq!(reported, Input::Pasted);
    }

    #[test]
    fn a_report_always_lands_so_a_wrong_one_can_be_corrected() {
        let mut i = Input::Pasted;
        assert!(i.set(Input::Typed), "a heuristic misfire is correctable");
        assert_eq!(i, Input::Typed);
        assert!(!i.set(Input::Typed), "but repeating it writes nothing");
    }
}
