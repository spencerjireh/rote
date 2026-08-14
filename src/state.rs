//! The wire types: what a front end sees, and what it may ask for.
//!
//! This is a public contract in the same sense as the `Hunk` schema in
//! DESIGN.md §4, and for the same reason — three front ends will be written
//! against it (the terminal pane, an nvim plugin, a browser) and they will not
//! all be updated on the same day. Field names and variant tags are pinned by
//! tests here, deliberately, so that changing one is a decision rather than an
//! accident.
//!
//! It is a thin envelope over types that are *already* contracts rather than a
//! parallel schema: `Hunk`, `Status`, `Op` and `State` all serialize lowercase
//! and already cross the `--json` seam. A second set of view types would be two
//! schemas to keep in agreement forever, and they would disagree.
//!
//! Nothing here does I/O. `Snapshot::build` takes the anchor rather than
//! resolving it, because resolving one means reading the real file — and the
//! whole point of putting `anchor_line` on the wire is that a client never has
//! to.

use crate::hunks::{Hunk, Op, Status};
use crate::present::AnchorVia;
use crate::session::{Manifest, State, Terminal};
use serde::{Deserialize, Serialize};

/// Bumped when a change would break a front end written against the old shape.
/// Additive changes — a new optional field, a new event variant — do not bump.
pub const WIRE_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Snapshot {
    pub wire_version: u32,
    /// The staleness token. A client echoes it back with any mutating verb.
    pub generation: u64,
    pub state: State,
    pub task: String,
    /// The repository moved under the session (DESIGN.md §9.2).
    pub drift: bool,
    pub counts: Counts,
    /// The hunk to work on, with everything needed to render and navigate it.
    pub active: Option<Presented>,
    /// Pending hunks in queue order. Summaries only — see `QueueItem`.
    pub queue: Vec<QueueItem>,
    pub notices: Vec<Notice>,
}

/// What a daemon says about itself.
///
/// The handshake: a front end reads this first and can refuse politely rather
/// than misinterpreting a payload it does not understand. `project_hash` is the
/// one field a client must check — it is what proves the daemon on this port is
/// serving the repository the client thinks it is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Health {
    pub ok: bool,
    pub wire_version: u32,
    pub manifest_version: u32,
    pub rote_version: String,
    pub pid: u32,
    pub port: u16,
    pub project_hash: String,
    pub repo_root: String,
    pub shadow_dir: String,
    /// `None` when the session has been archived and the daemon is winding up.
    pub session_state: Option<State>,
    pub task: String,
    /// `None` before the engine's first publish.
    pub generation: Option<u64>,
    /// Attached event streams, as of the last published frame rather than as of
    /// now: a client is only noticed to have gone when a send to it fails, and
    /// an idle daemon publishes nothing.
    pub subscribers: usize,
    pub uptime_ms: u64,
}

/// One hunk in full, for a client that has a summary and wants the text.
///
/// `QueueItem` carries no line bodies so a snapshot stays small; this is the
/// other half of that trade. Not a `Presented`: `position` and `total` are
/// meaningless for a hunk that is typed or skipped, and zero would be a lie a
/// front end will happily render.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HunkDetail {
    pub generation: u64,
    pub hunk: Hunk,
    pub anchor_line: usize,
    pub anchor_via: AnchorVia,
    pub real_path: String,
}

/// The active hunk, resolved against the real file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Presented {
    pub hunk: Hunk,
    /// 1-based position in the pending queue, and the queue's length.
    pub position: usize,
    pub total: usize,
    /// Where this hunk starts in the real file *now*, not where it started when
    /// the queue was built. This is the field that lets a client be a pure
    /// client: without it, rendering a jump target means reading the file, and
    /// a browser front end cannot.
    pub anchor_line: usize,
    pub anchor_via: AnchorVia,
    /// Absolute path, for the open action.
    pub real_path: String,
}

