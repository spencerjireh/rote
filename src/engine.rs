//! The watch engine: reclassify on save, advance the queue, publish snapshots.
//!
//! Everything that decides *when* rote acts lives here, and it is written to be
//! testable without a filesystem or a stopwatch. Time enters as a parameter:
//! `step(ev, now)` is handed the moment rather than reading one, so a test names
//! the moment it wants and every grace-window case is exact instead of a
//! two-second sleep that is flaky under load. The divergence state machine is a
//! separate `Watchdog` with no I/O at all.
//!
//! The engine is not privileged. It mutates the manifest through
//! `session::with_session` exactly as the CLI does, and it re-verifies the hunk
//! it is about to write is still the one it classified — classification happens
//! outside the lock, so the queue can move underneath it.

use crate::config::Config;
use crate::curator;
use crate::hunks::{Divergence, Hunk, Status};
use crate::paths::ProjectPaths;
use crate::present::{self, Classification};
use crate::session::{self, Manifest};
use crate::state::{self, Notice};
use anyhow::{Context, Result};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender};
use std::time::Duration;

/// Minimum wall time between two full re-diffs of the trees.
///
/// The debounce collapses a burst; this bounds the damage when saves arrive
/// steadily forever. A full recompute shells out to git once per changed file.
pub const RECOMPUTE_FLOOR: Duration = Duration::from_millis(1500);

/// How long the loop blocks waiting for an event before looking at its timers.
pub const TICK: Duration = Duration::from_millis(250);

/// The tick used when the watcher has failed and we are polling instead.
pub const DEGRADED_TICK: Duration = Duration::from_secs(2);

/// A full recompute happens at least this often even in total silence, because
/// it is the only thing that can notice the repository moved under the session.
pub const FLOOR_SWEEP: Duration = Duration::from_secs(30);

/// Milliseconds since the engine started.
///
/// A plain integer rather than `Instant` so tests can name a moment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Tick(pub u64);

impl Tick {
    fn since(self, earlier: Tick) -> u64 {
        self.0.saturating_sub(earlier.0)
    }
}

/// Which tree an event came from. The agent's work and the user's typing mean
/// entirely different things.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    Real,
    Shadow,
}

/// A verb, and somewhere to send the answer.
///
/// The reply channel is optional because two callers want different things: an
/// HTTP handler is holding a socket open and needs the outcome, while the pane
/// pressing `s` learns what happened from the next snapshot like everything
/// else. Making it optional is what keeps the in-process path from having to
/// invent a channel it will not read.
#[derive(Debug)]
pub struct CommandRequest {
    pub request: state::Request,
    pub reply: Option<std::sync::mpsc::Sender<state::Response>>,
}

impl From<state::Command> for CommandRequest {
    /// A verb with no generation and nobody waiting: fire and forget.
    fn from(command: state::Command) -> Self {
        Self {
            request: state::Request {
                wire_version: state::WIRE_VERSION,
                generation: None,
                command,
            },
            reply: None,
        }
    }
}

/// Deliberately not `PartialEq`: a reply channel has no meaningful equality,
/// and nothing in the codebase compares an event.
#[derive(Debug)]
pub enum EngineEvent {
    /// Something under this repo-relative path may have changed. Deliberately
    /// carries no `EventKind`: the engine re-reads from disk and decides for
    /// itself, which is what makes atomic-rename saves and coalesced
    /// directory-granular events harmless.
    Changed(Origin, PathBuf),
    Command(CommandRequest),
    /// The watcher itself failed. Degrade to polling rather than dying.
    WatchError(String),
}

/// What a save did to one hunk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Observation {
    Typed,
    /// On its way to the proposal. Never a question, however long the pause.
    InProgress,
    /// Finished, and different. A question, once the file stops moving.
    Disagrees {
        actual: Vec<String>,
    },
    Untouched,
}

/// Classify one hunk, splitting `Diverged` into "still typing" and "disagrees".
///
/// `classify` cannot make this distinction and should not: on editor exit the
/// user had declared they were finished, so every difference was a real one.
/// Under a watcher there is no such declaration, and the same verdict arrives
/// after every keystroke-flush.
pub fn observe(
    before: &[String],
    after: &[String],
    hunk: &Hunk,
    strict_whitespace: bool,
) -> Observation {
    match present::classify(before, after, hunk, strict_whitespace) {
        Classification::Typed => Observation::Typed,
        Classification::Untouched => Observation::Untouched,
        Classification::Diverged { actual } => {
            // `classify` decides "untouched" by comparing the whole file, which
            // is right for one editor session over one hunk and wrong for a
            // watcher: a file usually holds several hunks, so editing any one
            // of them makes every other hunk in it look touched. Untouched text
            // is rarely a prefix of its proposal, so each of those would arm a
            // question about work the user has not started.
            //
            // The narrower question is the correct one here: did *this region*
            // move? Note this cannot swallow a botched transcription — that
            // leaves the region different from where it started, by definition.
            if present::region_of(before, hunk) == actual {
                return Observation::Untouched;
            }
            if present::is_in_progress(&actual, &hunk.new_lines, strict_whitespace) {
                Observation::InProgress
            } else {
                Observation::Disagrees { actual }
            }
        }
    }
}

/// What a mutation closure decided.
enum Decision<T> {
    Apply(T),
    Reject(String),
    /// Nothing to write. Reported as applied — the caller asked for a state the
    /// manifest is already in — but no write, so no generation bump and no
    /// snapshot invalidated for every subscriber.
    Nothing,
}

