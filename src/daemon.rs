//! The daemon: finding one, addressing one, and (later) being one.
//!
//! Exactly one process may own the watch engine for a project, and it proves
//! that by holding the `watch.lock` flock for the engine's whole lifetime
//! (`ProjectPaths::watch_lock_path`). Everything in this module is downstream
//! of that invariant.
//!
//! `daemon.json` is the address book: where a running daemon listens and the
//! token to speak to it. It is deliberately **not** the authority on whether a
//! daemon exists — it outlives a `kill -9`, and a `rote watch --local` pane
//! owns the engine without writing one at all. The flock is the authority,
//! because the kernel maintains it and a file cannot.

use crate::config::Config;
use crate::engine::{Engine, EngineEvent};
use crate::http;
use crate::lockfile::Lock;
use crate::paths::{write_atomic_mode, ProjectPaths};
use crate::session::{Manifest, Terminal};
use crate::state;
use crate::watcher;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::io::Write as _;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::time::{Duration, Instant};
use tiny_http::{Header, Request, Response, ResponseBox, Server};

/// Bytes of entropy in a session token, before hex encoding.
const TOKEN_BYTES: usize = 32;

/// How long the accept loop blocks before looking at its own timers.
pub const ACCEPT_TICK: Duration = Duration::from_millis(100);

/// How long the daemon waits for the engine token before giving up.
///
/// Shorter than `Lock::acquire`'s five seconds: a CLI verb holding the token
/// for the three milliseconds of a direct mutation must not fail a daemon
/// start, and a daemon blocking five seconds behind a `--local` pane's
/// lifetime lock is five seconds of `rote start` waiting on nothing.
pub const LOCK_WAIT: Duration = Duration::from_secs(2);

/// A read handler waits this long on the hub before giving up with a 503.
pub const HUB_TIMEOUT: Duration = Duration::from_secs(2);

/// How long to wait for the engine thread to finish after the loop ends.
pub const JOIN_CEILING: Duration = Duration::from_secs(2);

/// How long an idle event stream waits before writing a keepalive comment.
///
/// The keepalive is what makes a vanished client detectable: without a write
/// there is nothing to fail, so a browser tab closed an hour ago would still
/// hold a thread and a socket.
pub const SSE_HEARTBEAT: Duration = Duration::from_secs(15);

/// How many frames a subscriber may fall behind before it is dropped.
///
/// Eight whole snapshots behind on loopback is not slow, it is broken.
pub const SSE_QUEUE: usize = 8;

/// A verb handler waits this long on the engine before giving up with a 503.
/// Longer than the hub's ceiling because the engine may be inside a recompute
/// holding the manifest lock.
pub const COMMAND_TIMEOUT: Duration = Duration::from_secs(5);

/// Where a daemon is listening, and how to prove you may talk to it.
///
/// Written as one file rather than the conventional separate pid and port
/// files: three files can be read while only some of them are current, and a
/// client that pairs this run's port with the last run's token gets a
/// confusing 401 instead of an honest "nothing is running".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Endpoint {
    pub pid: u32,
    pub port: u16,
    pub token: String,
    /// Which project this daemon serves. Checked before anything is believed —
    /// or signalled. PIDs are recycled, and a stale file that outlived a reboot
    /// must never get an unrelated process killed.
    pub project_hash: String,
    pub repo_root: String,
    pub wire_version: u32,
    pub rote_version: String,
    pub started_at: String,
}

impl Endpoint {
    pub fn new(project: &ProjectPaths, pid: u32, port: u16, token: String) -> Self {
        Self {
            pid,
            port,
            token,
            project_hash: project.hash.clone(),
            repo_root: project.repo_root.to_string_lossy().into_owned(),
            wire_version: crate::state::WIRE_VERSION,
            rote_version: env!("CARGO_PKG_VERSION").to_string(),
            started_at: crate::now_iso8601(),
        }
    }

    /// `None` when nothing is published, or when what is published is
    /// unreadable. A malformed address book is the same as no address book;
    /// there is nothing a caller could usefully do differently.
    pub fn read(project: &ProjectPaths) -> Option<Self> {
        let bytes = std::fs::read(project.daemon_json()).ok()?;
        serde_json::from_slice(&bytes).ok()
    }

    /// Owner-readable only: this file holds a bearer token.
    pub fn write(&self, project: &ProjectPaths) -> Result<()> {
        project.ensure_state_dir()?;
        let mut bytes = serde_json::to_vec_pretty(self).context("cannot serialize the endpoint")?;
        bytes.push(b'\n');
        write_atomic_mode(&project.daemon_json(), &bytes, 0o600)
    }

    pub fn remove(project: &ProjectPaths) {
        // Best effort. A stale file is harmless — `matches` and the health
        // check are what make it safe to ignore one.
        let _ = std::fs::remove_file(project.daemon_json());
    }

