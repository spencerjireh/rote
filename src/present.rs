//! HunkPresenter: render, anchor, launch the editor, classify. DESIGN.md §6.
//!
//! Anchoring and classification are pure functions over line slices. They are
//! the two places a subtle bug would silently mis-record what the user typed, so
//! they are kept free of I/O and tested directly.

use crate::hunks::{Hunk, Op};
use anyhow::{Context, Result};
use owo_colors::OwoColorize;
use std::path::Path;

/// How an anchor was located, worst case last.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnchorVia {
    ContextBefore,
    ContextAfter,
    Fuzzy,
    /// Nothing matched; fell back to the stored hint.
    Hint,
    /// The hunk has no context at all (a new file).
    NoContext,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Anchor {
    /// 1-based line where the user starts typing.
    pub line: usize,
    pub via: AnchorVia,
}

impl Anchor {
    /// 0-based index into the line vector.
    pub fn index(&self) -> usize {
        self.line.saturating_sub(1)
    }
}

fn normalize(s: &str) -> &str {
    s.trim()
}

/// Every 0-based index where `needle` appears as a run of lines in `haystack`.
fn find_all(haystack: &[String], needle: &[String], fuzzy: bool) -> Vec<usize> {
    if needle.is_empty() || needle.len() > haystack.len() {
        return Vec::new();
    }
    let matches = |a: &str, b: &str| {
        if fuzzy {
            normalize(a) == normalize(b)
        } else {
            a == b
        }
    };
    (0..=haystack.len() - needle.len())
        .filter(|&i| {
            haystack[i..i + needle.len()]
                .iter()
                .zip(needle)
                .all(|(a, b)| matches(a, b))
        })
        .collect()
}

/// Pick the candidate nearest the advisory hint (DESIGN.md §6: ambiguity is
/// resolved by proximity, since files drift as the user types above the hunk).
fn nearest(candidates: &[usize], hint: usize) -> Option<usize> {
    candidates
        .iter()
        .copied()
        .min_by_key(|&c| (c as isize - hint as isize).unsigned_abs())
}

/// Locate where this hunk's new lines belong in the file as it now stands.
///
/// The stored `anchor_hint` is advisory only: the user may have typed earlier
/// hunks above this one and shifted everything down.
pub fn find_anchor(file_lines: &[String], hunk: &Hunk) -> Anchor {
    let hint_idx = hunk.anchor_hint.saturating_sub(1);

    if !hunk.context_before.is_empty() {
        let hits = find_all(file_lines, &hunk.context_before, false);
        if let Some(i) = nearest(&hits, hint_idx.saturating_sub(hunk.context_before.len())) {
            return Anchor {
                line: i + hunk.context_before.len() + 1,
                via: AnchorVia::ContextBefore,
            };
        }
    }

    if !hunk.context_after.is_empty() {
        let hits = find_all(file_lines, &hunk.context_after, false);
        if let Some(i) = nearest(&hits, hint_idx) {
            return Anchor {
                line: i + 1,
                via: AnchorVia::ContextAfter,
            };
        }
    }

    // Fuzzy: same search ignoring leading and trailing whitespace.
    if !hunk.context_before.is_empty() {
        let hits = find_all(file_lines, &hunk.context_before, true);
        if let Some(i) = nearest(&hits, hint_idx.saturating_sub(hunk.context_before.len())) {
            return Anchor {
                line: i + hunk.context_before.len() + 1,
                via: AnchorVia::Fuzzy,
            };
        }
    }
    if !hunk.context_after.is_empty() {
        let hits = find_all(file_lines, &hunk.context_after, true);
        if let Some(i) = nearest(&hits, hint_idx) {
            return Anchor {
                line: i + 1,
                via: AnchorVia::Fuzzy,
            };
        }
    }

    let via = if hunk.context_before.is_empty() && hunk.context_after.is_empty() {
        AnchorVia::NoContext
    } else {
        AnchorVia::Hint
    };
    Anchor {
        line: hunk.anchor_hint.max(1),
        via,
    }
}

/// What the editor session produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Classification {
    /// Matches the proposal.
    Typed,
    /// Still the old content — nothing was done.
    Untouched,
    /// Something else. Both versions are kept.
    Diverged { actual: Vec<String> },
}

fn lines_eq(a: &[String], b: &[String], strict_whitespace: bool) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).all(|(x, y)| {
        if strict_whitespace {
            x == y
        } else {
            // Trailing whitespace only — leading whitespace is indentation and
            // always significant. This is also what absorbs a stray `\r`.
            x.trim_end() == y.trim_end()
        }
    })
}

