//! rote — type in the agent's code by hand.
//!
//! Claude Code runs inside a shadow clone of the repository and works normally.
//! The deliverable of a session is the diff between shadow and real, served back
//! hunk by hunk so every line entering the real tree is typed by the user.
//!
//! See ARCHITECTURE.md for the shape and DESIGN.md for the contracts.

pub mod config;
pub mod curator;
pub mod daemon;
pub mod detect;
pub mod diffparse;
pub mod engine;
pub mod git;
pub mod http;
pub mod hunks;
pub mod model;
pub mod pane;
pub mod paths;
pub mod present;
pub mod review;
pub mod session;
pub mod shadow;
pub mod state;
pub mod watcher;
pub mod web;

use anyhow::{Context, Result};
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

/// `n` bytes of kernel entropy, hex encoded.
///
/// `/dev/urandom` through ordinary file I/O rather than `getrandom`/`rand` (a
/// dependency for two calls) or `libc::getentropy` (an `unsafe` block, and a
/// `// Safety:` line, to buy nothing over a read that cannot fail on any
/// platform rote supports).
pub fn random_hex(bytes: usize) -> Result<String> {
    use std::io::Read as _;
    let mut buf = vec![0u8; bytes];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut buf))
        .context("cannot read entropy from /dev/urandom")?;
    Ok(buf.iter().map(|b| format!("{b:02x}")).collect())
}

/// Timestamp for manifest fields: RFC 3339 / ISO 8601 extended, UTC.
pub fn now_iso8601() -> String {
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".into())
}

/// Timestamp for archive filenames: ISO 8601 *basic* format (`20260813T102200Z`).
///
/// Still ISO 8601, but without the colons of the extended form, which are
/// tedious to type and quote in a shell — and these names exist to be found and
/// opened by hand.
pub fn now_timestamp_slug() -> Result<String> {
    let now = OffsetDateTime::now_utc();
    let fmt = time::macros::format_description!("[year][month][day]T[hour][minute][second]Z");
    now.format(&fmt).context("cannot format timestamp")
}

/// Human-readable age of an RFC 3339 timestamp, as " (2h 14m)" or "" if unknown.
pub fn age_since(created_at: &str) -> String {
    let Ok(then) = OffsetDateTime::parse(created_at, &Rfc3339) else {
        return String::new();
    };
    let secs = (OffsetDateTime::now_utc() - then).whole_seconds();
    if secs < 0 {
        return String::new();
    }
    let (h, m) = (secs / 3600, (secs % 3600) / 60);
    if h > 0 {
        format!("  ({h}h {m}m)")
    } else if m > 0 {
        format!("  ({m}m)")
    } else {
        format!("  ({secs}s)")
    }
}
