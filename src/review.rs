//! The `done` pipeline: checks, the session diff, and the reviewer. DESIGN.md §8.

use crate::config::Config;
use crate::git;
use crate::hunks::Status;
use crate::model;
use crate::paths::ProjectPaths;
use crate::session::Manifest;
use anyhow::{Context, Result};
use std::path::Path;
use std::process::Command;
use std::time::Duration;

/// The reviewer's instructions. A constant so the payload is reproducible.
pub const REVIEW_PROMPT: &str = "You are reviewing code changes that were typed in manually. \
Review the session diff for bugs, inconsistencies, and incomplete changes. Pay special attention \
to the manual divergences: distinguish deliberate refactors from likely transcription typos \
(single-character differences, transposed identifiers, wrong operators) and flag the typos \
explicitly with file and line. Note anything a skipped hunk leaves broken. Be terse. Do not \
explain concepts. Output findings as a flat list ordered by severity; if nothing is wrong, say \
so in one line.";

/// How long the reviewer gets before rote gives up and moves on.
pub const REVIEW_TIMEOUT: Duration = Duration::from_secs(300);

/// Run the `[checks]` commands in the REAL tree, streaming output.
///
/// Stops at the first failure: a red test suite makes everything after it noise.
pub fn run_checks(project: &ProjectPaths, cfg: &Config) -> Result<()> {
    for cmd in &cfg.check_commands {
        println!("check: {cmd}");
        let status = Command::new("sh")
            .arg("-c")
            .arg(cmd)
            .current_dir(&project.repo_root)
            .status()
            .with_context(|| format!("cannot run check `{cmd}`"))?;
        if !status.success() {
            anyhow::bail!(
                "check failed: {cmd}\n\
                 Fix it and re-run `rote done`, or skip the checks with --no-checks."
            );
        }
    }
    Ok(())
}

/// The diff from the session's starting point to the real tree as it is now.
///
/// Materializes the baseline (`baseline.head` + `baseline.patch`) into a temp
/// directory and diffs the real tree against it, per file. Correctness over
/// elegance — this runs once per session.
pub fn session_diff(project: &ProjectPaths, manifest: &Manifest) -> Result<Vec<u8>> {
    let scratch = tempfile::tempdir().context("cannot create a temp dir for the baseline")?;
    let base = scratch.path().join("baseline");
    std::fs::create_dir_all(&base)?;

    // A worktree of the shadow clone at the session's starting commit, plus the
    // uncommitted changes that were in flight when the session began.
    git::materialize_worktree(&project.shadow_dir, &manifest.baseline.head, &base)?;
    let patch = std::fs::read(project.baseline_patch()).unwrap_or_default();
    if !patch.is_empty() {
        // A baseline patch that no longer applies is not fatal: the diff is
        // advisory input to a reviewer, not a correctness mechanism.
        if let Err(e) = git::apply_patch(&base, &patch) {
            eprintln!("warning: could not restore the session's starting state exactly: {e:#}");
        }
    }

    let mut candidates = git::status_paths(&project.repo_root)?;
    candidates.extend(git::status_paths(&project.shadow_dir)?);
    for h in &manifest.hunks {
        candidates.push(std::path::PathBuf::from(&h.file));
    }
    candidates.sort();
    candidates.dedup();

    let devnull = std::path::PathBuf::from("/dev/null");
    let mut out = Vec::new();
    for rel in candidates {
        let before = base.join(&rel);
        let now = project.repo_root.join(&rel);
        let before_bytes = std::fs::read(&before).ok();
        let now_bytes = std::fs::read(&now).ok();
        if before_bytes == now_bytes {
            continue;
        }
        let a = if before_bytes.is_some() {
            &before
        } else {
            &devnull
        };
        let b = if now_bytes.is_some() { &now } else { &devnull };
        let raw = git::diff_no_index(a, b, crate::hunks::CONTEXT_LINES)?;
        out.extend(git::relativize_no_index_diff(&raw, &rel.to_string_lossy()));
    }

    // Drop the worktree registration so the shadow's git dir stays tidy.
    let _ = git::prune_worktrees(&project.shadow_dir);
    Ok(out)
}

/// Assemble the reviewer payload exactly as DESIGN.md §8 specifies.
pub fn build_payload(manifest: &Manifest, session_diff: &[u8]) -> String {
    let mut out = String::new();

    out.push_str("== TASK ==\n");
    out.push_str(if manifest.task.is_empty() {
        "(unspecified)"
    } else {
        &manifest.task
    });
    out.push_str("\n\n== SESSION DIFF (baseline → current working tree) ==\n");
    out.push_str(&String::from_utf8_lossy(session_diff));

    out.push_str("\n== MANUAL DIVERGENCES (proposal vs. what was typed) ==\n");
    let mut any = false;
    for h in manifest
        .hunks
        .iter()
        .filter(|h| h.status == Status::Diverged)
    {
        let Some(d) = &h.divergence else { continue };
        any = true;
        out.push_str(&format!("file {} hunk {}:\n", h.file, h.id));
        out.push_str("  proposal:\n");
        for l in &d.proposed {
            out.push_str(&format!("    {l}\n"));
        }
        out.push_str("  typed:\n");
        for l in &d.actual {
            out.push_str(&format!("    {l}\n"));
        }
    }
    if !any {
        out.push_str("(none)\n");
    }

    out.push_str("\n== SKIPPED HUNKS ==\n");
    let mut any = false;
    for h in manifest
        .hunks
        .iter()
        .filter(|h| h.status == Status::Skipped)
    {
        any = true;
        out.push_str(&format!("{}:{}\n", h.file, h.anchor_hint));
        for l in &h.new_lines {
            out.push_str(&format!("    {l}\n"));
        }
        if h.new_lines.is_empty() {
            if let Some(note) = &h.note {
                out.push_str(&format!("    ({note})\n"));
            }
        }
    }
    if !any {
        out.push_str("(none)\n");
    }

    out
}