/// Read the region this hunk owns, given where it starts.
fn region<'a>(
    file_lines: &'a [String],
    start: usize,
    hunk: &Hunk,
    expected_len: usize,
) -> &'a [String] {
    if start >= file_lines.len() {
        return &[];
    }
    if !hunk.context_after.is_empty() {
        let hits = find_all(&file_lines[start..], &hunk.context_after, false);
        if let Some(offset) = hits.first() {
            return &file_lines[start..start + offset];
        }
    }
    let end = (start + expected_len).min(file_lines.len());
    &file_lines[start..end]
}

/// Decide what the user did, by comparing the file before and after. DESIGN.md §6.
///
/// "Untouched" is decided by comparing against `before` rather than by checking
/// whether the region still equals `old_lines`. That inference looks equivalent
/// and is not: for a pure insert `old_lines` is empty, so on a hunk with no
/// trailing context the check compares an empty region against an empty vector
/// and is vacuously true — reporting every botched transcription of a trailing
/// insert as "untouched" and silently skipping the divergence prompt.
pub fn classify(
    before: &[String],
    after: &[String],
    hunk: &Hunk,
    strict_whitespace: bool,
) -> Classification {
    // Did they type the proposal? Checked first: a correct transcription is
    // `typed` even in the odd case where it leaves the file byte-identical.
    let anchor = find_anchor(after, hunk);
    let as_new = region(after, anchor.index(), hunk, hunk.new_lines.len());
    if lines_eq(as_new, &hunk.new_lines, strict_whitespace) {
        return Classification::Typed;
    }

    // Nothing happened at all.
    if before == after {
        return Classification::Untouched;
    }

    Classification::Diverged {
        actual: as_new.to_vec(),
    }
}

/// Classify a whole-file hunk that cannot be typed, by comparing bytes.
///
/// The gate for binaries and generated files: no editor, no prompt, no honor
/// system — either the file matches the shadow or the hunk stays pending.
pub fn classify_by_bytes(real: &Path, shadow: &Path) -> Classification {
    let a = std::fs::read(real).ok();
    let b = std::fs::read(shadow).ok();
    if a == b {
        Classification::Typed
    } else {
        Classification::Untouched
    }
}

/// Render one hunk for the terminal. No prose, no explanation.
pub fn render(hunk: &Hunk, position: usize, total: usize, anchor: &Anchor, color: bool) -> String {
    let mut out = String::new();
    let op = match hunk.op {
        Op::Insert => "insert",
        Op::Replace => "replace",
        Op::Delete => "delete",
        Op::CreateFile => "create file",
        Op::DeleteFile => "delete file",
    };

    let header = format!(
        "── hunk {position}/{total} ── {}:{} ── {op} ",
        hunk.file, anchor.line
    );
    let rule: String = "─".repeat(60usize.saturating_sub(header.chars().count()).max(3));
    out.push_str(&format!("{header}{rule}\n"));

    if let Some(note) = &hunk.note {
        out.push_str(&paint(&format!("   {note}\n"), Paint::Dim, color));
    }

    if hunk.is_untypeable() {
        out.push_str(&paint(
            "   (not typed line by line — rote checks the file matches)\n",
            Paint::Dim,
            color,
        ));
    } else {
        for l in &hunk.context_before {
            out.push_str(&paint(&format!("   {l}\n"), Paint::Dim, color));
        }
        for l in &hunk.old_lines {
            out.push_str(&paint(&format!(" - {l}\n"), Paint::Red, color));
        }
        for l in &hunk.new_lines {
            out.push_str(&paint(&format!(" + {l}\n"), Paint::Green, color));
        }
        for l in &hunk.context_after {
            out.push_str(&paint(&format!("   {l}\n"), Paint::Dim, color));
        }
    }

    out.push_str(&format!("{}\n", "─".repeat(60)));
    if anchor.via == AnchorVia::Hint {
        out.push_str(&paint(
            "   context not found — anchoring on the last known line\n",
            Paint::Yellow,
            color,
        ));
    }
    out
}

