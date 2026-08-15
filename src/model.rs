//! One headless `claude -p` call, written once.
//!
//! rote invokes a model in three places — the reviewer at `rote done`, the
//! curator behind the queue, and `rote doctor --deep` — and every one of them
//! wants the same thing: text in, text out, no filesystem, and a hard ceiling on
//! how long it may take. This module is that call, and nothing else in the tree
//! spawns `claude` for an answer.
//!
//! The reason it is a module rather than a helper is the deadlock. The obvious
//! implementation writes the payload to the child's stdin and then reads its
//! output, and that hangs forever the moment both pipes fill: the parent is
//! blocked in `write_all` because the child is not reading, and the child is
//! blocked in `write` because the parent is not reading. Pipe buffers are small
//! — 16 KiB on macOS, 64 KiB on Linux — so a session diff or a queue of hunks
//! clears the threshold routinely. It is not a corner case; it is the ordinary
//! payload on an ordinary day.

use anyhow::{Context, Result};
use std::io::{Read, Write};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::mpsc::{channel, Sender};
use std::time::{Duration, Instant};

/// Tool-restriction flags for every headless invocation.
///
/// This is the one place rote touches Claude Code's flag surface, so it lives in
/// a single named constant and is expected to need updating across CLI versions.
/// It is what makes ARCHITECTURE.md's "never gets filesystem access" true: an
/// empty cwd alone would not stop `claude -p` reading whatever it liked.
///
/// Principle 1 is unaffected — that governs the *session* agent, which rote
/// launches completely unconfigured.
pub const TOOL_FLAGS: &[&str] = &["--tools", ""];

/// Poll interval while waiting for the child to exit.
const POLL: Duration = Duration::from_millis(25);

/// How long to wait for the reader threads once the child is gone.
///
/// Reaching this means a pipe is still held open by something that outlived the
/// child, and the bytes are not coming. See `run` for why that is possible.
const DRAIN_CEILING: Duration = Duration::from_secs(2);

/// What to ask, and how long to allow for it.
///
/// A struct rather than five parameters: clippy's `too_many_arguments` is right
/// here, and a call site reads better naming its fields than counting commas.
pub struct Invocation<'a> {
    /// The command, as configured. `claude_cmd[0]` is the program.
    pub claude_cmd: &'a [String],
    /// The instruction, passed as the argument to `-p`.
    pub prompt: &'a str,
    /// Appended after the tool flags — `model_args` from the config.
    pub extra_args: &'a [String],
    pub timeout: Duration,
}

/// What came back. A non-zero `status` is reported, never swallowed.
#[derive(Debug)]
pub struct Completed {
    pub status: ExitStatus,
    pub stdout: String,
    pub stderr: String,
}

impl Completed {
    /// The stderr text, trimmed, for an error message.
    pub fn complaint(&self) -> String {
        self.stderr.trim().to_string()
    }
}

/// Which pipe a chunk came from. Private: the caller sees two named fields.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stream {
    Out,
    Err,
}