/// Either the value a mutation produced, or the reason there was not one.
enum Mutated<T> {
    Applied(T),
    Refused(state::Outcome),
}

/// Has the session been archived out from under us?
fn session_gone(project: &ProjectPaths) -> bool {
    matches!(Manifest::load(project), Ok(None))
}

#[derive(Debug, Clone)]
struct Candidate {
    actual: Vec<String>,
    armed_at: Tick,
}

/// The quiescence timer for divergence questions.
///
/// Pure, so the whole of the "do not interrupt someone mid-thought" behaviour
/// is unit-testable with no filesystem and no sleeps.
///
/// Two layers, and both are needed. The in-progress predicate handles typing
/// top to bottom, which is most transcription, instantly and with no delay at
/// all. The timer handles everything else — pasting the middle, typing
/// bottom-up, fixing a line — where no prefix relationship ever holds and only
/// stillness can tell "wrong" from "not finished".
#[derive(Debug, Default)]
pub struct Watchdog {
    armed: HashMap<String, Candidate>,
}

impl Watchdog {
    /// Record what a save did. Re-arming on every disagreement is the point:
    /// the window measures stillness, not elapsed time since the first mistake.
    pub fn observe(&mut self, hunk_id: &str, outcome: &Observation, now: Tick) {
        match outcome {
            Observation::Disagrees { actual } => {
                self.armed.insert(
                    hunk_id.to_string(),
                    Candidate {
                        actual: actual.clone(),
                        armed_at: now,
                    },
                );
            }
            // Anything else means there is nothing to ask about right now.
            _ => {
                self.armed.remove(hunk_id);
            }
        }
    }

    /// Questions whose file has been still for the whole window.
    pub fn due(&self, now: Tick, grace_ms: u64) -> Vec<(String, Vec<String>)> {
        let mut out: Vec<(String, Vec<String>)> = self
            .armed
            .iter()
            .filter(|(_, c)| now.since(c.armed_at) >= grace_ms)
            .map(|(id, c)| (id.clone(), c.actual.clone()))
            .collect();
        // Deterministic order: a HashMap's iteration order is not, and the
        // questions become manifest writes.
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }

    pub fn disarm(&mut self, hunk_id: &str) {
        self.armed.remove(hunk_id);
    }

    pub fn is_armed(&self, hunk_id: &str) -> bool {
        self.armed.contains_key(hunk_id)
    }
}

/// What to say once a curation lands.
fn curated_line(outcome: &curator::Outcome) -> String {
    if outcome.beyond_cap > 0 {
        format!(
            "curated the first {} hunks; the other {} keep file order",
            curator::MAX_HUNKS,
            outcome.beyond_cap
        )
    } else {
        "the queue is now in teaching order".to_string()
    }
}

/// Why a full recompute is pending.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Pending {
    No,
    /// Wait out the debounce, then re-diff.
    At(Tick),
}

pub struct Engine {
    project: ProjectPaths,
    cfg: Config,
    /// What `now()` counts from. `step` takes the moment as a parameter, so this
    /// is only consulted by `run`'s own loop — which is why there is no clock
    /// abstraction here: the tests drive `step` directly and never need one.
    started_at: std::time::Instant,
    /// Each file's lines as of the last time its hunks became answerable.
    baselines: HashMap<String, Vec<String>>,
    watchdog: Watchdog,
    recompute: Pending,
    last_recompute: Option<Tick>,
    /// Emitted state is suppressed when nothing moved, so a quiet watcher does
    /// not redraw the pane forever.
    last_published: Option<u64>,
    drift: bool,
    degraded: bool,
    started: bool,
    /// Results from curator threads.
    ///
    /// The engine keeps its own `Sender`, so `try_recv` can never report
    /// `Disconnected` and there is no spurious-shutdown case to handle. An
    /// `EngineEvent` variant would have worked too, but this needs no change to
    /// a type that is deliberately not `PartialEq`.
    curation_tx: Sender<curator::Outcome>,
    curation_rx: Receiver<curator::Outcome>,
    curation_in_flight: bool,
    curation_passes: u32,
    last_curation_fingerprint: Option<String>,
    /// When the *agent* last wrote. Separate from `recompute`, which the user's
    /// own typing arms as well.
    last_shadow_change: Option<Tick>,
    /// Hunk `key`s seen mid-transcription — the region a proper prefix of the
    /// proposal at some save. That is proof a human was in the loop, and it is
    /// the only thing rote can honestly infer about how content arrived.
    ///
    /// By `key` rather than `id` because ids churn as the user types near a
    /// hunk, and by content rather than by file because that is what survives.
    /// In memory, and deliberately: a daemon restart degrades one hunk to
    /// `unknown`, which is a designed answer, and persisting it would mean a
    /// fourth on-disk artifact with its own version and session stamps for an
    /// advisory counter that gates nothing.
    ///
    /// Untouched by `retake_baseline` and `disarm`, which is the obvious wrong
    /// guess: those are about file contents and the watchdog, and this is keyed
    /// by content identity.
    typing: HashSet<String>,
}