    /// Does this endpoint describe a daemon for *this* project?
    pub fn matches(&self, project: &ProjectPaths) -> bool {
        self.project_hash == project.hash
    }

    pub fn base_url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }
}

/// A fresh session token: 32 bytes of kernel entropy, hex encoded.
pub fn mint_token() -> Result<String> {
    crate::random_hex(TOKEN_BYTES)
}

// ------------------------------------------------- finding, starting, ending

/// How long to wait for a freshly spawned daemon to answer.
pub const SPAWN_WAIT: Duration = Duration::from_secs(3);

/// Poll interval while waiting for one. House style: `model::run`.
const SPAWN_POLL: Duration = Duration::from_millis(25);

/// How long a reaped daemon gets to exit on its own before it is signalled.
pub const REAP_WAIT: Duration = Duration::from_secs(3);

/// A short ceiling for probes: a daemon that cannot answer promptly is one a
/// caller should stop waiting on.
const PROBE_TIMEOUT: Duration = Duration::from_secs(2);

/// Ask an endpoint who it is.
///
/// `None` when nothing answers promptly, or when what answers is not a rote
/// daemon. `GET /health` is answerable without touching the engine, so this
/// cannot be blocked by a classification in flight.
fn health_of(ep: &Endpoint) -> Option<state::Health> {
    http::send(ep.port, &ep.token, "GET", "/health", None, PROBE_TIMEOUT)
        .ok()
        .filter(|r| r.is_ok())?
        .json()
        .ok()
}

/// Is something on this port a daemon serving *this* project?
///
/// The port is the weak link. A daemon that died frees it, and anything at all may
/// take it before we look — so the hash in the reply is what closes the gap, and
/// the hash in the *file* cannot: that one is true by construction.
fn serves(ep: &Endpoint, project: &ProjectPaths) -> bool {
    health_of(ep).is_some_and(|h| h.project_hash == project.hash)
}

/// Find a live daemon for this project, if there is one.
///
/// Both halves matter. The endpoint file can outlive its author — a `kill -9`
/// leaves it behind — so it is believed only after something answers on the
/// port and says it serves this project. A pid check alone would not do either:
/// pids are recycled.
pub fn discover(project: &ProjectPaths) -> Option<Endpoint> {
    let ep = Endpoint::read(project)?;
    if !ep.matches(project) {
        return None;
    }
    serves(&ep, project).then_some(ep)
}

/// Start a daemon in the background and wait until it answers.
///
/// Detached with `setsid` so it outlives the terminal that started it. Without
/// that it shares claude's session, and the SIGHUP when that pty closes takes
/// the daemon with it — which is precisely when the user is most likely to
/// still be typing.
pub fn spawn_detached(project: &ProjectPaths) -> Result<Endpoint> {
    use std::os::unix::process::CommandExt as _;

    project.ensure_state_dir()?;
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(project.daemon_log())
        .with_context(|| format!("cannot open {}", project.daemon_log().display()))?;
    let log_err = log.try_clone()?;

    let exe = std::env::current_exe().context("cannot find the rote binary")?;
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("--project")
        .arg(&project.repo_root)
        .arg("daemon")
        .arg("--foreground")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::from(log))
        .stderr(std::process::Stdio::from(log_err));

    // Safety: `setsid` is async-signal-safe and is on the POSIX list of calls
    // permitted between fork and exec.
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }

    let child = cmd.spawn().context("cannot start the rote daemon")?;
    let pid = child.id();

    // Wait for this child's endpoint — but settle for anyone's.
    //
    // Preferring our own pid is what stops a file left by a previous run from
    // looking like success. Accepting someone else's is what makes two front
    // ends starting at the same moment work: both find nothing, both spawn, one
    // wins the engine token and the other exits. The loser's parent has still
    // got what it asked for, which is a daemon — not necessarily *its* daemon.
    let start = Instant::now();
    loop {
        if let Some(ep) = Endpoint::read(project).filter(|e| e.pid == pid && e.matches(project)) {
            // The same strictness `discover` uses, which is free here — the
            // endpoint is already filtered by pid and hash — and closes the case
            // where our child died inside the spawn window and something else
            // took its port.
            if serves(&ep, project) {
                return Ok(ep);
            }
        }
        if let Some(ep) = discover(project) {
            return Ok(ep);
        }
        if start.elapsed() >= SPAWN_WAIT {
            anyhow::bail!(
                "the rote daemon did not start within {}s.\nIts output is in {}.",
                SPAWN_WAIT.as_secs(),
                project.daemon_log().display()
            );
        }
        std::thread::sleep(SPAWN_POLL);
    }
}

/// A daemon for this project, starting one if necessary.
pub fn ensure_running(project: &ProjectPaths) -> Result<Endpoint> {
    match discover(project) {
        Some(ep) => Ok(ep),
        None => spawn_detached(project),
    }
}