/// A queue entry without its line bodies.
///
/// Summaries rather than whole hunks so that a snapshot stays small at two
/// hundred pending hunks — the active hunk carries its text, and anything else
/// is fetched by id when a client actually wants it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueueItem {
    pub id: String,
    pub key: String,
    pub file: String,
    pub op: Op,
    pub anchor_hint: usize,
    /// True when this hunk is waiting on a keep-or-retry answer.
    pub has_question: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub curator_note: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Counts {
    pub pending: usize,
    pub typed: usize,
    pub diverged: usize,
    pub skipped: usize,
    pub total: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Level {
    Info,
    Warn,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Notice {
    pub level: Level,
    pub text: String,
}

impl Notice {
    pub fn info(text: impl Into<String>) -> Self {
        Self {
            level: Level::Info,
            text: text.into(),
        }
    }

    pub fn warn(text: impl Into<String>) -> Self {
        Self {
            level: Level::Warn,
            text: text.into(),
        }
    }
}

/// What the engine publishes.
///
/// Every state change carries a whole `Snapshot`, never a delta. A delta
/// protocol needs a resync story, and a client that misses one frame is
/// silently wrong from then on — which is the worst failure mode available,
/// because nothing looks broken. The snapshot is a few kilobytes at human
/// event rates.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    /// Boxed: this variant is an order of magnitude larger than the others, and
    /// an unboxed enum is as large as its largest variant.
    Snapshot(Box<Snapshot>),
    Notice(Notice),
    /// A keepalive that is not a redraw. Carries the generation so a client can
    /// notice it has fallen behind without being sent a frame.
    Heartbeat {
        generation: u64,
    },
    /// The session ended. This means *exit*, not *redraw*.
    Closed {
        terminal: Option<Terminal>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Resolution {
    /// Record what the user typed; the proposal is not offered again.
    Keep,
    /// Withdraw the question and keep watching.
    Retry,
}

/// What a front end may ask for.
///
/// Deliberately excludes `done`, `abort` and `start`: those confirm
/// interactively and `exec`, so they stay CLI-only. Note there is no verb that
/// sets a hunk to `typed` — that is producible only by the classifier, whose
/// input came from the shadow.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "verb", rename_all = "snake_case")]
pub enum Command {
    Skip {
        hunk_id: String,
    },
    Resolve {
        hunk_id: String,
        choice: Resolution,
    },
    /// Make a hunk the one `show` re-prints. Display only.
    Show {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        hunk_id: Option<String>,
    },
    /// Force a full recompute.
    Refresh,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Request {
    pub wire_version: u32,
    /// The generation the client was looking at. Checked only when present: a
    /// client that does not care about races may omit it, but one answering a
    /// question about a hunk that has since been reworked must not.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generation: Option<u64>,
    #[serde(flatten)]
    pub command: Command,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum Outcome {
    Applied,
    /// The world moved. The current generation is included so the client can
    /// re-read and re-issue without a round trip to discover it.
    Stale {
        current: u64,
    },
    Rejected {
        reason: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Response {
    pub generation: u64,
    #[serde(flatten)]
    pub outcome: Outcome,
}

impl Snapshot {
    /// Assemble a snapshot from a manifest.
    ///
    /// `anchor` is passed in rather than computed: resolving it reads the real
    /// file, and this function is pure so that it can be tested without one.
    pub fn build(
        manifest: &Manifest,
        anchor: Option<(usize, AnchorVia, String)>,
        drift: bool,
        notices: Vec<Notice>,
    ) -> Self {
        let queue: Vec<QueueItem> = manifest
            .queue_view()
            .iter()
            .map(|h| QueueItem {
                id: h.id.clone(),
                key: h.key.clone(),
                file: h.file.clone(),
                op: h.op,
                anchor_hint: h.anchor_hint,
                has_question: h.pending_divergence.is_some(),
                curator_note: h.curator_note.clone(),
            })
            .collect();

        let total = queue.len();
        let active = manifest.active().and_then(|h| {
            let (anchor_line, anchor_via, real_path) = anchor?;
            Some(Presented {
                position: manifest.queue_position(&h.id).unwrap_or(1),
                total,
                hunk: h.clone(),
                anchor_line,
                anchor_via,
                real_path,
            })
        });

        Self {
            wire_version: WIRE_VERSION,
            generation: manifest.generation,
            state: manifest.state,
            task: manifest.task.clone(),
            drift,
            counts: Counts {
                pending: manifest.count(Status::Pending),
                typed: manifest.count(Status::Typed),
                diverged: manifest.count(Status::Diverged),
                skipped: manifest.count(Status::Skipped),
                total: manifest.hunks.len(),
            },
            active,
            queue,
            notices,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn event_variants_are_tagged_by_type() {
        // Pinned: three front ends will match on these strings.
        let ev = Event::Heartbeat { generation: 7 };
        assert_eq!(
            serde_json::to_value(&ev).unwrap(),
            json!({"type": "heartbeat", "generation": 7})
        );

        let ev = Event::Closed {
            terminal: Some(Terminal::Done),
        };
        assert_eq!(
            serde_json::to_value(&ev).unwrap(),
            json!({"type": "closed", "terminal": "done"})
        );

        let ev = Event::Notice(Notice::warn("baseline drift"));
        assert_eq!(
            serde_json::to_value(&ev).unwrap(),
            json!({"type": "notice", "level": "warn", "text": "baseline drift"})
        );
    }

    #[test]
    fn commands_are_tagged_by_verb_and_flattened_into_a_request() {
        let req = Request {
            wire_version: WIRE_VERSION,
            generation: Some(42),
            command: Command::Resolve {
                hunk_id: "h-abc".into(),
                choice: Resolution::Keep,
            },
        };
        assert_eq!(
            serde_json::to_value(&req).unwrap(),
            json!({
                "wire_version": 1,
                "generation": 42,
                "verb": "resolve",
                "hunk_id": "h-abc",
                "choice": "keep"
            })
        );
    }

    #[test]
    fn a_hand_written_request_deserializes() {
        // The direction of travel in Stage 2: a front end writes this by hand,
        // in whatever language it is written in.
        let text = r#"{"wire_version":1,"verb":"skip","hunk_id":"h-1a2b"}"#;
        let req: Request = serde_json::from_str(text).unwrap();
        assert_eq!(req.wire_version, 1);
        assert_eq!(req.generation, None, "an absent generation is not a race");
        assert_eq!(
            req.command,
            Command::Skip {
                hunk_id: "h-1a2b".into()
            }
        );

        // `show` with no id is legal and means "the last presented one".
        let req: Request = serde_json::from_str(r#"{"wire_version":1,"verb":"show"}"#).unwrap();
        assert_eq!(req.command, Command::Show { hunk_id: None });

        // `refresh` carries nothing at all.
        let req: Request = serde_json::from_str(r#"{"wire_version":1,"verb":"refresh"}"#).unwrap();
        assert_eq!(req.command, Command::Refresh);
    }

    #[test]
    fn a_stale_response_carries_the_current_generation() {
        let r = Response {
            generation: 9,
            outcome: Outcome::Stale { current: 11 },
        };
        assert_eq!(
            serde_json::to_value(&r).unwrap(),
            json!({"generation": 9, "outcome": "stale", "current": 11})
        );
    }

    #[test]
    fn anchor_via_serializes_snake_case() {
        assert_eq!(
            serde_json::to_value(AnchorVia::ContextBefore).unwrap(),
            json!("context_before")
        );
        assert_eq!(
            serde_json::to_value(AnchorVia::NoContext).unwrap(),
            json!("no_context")
        );
    }

    #[test]
    fn every_wire_type_round_trips() {
        let snap = Snapshot {
            wire_version: WIRE_VERSION,
            generation: 3,
            state: State::Transcribing,
            task: "add tagging".into(),
            drift: false,
            counts: Counts {
                pending: 2,
                typed: 1,
                diverged: 0,
                skipped: 0,
                total: 3,
            },
            active: None,
            queue: vec![],
            notices: vec![Notice::info("hello")],
        };
        let text = serde_json::to_string(&snap).unwrap();
        assert_eq!(serde_json::from_str::<Snapshot>(&text).unwrap(), snap);

        let ev = Event::Snapshot(Box::new(snap));
        let text = serde_json::to_string(&ev).unwrap();
        assert_eq!(serde_json::from_str::<Event>(&text).unwrap(), ev);
    }
}