impl Engine {
    pub fn new(project: ProjectPaths, cfg: Config) -> Self {
        let (curation_tx, curation_rx) = std::sync::mpsc::channel();
        Self {
            project,
            cfg,
            started_at: std::time::Instant::now(),
            baselines: HashMap::new(),
            watchdog: Watchdog::default(),
            recompute: Pending::No,
            last_recompute: None,
            last_published: None,
            drift: false,
            degraded: false,
            started: false,
            curation_tx,
            curation_rx,
            curation_in_flight: false,
            curation_passes: 0,
            last_curation_fingerprint: None,
            last_shadow_change: None,
            typing: HashSet::new(),
        }
    }

    pub fn now(&self) -> Tick {
        Tick(self.started_at.elapsed().as_millis() as u64)
    }

    /// How long the caller should block before calling `step(None, ..)` again.
    pub fn tick(&self) -> Duration {
        if self.degraded {
            DEGRADED_TICK
        } else {
            TICK
        }
    }

    /// Handle one event (or a bare timer tick) and publish what changed.
    ///
    /// A single entry point rather than separate `handle`/`poll` methods so a
    /// test can advance time by passing `None` and get the timers evaluated in
    /// exactly the order the real loop evaluates them.
    pub fn step(&mut self, ev: Option<EngineEvent>, now: Tick) -> Result<Vec<state::Event>> {
        match self.step_inner(ev, now) {
            Ok(out) => Ok(out),
            // A session can vanish *during* a step, not only between them:
            // `rote done` deletes session.json and then storms both watchers
            // with a `git clean`, so a `Manifest::require` deep in the call
            // stack is racing the teardown. Ending is the correct outcome, and
            // reporting it as an engine failure would make a normal close look
            // like a crash in the pane and in the daemon's log.
            Err(e) if session_gone(&self.project) => {
                let _ = e;
                Ok(vec![state::Event::Closed { terminal: None }])
            }
            Err(e) => Err(e),
        }
    }

    fn step_inner(&mut self, ev: Option<EngineEvent>, now: Tick) -> Result<Vec<state::Event>> {
        let mut notices: Vec<Notice> = Vec::new();

        // The session ending is not an error; it is the reason to stop.
        if Manifest::load(&self.project)?.is_none() {
            return Ok(vec![state::Event::Closed { terminal: None }]);
        }

        if !self.started {
            self.started = true;
            self.recompute = Pending::At(now);
        }

        if let Some(ev) = ev {
            match ev {
                EngineEvent::Changed(Origin::Real, rel) => {
                    self.on_real_change(&rel, now, &mut notices)?;
                }
                EngineEvent::Changed(Origin::Shadow, _) => {
                    // The agent worked. Only a full re-diff can say what it did.
                    self.last_shadow_change = Some(now);
                    self.arm_recompute(now);
                }
                EngineEvent::Command(req) => {
                    let reply = req.reply;
                    let outcome = self.apply(&req.request, now, &mut notices)?;
                    if let Some(tx) = reply {
                        let generation = Manifest::load(&self.project)?
                            .map(|m| m.generation)
                            .unwrap_or(0);
                        let _ = tx.send(state::Response {
                            generation,
                            outcome,
                        });
                    }
                }
                EngineEvent::WatchError(msg) => {
                    if !self.degraded {
                        self.degraded = true;
                        notices.push(Notice::warn(format!(
                            "the file watcher failed ({msg}). Falling back to polling every {}s.",
                            DEGRADED_TICK.as_secs()
                        )));
                    }
                }
            }
        } else if self.degraded {
            // No watcher to tell us anything: re-read every file with work in it.
            let files = self.pending_files()?;
            for rel in files {
                self.classify_file(&rel, now, &mut notices)?;
            }
        }

        self.raise_due_questions(now, &mut notices)?;
        self.run_due_recompute(now, &mut notices)?;
        self.run_due_curation(now, &mut notices);

        self.publish(notices)
    }

    /// A change under the user's tree.
    fn on_real_change(
        &mut self,
        rel: &std::path::Path,
        now: Tick,
        notices: &mut Vec<Notice>,
    ) -> Result<()> {
        let rel_str = rel.to_string_lossy().to_string();
        let manifest = Manifest::require(&self.project)?;
        let known = manifest.hunks.iter().any(|h| h.file == rel_str);
        if known {
            // The fast path: pure functions and one file read, no subprocess.
            self.classify_file(&rel_str, now, notices)?;
        } else {
            // A file the queue has never heard of. Only a re-diff can tell
            // whether it is now part of the session.
            self.arm_recompute(now);
        }
        Ok(())
    }