/// Run the reviewer. Advisory: any failure warns and returns `None`.
pub fn run_reviewer(cfg: &Config, payload: &str) -> Option<String> {
    match try_reviewer(cfg, payload) {
        Ok(text) => Some(text),
        Err(e) => {
            eprintln!(
                "warning: the reviewer did not run ({e:#}). Continuing — review is advisory."
            );
            None
        }
    }
}

fn try_reviewer(cfg: &Config, payload: &str) -> Result<String> {
    // The session diff runs to hundreds of kilobytes on an ordinary session, so
    // this call is the one that used to deadlock: see `model::run`.
    let out = model::run(
        &model::Invocation {
            claude_cmd: &cfg.claude_cmd,
            prompt: REVIEW_PROMPT,
            extra_args: &cfg.review_model_args,
            timeout: REVIEW_TIMEOUT,
        },
        payload,
    )?;
    if !out.status.success() {
        anyhow::bail!(
            "exit {}: {}",
            out.status.code().unwrap_or(-1),
            out.complaint()
        );
    }
    Ok(out.stdout)
}

/// Where the archived residue patch for this session will land, for messaging.
pub fn describe_residue(patch: &Path, bytes: usize) -> Option<String> {
    (bytes > 0).then(|| format!("the agent's unabsorbed work: {}", patch.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hunks::{Divergence, Hunk, Op};
    use crate::session::{BaselineRecord, State, MANIFEST_VERSION};

    fn hunk(file: &str, status: Status) -> Hunk {
        Hunk {
            id: "h-abcd1234".into(),
            key: "k-abcd1234".into(),
            file: file.into(),
            op: Op::Replace,
            context_before: vec![],
            old_lines: vec!["old".into()],
            new_lines: vec!["new".into()],
            context_after: vec![],
            anchor_hint: 42,
            status,
            divergence: None,
            note: None,
            pending_divergence: None,
            curator_note: None,
            curator_rank: None,
            input: Default::default(),
        }
    }

    fn manifest(task: &str, hunks: Vec<Hunk>) -> Manifest {
        Manifest {
            version: MANIFEST_VERSION,
            state: State::Transcribing,
            task: task.into(),
            created_at: "2026-01-01T00:00:00Z".into(),
            project_root: "/p".into(),
            shadow_dir: "/s".into(),
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
    fn payload_has_all_four_sections_in_order() {
        let p = build_payload(&manifest("add tagging", vec![]), b"diff --git a/x b/x\n");
        let task = p.find("== TASK ==").unwrap();
        let diff = p.find("== SESSION DIFF").unwrap();
        let div = p.find("== MANUAL DIVERGENCES").unwrap();
        let skip = p.find("== SKIPPED HUNKS ==").unwrap();
        assert!(task < diff && diff < div && div < skip, "{p}");
        assert!(p.contains("add tagging"));
        assert!(p.contains("diff --git a/x b/x"));
    }

    #[test]
    fn an_empty_task_reads_as_unspecified() {
        let p = build_payload(&manifest("", vec![]), b"");
        assert!(p.contains("== TASK ==\n(unspecified)"), "{p}");
    }

    #[test]
    fn empty_sections_say_none() {
        let p = build_payload(&manifest("t", vec![]), b"");
        assert_eq!(p.matches("(none)").count(), 2, "divergences and skips: {p}");
    }

    #[test]
    fn divergences_show_both_versions() {
        let mut h = hunk("src/models.py", Status::Diverged);
        h.divergence = Some(Divergence {
            proposed: vec!["    tags = Manager()".into()],
            actual: vec!["    tags = TaggableManager()".into()],
        });
        let p = build_payload(&manifest("t", vec![h]), b"");
        assert!(p.contains("file src/models.py hunk h-abcd1234:"), "{p}");
        // Payload indents by four; the source line keeps its own indentation.
        assert!(p.contains("  proposal:\n        tags = Manager()"), "{p}");
        assert!(
            p.contains("  typed:\n        tags = TaggableManager()"),
            "{p}"
        );
    }

    #[test]
    fn skipped_hunks_are_listed_with_location_and_lines() {
        let p = build_payload(
            &manifest("t", vec![hunk("src/views.py", Status::Skipped)]),
            b"",
        );
        assert!(p.contains("src/views.py:42"), "{p}");
        assert!(p.contains("    new"), "{p}");
    }

    #[test]
    fn typed_hunks_appear_in_neither_list() {
        let p = build_payload(&manifest("t", vec![hunk("a.rs", Status::Typed)]), b"");
        assert_eq!(p.matches("(none)").count(), 2, "{p}");
        assert!(!p.contains("a.rs"), "{p}");
    }

    #[test]
    fn an_untypeable_skipped_hunk_reports_its_note() {
        let mut h = hunk("logo.png", Status::Skipped);
        h.new_lines.clear();
        h.old_lines.clear();
        h.note = Some("binary — copy it across yourself".into());
        let p = build_payload(&manifest("t", vec![h]), b"");
        assert!(p.contains("(binary — copy it across yourself)"), "{p}");
    }

    #[test]
    fn the_review_prompt_is_a_stable_constant() {
        assert!(REVIEW_PROMPT.contains("typed in manually"));
        assert!(REVIEW_PROMPT.contains("transcription typos"));
        assert!(REVIEW_PROMPT.ends_with("say so in one line."));
    }
}
