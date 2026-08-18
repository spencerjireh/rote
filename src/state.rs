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
//! Nothing here does I/O. `Snapshot::build` is handed the means of resolving an
//! anchor rather than resolving one itself, because resolving means reading the
//! real file — and the whole point of putting `anchor_line` on the wire is that a
//! client never has to. It needs more than one now: the active hunk and the hunk
//! carrying an open question are usually different (§6), and both go on the wire.

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
    /// The hunk waiting on a keep-or-retry answer, if any.
    ///
    /// Separate from `active` because §6 has the queue advance past a question
    /// rather than block on it, so the two are usually different hunks — and a
    /// front end has to render this one to ask the question honestly. `QueueItem`
    /// carries only `has_question`, which is enough to mark it in a list and not
    /// enough to show what was proposed against what was typed.
    ///
    /// Optional and omitted when unset, so this is additive: `WIRE_VERSION` is
    /// unchanged, and a client written against the old shape still parses.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub question: Option<Presented>,
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

/// What a front end may report about how a hunk arrived.
///
/// Deliberately not `hunks::Input`: `unknown` is not something a front end can
/// assert — it is the absence of an assertion — and an unrepresentable illegal
/// state beats a runtime rejection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Reported {
    Typed,
    Pasted,
}

impl From<Reported> for crate::hunks::Input {
    fn from(r: Reported) -> Self {
        match r {
            Reported::Typed => crate::hunks::Input::Typed,
            Reported::Pasted => crate::hunks::Input::Pasted,
        }
    }
}

/// What a front end may ask for.
///
/// Deliberately excludes `done`, `abort` and `start`: those confirm
/// interactively and `exec`, so they stay CLI-only. Note there is no verb that
/// sets a hunk to `typed` — that is producible only by the classifier, whose
/// input came from the shadow.
///
/// `report` does not weaken that. It writes `input` and nothing else: not
/// `status`, not `pending_divergence`, not `last_presented`. It cannot create a
/// hunk or resurrect one, and it is accepted on a hunk that has already reached
/// a terminal status — precisely so that a paste report cannot lose a race
/// against the classifier that is about to mark the same hunk typed.
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
    /// Say how a hunk's content arrived. Advisory; changes no status.
    Report {
        hunk_id: String,
        input: Reported,
    },
    /// Force a full recompute, and a republished snapshot even if it finds
    /// nothing — the sender is saying it thinks it is out of sync.
    ///
    /// The one verb whose `generation` is *not* checked. It asserts nothing about
    /// the queue's contents, so there is nothing to be stale against, and
    /// refusing it would deny the client that has fallen behind the verb that
    /// recovers from exactly that.
    Refresh,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Request {
    pub wire_version: u32,
    /// The generation the client was looking at. Checked only when present: a
    /// client that does not care about races may omit it, but one answering a
    /// question about a hunk that has since been reworked must not. `refresh` is
    /// the one verb that ignores it even when present — see `Command::Refresh`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generation: Option<u64>,
    #[serde(flatten)]
    pub command: Command,
}

/// Why a command was refused, as a variant rather than as prose.
///
/// `reason` is for a human to read and is free to be reworded. This is what a
/// client is allowed to branch on. The two are not redundant: `rote resolve`
/// treats "no open question" as a benign no-op and everything else as an error,
/// and it used to tell them apart with `reason.contains("no open question")` —
/// a substring match against a string composed in `engine.rs`, with nothing
/// pinning the two together. Rewording the message would have turned a no-op
/// into a hard error, silently.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Cause {
    NoSuchHunk,
    NoOpenQuestion,
    WireVersion,
}

impl Cause {
    /// The human string this cause renders as.
    ///
    /// Kept here so the wire text has one origin. `WireVersion` is the exception
    /// — it names two numbers, so its message is composed at the point it is
    /// raised and this is only the prefix.
    pub fn message(self) -> &'static str {
        match self {
            Cause::NoSuchHunk => "no such hunk",
            Cause::NoOpenQuestion => "no open question on that hunk",
            Cause::WireVersion => "wire version",
        }
    }
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
        /// Additive, and optional for exactly the reason §12 gives: a client
        /// written against the old shape still reads `reason` and is unaffected,
        /// so this is not a `WIRE_VERSION` bump.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cause: Option<Cause>,
    },
}