    /// Classify every pending hunk in one file against its baseline.
    fn classify_file(&mut self, rel: &str, now: Tick, notices: &mut Vec<Notice>) -> Result<()> {
        let manifest = Manifest::require(&self.project)?;
        let real = self.project.repo_root.join(rel);
        let after = present::read_lines(&real)?;
        let strict = self.cfg.strict_whitespace;

        let targets: Vec<Hunk> = manifest
            .hunks
            .iter()
            .filter(|h| h.file == rel && h.status == Status::Pending)
            .cloned()
            .collect();
        if targets.is_empty() {
            return Ok(());
        }

        // Absent baseline means this file has not been seen since the last
        // recompute; the current contents are the honest starting point.
        let before = self
            .baselines
            .entry(rel.to_string())
            .or_insert_with(|| after.clone())
            .clone();

        let mut typed_any = false;
        for hunk in &targets {
            if hunk.is_untypeable() {
                // No line-by-line typing for these; the gate is byte equality.
                let shadow = self.project.shadow_dir.join(rel);
                // `false`: a binary or generated file that matched byte for
                // byte was verified, not typed, and claiming otherwise would be
                // the one lie this whole feature exists to avoid.
                if present::classify_by_bytes(&real, &shadow) == Classification::Typed
                    && self.commit_typed(&hunk.id, &hunk.key, false)?
                {
                    typed_any = true;
                }
                continue;
            }

            let outcome = observe(&before, &after, hunk, strict);
            let in_progress = outcome == Observation::InProgress;
            self.watchdog.observe(&hunk.id, &outcome, now);
            match outcome {
                Observation::Typed => {
                    let saw_typing = self.typing.contains(&hunk.key);
                    if self.commit_typed(&hunk.id, &hunk.key, saw_typing)? {
                        typed_any = true;
                        notices.push(Notice::info(format!("typed — {}", hunk.file)));
                    }
                }
                Observation::InProgress | Observation::Untouched => {
                    if in_progress {
                        // Mid-hunk: the region is a proper prefix of the
                        // proposal, which one paste of the whole thing cannot
                        // produce — that lands on `Typed` at the first save.
                        self.typing.insert(hunk.key.clone());
                    }
                    // If a question was open and the text is now on its way to
                    // the proposal, withdraw it rather than making them answer.
                    if hunk.pending_divergence.is_some() {
                        self.withdraw_question(&hunk.id)?;
                    }
                }
                Observation::Disagrees { .. } => {}
            }
        }

        if typed_any {
            // The region now matches the shadow, so the hunk must leave the
            // fresh set and every later anchor in the file has shifted.
            self.retake_baseline(rel)?;

            // Every other hunk in this file was just judged against a baseline
            // that predates the line we accepted, so `before != after` held for
            // reasons that had nothing to do with them — and an untouched hunk
            // whose old text is not a prefix of its proposal reads as a
            // disagreement. Disarm them: typing one hunk correctly must never
            // raise a question about its neighbour. A genuine disagreement
            // re-arms on the next save, now against an honest baseline.
            for hunk in &targets {
                self.watchdog.disarm(&hunk.id);
            }
            self.arm_recompute(now);
        }
        Ok(())
    }

    /// Write a terminal status, but only if the hunk is still what we classified.
    ///
    /// Classification runs unlocked, so between reading the file and writing the
    /// verdict the agent may have reworked this region — which gives it a new id
    /// and a new key. Committing anyway would mark a hunk typed that the user
    /// has never seen.
    /// A miss here writes nothing, so it does not move the generation and does
    /// not invalidate any client's view of the world.
    fn commit_typed(&self, id: &str, key: &str, saw_typing: bool) -> Result<bool> {
        let (out, _) = session::with_session_maybe(&self.project, |m| {
            let Some(h) = m.find_mut(id) else {
                return Ok(None);
            };
            if h.status != Status::Pending || h.key != key {
                return Ok(None);
            }
            h.status = Status::Typed;
            h.pending_divergence = None;
            // Inside the compare-and-swap, not beside it: a verdict written
            // after a rejected commit would land on a hunk the guard above just
            // refused. `fill` rather than `set` — an inference must never
            // overwrite what a front end reported.
            if saw_typing {
                h.input.fill(crate::hunks::Input::Typed);
            }
            m.last_presented = Some(id.to_string());
            Ok(Some(()))
        })?;
        Ok(out.is_some())
    }

    fn withdraw_question(&self, id: &str) -> Result<()> {
        session::with_session_maybe(&self.project, |m| {
            // Only a hunk that actually carries a question is a write.
            match m.find_mut(id) {
                Some(h) if h.pending_divergence.is_some() => {
                    h.pending_divergence = None;
                    Ok(Some(()))
                }
                _ => Ok(None),
            }
        })?;
        Ok(())
    }

    /// Promote candidates that have sat still for the whole grace window.
    fn raise_due_questions(&mut self, now: Tick, notices: &mut Vec<Notice>) -> Result<()> {
        let grace = self.cfg.watch_divergence_grace_ms;
        let due = self.watchdog.due(now, grace);
        for (armed_id, actual) in due {
            let manifest = Manifest::require(&self.project)?;

            // The hunk a question is about can change identity between the
            // disagreement and the moment it becomes due. Once the user's own
            // text is in the tree, a recompute re-diffs that region as
            // (theirs -> proposal), which is a different id — rule 5 drops the
            // entry we armed against and rule 6 appends the new one. Following
            // the content is what keeps the question alive across that; without
            // it the candidate is disarmed as "gone", the baseline has already
            // refreshed to include their text so nothing re-arms, and the
            // disagreement can never be raised again.
            let target = manifest
                .find(&armed_id)
                .filter(|h| h.status == Status::Pending);
            let target = match target {
                Some(h) => Some(h),
                None => manifest
                    .pending()
                    .find(|h| h.old_lines == actual && h.pending_divergence.is_none()),
            };

            let Some(h) = target else {
                self.watchdog.disarm(&armed_id);
                continue;
            };
            if h.pending_divergence.is_some() {
                continue; // already asked
            }
            let id = h.id.clone();
            let proposed = h.new_lines.clone();
            let key = h.key.clone();
            let file = h.file.clone();
            let (asked, _) = session::with_session_maybe(&self.project, |m| {
                let Some(h) = m.find_mut(&id) else {
                    return Ok(None);
                };
                if h.status != Status::Pending || h.key != key {
                    return Ok(None);
                }
                h.pending_divergence = Some(Divergence { proposed, actual });
                Ok(Some(()))
            })?;
            self.watchdog.disarm(&armed_id);
            if asked.is_some() {
                notices.push(Notice::info(format!("your version differs — {file}")));
            }
        }
        Ok(())
    }

