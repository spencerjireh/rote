//! `rote watch`: the reading surface.
//!
//! A full-screen pane that redraws the current hunk as the engine reclassifies
//! it. This is the half of the tool the old design got structurally wrong —
//! `rote next` printed a hunk and then launched an editor over the top of it,
//! so the proposal was erased at the exact moment you could start typing it.
//! Here the pane and the editor are different windows and neither moves.
//!
//! Rendering is a pure function of a `state::Snapshot`, so the pane is already
//! shaped like the protocol client it becomes in Stage 3: it reads no files and
//! consults no manifest. Everything it draws arrived in one value.
//!
//! Terminal handling is deliberately small. Plain ANSI, one `write_all` of the
//! whole frame, no cursor tracking, no resize handling — the frame is a fixed
//! width, so a resize costs nothing to ignore.

use crate::config::Config;
use crate::engine::{Engine, EngineEvent, RealClock};
use crate::paths::{Lock, ProjectPaths};
use crate::present::{self, Anchor};
use crate::state::{self, Snapshot};
use crate::watcher;
use anyhow::{Context, Result};
use std::io::{IsTerminal, Read, Write};
use std::time::{Duration, Instant};

/// The alternate screen is what restores the user's scrollback on exit.
const ALT_ENTER: &str = "\x1b[?1049h";
const ALT_LEAVE: &str = "\x1b[?1049l";
const CURSOR_HIDE: &str = "\x1b[?25l";
const CURSOR_SHOW: &str = "\x1b[?25h";
const HOME_CLEAR: &str = "\x1b[H\x1b[2J";

/// How long a raw-mode read waits before returning empty, in tenths of a second.
/// Doubles as the pane's event-loop tick.
const READ_TENTHS: u8 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    Skip,
    Keep,
    Retry,
    Open,
    Refresh,
    Quit,
    Ignored,
}

/// Single keypresses, because every action here is one.
pub fn decode(b: u8) -> Key {
    match b {
        b's' => Key::Skip,
        b'k' => Key::Keep,
        b'r' => Key::Retry,
        b'o' => Key::Open,
        b'g' => Key::Refresh,
        b'q' => Key::Quit,
        // Ctrl-C. `ISIG` is cleared in raw mode, so this arrives as a byte
        // rather than a signal — which is the point: the default SIGINT
        // handler would kill the process with the terminal still raw.
        0x03 => Key::Quit,
        _ => Key::Ignored,
    }
}

/// Restores the terminal however the pane exits.
struct RawMode {
    fd: i32,
    saved: libc::termios,
}

impl RawMode {
    /// `Ok(None)` when stdin is not a terminal — a pipe needs no raw mode and
    /// must not have one.
    fn enable() -> Result<Option<Self>> {
        if !std::io::stdin().is_terminal() {
            return Ok(None);
        }
        let fd = libc::STDIN_FILENO;
        // Safety: `termios` is a plain C struct and `tcgetattr` fills it.
        let mut saved: libc::termios = unsafe { std::mem::zeroed() };
        if unsafe { libc::tcgetattr(fd, &mut saved) } != 0 {
            anyhow::bail!("cannot read the terminal settings: {}", last_error());
        }
        let mut raw = saved;
        // ICANON: deliver bytes without waiting for a newline.
        // ECHO: the pane draws the screen; the shell must not also print keys.
        // ISIG: see `decode` — Ctrl-C must reach us, not the default handler.
        raw.c_lflag &= !(libc::ICANON | libc::ECHO | libc::ISIG);
        // A read returns after at most READ_TENTHS/10 seconds with whatever is
        // there, including nothing. That timeout is the event loop's tick, and
        // it is why the pane needs no reader thread competing for stdin — which
        // matters because the `open` action hands stdin to an editor.
        raw.c_cc[libc::VMIN] = 0;
        raw.c_cc[libc::VTIME] = READ_TENTHS;
        if unsafe { libc::tcsetattr(fd, libc::TCSANOW, &raw) } != 0 {
            anyhow::bail!("cannot configure the terminal: {}", last_error());
        }
        Ok(Some(Self { fd, saved }))
    }

