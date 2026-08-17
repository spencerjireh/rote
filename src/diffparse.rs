//! Unified-diff parser. DESIGN.md §5.
//!
//! Hand-rolled because the format rote consumes is small and fixed: the output
//! of `git diff --no-index` over exactly two files. Pure functions over bytes —
//! no I/O, so the fixtures below are the whole test surface.
//!
//! Over bytes literally, including the line split: `str::lines` strips a trailing
//! `\r`, and the disk side does not. See `split_lines`.

use anyhow::{bail, Result};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiffLine {
    Context(String),
    Removed(String),
    Added(String),
}

impl DiffLine {
    pub fn text(&self) -> &str {
        match self {
            DiffLine::Context(s) | DiffLine::Removed(s) | DiffLine::Added(s) => s,
        }
    }

    pub fn is_change(&self) -> bool {
        !matches!(self, DiffLine::Context(_))
    }
}

/// One `@@ … @@` hunk, lines in file order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawHunk {
    pub old_start: usize,
    pub new_start: usize,
    pub lines: Vec<DiffLine>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ParsedDiff {
    /// Old side was `/dev/null` — the file is new in the shadow.
    pub old_is_devnull: bool,
    /// New side was `/dev/null` — the file is gone from the shadow.
    pub new_is_devnull: bool,
    /// git declined to diff the contents.
    pub is_binary: bool,
    pub hunks: Vec<RawHunk>,
}

/// Split on `\n`, keeping every byte in between — a trailing `\r` included — and
/// keeping a final line that has no terminator.
///
/// This must agree exactly with `present::split_lines`, which does the same job
/// on the disk side, and `str::lines` does not: it strips a trailing `\r`. So a
/// CRLF file's proposal never carried the `\r` that the file on disk did, and the
/// two could only ever be compared under a whitespace tolerance. With
/// `strict_whitespace = true` that tolerance is gone and such a hunk could never
/// classify `Typed` at all — it diverged on a byte the parser had thrown away.
pub(crate) fn split_lines(bytes: &[u8]) -> Vec<&[u8]> {
    if bytes.is_empty() {
        return Vec::new();
    }
    // One trailing newline is a terminator, not an empty final line.
    let body = bytes.strip_suffix(b"\n").unwrap_or(bytes);
    if body.is_empty() {
        return vec![body];
    }
    body.split(|b| *b == b'\n').collect()
}

/// Parse `git diff --no-index` output for a single file pair.
pub fn parse(diff: &[u8]) -> Result<ParsedDiff> {
    let mut out = ParsedDiff::default();
    let mut current: Option<RawHunk> = None;

    for raw in split_lines(diff) {
        // Decoded per line, after the split rather than before it, so the split
        // sees bytes. Content lines are UTF-8 by the time they get here —
        // `compute_hunks` gates a file that is not — so the lossy path only ever
        // covers headers, and those are discarded. Making it fallible instead
        // would let an oddly encoded pathname turn a working diff into an error.
        let line: &str = &String::from_utf8_lossy(raw);
        // Headers. Checked before the body prefixes because "--- " and "+++ "
        // would otherwise look like removed/added content.
        if let Some(rest) = line.strip_prefix("--- ") {
            out.old_is_devnull = is_devnull(rest);
            continue;
        }
        if let Some(rest) = line.strip_prefix("+++ ") {
            out.new_is_devnull = is_devnull(rest);
            continue;
        }
        if line.starts_with("diff --git")
            || line.starts_with("index ")
            || line.starts_with("old mode ")
            || line.starts_with("new mode ")
            || line.starts_with("new file mode ")
            || line.starts_with("deleted file mode ")
            || line.starts_with("similarity index ")
            || line.starts_with("rename from ")
            || line.starts_with("rename to ")
        {
            continue;
        }
        if line.starts_with("Binary files ") || line.starts_with("GIT binary patch") {
            out.is_binary = true;
            continue;
        }
        if line.starts_with("@@") {
            if let Some(h) = current.take() {
                out.hunks.push(h);
            }
            current = Some(parse_hunk_header(line)?);
            continue;
        }

        let Some(hunk) = current.as_mut() else {
            // Anything before the first @@ that we did not recognise is preamble.
            continue;
        };

        // "\ No newline at end of file" annotates the preceding line; it is not
        // content and must not become a line of its own.
        if line.starts_with('\\') {
            continue;
        }

        match line.as_bytes().first() {
            Some(b' ') => hunk.lines.push(DiffLine::Context(line[1..].to_string())),
            Some(b'-') => hunk.lines.push(DiffLine::Removed(line[1..].to_string())),
            Some(b'+') => hunk.lines.push(DiffLine::Added(line[1..].to_string())),
            // A completely empty line in the body is an empty context line: git
            // emits " " for it, but some tools strip the trailing space.
            None => hunk.lines.push(DiffLine::Context(String::new())),
            _ => continue,
        }
    }

    if let Some(h) = current.take() {
        out.hunks.push(h);
    }
    Ok(out)
}