    fn arm_recompute(&mut self, now: Tick) {
        let at = Tick(now.0 + self.cfg.watch_debounce_ms);
        // Trailing edge: a burst of saves keeps pushing the deadline out, so a
        // steady stream of keystrokes costs exactly one re-diff at the end.
        self.recompute = Pending::At(at);
    }

    fn run_due_recompute(&mut self, now: Tick, notices: &mut Vec<Notice>) -> Result<()> {
        let due = match self.recompute {
            Pending::At(at) if now >= at => true,
            _ => {
                // Even in silence, sweep occasionally: HEAD can move without
                // any file under either tree changing.
                !matches!(self.last_recompute,
                    Some(last) if now.since(last) < FLOOR_SWEEP.as_millis() as u64)
            }
        };
        if !due {
            return Ok(());
        }
        if let Some(last) = self.last_recompute {
            if now.since(last) < RECOMPUTE_FLOOR.as_millis() as u64 {
                return Ok(()); // too soon; the deadline stays armed
            }
        }

        // The pending keys come back from the same closure rather than a second
        // read: it already holds the manifest under the lock.
        let (report, pending) =
            session::with_session_recomputed(&self.project, &self.cfg, |m, r| {
                Ok((r.clone(), curator::pending_keys(m)))
            })?;
        // A hunk that has left the queue can never be committed again, so its
        // evidence is dead weight. Bounded by the queue, not by the session.
        let live: HashSet<String> = pending.into_iter().collect();
        self.typing.retain(|k| live.contains(k));
        self.recompute = Pending::No;
        self.last_recompute = Some(now);
        self.drift = report.drift;
        for w in report.warnings {
            notices.push(Notice::warn(w));
        }
        self.refresh_baselines()?;
        Ok(())
    }

    /// Take a baseline for every file with work in it that lacks one, and drop
    /// baselines for files that no longer do.
    fn refresh_baselines(&mut self) -> Result<()> {
        let files = self.pending_files()?;
        let wanted: HashSet<&String> = files.iter().collect();
        self.baselines.retain(|k, _| wanted.contains(k));
        for rel in &files {
            if !self.baselines.contains_key(rel) {
                let lines = present::read_lines(&self.project.repo_root.join(rel))?;
                self.baselines.insert(rel.clone(), lines);
            }
        }
        Ok(())
    }

    /// Re-read a file's baseline right now.
    ///
    /// Called the instant a hunk in it reaches a terminal status. Without this
    /// the next hunk in the same file is compared against a snapshot that
    /// predates the one just typed, so `before != after` holds trivially and the
    /// user's very first keystroke on it reads as a disagreement.
    fn retake_baseline(&mut self, rel: &str) -> Result<()> {
        let lines = present::read_lines(&self.project.repo_root.join(rel))?;
        self.baselines.insert(rel.to_string(), lines);
        Ok(())
    }

    fn pending_files(&self) -> Result<Vec<String>> {
        let manifest = Manifest::require(&self.project)?;
        let mut files: Vec<String> = manifest.pending().map(|h| h.file.clone()).collect();
        files.sort();
        files.dedup();
        Ok(files)
    }