/// Run the model, and come back.
///
/// Three threads and a poll loop, and each of them is load-bearing:
///
/// - **stdin gets its own thread** so a full pipe blocks that thread and not
///   this one. This is the deadlock, and it is the whole reason the function
///   exists.
/// - **stdout and stderr each get a thread**, because draining only one of them
///   deadlocks on the other. `wait_with_output` does this correctly, but only
///   when it is called *before* the child exits — the previous implementation
///   polled `try_wait` first and so called it after, which cannot help a child
///   that is blocked writing and therefore never exits.
/// - **this thread polls `try_wait`** rather than reading, so the deadline is
///   armed for the whole call rather than only after the payload has landed.
///   Killing the child is what unblocks the other three: it closes the pipe
///   ends, driving the readers to EOF and the writer to `EPIPE`.
///
/// The results are collected over a channel rather than by joining the reader
/// threads. If `claude` spawns a grandchild that inherits the pipe, the pipe
/// stays open after `claude` itself exits, `read_to_end` does not return, and an
/// unconditional `join` would hang this thread forever — reintroducing exactly
/// the failure this function exists to remove, one level down. `recv_timeout`
/// bounds the wait and orphans the thread instead, the same trade
/// `daemon::join_within` makes.
pub fn run(inv: &Invocation, payload: &str) -> Result<Completed> {
    // An empty temp cwd: the model runs in neither tree. Held for the whole
    // call, so it outlives the child.
    let scratch = tempfile::tempdir().context("cannot create a temp cwd for the model")?;

    let (program, base_args) = inv
        .claude_cmd
        .split_first()
        .context("claude_cmd is empty")?;
    let mut child = Command::new(program)
        .args(base_args)
        .arg("-p")
        .arg(inv.prompt)
        .args(TOOL_FLAGS)
        .args(inv.extra_args)
        .current_dir(scratch.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("cannot run `{}`", inv.claude_cmd.join(" ")))?;

    let mut stdin = child.stdin.take().context("model stdin unavailable")?;
    let owned = payload.as_bytes().to_vec();
    std::thread::spawn(move || {
        // A model that answers without reading its input is behaving perfectly
        // well, and closing the pipe on us is how it says so. Reporting the
        // resulting `EPIPE` would turn a good answer into a failure.
        let _ = stdin.write_all(&owned);
        // Dropped here rather than at the end of the call: the child is waiting
        // for EOF, and holding the handle open would make it wait forever.
    });

    let (tx, rx) = channel::<(Stream, Vec<u8>)>();
    drain(
        child.stdout.take().context("model stdout unavailable")?,
        Stream::Out,
        tx.clone(),
    );
    drain(
        child.stderr.take().context("model stderr unavailable")?,
        Stream::Err,
        tx,
    );

    let start = Instant::now();
    let mut timed_out = false;
    let status = loop {
        match child.try_wait().context("cannot wait for the model")? {
            Some(status) => break status,
            None if start.elapsed() >= inv.timeout => {
                timed_out = true;
                let _ = child.kill();
                break child.wait().context("cannot reap the model")?;
            }
            None => std::thread::sleep(POLL),
        }
    };

    let (mut out, mut err) = (Vec::new(), Vec::new());
    for _ in 0..2 {
        match rx.recv_timeout(DRAIN_CEILING) {
            Ok((Stream::Out, bytes)) => out = bytes,
            Ok((Stream::Err, bytes)) => err = bytes,
            // Disconnected or slow. Whatever arrived is what there is.
            Err(_) => break,
        }
    }

    let completed = Completed {
        status,
        stdout: String::from_utf8_lossy(&out).into_owned(),
        stderr: String::from_utf8_lossy(&err).into_owned(),
    };
    if timed_out {
        // Carrying the stderr matters: a model that spent five minutes
        // complaining about a bad flag should say so, rather than leaving
        // "timed out" as the only diagnostic anyone ever sees.
        let complaint = completed.complaint();
        let tail = if complaint.is_empty() {
            String::new()
        } else {
            format!(": {complaint}")
        };
        anyhow::bail!("timed out after {}s{tail}", inv.timeout.as_secs());
    }
    Ok(completed)
}