/// Stop the daemon, if there is one, before the caller tears the session down.
///
/// Must happen **before** `shadow::sync` resets the shadow: that runs
/// `git reset --hard` and `git clean -fdx`, which the daemon's watcher is
/// watching recursively, and it would be classifying against a session that no
/// longer exists. Before the manifest lock is taken, too — the daemon may be
/// mid-write, and waiting five seconds behind a process we are about to stop is
/// five seconds of nothing.
pub fn reap(project: &ProjectPaths, terminal: Option<Terminal>) {
    let Some(ep) = Endpoint::read(project) else {
        return;
    };
    // Never signal a pid we cannot prove is ours. After a `kill -9` and a
    // reboot a stale file can name a pid that now belongs to something else
    // entirely, and a tool that kills strangers is a tool nobody trusts.
    if !ep.matches(project) {
        Endpoint::remove(project);
        return;
    }

    let reason = match terminal {
        Some(Terminal::Done) => "done",
        Some(Terminal::Aborted) => "aborted",
        None => "idle",
    };
    let body = serde_json::json!({ "reason": reason });
    let bytes = serde_json::to_vec(&body).unwrap_or_default();
    // Asking is what lets it tell every attached pane *how* the session ended
    // before it goes; after `archive_and_clear` the live manifest is gone and
    // the daemon would have nothing left to read.
    let _ = http::send(
        ep.port,
        &ep.token,
        "POST",
        "/shutdown",
        Some(&bytes),
        PROBE_TIMEOUT,
    );

    if !wait_for_exit(ep.pid, REAP_WAIT) {
        signal(ep.pid, libc::SIGTERM);
        if !wait_for_exit(ep.pid, REAP_WAIT) {
            signal(ep.pid, libc::SIGKILL);
            let _ = wait_for_exit(ep.pid, REAP_WAIT);
        }
    }
    Endpoint::remove(project);
    // `watch.lock` is deliberately never unlinked, per the doctrine in paths.rs:
    // removing a lock file another process may hold open is the classic race.
}

fn wait_for_exit(pid: u32, ceiling: Duration) -> bool {
    let start = Instant::now();
    loop {
        if !crate::lockfile::pid_is_live(pid) {
            return true;
        }
        if start.elapsed() >= ceiling {
            return false;
        }
        std::thread::sleep(SPAWN_POLL);
    }
}

fn signal(pid: u32, sig: libc::c_int) {
    // Safety: a plain kill(2) on a pid we have just proved belongs to this
    // project's daemon.
    unsafe {
        libc::kill(pid as libc::pid_t, sig);
    }
}

/// Who owns the engine, and therefore how a mutation must be made.
pub enum Owner {
    /// Nobody. The guard is held for as long as the caller mutates directly.
    Nobody(Lock),
    /// A daemon, at this address.
    Daemon(Endpoint),
    /// Something holds the engine token but does not answer HTTP — a
    /// `rote watch --local` pane, or a wedged daemon.
    Opaque { pid: Option<u32> },
}

/// Decide how to mutate, by trying to take the engine token.
///
/// The check *is* the exclusion, which is why this beats asking `daemon.json`
/// first: acquire the token and no engine exists to be desynchronized, so a
/// direct mutation is safe for exactly as long as the guard is held. Fail to
/// acquire it and an engine does exist, so the mutation has to be its decision
/// rather than ours.
pub fn owner(project: &ProjectPaths) -> Result<Owner> {
    project.ensure_state_dir()?;
    if let Some(guard) = Lock::try_acquire(&project.watch_lock_path())? {
        return Ok(Owner::Nobody(guard));
    }
    match discover(project) {
        Some(ep) => Ok(Owner::Daemon(ep)),
        None => Ok(Owner::Opaque {
            pid: Endpoint::read(project).map(|e| e.pid),
        }),
    }
}

/// Send a verb to a running daemon and wait for its answer.
pub fn send_command(ep: &Endpoint, request: &state::Request) -> Result<state::Response> {
    let bytes = serde_json::to_vec(request).context("cannot serialize the command")?;
    let reply = http::send(
        ep.port,
        &ep.token,
        "POST",
        "/command",
        Some(&bytes),
        http::CLIENT_TIMEOUT,
    )?;
    if !reply.is_ok() {
        anyhow::bail!(
            "the rote daemon refused the request (status {})",
            reply.status
        );
    }
    reply.json()
}

/// What a daemon did with a verb, for a caller that only has one to send.
pub enum Applied {
    Yes,
    /// Refused. `cause` is the variant to branch on; `reason` is for the human.
    Rejected {
        cause: Option<state::Cause>,
        reason: String,
    },
}