/// A mini-diff of the proposal against what the user actually typed.
pub fn render_divergence(proposed: &[String], actual: &[String], color: bool) -> String {
    let mut out = String::from("your version differs from the proposal:\n");
    for l in proposed {
        out.push_str(&paint(&format!("  proposal │ {l}\n"), Paint::Green, color));
    }
    for l in actual {
        out.push_str(&paint(&format!("  yours    │ {l}\n"), Paint::Yellow, color));
    }
    out
}

enum Paint {
    Dim,
    Red,
    Green,
    Yellow,
}

fn paint(s: &str, how: Paint, color: bool) -> String {
    if !color {
        return s.to_string();
    }
    match how {
        Paint::Dim => s.dimmed().to_string(),
        Paint::Red => s.red().to_string(),
        Paint::Green => s.green().to_string(),
        Paint::Yellow => s.yellow().to_string(),
    }
}

/// Which editor to run: `$ROTE_EDITOR`, else config, else the built-in default.
///
/// `$EDITOR` is deliberately not consulted — transcription needs the `+LINE`
/// convention, and silently inheriting a pager or `ed` is a worse failure than
/// an explicit chain.
pub fn editor_command(cfg_editor: &str) -> Vec<String> {
    let spec = std::env::var("ROTE_EDITOR")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| cfg_editor.to_string());
    spec.split_whitespace().map(String::from).collect()
}

/// Open the editor on a hunk and wait. Returns false if it exited nonzero,
/// which is treated as "untouched" (DESIGN.md §9.6).
pub fn launch_editor(
    editor: &[String],
    file: &Path,
    line: usize,
    is_new_file: bool,
) -> Result<bool> {
    let (program, base_args) = editor.split_first().context("editor command is empty")?;
    let mut cmd = std::process::Command::new(program);
    cmd.args(base_args);
    // A create_file hunk must not have the file conjured up by the invocation;
    // the user creates it as they type.
    if !is_new_file {
        cmd.arg(format!("+{line}"));
    }
    cmd.arg(file);

    let status = cmd.status().with_context(|| {
        format!(
            "cannot launch editor `{}`.\nSet ROTE_EDITOR or `editor` in ~/.config/rote/config.toml.",
            editor.join(" ")
        )
    })?;
    Ok(status.success())
}

/// Split file contents into lines, dropping a single trailing newline's empty
/// tail so a file and its line vector round-trip.
pub fn split_lines(text: &str) -> Vec<String> {
    let mut v: Vec<String> = text.split('\n').map(String::from).collect();
    if v.last().map(|s| s.is_empty()).unwrap_or(false) {
        v.pop();
    }
    v
}