    /// Apply a front-end verb.
    pub fn apply(
        &mut self,
        req: &state::Request,
        now: Tick,
        notices: &mut Vec<Notice>,
    ) -> Result<state::Outcome> {
        if req.wire_version != state::WIRE_VERSION {
            return Ok(state::Outcome::Rejected {
                reason: format!(
                    "this daemon speaks wire version {}, not {}",
                    state::WIRE_VERSION,
                    req.wire_version
                ),
            });
        }
        let want = req.generation;

        match &req.command {
            state::Command::Skip { hunk_id } => {
                let (file, generation) = self.mutate(want, |m| {
                    let Some(h) = m.find_mut(hunk_id) else {
                        return Ok(Decision::Reject("no such hunk".into()));
                    };
                    h.status = Status::Skipped;
                    let file = h.file.clone();
                    m.last_presented = Some(hunk_id.clone());
                    Ok(Decision::Apply(file))
                })?;
                match file {
                    Mutated::Applied(file) => {
                        self.watchdog.disarm(hunk_id);
                        self.retake_baseline(&file)?;
                        notices.push(Notice::info(format!("skipped — {file}")));
                        Ok(state::Outcome::Applied)
                    }
                    Mutated::Refused(o) => {
                        let _ = generation;
                        Ok(o)
                    }
                }
            }
            state::Command::Resolve { hunk_id, choice } => {
                let choice = *choice;
                let (file, _) = self.mutate(want, |m| {
                    let Some(h) = m.find_mut(hunk_id) else {
                        return Ok(Decision::Reject("no such hunk".into()));
                    };
                    let Some(d) = h.pending_divergence.take() else {
                        return Ok(Decision::Reject("no open question on that hunk".into()));
                    };
                    match choice {
                        state::Resolution::Keep => {
                            h.status = Status::Diverged;
                            h.divergence = Some(d);
                        }
                        // Retry has no editor to relaunch any more: it withdraws
                        // the question and lets the user keep typing.
                        state::Resolution::Retry => {}
                    }
                    let file = h.file.clone();
                    m.last_presented = Some(hunk_id.clone());
                    Ok(Decision::Apply(file))
                })?;
                match file {
                    Mutated::Applied(file) => {
                        self.watchdog.disarm(hunk_id);
                        self.retake_baseline(&file)?;
                        notices.push(Notice::info(match choice {
                            state::Resolution::Keep => format!("kept your version — {file}"),
                            state::Resolution::Retry => format!("still watching — {file}"),
                        }));
                        Ok(state::Outcome::Applied)
                    }
                    Mutated::Refused(o) => Ok(o),
                }
            }
            state::Command::Show { hunk_id } => {
                let Some(id) = hunk_id.clone() else {
                    return Ok(state::Outcome::Applied);
                };
                let (out, _) = self.mutate(want, |m| {
                    if m.find(&id).is_none() {
                        return Ok(Decision::Reject("no such hunk".into()));
                    }
                    m.last_presented = Some(id.clone());
                    Ok(Decision::Apply(()))
                })?;
                Ok(match out {
                    Mutated::Applied(()) => state::Outcome::Applied,
                    Mutated::Refused(o) => o,
                })
            }
            state::Command::Report { hunk_id, input } => {
                let value = crate::hunks::Input::from(*input);
                let (out, _) = self.mutate(want, |m| {
                    let Some(h) = m.find_mut(hunk_id) else {
                        return Ok(Decision::Reject("no such hunk".into()));
                    };
                    // `input` and nothing else. Not status, not the question,
                    // not `last_presented` — a front end reports how content
                    // arrived, it does not move the queue.
                    if !h.input.set(value) {
                        return Ok(Decision::Nothing);
                    }
                    Ok(Decision::Apply(()))
                })?;
                // No notice: a report is not news. The republished snapshot
                // carries the new value like any other field.
                Ok(match out {
                    Mutated::Applied(()) => state::Outcome::Applied,
                    Mutated::Refused(o) => o,
                })
            }
            state::Command::Refresh => {
                self.recompute = Pending::At(now);
                self.last_recompute = None;
                Ok(state::Outcome::Applied)
            }
        }
    }

    /// Run a mutation with the staleness check **inside** the lock.
    ///
    /// The check has to happen in the same read-modify-write cycle as the write
    /// itself. Comparing the generation in the handler and then locking to write
    /// is a TOCTOU, which would make the whole mechanism decorative — it is the
    /// exact shape `with_session`'s doc comment exists to forbid.
    fn mutate<T>(
        &self,
        want: Option<u64>,
        f: impl FnOnce(&mut Manifest) -> Result<Decision<T>>,
    ) -> Result<(Mutated<T>, u64)> {
        let mut refusal: Option<state::Outcome> = None;
        let (applied, generation) = session::with_session_maybe(&self.project, |m| {
            if let Some(g) = want {
                if m.generation != g {
                    refusal = Some(state::Outcome::Stale {
                        current: m.generation,
                    });
                    return Ok(None);
                }
            }
            match f(m)? {
                Decision::Apply(v) => Ok(Some(v)),
                Decision::Reject(reason) => {
                    refusal = Some(state::Outcome::Rejected { reason });
                    Ok(None)
                }
                // `refusal` stays unset, which the tail below already maps to
                // `Applied`.
                Decision::Nothing => Ok(None),
            }
        })?;
        Ok(match applied {
            Some(v) => (Mutated::Applied(v), generation),
            None => (
                Mutated::Refused(refusal.unwrap_or(state::Outcome::Applied)),
                generation,
            ),
        })
    }

    // ------------------------------------------------------------- curation

    /// Absorb any finished pass, then consider starting one.
    ///
    /// Returns nothing and propagates nothing. An `Err` from here would reach
    /// `Engine::run`, which reports "the engine stopped" and takes the daemon
    /// with it — so a curator that cannot write its cache would kill the thing
    /// that watches you type. Every failure is a notice instead. That is not
    /// defensiveness; it is the whole contract of an advisory pass.
    fn run_due_curation(&mut self, now: Tick, notices: &mut Vec<Notice>) {
        // Absorb first, and unconditionally: a result must land even when the
        // conditions that started it no longer hold.
        while let Ok(outcome) = self.curation_rx.try_recv() {
            self.curation_in_flight = false;
            if let Some(e) = &outcome.error {
                notices.push(Notice::warn(format!(
                    "the curator did not run ({e}). Keeping the file order."
                )));
            }
            match self.absorb_curation(&outcome) {
                Ok(true) => notices.push(Notice::info(curated_line(&outcome))),
                Ok(false) => {}
                Err(e) => notices.push(Notice::warn(format!(
                    "the curation could not be recorded ({e:#}). Keeping the file order."
                ))),
            }
        }

        if let Err(e) = self.start_curation(now, notices) {
            notices.push(Notice::warn(format!("the curator did not start ({e:#}).")));
        }
    }