/// Send one verb and triage the answer.
///
/// Three CLI commands each built the same `Request` and matched the same three
/// outcomes, which meant the staleness message existed verbatim in three places
/// and could have drifted in any of them. `Stale` is handled here rather than
/// returned because no caller has ever had anything to do about it: the verb did
/// not land and the queue has moved, so the only honest answer is to say so.
///
/// `generation: None` — a CLI invocation has not been looking at a snapshot, so
/// it has no generation to be stale against. The pane, which has, still builds
/// its own requests.
pub fn apply(ep: &Endpoint, command: state::Command) -> Result<Applied> {
    let response = send_command(
        ep,
        &state::Request {
            wire_version: state::WIRE_VERSION,
            generation: None,
            command,
        },
    )?;
    match response.outcome {
        state::Outcome::Applied => Ok(Applied::Yes),
        state::Outcome::Rejected { reason, cause } => Ok(Applied::Rejected { cause, reason }),
        state::Outcome::Stale { current } => {
            anyhow::bail!("the queue moved underneath that (now at generation {current})")
        }
    }
}

/// Ask the daemon what the world looks like.
pub fn fetch_state(ep: &Endpoint) -> Result<state::Snapshot> {
    let reply = http::send(
        ep.port,
        &ep.token,
        "GET",
        "/state",
        None,
        http::CLIENT_TIMEOUT,
    )?;
    if !reply.is_ok() {
        anyhow::bail!("the rote daemon is not ready (status {})", reply.status);
    }
    reply.json()
}

/// The error for "something owns the queue but will not talk to us".
pub fn opaque_owner_error(project: &ProjectPaths, pid: Option<u32>) -> anyhow::Error {
    match pid.filter(|p| crate::lockfile::pid_is_live(*p)) {
        Some(pid) => anyhow::anyhow!(
            "pid {pid} owns this project's queue but is not answering.\n\
             Stop it, then try again."
        ),
        None => anyhow::anyhow!(
            "something owns this project's queue but is not answering.\n\
             If it has crashed, remove {}.",
            project.watch_lock_path().display()
        ),
    }
}

// ---------------------------------------------------------------- the hub

/// What the hub can be asked to do.
///
/// A single-consumer actor rather than shared state behind a lock. The
/// codebase has no `Arc`, no `Mutex` and no atomics, and this does not need
/// to be the exception: every piece of state has one owning thread and every
/// interaction is a message.
enum HubMsg {
    /// Something the engine published.
    Publish(state::Event),
    /// A read handler asking what the world looks like.
    Peek(Sender<HubView>),
    /// A new event stream. Gets the cached snapshot immediately, which is why
    /// SSE needs no `Last-Event-ID` and no resume: every frame is whole.
    Subscribe(std::sync::mpsc::SyncSender<state::Event>),
    /// The session is over. Ends the hub loop, which is the shutdown trigger.
    Close(Option<Terminal>),
}

/// The hub's answer to a read.
#[derive(Debug, Clone, Default)]
struct HubView {
    snapshot: Option<state::Snapshot>,
    subscribers: usize,
}

/// The hub thread: cache the last snapshot, answer reads, notice the end.
///
/// Separate from the engine's sink closure because `GET /state` and
/// `GET /health` must be answerable **without touching the engine** — `drift`
/// lives nowhere else, and an HTTP request must never be able to block the
/// thread that is classifying the user's keystrokes.
fn run_hub(rx: Receiver<HubMsg>, done: Sender<Option<Terminal>>) {
    let mut view = HubView::default();
    let mut subscribers: Vec<std::sync::mpsc::SyncSender<state::Event>> = Vec::new();

    for msg in rx {
        match msg {
            HubMsg::Publish(state::Event::Snapshot(s)) => {
                view.snapshot = Some((*s).clone());
                fan_out(&mut subscribers, state::Event::Snapshot(s));
                view.subscribers = subscribers.len();
            }
            HubMsg::Publish(state::Event::Closed { terminal }) => {
                // Tell everyone before going, so a pane can say "session closed"
                // rather than "the daemon vanished".
                fan_out(&mut subscribers, state::Event::Closed { terminal });
                let _ = done.send(terminal);
                return;
            }
            // The engine emits one of these every tick. Four frames a second
            // down a socket is noise; each stream writes its own keepalive when
            // it has actually been idle.
            HubMsg::Publish(state::Event::Heartbeat { .. }) => {}
            HubMsg::Publish(other) => fan_out(&mut subscribers, other),
            HubMsg::Peek(reply) => {
                // A caller that has given up is not an error.
                let _ = reply.send(view.clone());
            }
            HubMsg::Subscribe(sink) => {
                if let Some(s) = &view.snapshot {
                    // A client that attaches mid-session sees the world at once.
                    if sink
                        .try_send(state::Event::Snapshot(Box::new(s.clone())))
                        .is_err()
                    {
                        continue;
                    }
                }
                subscribers.push(sink);
                view.subscribers = subscribers.len();
            }
            HubMsg::Close(terminal) => {
                fan_out(&mut subscribers, state::Event::Closed { terminal });
                let _ = done.send(terminal);
                return;
            }
        }
    }
}