impl Outcome {
    /// A refusal carrying both its prose and its variant.
    pub fn rejected(cause: Cause) -> Self {
        Outcome::Rejected {
            reason: cause.message().to_string(),
            cause: Some(cause),
        }
    }
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
        locate: impl Fn(&Hunk) -> (usize, AnchorVia, String),
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
        let present = |h: &Hunk| {
            let (anchor_line, anchor_via, real_path) = locate(h);
            Presented {
                position: manifest.queue_position(&h.id).unwrap_or(1),
                total,
                hunk: h.clone(),
                anchor_line,
                anchor_via,
                real_path,
            }
        };
        let active = manifest.active().map(&present);
        // Usually a different hunk from `active`, and often the whole reason a
        // front end has anything to ask about.
        let question = manifest.questioned().map(&present);

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
            question,
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

        // `report` says how content arrived, and nothing else.
        let text = r#"{"wire_version":1,"verb":"report","hunk_id":"h-1a2b","input":"pasted"}"#;
        let req: Request = serde_json::from_str(text).unwrap();
        assert_eq!(
            req.command,
            Command::Report {
                hunk_id: "h-1a2b".into(),
                input: Reported::Pasted,
            }
        );
    }

    #[test]
    fn a_front_end_cannot_report_that_it_does_not_know() {
        // `unknown` is the absence of an assertion. Making it unrepresentable on
        // the wire beats rejecting it at runtime.
        let text = r#"{"wire_version":1,"verb":"report","hunk_id":"h-1","input":"unknown"}"#;
        assert!(serde_json::from_str::<Request>(text).is_err());
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
    fn a_rejection_carries_both_its_prose_and_its_variant() {
        let r = Response {
            generation: 3,
            outcome: Outcome::rejected(Cause::NoOpenQuestion),
        };
        assert_eq!(
            serde_json::to_value(&r).unwrap(),
            json!({
                "generation": 3,
                "outcome": "rejected",
                "reason": "no open question on that hunk",
                "cause": "no_open_question",
            })
        );
    }

    /// The field is additive, so a payload written before it existed — or by a
    /// client that omits it — must still deserialize. This is what makes adding
    /// it not a `WIRE_VERSION` bump (§12).
    #[test]
    fn a_rejection_without_a_cause_still_parses() {
        let r: Response = serde_json::from_value(json!({
            "generation": 3,
            "outcome": "rejected",
            "reason": "no such hunk",
        }))
        .unwrap();
        assert_eq!(
            r.outcome,
            Outcome::Rejected {
                reason: "no such hunk".into(),
                cause: None,
            }
        );
    }

    /// `rote resolve` treats this one cause as a benign no-op and everything
    /// else as an error. It used to tell them apart with a substring match on
    /// prose composed in `engine.rs`; this pins the pair the CLI relies on, so
    /// rewording the message can no longer silently turn a no-op into a failure.
    #[test]
    fn the_benign_refusal_keeps_its_wire_text() {
        assert_eq!(
            Cause::NoOpenQuestion.message(),
            "no open question on that hunk"
        );
        assert!(Cause::NoOpenQuestion.message().contains("no open question"));
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
            question: None,
            notices: vec![Notice::info("hello")],
        };
        let text = serde_json::to_string(&snap).unwrap();
        assert_eq!(serde_json::from_str::<Snapshot>(&text).unwrap(), snap);
        assert!(
            !text.contains("question"),
            "optional and omitted when unset, so this stays additive: {text}"
        );

        let ev = Event::Snapshot(Box::new(snap));
        let text = serde_json::to_string(&ev).unwrap();
        assert_eq!(serde_json::from_str::<Event>(&text).unwrap(), ev);
    }
}