    fn start_curation(&mut self, now: Tick, notices: &mut Vec<Notice>) -> Result<()> {
        // Quiet means the agent has stopped *and* no re-diff is owed, so the
        // pending set is not about to change under the pass. `last_recompute`
        // being set is what proves a queue was ever built.
        let quiet = self.recompute == Pending::No
            && self.last_recompute.is_some()
            && match self.last_shadow_change {
                // Nothing from the agent since this engine started, which is the
                // ordinary `rote start` case: the work landed before the daemon
                // did, and there is nothing to wait for.
                None => true,
                Some(t) => now.since(t) >= curator::CURATOR_SETTLE_MS,
            };

        let manifest = Manifest::require(&self.project)?;
        let Some(pass) = curator::start(
            &self.project,
            &manifest,
            &curator::Trigger {
                enabled: self.cfg.curator_enabled,
                in_flight: self.curation_in_flight,
                quiet,
                passes: self.curation_passes,
                last_fingerprint: self.last_curation_fingerprint.as_deref(),
            },
        ) else {
            return Ok(());
        };

        let curator::Pass {
            fingerprint,
            task,
            candidates,
        } = pass;
        let cfg = self.cfg.clone();
        let tx = self.curation_tx.clone();
        std::thread::spawn(move || {
            let _ = tx.send(curator::run_pass(&cfg, &task, candidates));
        });

        // Recorded at spawn rather than when the outcome lands. That covers the
        // in-flight window, and — the case that actually bites — it covers a
        // cache write that fails: without it, a full or read-only state dir
        // would have the curator retry forever.
        self.curation_in_flight = true;
        self.curation_passes += 1;
        self.last_curation_fingerprint = Some(fingerprint);
        notices.push(Notice::info(
            "curating — putting the queue in teaching order",
        ));
        Ok(())
    }

    /// Take a finished pass, if there is still a session to take it into.
    ///
    /// The guard is the engine's business rather than the curator's: a manifest
    /// archived while the model was thinking means dropping the outcome early
    /// avoids a pointless warning during teardown. The session stamp would have
    /// made a stray write inert anyway.
    fn absorb_curation(&self, outcome: &curator::Outcome) -> Result<bool> {
        if Manifest::load(&self.project)?.is_none() {
            return Ok(false);
        }
        curator::absorb(&self.project, outcome, &|m| self.pinned_key(m))
    }

    /// The hunk that must keep the head, if any.
    ///
    /// "Freeze current, reorder ahead": a curation landing mid-session must not
    /// move what you are looking at. But only once you are *in* it — with
    /// nothing typed and nothing started, there is no place to lose, and the
    /// curator's opinion about what to do first is the entire point of asking.
    ///
    /// The touched test goes through `observe` rather than `is_in_progress`,
    /// which returns true for an empty region ("the state before the first
    /// keystroke") and so cannot tell started from not-started. `observe`
    /// compares the region against the baseline first and answers `Untouched`.
    fn pinned_key(&self, m: &Manifest) -> Option<String> {
        let head = m.head_of_queue()?;
        let engaged = m.hunks.iter().any(|h| h.status != Status::Pending)
            || head.pending_divergence.is_some()
            || self.head_is_touched(head);
        engaged.then(|| head.key.clone())
    }

    fn head_is_touched(&self, head: &Hunk) -> bool {
        let Some(before) = self.baselines.get(&head.file) else {
            return false;
        };
        let Ok(after) = present::read_lines(&self.project.repo_root.join(&head.file)) else {
            return false;
        };
        observe(before, &after, head, self.cfg.strict_whitespace) != Observation::Untouched
    }

    /// Build a snapshot, emitting one only when something actually moved.
    fn publish(&mut self, notices: Vec<Notice>) -> Result<Vec<state::Event>> {
        let manifest = Manifest::require(&self.project)?;
        let changed = self.last_published != Some(manifest.generation);
        if !changed && notices.is_empty() {
            return Ok(vec![state::Event::Heartbeat {
                generation: manifest.generation,
            }]);
        }
        self.last_published = Some(manifest.generation);

        let anchor = manifest.active().map(|h| {
            let l = present::locate(&self.project.repo_root, h);
            (
                l.anchor.line,
                l.anchor.via,
                l.real_path.to_string_lossy().into_owned(),
            )
        });
        let snap = state::Snapshot::build(&manifest, anchor, self.drift, notices);
        Ok(vec![state::Event::Snapshot(Box::new(snap))])
    }