/// Send to every subscriber, dropping the ones that have gone or fallen behind.
///
/// There is no unsubscribe message: a stream thread that exits drops its
/// receiver, and this notices on the next send. One mechanism, and no
/// bookkeeping that can leak.
fn fan_out(subscribers: &mut Vec<std::sync::mpsc::SyncSender<state::Event>>, ev: state::Event) {
    subscribers.retain(|s| s.try_send(ev.clone()).is_ok());
}

// ---------------------------------------------------------------- serving

#[derive(Debug, Clone, Copy, Default)]
pub struct ServeOptions {
    /// Exit after this long no matter what. For tests: a wedged daemon should
    /// fail a suite rather than outlive the machine running it.
    pub timeout_ms: Option<u64>,
}

/// Be the daemon.
///
/// Takes the engine token first and holds it for the whole run, so the
/// single-engine invariant is established before anything else happens — there
/// is no window in which a second engine could be constructed.
pub fn serve(project: &ProjectPaths, cfg: &Config, opts: ServeOptions) -> Result<()> {
    let started = Instant::now();

    let lock = Lock::acquire_within(&project.watch_lock_path(), LOCK_WAIT).map_err(|e| {
        anyhow::anyhow!(
            "{e:#}\nSomething already owns this project's queue — another daemon, \
             or a `rote watch --local` pane."
        )
    })?;
    let _lock = lock;

    Manifest::require(project)?;
    crate::shadow::ensure_no_operation_in_progress(project)?;

    // Port 0: the kernel picks. A fixed port is a collision waiting to happen
    // the first time two projects are open at once.
    let server = Server::http("127.0.0.1:0")
        .map_err(|e| anyhow::anyhow!("cannot listen on 127.0.0.1: {e}"))?;
    let port = server
        .server_addr()
        .to_ip()
        .context("the listener is not an IP socket")?
        .port();
    let token = mint_token()?;

    let (engine_tx, engine_rx) = channel::<EngineEvent>();
    let (hub_tx, hub_rx) = channel::<HubMsg>();
    let (done_tx, done_rx) = channel::<Option<Terminal>>();

    let watch_tx = engine_tx.clone();
    let _watch = watcher::spawn(project, cfg, move |ev| watch_tx.send(ev).is_ok())?;

    let hub_thread = std::thread::spawn(move || run_hub(hub_rx, done_tx));

    let engine_hub = hub_tx.clone();
    let engine_project = project.clone();
    let engine_cfg = cfg.clone();
    let engine_thread = std::thread::spawn(move || {
        let mut engine = Engine::new(engine_project, engine_cfg);
        // `Engine::run`'s first caller. It was written for this and has been
        // waiting since the engine landed.
        let out = engine.run(&engine_rx, |ev| {
            Ok(engine_hub.send(HubMsg::Publish(ev)).is_ok())
        });
        if let Err(e) = out {
            eprintln!("rote daemon: the engine stopped: {e:#}");
        }
        // However the engine ended, the daemon is finished.
        let _ = engine_hub.send(HubMsg::Close(None));
    });

    Endpoint::new(project, std::process::id(), port, token.clone()).write(project)?;
    println!(
        "rote daemon listening on 127.0.0.1:{port} (pid {})",
        std::process::id()
    );

    let deadline = opts
        .timeout_ms
        .map(|ms| Instant::now() + Duration::from_millis(ms));
    let ctx = Ctx {
        project: project.clone(),
        token,
        port,
        started,
        hub: hub_tx.clone(),
        engine: engine_tx.clone(),
    };

    let mut terminal = None;
    let mut timed_out = false;
    loop {
        match done_rx.try_recv() {
            Ok(t) => {
                terminal = t;
                break;
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => {}
            Err(std::sync::mpsc::TryRecvError::Disconnected) => break,
        }
        if let Some(d) = deadline {
            if Instant::now() >= d {
                timed_out = true;
                break;
            }
        }
        match server.recv_timeout(ACCEPT_TICK) {
            Ok(Some(request)) => handle(&ctx, request),
            Ok(None) => {}
            Err(e) => {
                eprintln!("rote daemon: cannot accept: {e}");
                break;
            }
        }
    }

    // Order matters: stop feeding the engine before waiting on it, or `run`
    // never sees `Disconnected` and the join hangs.
    drop(_watch);
    drop(engine_tx);
    let _ = hub_tx.send(HubMsg::Close(terminal));
    join_within(engine_thread, JOIN_CEILING);
    join_within(hub_thread, JOIN_CEILING);

    Endpoint::remove(project);
    drop(server);

    if timed_out {
        anyhow::bail!(
            "the rote daemon timed out after {}ms",
            opts.timeout_ms.unwrap()
        );
    }
    Ok(())
}

/// Join, but never forever. A thread wedged on a lock must not stop the daemon
/// from releasing the engine token.
fn join_within<T>(handle: std::thread::JoinHandle<T>, ceiling: Duration) {
    let start = Instant::now();
    loop {
        if handle.is_finished() {
            let _ = handle.join();
            return;
        }
        if start.elapsed() >= ceiling {
            return; // detached; the process is about to exit anyway
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Everything a request handler needs, and nothing it could mutate.
struct Ctx {
    project: ProjectPaths,
    token: String,
    port: u16,
    started: Instant,
    hub: Sender<HubMsg>,
    engine: Sender<EngineEvent>,
}

impl Ctx {
    fn peek(&self) -> Option<HubView> {
        let (tx, rx) = channel();
        self.hub.send(HubMsg::Peek(tx)).ok()?;
        rx.recv_timeout(HUB_TIMEOUT).ok()
    }
}

fn json(status: u16, value: &impl Serialize) -> ResponseBox {
    let body = serde_json::to_vec(value).unwrap_or_else(|_| b"{}".to_vec());
    Response::from_data(body)
        .with_status_code(status)
        .with_header(header("Content-Type", "application/json"))
        .boxed()
}

fn error(status: u16, code: &str) -> ResponseBox {
    json(status, &serde_json::json!({ "error": code }))
}

fn header(name: &str, value: &str) -> Header {
    Header::from_bytes(name.as_bytes(), value.as_bytes())
        .expect("a header rote wrote itself is well-formed")
}

/// Authorize, then route. Every rejection is a JSON body with a stable code.
fn handle(ctx: &Ctx, request: Request) {
    let url = request.url().to_string();
    let (path, query) = match url.split_once('?') {
        Some((p, q)) => (p.to_string(), q.to_string()),
        None => (url, String::new()),
    };
    let method = request.method().as_str().to_string();

    if let Some(rejection) = reject(ctx, &request, &path, &query) {
        let _ = request.respond(rejection);
        return;
    }

    // The event stream takes the socket over rather than returning a response,
    // so it is routed before anything that produces one.
    if method == "GET" && path == "/events" {
        events(ctx, request);
        return;
    }

    let mut request = request;
    let response = match (method.as_str(), path.as_str()) {
        ("GET", "/") => crate::web::page(),
        ("GET", "/health") => health(ctx),
        ("GET", "/state") => snapshot(ctx),
        ("GET", p) if p.starts_with("/hunk/") => hunk_detail(ctx, &p["/hunk/".len()..]),
        ("POST", "/command") => command(ctx, &mut request),
        ("POST", "/shutdown") => shutdown(ctx, &mut request),
        ("GET", _) | ("POST", _) => error(404, "not_found"),
        _ => Response::from_data(Vec::new())
            .with_status_code(405)
            .with_header(header("Allow", "GET, POST"))
            .boxed(),
    };
    let _ = request.respond(response);
}

/// Read a JSON body, refusing anything that is not one.
fn read_json_body<T: serde::de::DeserializeOwned>(request: &mut Request) -> Result<T, ResponseBox> {
    let content_type = request
        .headers()
        .iter()
        .find(|h| h.field.equiv("Content-Type"))
        .map(|h| h.value.as_str().to_string())
        .unwrap_or_default();
    if !http::is_json_content_type(&content_type) {
        // A browser form can only send three content types, none of them this
        // one — which is what stops a hostile page firing a simple-CORS POST at
        // every port on the loopback.
        return Err(error(415, "expected_json"));
    }
    if let Some(len) = request.body_length() {
        if len > http::MAX_BODY {
            return Err(error(413, "body_too_large"));
        }
    }

    use std::io::Read as _;
    let mut body = Vec::new();
    if request
        .as_reader()
        .take(http::MAX_BODY as u64 + 1)
        .read_to_end(&mut body)
        .is_err()
    {
        return Err(error(400, "unreadable_body"));
    }
    if body.len() > http::MAX_BODY {
        return Err(error(413, "body_too_large"));
    }
    serde_json::from_slice(&body).map_err(|_| error(400, "bad_json"))
}

/// Serve `text/event-stream` on its own thread, for as long as the client reads.
///
/// The response is written by hand through `Request::into_writer` rather than
/// returned as a `Response`, and this is not a stylistic choice. tiny_http's
/// chunked path is `Encoder::new(writer)` followed by `io::copy` — the encoder
/// buffers 8 KiB and `io::copy` never flushes, and for an infinite reader it
/// never returns either, so nothing after it runs. A three-hundred-byte frame
/// would sit invisible in that buffer until eight kilobytes had accumulated.
/// The whole point of this stream is that a frame arrives when it happens, so
/// every frame is followed by an explicit `flush`.
///
/// Framing is end-of-body-at-close: no `Content-Length`, no
/// `Transfer-Encoding`. Legal HTTP/1.1, what every `EventSource` handles, and
/// three fewer lines of chunk framing to get wrong on a loopback socket with no
/// intermediaries.
fn events(ctx: &Ctx, request: Request) {
    let (tx, rx) = std::sync::mpsc::sync_channel::<state::Event>(SSE_QUEUE);
    if ctx.hub.send(HubMsg::Subscribe(tx)).is_err() {
        let _ = request.respond(error(503, "hub_gone"));
        return;
    }

    std::thread::spawn(move || {
        let mut w = request.into_writer();
        let head = "HTTP/1.1 200 OK\r\n\
                    Content-Type: text/event-stream\r\n\
                    Cache-Control: no-store\r\n\
                    Connection: close\r\n\
                    X-Accel-Buffering: no\r\n\r\n";
        if w.write_all(head.as_bytes()).is_err() || w.flush().is_err() {
            return;
        }

        loop {
            let frame = match rx.recv_timeout(SSE_HEARTBEAT) {
                Ok(ev) => {
                    let name = event_name(&ev);
                    let data = serde_json::to_string(&ev).unwrap_or_else(|_| "{}".into());
                    // `event:` mirrors the type tag so a browser can use
                    // addEventListener; `data:` is the whole event so a client
                    // that only listens to onmessage is equally correct.
                    let done = matches!(ev, state::Event::Closed { .. });
                    let frame = format!("event: {name}\ndata: {data}\n\n");
                    if w.write_all(frame.as_bytes()).is_err() || w.flush().is_err() {
                        return;
                    }
                    if done {
                        return;
                    }
                    continue;
                }
                // A comment line. Invisible to `EventSource`, and the only
                // reason a silent stream ever notices its client has gone.
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => ":\n\n".to_string(),
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return,
            };
            if w.write_all(frame.as_bytes()).is_err() || w.flush().is_err() {
                return;
            }
        }
    });
}

fn event_name(ev: &state::Event) -> &'static str {
    match ev {
        state::Event::Snapshot(_) => "snapshot",
        state::Event::Notice(_) => "notice",
        state::Event::Heartbeat { .. } => "heartbeat",
        state::Event::Closed { .. } => "closed",
    }
}

/// Stop, telling every attached client how the session ended.
///
/// The reaper is the only thing that knows the reason, and it knows it before
/// the archive exists — so it says so rather than leaving the daemon to guess
/// from a directory listing after the fact.
fn shutdown(ctx: &Ctx, request: &mut Request) -> ResponseBox {
    #[derive(serde::Deserialize)]
    struct Body {
        reason: String,
    }
    let body: Body = match read_json_body(request) {
        Ok(v) => v,
        Err(rejection) => return rejection,
    };
    let terminal = match body.reason.as_str() {
        "done" => Some(Terminal::Done),
        "aborted" => Some(Terminal::Aborted),
        _ => None,
    };
    let _ = ctx.hub.send(HubMsg::Close(terminal));
    json(202, &serde_json::json!({ "ok": true }))
}

/// A verb. The engine answers; this thread only carries the reply back.
fn command(ctx: &Ctx, request: &mut Request) -> ResponseBox {
    let parsed: state::Request = match read_json_body(request) {
        Ok(v) => v,
        Err(rejection) => return rejection,
    };

    let (tx, rx) = channel();
    let sent = ctx
        .engine
        .send(EngineEvent::Command(crate::engine::CommandRequest {
            request: parsed,
            reply: Some(tx),
        }));
    if sent.is_err() {
        return error(503, "engine_gone");
    }
    // The engine may be inside a recompute holding the manifest lock, so this
    // is generous — but it is bounded, because a handler that waits forever is
    // a socket that never closes.
    match rx.recv_timeout(COMMAND_TIMEOUT) {
        Ok(response) => json(200, &response),
        Err(_) => error(503, "engine_timeout"),
    }
}

/// The checks every request passes before anything looks at the path.
fn reject(ctx: &Ctx, request: &Request, path: &str, query: &str) -> Option<ResponseBox> {
    let headers: Vec<(String, String)> = request
        .headers()
        .iter()
        .map(|h| {
            (
                h.field.as_str().as_str().to_string(),
                h.value.as_str().to_string(),
            )
        })
        .collect();
    match http::authorize(&headers, path, query, ctx.port, &ctx.token)? {
        http::Rejection::ForbiddenOrigin => Some(error(403, "forbidden_origin")),
        http::Rejection::ForbiddenHost => Some(error(403, "forbidden_host")),
        http::Rejection::Unauthorized => Some(error(401, "unauthorized")),
    }
}

fn health(ctx: &Ctx) -> ResponseBox {
    let view = ctx.peek().unwrap_or_default();
    let manifest = Manifest::load(&ctx.project).ok().flatten();
    let body = state::Health {
        ok: true,
        wire_version: state::WIRE_VERSION,
        manifest_version: crate::session::MANIFEST_VERSION,
        rote_version: env!("CARGO_PKG_VERSION").to_string(),
        pid: std::process::id(),
        port: ctx.port,
        project_hash: ctx.project.hash.clone(),
        repo_root: ctx.project.repo_root.to_string_lossy().into_owned(),
        shadow_dir: ctx.project.shadow_dir.to_string_lossy().into_owned(),
        session_state: manifest.as_ref().map(|m| m.state),
        task: manifest.map(|m| m.task).unwrap_or_default(),
        generation: view.snapshot.as_ref().map(|s| s.generation),
        subscribers: view.subscribers,
        uptime_ms: ctx.started.elapsed().as_millis() as u64,
    };
    json(200, &body)
}

/// The engine's last published view, not a fresh read.
///
/// `drift` is engine-only state, so a handler that re-derived a snapshot here
/// would either omit it or shell out to git per request — and could disagree
/// with what every SSE subscriber was just told. `generation` is how a client
/// knows how fresh this is.
fn snapshot(ctx: &Ctx) -> ResponseBox {
    match ctx.peek() {
        None => error(503, "hub_timeout"),
        Some(view) => match view.snapshot {
            Some(s) => json(200, &s),
            None => Response::from_data(
                serde_json::to_vec(&serde_json::json!({"error": "warming_up"})).unwrap(),
            )
            .with_status_code(503)
            .with_header(header("Content-Type", "application/json"))
            .with_header(header("Retry-After", "1"))
            .boxed(),
        },
    }
}

/// One hunk in full, anchored against the real file as it stands.
fn hunk_detail(ctx: &Ctx, id: &str) -> ResponseBox {
    let manifest = match Manifest::load(&ctx.project) {
        Ok(Some(m)) => m,
        _ => return error(503, "no_session"),
    };
    let Some(hunk) = manifest.find(id) else {
        return error(404, "no_such_hunk");
    };
    let located = crate::present::locate(&ctx.project.repo_root, hunk);
    json(
        200,
        &state::HunkDetail {
            generation: manifest.generation,
            hunk: hunk.clone(),
            anchor_line: located.anchor.line,
            anchor_via: located.anchor.via,
            real_path: located.real_path.to_string_lossy().into_owned(),
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::watcher::Filter;

    fn project(dir: &std::path::Path) -> ProjectPaths {
        let repo = dir.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        ProjectPaths::resolve_in(&repo, &dir.join("cache"), &dir.join("data")).unwrap()
    }

    #[test]
    fn a_token_is_sixty_four_hex_characters_and_never_repeats() {
        let a = mint_token().unwrap();
        let b = mint_token().unwrap();
        assert_eq!(a.len(), TOKEN_BYTES * 2);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b, "two mints must not collide");
    }

    #[test]
    fn the_endpoint_file_is_owner_readable_only() {
        // It holds a bearer token. 0644 under a normal umask would hand it to
        // every account on a shared machine.
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        let p = project(dir.path());
        Endpoint::new(&p, 42, 5000, "deadbeef".into())
            .write(&p)
            .unwrap();

        let mode = std::fs::metadata(p.daemon_json())
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600, "got {:o}", mode & 0o777);
        let dir_mode = std::fs::metadata(&p.state_dir)
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(dir_mode & 0o777, 0o700, "got {:o}", dir_mode & 0o777);
    }

    #[test]
    fn an_endpoint_round_trips_and_knows_its_project() {
        let dir = tempfile::tempdir().unwrap();
        let p = project(dir.path());
        let ep = Endpoint::new(&p, 42, 5000, "deadbeef".into());
        ep.write(&p).unwrap();

        let back = Endpoint::read(&p).expect("readable");
        assert_eq!(back, ep);
        assert!(back.matches(&p));
        assert_eq!(back.base_url(), "http://127.0.0.1:5000");
    }

    #[test]
    fn an_endpoint_from_another_project_is_not_ours() {
        // The guard that stops a stale file getting an unrelated pid killed.
        let dir = tempfile::tempdir().unwrap();
        let p = project(dir.path());
        let mut ep = Endpoint::new(&p, 42, 5000, "deadbeef".into());
        ep.project_hash = "0123456789ab".into();
        assert!(!ep.matches(&p));
    }

    #[test]
    fn a_missing_or_corrupt_endpoint_reads_as_nothing_running() {
        let dir = tempfile::tempdir().unwrap();
        let p = project(dir.path());
        assert!(Endpoint::read(&p).is_none(), "nothing published yet");

        p.ensure_state_dir().unwrap();
        std::fs::write(p.daemon_json(), b"{ truncated by kill -9").unwrap();
        assert!(
            Endpoint::read(&p).is_none(),
            "unreadable is the same as absent"
        );
    }

    #[test]
    fn the_endpoint_file_is_invisible_to_the_watcher() {
        // Otherwise the daemon publishing its own address would wake its own
        // engine, once per write, forever.
        let dir = tempfile::tempdir().unwrap();
        let p = project(dir.path());
        let f = Filter::new(&p, &crate::config::Config::default()).unwrap();
        assert_eq!(f.classify(&p.daemon_json()), None);
        assert_eq!(f.classify(&p.state_dir.join(".daemon.json.tmp")), None);
        assert_eq!(f.classify(&p.daemon_log()), None);
    }
}