    /// Put the terminal back for the duration of `f` — the editor needs a
    /// terminal it can drive itself.
    fn suspended<T>(&self, f: impl FnOnce() -> T) -> T {
        unsafe { libc::tcsetattr(self.fd, libc::TCSANOW, &self.saved) };
        let out = f();
        let mut raw = self.saved;
        raw.c_lflag &= !(libc::ICANON | libc::ECHO | libc::ISIG);
        raw.c_cc[libc::VMIN] = 0;
        raw.c_cc[libc::VTIME] = READ_TENTHS;
        unsafe { libc::tcsetattr(self.fd, libc::TCSANOW, &raw) };
        out
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        unsafe { libc::tcsetattr(self.fd, libc::TCSANOW, &self.saved) };
    }
}

fn last_error() -> String {
    std::io::Error::last_os_error().to_string()
}

/// Owns the alternate screen, so an early return still hands the terminal back.
struct Screen {
    active: bool,
}

impl Screen {
    fn enter(active: bool) -> Self {
        if active {
            let mut out = std::io::stdout().lock();
            let _ = out.write_all(format!("{ALT_ENTER}{CURSOR_HIDE}").as_bytes());
            let _ = out.flush();
        }
        Self { active }
    }

    fn draw(&self, frame: &str) {
        let mut out = std::io::stdout().lock();
        if self.active {
            // One write for the whole frame: no tearing, and no cursor
            // arithmetic to get wrong.
            let _ = out.write_all(format!("{HOME_CLEAR}{frame}").as_bytes());
        } else {
            let _ = out.write_all(frame.as_bytes());
        }
        let _ = out.flush();
    }

    fn suspended<T>(&self, f: impl FnOnce() -> T) -> T {
        if self.active {
            let mut out = std::io::stdout().lock();
            let _ = out.write_all(format!("{CURSOR_SHOW}{ALT_LEAVE}").as_bytes());
            let _ = out.flush();
        }
        let out = f();
        if self.active {
            let mut o = std::io::stdout().lock();
            let _ = o.write_all(format!("{ALT_ENTER}{CURSOR_HIDE}").as_bytes());
            let _ = o.flush();
        }
        out
    }
}

impl Drop for Screen {
    fn drop(&mut self) {
        if self.active {
            let mut out = std::io::stdout().lock();
            let _ = out.write_all(format!("{CURSOR_SHOW}{ALT_LEAVE}").as_bytes());
            let _ = out.flush();
        }
    }
}

/// The whole pane, as a string. Pure: this is the Stage 3 client already.
pub fn render_frame(snap: &Snapshot, color: bool) -> String {
    let mut out = String::new();
    out.push_str(&format!("rote · {}\n", snap.task));
    if snap.drift {
        out.push_str(
            "the repository moved under this session — hunks are recomputed as it is now\n",
        );
    }
    out.push('\n');

    match &snap.active {
        None => {
            out.push_str("nothing to transcribe.\n");
            if snap.counts.total > 0 {
                out.push_str("every hunk is accounted for — `rote done` closes the session.\n");
            } else {
                out.push_str("waiting for the agent's work to land.\n");
            }
        }
        Some(p) => {
            let anchor = Anchor {
                line: p.anchor_line,
                via: p.anchor_via,
            };
            if let Some(note) = &p.hunk.curator_note {
                out.push_str(&format!("{note}\n"));
            }
            out.push_str(&present::render(
                &p.hunk, p.position, p.total, &anchor, color,
            ));
            // The jump target, on its own line and in the conventional form, so
            // a terminal that linkifies `file:line` can act on it.
            out.push_str(&format!("{}:{}\n", p.hunk.file, p.anchor_line));
            if let Some(d) = &p.hunk.pending_divergence {
                out.push('\n');
                out.push_str(&present::render_divergence(&d.proposed, &d.actual, color));
            }
        }
    }

    out.push('\n');
    let c = &snap.counts;
    out.push_str(&format!(
        "{} pending · {} typed · {} diverged · {} skipped\n",
        c.pending, c.typed, c.diverged, c.skipped
    ));
    for n in &snap.notices {
        out.push_str(&format!("{}\n", n.text));
    }

    out.push_str(&footer(snap));
    out
}