pub fn read_lines(path: &Path) -> Result<Vec<String>> {
    match std::fs::read_to_string(path) {
        Ok(t) => Ok(split_lines(&t)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(e).with_context(|| format!("cannot read {}", path.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hunks::Status;

    fn lines(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    fn hunk(before: &[&str], old: &[&str], new: &[&str], after: &[&str], hint: usize) -> Hunk {
        Hunk {
            id: "h-test".into(),
            key: "k-test".into(),
            file: "f.rs".into(),
            op: Op::Replace,
            context_before: lines(before),
            old_lines: lines(old),
            new_lines: lines(new),
            context_after: lines(after),
            anchor_hint: hint,
            status: Status::Pending,
            divergence: None,
            note: None,
            pending_divergence: None,
            curator_note: None,
            curator_rank: None,
            input: Default::default(),
        }
    }

    #[test]
    fn anchors_on_context_before() {
        let file = lines(&["fn a() {", "    one();", "}"]);
        let h = hunk(&["fn a() {"], &["    one();"], &["    two();"], &["}"], 2);
        let a = find_anchor(&file, &h);
        assert_eq!(a.line, 2);
        assert_eq!(a.via, AnchorVia::ContextBefore);
    }

    #[test]
    fn anchor_survives_lines_inserted_above() {
        // The user typed an earlier hunk, pushing this one down by three lines.
        let file = lines(&[
            "// added",
            "// added",
            "// added",
            "fn a() {",
            "    one();",
            "}",
        ]);
        let h = hunk(&["fn a() {"], &["    one();"], &["    two();"], &["}"], 2);
        let a = find_anchor(&file, &h);
        assert_eq!(a.line, 5, "found by context, not by the stale hint");
        assert_eq!(a.via, AnchorVia::ContextBefore);
    }

    #[test]
    fn ambiguous_context_resolves_to_the_nearest_match() {
        // The same context appears twice; the hint decides which one.
        let file = lines(&[
            "    guard();",
            "    body();",
            "    tail();",
            "    pad();",
            "    pad();",
            "    guard();",
            "    body();",
            "    tail();",
        ]);
        let h = hunk(
            &["    guard();"],
            &["    body();"],
            &["    NEW();"],
            &["    tail();"],
            7,
        );
        let a = find_anchor(&file, &h);
        assert_eq!(a.line, 7, "the second occurrence is nearer the hint");

        let h_early = hunk(
            &["    guard();"],
            &["    body();"],
            &["    NEW();"],
            &["    tail();"],
            2,
        );
        assert_eq!(find_anchor(&file, &h_early).line, 2);
    }

    #[test]
    fn falls_back_to_context_after_when_before_is_gone() {
        let file = lines(&["totally", "different", "}"]);
        let h = hunk(&["fn a() {"], &["    one();"], &["    two();"], &["}"], 2);
        let a = find_anchor(&file, &h);
        assert_eq!(a.via, AnchorVia::ContextAfter);
        assert_eq!(a.line, 3);
    }

    #[test]
    fn falls_back_to_fuzzy_when_indentation_changed() {
        let file = lines(&["fn a() {", "        one();", "  }"]);
        let h = hunk(&["    fn a() {"], &["one();"], &["two();"], &["    }"], 2);
        let a = find_anchor(&file, &h);
        assert_eq!(a.via, AnchorVia::Fuzzy);
    }

    #[test]
    fn falls_back_to_the_hint_when_nothing_matches() {
        let file = lines(&["nothing", "matches", "here"]);
        let h = hunk(&["fn a() {"], &["    one();"], &["    two();"], &["}"], 2);
        let a = find_anchor(&file, &h);
        assert_eq!(a.via, AnchorVia::Hint);
        assert_eq!(a.line, 2);
    }

    #[test]
    fn classifies_an_exact_transcription_as_typed() {
        let h = hunk(&["fn a() {"], &["    one();"], &["    two();"], &["}"], 2);
        let before = lines(&["fn a() {", "    one();", "}"]);
        let after = lines(&["fn a() {", "    two();", "}"]);
        assert_eq!(classify(&before, &after, &h, false), Classification::Typed);
    }

    #[test]
    fn classifies_an_unopened_file_as_untouched() {
        let h = hunk(&["fn a() {"], &["    one();"], &["    two();"], &["}"], 2);
        let unchanged = lines(&["fn a() {", "    one();", "}"]);
        assert_eq!(
            classify(&unchanged, &unchanged, &h, false),
            Classification::Untouched
        );
    }

    #[test]
    fn classifies_a_variant_as_divergence_and_keeps_it() {
        let h = hunk(&["fn a() {"], &["    one();"], &["    two();"], &["}"], 2);
        let before = lines(&["fn a() {", "    one();", "}"]);
        let variant = lines(&["fn a() {", "    two_but_mine();", "}"]);
        match classify(&before, &variant, &h, false) {
            Classification::Diverged { actual } => {
                assert_eq!(actual, lines(&["    two_but_mine();"]))
            }
            other => panic!("expected divergence, got {other:?}"),
        }
    }

    #[test]
    fn trailing_whitespace_is_tolerated_by_default() {
        let h = hunk(&["fn a() {"], &["    one();"], &["    two();"], &["}"], 2);
        let before = lines(&["fn a() {", "    one();", "}"]);
        let with_trailing = lines(&["fn a() {", "    two();   ", "}"]);
        assert_eq!(
            classify(&before, &with_trailing, &h, false),
            Classification::Typed
        );
        // ...but not when the user asks for strictness.
        assert!(matches!(
            classify(&before, &with_trailing, &h, true),
            Classification::Diverged { .. }
        ));
    }

    #[test]
    fn a_stray_carriage_return_is_absorbed() {
        // DESIGN.md §9.11: CRLF is covered by the same tolerance.
        let h = hunk(&["fn a() {"], &["    one();"], &["    two();"], &["}"], 2);
        let before = lines(&["fn a() {", "    one();", "}"]);
        let crlf = lines(&["fn a() {", "    two();\r", "}"]);
        assert_eq!(classify(&before, &crlf, &h, false), Classification::Typed);
    }

    #[test]
    fn leading_whitespace_is_never_tolerated() {
        // Indentation is meaning, not formatting.
        let h = hunk(&["fn a() {"], &["    one();"], &["    two();"], &["}"], 2);
        let before = lines(&["fn a() {", "    one();", "}"]);
        let wrong_indent = lines(&["fn a() {", "two();", "}"]);
        assert!(matches!(
            classify(&before, &wrong_indent, &h, false),
            Classification::Diverged { .. }
        ));
    }

    #[test]
    fn a_multi_line_insertion_classifies_as_typed() {
        let h = hunk(&["start"], &[], &["one", "two", "three"], &["end"], 2);
        let before = lines(&["start", "end"]);
        let after = lines(&["start", "one", "two", "three", "end"]);
        assert_eq!(classify(&before, &after, &h, false), Classification::Typed);
    }

    #[test]
    fn a_deletion_is_typed_once_the_lines_are_gone() {
        let h = hunk(&["keep"], &["remove me", "and me"], &[], &["also keep"], 2);
        let before = lines(&["keep", "remove me", "and me", "also keep"]);
        let after = lines(&["keep", "also keep"]);
        assert_eq!(classify(&before, &after, &h, false), Classification::Typed);
        // Before deleting, it reads as untouched.
        assert_eq!(
            classify(&before, &before, &h, false),
            Classification::Untouched
        );
    }

    #[test]
    fn a_new_file_classifies_from_empty() {
        let mut h = hunk(&[], &[], &["line one", "line two"], &[], 1);
        h.op = Op::CreateFile;
        assert_eq!(classify(&[], &[], &h, false), Classification::Untouched);
        let created = lines(&["line one", "line two"]);
        assert_eq!(classify(&[], &created, &h, false), Classification::Typed);
    }

    #[test]
    fn a_botched_trailing_insert_is_a_divergence_not_untouched() {
        // No context_after (the insert lands at end of file) and no old_lines.
        // The old "does the region still equal old_lines" check compared two
        // empty vectors here and always said untouched, so a mistyped insert
        // skipped the divergence prompt entirely.
        let h = hunk(
            &["def add(a, b):", "    return a + b"],
            &[],
            &["", "def sub(a, b):", "    return a - b"],
            &[],
            3,
        );
        let before = lines(&["def add(a, b):", "    return a + b"]);
        let mistyped = lines(&[
            "def add(a, b):",
            "    return a + b",
            "",
            "def sub(a, b):",
            "    return a + b",
        ]);
        match classify(&before, &mistyped, &h, false) {
            Classification::Diverged { actual } => {
                assert!(actual.iter().any(|l| l.contains("a + b")), "{actual:?}");
            }
            other => panic!("a mistyped insert must diverge, got {other:?}"),
        }
        // The correct transcription still reads as typed.
        let correct = lines(&[
            "def add(a, b):",
            "    return a + b",
            "",
            "def sub(a, b):",
            "    return a - b",
        ]);
        assert_eq!(
            classify(&before, &correct, &h, false),
            Classification::Typed
        );
    }

    #[test]
    fn editor_resolution_prefers_the_env_var() {
        std::env::set_var("ROTE_EDITOR", "my-editor --wait");
        assert_eq!(editor_command("nvim"), vec!["my-editor", "--wait"]);
        std::env::remove_var("ROTE_EDITOR");
        assert_eq!(editor_command("hx"), vec!["hx"]);
    }

    #[test]
    fn split_lines_round_trips_a_trailing_newline() {
        assert_eq!(split_lines("a\nb\n"), lines(&["a", "b"]));
        assert_eq!(split_lines("a\nb"), lines(&["a", "b"]));
        assert_eq!(split_lines(""), Vec::<String>::new());
    }

    #[test]
    fn render_is_plain_without_color() {
        let h = hunk(&["fn a() {"], &["    one();"], &["    two();"], &["}"], 2);
        let anchor = Anchor {
            line: 2,
            via: AnchorVia::ContextBefore,
        };
        let out = render(&h, 1, 3, &anchor, false);
        assert!(out.contains("hunk 1/3"));
        assert!(out.contains("f.rs:2"));
        assert!(out.contains("replace"));
        assert!(out.contains(" -     one();"));
        assert!(out.contains(" +     two();"));
        assert!(!out.contains('\u{1b}'), "no escape codes with --no-color");
    }

    #[test]
    fn render_warns_when_anchoring_fell_back_to_the_hint() {
        let h = hunk(&["gone"], &["old"], &["new"], &[], 5);
        let anchor = Anchor {
            line: 5,
            via: AnchorVia::Hint,
        };
        assert!(render(&h, 1, 1, &anchor, false).contains("context not found"));
    }
}
