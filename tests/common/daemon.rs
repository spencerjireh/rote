//! Driving a real `rote daemon` from a test.
//!
//! The daemon outlives the call that starts it, so every helper here is about
//! making that safe: a bounded wait instead of a sleep, a `Drop` that actually
//! reaps, and a hard `--timeout` on every spawn so a wedged daemon fails the
//! suite rather than outliving the machine running it.

#![allow(dead_code)]

use super::cli::Cli;
use rote::daemon::Endpoint;
use rote::http;
use std::process::Child;
use std::time::{Duration, Instant};

/// The ceiling on every daemon a test starts.
pub const TIMEOUT_MS: u64 = 20_000;

/// How long a `wait_until` will keep trying.
pub const WAIT: Duration = Duration::from_secs(10);

/// Poll interval. House style: `model::run`.
const POLL: Duration = Duration::from_millis(25);

/// How long to let a recursive watch take effect before writing to the tree.
///
/// Not paranoia: on an idle machine a test can write within a millisecond of
/// the daemon answering `/health`, and on macOS fsevents that write can predate
/// the subscription and never be delivered at all.
const WATCH_REGISTRATION: Duration = Duration::from_millis(400);

/// Wait for something to become true, or give up and say what we were waiting
/// for. A sleep long enough to be reliable is long enough to make a suite slow.
pub fn wait_until(what: &str, mut pred: impl FnMut() -> bool) {
    let start = Instant::now();
    loop {
        if pred() {
            return;
        }
        if start.elapsed() >= WAIT {
            panic!("timed out after {}s waiting for {what}", WAIT.as_secs());
        }
        std::thread::sleep(POLL);
    }
}

/// Same, but for something that produces a value.
pub fn wait_for<T>(what: &str, mut f: impl FnMut() -> Option<T>) -> T {
    let start = Instant::now();
    loop {
        if let Some(v) = f() {
            return v;
        }
        if start.elapsed() >= WAIT {
            panic!("timed out after {}s waiting for {what}", WAIT.as_secs());
        }
        std::thread::sleep(POLL);
    }
}

/// A running daemon, reaped when it goes out of scope.
pub struct Daemon {
    pub child: Child,
    pub endpoint: Endpoint,
}

impl Daemon {
    /// Start one and wait until it answers.
    ///
    /// The endpoint must carry *this child's* pid before it is believed: a
    /// `daemon.json` left by an earlier test in the same fixture would
    /// otherwise look exactly like success.
    pub fn start(cli: &Cli) -> Self {
        let child = cli.spawn(&[
            "daemon",
            "--foreground",
            "--timeout",
            &TIMEOUT_MS.to_string(),
        ]);
        let pid = child.id();
        let project = cli.fx.project();

        let endpoint = wait_for("the daemon to publish its endpoint", || {
            Endpoint::read(&project).filter(|e| e.pid == pid)
        });
        wait_until("the daemon to answer /health", || {
            get(&endpoint, "/health")
                .map(|r| r.is_ok())
                .unwrap_or(false)
        });
        // Answering /health does not mean the recursive watch is registered.
        // A write that lands inside that window is genuinely lost — the engine
        // recovers on its next floor sweep, but that is far longer than a test
        // waits. `tests/watcher.rs` sleeps here for the same reason.
        std::thread::sleep(WATCH_REGISTRATION);
        Self { child, endpoint }
    }

    pub fn port(&self) -> u16 {
        self.endpoint.port
    }

    pub fn token(&self) -> &str {
        &self.endpoint.token
    }

    pub fn get(&self, path: &str) -> http::Response {
        get(&self.endpoint, path).expect("the daemon should answer")
    }

    /// A request with a token of the caller's choosing, for the auth tests.
    pub fn get_with_token(&self, path: &str, token: &str) -> http::Response {
        http::send(
            self.endpoint.port,
            token,
            "GET",
            path,
            None,
            Duration::from_secs(5),
        )
        .expect("the daemon should answer even when it refuses")
    }

    pub fn post(&self, path: &str, body: &serde_json::Value) -> http::Response {
        let bytes = serde_json::to_vec(body).unwrap();
        http::send(
            self.endpoint.port,
            &self.endpoint.token,
            "POST",
            path,
            Some(&bytes),
            Duration::from_secs(5),
        )
        .expect("the daemon should answer")
    }

    /// Kill it now rather than at the end of the test.
    pub fn kill(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        // `Cli::spawn` has no kill-on-drop, and a leaked daemon holds the engine
        // token the next test needs.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// An open event stream, as an iterator of parsed frames.
///
/// Deliberately not buffered by the test either: `Frames` reads line by line,
/// so a frame that has not left the daemon is a frame this does not see.
pub struct Stream {
    pub frames: http::Frames<std::io::BufReader<std::net::TcpStream>>,
}

impl Daemon {
    /// Subscribe, using the query token the way `EventSource` has to.
    pub fn events(&self) -> Stream {
        use std::io::Write as _;
        let mut s = std::net::TcpStream::connect(("127.0.0.1", self.port())).unwrap();
        s.set_read_timeout(Some(WAIT)).unwrap();
        let req = format!(
            "GET /events?token={} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nConnection: close\r\n\r\n",
            self.token(),
            self.port()
        );
        s.write_all(req.as_bytes()).unwrap();

        let mut r = std::io::BufReader::new(s);
        let (status, headers) = http::read_head(&mut r).unwrap();
        assert_eq!(status, 200, "the stream should open");
        assert_eq!(
            http::header_of(&headers, "Content-Type"),
            Some("text/event-stream")
        );
        Stream {
            frames: http::Frames::new(r),
        }
    }
}

impl Stream {
    /// The next frame of any kind, or `None` once the stream ends.
    ///
    /// For a test that has to see *everything* the engine published rather than
    /// wait for one named frame — a high-water mark across a whole session, say.
    pub fn next_any(&mut self) -> Option<http::Frame> {
        self.frames.next().map(|f| f.expect("a readable frame"))
    }

    /// The next frame with this event name, skipping anything else.
    pub fn next_named(&mut self, name: &str) -> http::Frame {
        for f in self.frames.by_ref() {
            let f = f.expect("a readable frame");
            if f.event.as_deref() == Some(name) {
                return f;
            }
        }
        panic!("the stream ended before a {name} frame arrived");
    }
}

pub fn get(endpoint: &Endpoint, path: &str) -> Option<http::Response> {
    http::send(
        endpoint.port,
        &endpoint.token,
        "GET",
        path,
        None,
        Duration::from_secs(5),
    )
    .ok()
}