fn footer(snap: &Snapshot) -> String {
    let asking = snap
        .active
        .as_ref()
        .map(|p| p.hunk.pending_divergence.is_some())
        .unwrap_or(false);
    if asking {
        // Keep and retry are offered only when there is something to answer;
        // a footer that always lists them teaches the wrong model of the tool.
        "[k]eep mine  [r]etry  [s]kip  [o]pen  [q]uit\n".to_string()
    } else {
        "[s]kip  [o]pen  [g] refresh  [q]uit\n".to_string()
    }
}

/// How `rote watch` was asked to behave.
#[derive(Debug, Clone, Copy, Default)]
pub struct Options {
    /// Never take the terminal; print frames as they change. Inferred when
    /// stdin is not a tty.
    pub headless: bool,
    /// Exit once the queue is empty. For tests and scripts.
    pub exit_when_empty: bool,
    /// Give up after this long. A wedged watcher should fail a test, not hang
    /// the machine that is running it.
    pub timeout_ms: Option<u64>,
}

/// Run the pane until the session closes, the user quits, or a limit is hit.
pub fn run(project: &ProjectPaths, cfg: &Config, opts: Options) -> Result<()> {
    // One pane per project. Two would fight over the same queue and each would
    // see the other's writes as the world moving underneath it.
    let watch_lock = Lock::try_acquire(&project.watch_lock_path())?;
    if watch_lock.is_none() {
        anyhow::bail!(
            "another `rote watch` is already running on this project.\n\
             Close it, or if it has crashed, remove {}.",
            project.watch_lock_path().display()
        );
    }

    // Keys are read inline on this thread rather than fed in here, so that the
    // `open` action can hand stdin to an editor without a reader stealing it.
    let (tx, rx) = std::sync::mpsc::channel::<EngineEvent>();
    let _watch = watcher::spawn(project, cfg, move |ev| tx.send(ev).is_ok())?;

    let raw = if opts.headless {
        None
    } else {
        RawMode::enable()?
    };
    let interactive = raw.is_some();
    let screen = Screen::enter(interactive);

    let mut engine = Engine::new(project.clone(), cfg.clone(), Box::new(RealClock::default()));
    let deadline = opts
        .timeout_ms
        .map(|ms| Instant::now() + Duration::from_millis(ms));

    let mut snapshot: Option<Snapshot> = None;
    let mut pending: Option<EngineEvent> = None;
    let mut last_frame = String::new();

    loop {
        let now = engine.now();
        for ev in engine.step(pending.take(), now)? {
            match ev {
                state::Event::Snapshot(s) => snapshot = Some(*s),
                state::Event::Closed { .. } => {
                    screen.draw("the session is closed.\n");
                    return Ok(());
                }
                state::Event::Notice(_) | state::Event::Heartbeat { .. } => {}
            }
        }

        if let Some(snap) = &snapshot {
            let frame = render_frame(snap, cfg.color && interactive);
            // Redraw only on change: a quiet watcher should not repaint forever,
            // and in headless mode a repeat would be noise in the output.
            if frame != last_frame {
                screen.draw(&frame);
                last_frame = frame;
            }
            if opts.exit_when_empty && snap.counts.pending == 0 && snap.counts.total > 0 {
                return Ok(());
            }
        }

        if let Some(d) = deadline {
            if Instant::now() >= d {
                anyhow::bail!(
                    "`rote watch` timed out after {}ms",
                    opts.timeout_ms.unwrap()
                );
            }
        }

        // Reading stdin on this thread rather than a reader thread is what lets
        // the `open` action hand the terminal to an editor without a second
        // reader stealing its input.
        if let Some(raw) = &raw {
            if let Some(key) = read_key()? {
                match key {
                    Key::Quit => return Ok(()),
                    Key::Open => {
                        if let Some(p) = snapshot.as_ref().and_then(|s| s.active.as_ref()) {
                            let editor = present::editor_command(&cfg.editor);
                            let path = std::path::PathBuf::from(&p.real_path);
                            let line = p.anchor_line;
                            let is_new = p.hunk.op == crate::hunks::Op::CreateFile;
                            // Events queue up while the editor has the terminal,
                            // so the user comes back to a hunk that has already
                            // been classified.
                            let r = screen.suspended(|| {
                                raw.suspended(|| {
                                    present::launch_editor(&editor, &path, line, is_new)
                                })
                            });
                            // The exit status is meaningless now: rote is not
                            // waiting on the editor to decide anything.
                            r?;
                            last_frame.clear(); // force a repaint
                        }
                    }
                    other => {
                        if let Some(cmd) = command_for(other, snapshot.as_ref()) {
                            pending = Some(EngineEvent::Command(cmd));
                            continue;
                        }
                    }
                }
            }
        }

        match rx.try_recv() {
            Ok(ev) => pending = Some(ev),
            Err(std::sync::mpsc::TryRecvError::Empty) => {
                if !interactive {
                    // Without a raw-mode read to pace the loop, block here.
                    match rx.recv_timeout(engine.tick()) {
                        Ok(ev) => pending = Some(ev),
                        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return Ok(()),
                    }
                }
            }
            Err(std::sync::mpsc::TryRecvError::Disconnected) => return Ok(()),
        }
    }
}