/// Read one pipe to the end on its own thread, then hand over the bytes.
fn drain<R: Read + Send + 'static>(mut pipe: R, which: Stream, tx: Sender<(Stream, Vec<u8>)>) {
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        // A read error is the same as a short read here: the caller gets what
        // arrived, and the exit status is what decides whether that is a
        // failure.
        let _ = pipe.read_to_end(&mut buf);
        let _ = tx.send((which, buf));
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};

    /// A `/bin/sh` stub standing in for `claude`.
    fn script(dir: &Path, name: &str, body: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, format!("#!/bin/sh\n{body}")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    fn invoke(path: &Path, payload: &str, timeout: Duration) -> Result<Completed> {
        let cmd = vec![path.to_string_lossy().into_owned()];
        run(
            &Invocation {
                claude_cmd: &cmd,
                prompt: "ignored by the stub",
                extra_args: &[],
                timeout,
            },
            payload,
        )
    }

    #[test]
    fn a_payload_larger_than_the_pipe_buffer_does_not_deadlock() {
        // The bug this module exists for. The stub fills its stdout *before*
        // reading a byte of stdin, which is what a real model does when it
        // streams. Write-then-read hangs here forever: the parent is blocked in
        // `write_all` and the child is blocked in `write`, and neither can move.
        let dir = tempfile::tempdir().unwrap();
        let stub = script(
            dir.path(),
            "chatty",
            "yes 0123456789012345678901234567890123456789 | head -c 200000\n\
             cat > /dev/null\n",
        );

        let payload = "x".repeat(1024 * 1024);
        let out = invoke(&stub, &payload, Duration::from_secs(20)).expect("it should come back");
        assert!(out.status.success());
        assert_eq!(out.stdout.len(), 200_000, "all of stdout was read");
    }

    #[test]
    fn a_child_that_never_stops_writing_is_still_killed_at_the_deadline() {
        // The second half of the same bug. Polling `try_wait` and only then
        // calling `wait_with_output` cannot help a child that never exits
        // *because* it is blocked writing — the old code burned the full
        // timeout here and then reported it as if the model were slow.
        let dir = tempfile::tempdir().unwrap();
        let stub = script(dir.path(), "spammer", "while true; do echo spam; done\n");

        let start = Instant::now();
        let err = invoke(&stub, "hello", Duration::from_millis(300)).unwrap_err();
        assert!(
            format!("{err:#}").contains("timed out"),
            "it should say why: {err:#}"
        );
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "and it should say so promptly, not after the drain ceiling stacks up"
        );
    }

    #[test]
    fn a_child_that_never_exits_is_killed_at_the_deadline() {
        let dir = tempfile::tempdir().unwrap();
        let stub = script(dir.path(), "sleeper", "cat > /dev/null\nsleep 60\n");

        let start = Instant::now();
        let err = invoke(&stub, "hello", Duration::from_millis(300)).unwrap_err();
        assert!(format!("{err:#}").contains("timed out"), "{err:#}");
        assert!(start.elapsed() < Duration::from_secs(10));
    }

    #[test]
    fn a_child_that_ignores_its_stdin_is_not_an_error() {
        // The writer thread takes an EPIPE the moment the stub exits. That is
        // the model declining to read, not a failure, and surfacing it would
        // discard a perfectly good answer.
        let dir = tempfile::tempdir().unwrap();
        let stub = script(dir.path(), "deaf", "printf 'ok'\nexit 0\n");

        let payload = "x".repeat(1024 * 1024);
        let out = invoke(&stub, &payload, Duration::from_secs(20)).expect("not an error");
        assert!(out.status.success());
        assert_eq!(out.stdout, "ok");
    }

    #[test]
    fn both_streams_and_the_exit_status_come_back() {
        let dir = tempfile::tempdir().unwrap();
        let stub = script(
            dir.path(),
            "noisy",
            "cat > /dev/null\nprintf 'findings'\nprintf 'a warning' >&2\nexit 3\n",
        );

        let out = invoke(&stub, "hello", Duration::from_secs(20)).unwrap();
        assert_eq!(out.status.code(), Some(3));
        assert_eq!(out.stdout, "findings");
        assert_eq!(out.stderr, "a warning");
        assert_eq!(out.complaint(), "a warning");
    }

    #[test]
    fn the_payload_reaches_the_child_byte_for_byte() {
        let dir = tempfile::tempdir().unwrap();
        let capture = dir.path().join("payload");
        let stub = script(
            dir.path(),
            "recorder",
            &format!("cat > {}\n", capture.display()),
        );

        let payload = "== TASK ==\nadd tagging\n\n== HUNKS ==\n[1] a.rs\n";
        invoke(&stub, payload, Duration::from_secs(20)).unwrap();
        assert_eq!(std::fs::read_to_string(&capture).unwrap(), payload);
    }

    #[test]
    fn the_prompt_and_the_tool_flags_reach_the_command() {
        let dir = tempfile::tempdir().unwrap();
        let capture = dir.path().join("args");
        let stub = script(
            dir.path(),
            "argv",
            &format!(
                "printf '%s\\n' \"$@\" > {}\ncat > /dev/null\n",
                capture.display()
            ),
        );

        let cmd = vec![stub.to_string_lossy().into_owned()];
        let extra = vec!["--model".to_string(), "claude-haiku-4-5".to_string()];
        run(
            &Invocation {
                claude_cmd: &cmd,
                prompt: "put these in order",
                extra_args: &extra,
                timeout: Duration::from_secs(20),
            },
            "",
        )
        .unwrap();

        let args: Vec<String> = std::fs::read_to_string(&capture)
            .unwrap()
            .lines()
            .map(str::to_string)
            .collect();
        assert_eq!(args[0], "-p");
        assert_eq!(args[1], "put these in order");
        assert_eq!(&args[2..4], TOOL_FLAGS);
        assert_eq!(&args[4..], &extra[..], "model_args come last, so they win");
    }

    #[test]
    fn a_command_that_does_not_exist_names_itself() {
        let cmd = vec!["definitely-not-a-real-binary-xyz".to_string()];
        let err = run(
            &Invocation {
                claude_cmd: &cmd,
                prompt: "hello",
                extra_args: &[],
                timeout: Duration::from_secs(5),
            },
            "",
        )
        .unwrap_err();
        assert!(
            format!("{err:#}").contains("definitely-not-a-real-binary-xyz"),
            "{err:#}"
        );
    }

    #[test]
    fn an_empty_command_is_refused_rather_than_spawning_a_shell() {
        let err = run(
            &Invocation {
                claude_cmd: &[],
                prompt: "hello",
                extra_args: &[],
                timeout: Duration::from_secs(5),
            },
            "",
        )
        .unwrap_err();
        assert!(
            format!("{err:#}").contains("claude_cmd is empty"),
            "{err:#}"
        );
    }
}