fn is_devnull(rest: &str) -> bool {
    let path = rest.split('\t').next().unwrap_or(rest).trim();
    path == "/dev/null"
}

/// `@@ -a,b +c,d @@ optional heading`
fn parse_hunk_header(line: &str) -> Result<RawHunk> {
    let body = line
        .strip_prefix("@@")
        .and_then(|s| s.split("@@").next())
        .map(str::trim)
        .unwrap_or_default();

    let mut old_start = 0usize;
    let mut new_start = 0usize;
    for field in body.split_whitespace() {
        let (sign, rest) = field.split_at(1);
        let start = rest.split(',').next().unwrap_or("0");
        let value: usize = start
            .parse()
            .map_err(|_| anyhow::anyhow!("malformed hunk header: {line}"))?;
        match sign {
            "-" => old_start = value,
            "+" => new_start = value,
            _ => bail!("malformed hunk header: {line}"),
        }
    }

    Ok(RawHunk {
        old_start,
        new_start,
        lines: Vec::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx(s: &str) -> DiffLine {
        DiffLine::Context(s.into())
    }
    fn rem(s: &str) -> DiffLine {
        DiffLine::Removed(s.into())
    }
    fn add(s: &str) -> DiffLine {
        DiffLine::Added(s.into())
    }

    #[test]
    fn parses_a_simple_modification() {
        let d = b"diff --git a/x b/x\nindex 111..222 100644\n--- a/x\n+++ b/x\n\
                  @@ -1,3 +1,3 @@\n one\n-two\n+TWO\n three\n";
        let p = parse(d).unwrap();
        assert!(!p.old_is_devnull && !p.new_is_devnull && !p.is_binary);
        assert_eq!(p.hunks.len(), 1);
        assert_eq!(p.hunks[0].old_start, 1);
        assert_eq!(p.hunks[0].new_start, 1);
        assert_eq!(
            p.hunks[0].lines,
            vec![ctx("one"), rem("two"), add("TWO"), ctx("three")]
        );
    }

    #[test]
    fn detects_a_new_file() {
        let d = b"diff --git a/new b/new\nnew file mode 100644\n--- /dev/null\n+++ b/new\n\
                  @@ -0,0 +1,2 @@\n+alpha\n+beta\n";
        let p = parse(d).unwrap();
        assert!(p.old_is_devnull);
        assert!(!p.new_is_devnull);
        assert_eq!(p.hunks[0].lines, vec![add("alpha"), add("beta")]);
    }

    #[test]
    fn detects_a_deleted_file() {
        let d = b"diff --git a/gone b/gone\ndeleted file mode 100644\n--- a/gone\n+++ /dev/null\n\
                  @@ -1,2 +0,0 @@\n-alpha\n-beta\n";
        let p = parse(d).unwrap();
        assert!(!p.old_is_devnull);
        assert!(p.new_is_devnull);
        assert_eq!(p.hunks[0].lines, vec![rem("alpha"), rem("beta")]);
    }

    #[test]
    fn no_newline_marker_is_not_content() {
        let d = b"--- a/x\n+++ b/x\n@@ -1 +1 @@\n-old\n\\ No newline at end of file\n+new\n\
                  \\ No newline at end of file\n";
        let p = parse(d).unwrap();
        assert_eq!(p.hunks[0].lines, vec![rem("old"), add("new")]);
    }

    #[test]
    fn parses_multiple_hunks() {
        let d = b"--- a/x\n+++ b/x\n@@ -1,2 +1,2 @@\n a\n-b\n+B\n@@ -10,2 +10,2 @@\n y\n-z\n+Z\n";
        let p = parse(d).unwrap();
        assert_eq!(p.hunks.len(), 2);
        assert_eq!(p.hunks[0].old_start, 1);
        assert_eq!(p.hunks[1].old_start, 10);
        assert_eq!(p.hunks[1].lines, vec![ctx("y"), rem("z"), add("Z")]);
    }

    #[test]
    fn empty_diff_yields_no_hunks() {
        let p = parse(b"").unwrap();
        assert!(p.hunks.is_empty());
        assert!(!p.is_binary);
    }

    #[test]
    fn detects_binary_files() {
        let d = b"diff --git a/img b/img\nindex 1..2 100644\nBinary files a/img and b/img differ\n";
        let p = parse(d).unwrap();
        assert!(p.is_binary);
        assert!(p.hunks.is_empty());
    }

    #[test]
    fn hunk_header_with_a_section_heading_parses() {
        let d = b"--- a/x\n+++ b/x\n@@ -12,3 +14,4 @@ fn main() {\n ctx\n+added\n";
        let p = parse(d).unwrap();
        assert_eq!(p.hunks[0].old_start, 12);
        assert_eq!(p.hunks[0].new_start, 14);
        assert_eq!(p.hunks[0].lines, vec![ctx("ctx"), add("added")]);
    }

    #[test]
    fn single_line_counts_omit_the_comma() {
        let d = b"--- a/x\n+++ b/x\n@@ -5 +5 @@\n-a\n+b\n";
        let p = parse(d).unwrap();
        assert_eq!(p.hunks[0].old_start, 5);
        assert_eq!(p.hunks[0].new_start, 5);
    }

    #[test]
    fn empty_context_line_is_preserved() {
        // A blank context line matters: splitting prefers blank-line boundaries.
        let d = b"--- a/x\n+++ b/x\n@@ -1,3 +1,3 @@\n a\n \n-b\n+B\n";
        let p = parse(d).unwrap();
        assert_eq!(
            p.hunks[0].lines,
            vec![ctx("a"), ctx(""), rem("b"), add("B")]
        );
    }

    #[test]
    fn crlf_content_keeps_its_carriage_return() {
        // `str::lines` would strip these, leaving a proposal that no CRLF file
        // could ever match byte for byte.
        let d = b"--- a/x\n+++ b/x\n@@ -1,2 +1,2 @@\n keep\r\n-old\r\n+new\r\n";
        let p = parse(d).unwrap();
        assert_eq!(
            p.hunks[0].lines,
            vec![ctx("keep\r"), rem("old\r"), add("new\r")]
        );
    }

    #[test]
    fn the_line_split_agrees_with_the_disk_side() {
        // The two halves of every classification comparison. A disagreement here
        // is a hunk that can never be typed.
        for input in ["", "\n", "a", "a\n", "a\nb", "a\n\n", "a\r\n", "a\r\nb\r\n"] {
            let ours: Vec<String> = split_lines(input.as_bytes())
                .into_iter()
                .map(|b| String::from_utf8_lossy(b).into_owned())
                .collect();
            assert_eq!(
                ours,
                crate::present::split_lines(input),
                "disagreed on {input:?}"
            );
        }
    }

    #[test]
    fn a_final_line_without_a_newline_is_kept() {
        let d = b"--- a/x\n+++ b/x\n@@ -1 +1 @@\n-old\n+new";
        let p = parse(d).unwrap();
        assert_eq!(p.hunks[0].lines, vec![rem("old"), add("new")]);
    }

    #[test]
    fn a_trailing_newline_does_not_add_an_empty_line() {
        let d = b"--- a/x\n+++ b/x\n@@ -1 +1 @@\n-old\n+new\n";
        let p = parse(d).unwrap();
        assert_eq!(p.hunks[0].lines, vec![rem("old"), add("new")]);
    }
}