/// Which verb a key means, given what is on screen.
fn command_for(key: Key, snap: Option<&Snapshot>) -> Option<state::Command> {
    let active = snap?.active.as_ref()?;
    let id = active.hunk.id.clone();
    match key {
        Key::Skip => Some(state::Command::Skip { hunk_id: id }),
        // Answering is only meaningful while a question is open; otherwise the
        // key is a stray press and doing nothing is the correct response.
        Key::Keep if active.hunk.pending_divergence.is_some() => Some(state::Command::Resolve {
            hunk_id: id,
            choice: state::Resolution::Keep,
        }),
        Key::Retry if active.hunk.pending_divergence.is_some() => Some(state::Command::Resolve {
            hunk_id: id,
            choice: state::Resolution::Retry,
        }),
        Key::Refresh => Some(state::Command::Refresh),
        _ => None,
    }
}

/// One byte from stdin, or `None` if the read timed out. Raw mode's `VTIME` is
/// what bounds this.
fn read_key() -> Result<Option<Key>> {
    let mut buf = [0u8; 1];
    match std::io::stdin().read(&mut buf) {
        Ok(0) => Ok(None),
        Ok(_) => Ok(Some(decode(buf[0]))),
        Err(e) if e.kind() == std::io::ErrorKind::Interrupted => Ok(None),
        Err(e) => Err(e).context("cannot read from the terminal"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hunks::{Divergence, Hunk, Op, Status};
    use crate::present::AnchorVia;
    use crate::session::State;
    use crate::state::{Counts, Presented, QueueItem};

    fn hunk() -> Hunk {
        Hunk {
            id: "h-one".into(),
            key: "k-one".into(),
            file: "src/posts.py".into(),
            op: Op::Replace,
            context_before: vec!["class Post:".into()],
            old_lines: vec!["    tags = Manager()".into()],
            new_lines: vec!["    tags = TagManager()".into()],
            context_after: vec![],
            anchor_hint: 48,
            status: Status::Pending,
            divergence: None,
            note: None,
            pending_divergence: None,
            curator_note: None,
            curator_rank: None,
            input: Default::default(),
        }
    }

    fn snap(active: Option<Hunk>) -> Snapshot {
        let queue = active
            .iter()
            .map(|h| QueueItem {
                id: h.id.clone(),
                key: h.key.clone(),
                file: h.file.clone(),
                op: h.op,
                anchor_hint: h.anchor_hint,
                has_question: h.pending_divergence.is_some(),
                curator_note: None,
            })
            .collect();
        Snapshot {
            wire_version: crate::state::WIRE_VERSION,
            generation: 1,
            state: State::Transcribing,
            task: "add tagging".into(),
            drift: false,
            counts: Counts {
                pending: active.iter().count(),
                typed: 6,
                diverged: 1,
                skipped: 0,
                total: 8,
            },
            active: active.map(|h| Presented {
                hunk: h,
                position: 3,
                total: 11,
                anchor_line: 48,
                anchor_via: AnchorVia::ContextBefore,
                real_path: "/repo/src/posts.py".into(),
            }),
            queue,
            notices: vec![],
        }
    }

    #[test]
    fn a_frame_shows_the_hunk_and_a_jump_target() {
        let f = render_frame(&snap(Some(hunk())), false);
        assert!(f.contains("hunk 3/11"), "{f}");
        assert!(f.contains("-     tags = Manager()"), "{f}");
        assert!(f.contains("+     tags = TagManager()"), "{f}");
        // The jump target is the whole interim answer to "how do I get there".
        assert!(f.contains("src/posts.py:48"), "{f}");
        assert!(f.contains("6 typed"), "{f}");
    }

    #[test]
    fn an_empty_queue_says_so_rather_than_drawing_nothing() {
        let f = render_frame(&snap(None), false);
        assert!(f.contains("nothing to transcribe"), "{f}");
        assert!(f.contains("rote done"), "{f}");
    }

    #[test]
    fn keep_and_retry_are_offered_only_while_a_question_is_open() {
        let quiet = render_frame(&snap(Some(hunk())), false);
        assert!(!quiet.contains("[k]eep"), "no question, no answer: {quiet}");

        let mut asked = hunk();
        asked.pending_divergence = Some(Divergence {
            proposed: vec!["    tags = TagManager()".into()],
            actual: vec!["    tags = TaggableManager()".into()],
        });
        let f = render_frame(&snap(Some(asked)), false);
        assert!(f.contains("[k]eep mine"), "{f}");
        assert!(f.contains("[r]etry"), "{f}");
        assert!(f.contains("your version differs"), "{f}");
        assert!(f.contains("TaggableManager"), "{f}");
    }

    #[test]
    fn a_frame_carries_no_escape_codes_without_color() {
        let f = render_frame(&snap(Some(hunk())), false);
        assert!(
            !f.contains('\u{1b}'),
            "escape codes leaked into a plain frame"
        );
    }

    #[test]
    fn ctrl_c_decodes_as_quit() {
        // ISIG is cleared, so this arrives as a byte. If it were not handled the
        // pane would ignore Ctrl-C entirely and the terminal would feel stuck.
        assert_eq!(decode(0x03), Key::Quit);
        assert_eq!(decode(b'q'), Key::Quit);
        assert_eq!(decode(b's'), Key::Skip);
        assert_eq!(decode(b'z'), Key::Ignored);
    }

    #[test]
    fn answering_keys_do_nothing_when_there_is_nothing_to_answer() {
        let s = snap(Some(hunk()));
        assert_eq!(command_for(Key::Keep, Some(&s)), None);
        assert_eq!(command_for(Key::Retry, Some(&s)), None);
        assert!(matches!(
            command_for(Key::Skip, Some(&s)),
            Some(state::Command::Skip { .. })
        ));
        // And nothing at all is safe when the queue is empty.
        let empty = snap(None);
        assert_eq!(command_for(Key::Skip, Some(&empty)), None);
    }
}