    /// Block on the channel, stepping until it closes or the session ends.
    ///
    /// Concrete `Receiver` rather than an injected trait: the tests drive `step`
    /// directly, which is strictly more precise than driving a fake source, and
    /// a trait whose only implementation is the real one is a trait that only
    /// costs indirection.
    pub fn run(
        &mut self,
        rx: &std::sync::mpsc::Receiver<EngineEvent>,
        mut sink: impl FnMut(state::Event) -> Result<bool>,
    ) -> Result<()> {
        loop {
            let ev = match rx.recv_timeout(self.tick()) {
                Ok(ev) => Some(ev),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => None,
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return Ok(()),
            };
            let now = self.now();
            let out = self
                .step(ev, now)
                .context("the watch engine failed while handling an event")?;
            for e in out {
                let closed = matches!(e, state::Event::Closed { .. });
                if !sink(e)? || closed {
                    return Ok(());
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hunks::Op;

    fn lines(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    fn hunk(old: &[&str], new: &[&str], after: &[&str]) -> Hunk {
        let mut h = Hunk {
            id: "h-one".into(),
            key: "k-one".into(),
            file: "a.rs".into(),
            op: Op::Replace,
            context_before: lines(&["fn a() {"]),
            old_lines: lines(old),
            new_lines: lines(new),
            context_after: lines(after),
            anchor_hint: 2,
            status: Status::Pending,
            divergence: None,
            note: None,
            pending_divergence: None,
            curator_note: None,
            curator_rank: None,
            input: Default::default(),
        };
        h.key = Hunk::compute_key(&h.file, &h.old_lines, &h.new_lines);
        h
    }

    // -------------------------------------------------------- observe

    #[test]
    fn a_half_typed_hunk_is_in_progress_not_a_disagreement() {
        // The whole reason the engine exists in this shape. `classify` calls
        // this Diverged, and a watcher built on that alone would interrupt the
        // user on every keystroke-flush.
        let h = hunk(&["    one();"], &["    two();"], &["}"]);
        let before = lines(&["fn a() {", "    one();", "}"]);

        for partial in ["", "    ", "    tw"] {
            let after = lines(&["fn a() {", partial, "}"]);
            assert_eq!(
                observe(&before, &after, &h, false),
                Observation::InProgress,
                "{partial:?} is on its way to the proposal"
            );
        }

        let done = lines(&["fn a() {", "    two();", "}"]);
        assert_eq!(observe(&before, &done, &h, false), Observation::Typed);
    }

    #[test]
    fn a_finished_but_different_line_disagrees() {
        let h = hunk(&["    one();"], &["    two();"], &["}"]);
        let before = lines(&["fn a() {", "    one();", "}"]);
        let mine = lines(&["fn a() {", "    my_own_thing();", "}"]);
        assert_eq!(
            observe(&before, &mine, &h, false),
            Observation::Disagrees {
                actual: lines(&["    my_own_thing();"])
            }
        );
    }

    // -------------------------------------------------------- watchdog

    #[test]
    fn a_long_pause_while_still_a_prefix_never_asks() {
        // Nothing is ever armed for an in-progress region, so no amount of
        // thinking time produces a question.
        let mut w = Watchdog::default();
        for t in [0, 1_000, 60_000, 3_600_000] {
            w.observe("h-one", &Observation::InProgress, Tick(t));
            assert!(w.due(Tick(t), 2000).is_empty(), "at {t}ms");
        }
    }

    #[test]
    fn a_disagreement_becomes_due_only_after_the_window() {
        let mut w = Watchdog::default();
        let d = Observation::Disagrees {
            actual: lines(&["mine"]),
        };
        w.observe("h-one", &d, Tick(1_000));

        assert!(
            w.due(Tick(1_500), 2000).is_empty(),
            "still inside the window"
        );
        assert!(w.due(Tick(2_999), 2000).is_empty(), "one ms short");
        let due = w.due(Tick(3_000), 2000);
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].0, "h-one");
        assert_eq!(due[0].1, lines(&["mine"]));
    }

    #[test]
    fn a_save_inside_the_window_rearms_it() {
        // The window measures stillness, not time since the first mistake.
        let mut w = Watchdog::default();
        let d = Observation::Disagrees {
            actual: lines(&["mine"]),
        };
        w.observe("h-one", &d, Tick(0));
        w.observe("h-one", &d, Tick(1_500));
        assert!(w.due(Tick(2_000), 2000).is_empty(), "re-armed at 1500");
        assert!(!w.due(Tick(3_500), 2000).is_empty(), "quiet since 1500");
    }

    #[test]
    fn fixing_the_text_withdraws_the_candidate() {
        let mut w = Watchdog::default();
        w.observe(
            "h-one",
            &Observation::Disagrees {
                actual: lines(&["wrong"]),
            },
            Tick(0),
        );
        assert!(w.is_armed("h-one"));
        w.observe("h-one", &Observation::InProgress, Tick(100));
        assert!(!w.is_armed("h-one"), "back on track, nothing to ask");
        assert!(w.due(Tick(10_000), 2000).is_empty());
    }

    #[test]
    fn a_zero_window_still_suppresses_an_in_progress_prefix() {
        // grace = 0 means "ask at the first quiet moment", not "ask always" —
        // layer one is what makes zero a safe setting.
        let mut w = Watchdog::default();
        w.observe("h-one", &Observation::InProgress, Tick(0));
        assert!(w.due(Tick(0), 0).is_empty());

        w.observe(
            "h-one",
            &Observation::Disagrees {
                actual: lines(&["mine"]),
            },
            Tick(0),
        );
        assert_eq!(
            w.due(Tick(0), 0).len(),
            1,
            "a real disagreement asks at once"
        );
    }

    #[test]
    fn typed_clears_any_armed_candidate() {
        let mut w = Watchdog::default();
        w.observe(
            "h-one",
            &Observation::Disagrees {
                actual: lines(&["wrong"]),
            },
            Tick(0),
        );
        w.observe("h-one", &Observation::Typed, Tick(10));
        assert!(!w.is_armed("h-one"));
    }

    #[test]
    fn questions_come_out_in_a_deterministic_order() {
        // They become manifest writes, and a HashMap's order is not an order.
        let mut w = Watchdog::default();
        let d = Observation::Disagrees {
            actual: lines(&["x"]),
        };
        for id in ["h-c", "h-a", "h-b"] {
            w.observe(id, &d, Tick(0));
        }
        let ids: Vec<String> = w
            .due(Tick(5_000), 100)
            .into_iter()
            .map(|(i, _)| i)
            .collect();
        assert_eq!(ids, vec!["h-a", "h-b", "h-c"]);
    }
}
