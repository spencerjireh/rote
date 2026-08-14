//! The daemon, over a real socket.
//!
//! Every test bounds its daemon with `--timeout` and reaps it on drop, so a
//! wedged one fails the suite rather than outliving the machine running it.

mod common;

use common::cli::{stderr, Cli};
use common::daemon::{wait_until, Daemon};
use common::Fixture;
use rote::daemon::Endpoint;
use rote::state;
use std::time::Duration;

fn session() -> Cli {
    let fx = Fixture::new();
    fx.write("a.rs", "fn a() {\n}\n");
    fx.commit_all("initial");
    let cli = Cli::with_fixture(fx);
    let out = cli.run(&["start", "--no-launch", "add work"]);
    assert!(out.status.success(), "{}", stderr(&out));
    std::fs::write(cli.shadow().join("a.rs"), "fn a() {\n    work();\n}\n").unwrap();
    cli
}

#[test]
fn health_reports_the_project_and_the_wire_version() {
    let cli = session();
    let d = Daemon::start(&cli);

    let health: state::Health = d.get("/health").json().unwrap();
    assert!(health.ok);
    assert_eq!(health.wire_version, state::WIRE_VERSION);
    assert_eq!(health.project_hash, cli.fx.project().hash);
    assert_eq!(health.pid, d.child.id());
    assert_eq!(health.port, d.port());
    assert_eq!(health.task, "add work");
    assert!(health.session_state.is_some());
}

#[test]
fn a_request_without_a_usable_token_is_refused() {
    let cli = session();
    let d = Daemon::start(&cli);

    assert_eq!(d.get_with_token("/health", "").status, 401);
    assert_eq!(d.get_with_token("/health", "not-the-token").status, 401);
    // A token of the right length but the wrong value, so the refusal cannot be
    // a length check standing in for a comparison.
    let wrong = "0".repeat(d.token().len());
    assert_eq!(d.get_with_token("/health", &wrong).status, 401);
    // And the real one still works.
    assert_eq!(d.get("/health").status, 200);
}

#[test]
fn a_foreign_origin_or_host_is_refused() {
    // The DNS-rebinding defence. A hostile page can point its own name at
    // 127.0.0.1, but it cannot change the Host header the browser sends.
    let cli = session();
    let d = Daemon::start(&cli);

    let raw = |extra: &str| -> u16 {
        use std::io::{BufReader, Write};
        let mut s = std::net::TcpStream::connect(("127.0.0.1", d.port())).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let req = format!(
            "GET /health HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nAuthorization: Bearer {}\r\n\
             {extra}Connection: close\r\n\r\n",
            d.port(),
            d.token()
        );
        s.write_all(req.as_bytes()).unwrap();
        let mut r = BufReader::new(s);
        rote::http::read_head(&mut r).unwrap().0
    };

    assert_eq!(raw(""), 200, "the control");
    assert_eq!(raw("Origin: http://evil.com\r\n"), 403);
    assert_eq!(
        raw("Origin: http://127.0.0.1.evil.com\r\n"),
        403,
        "a prefix must not be enough"
    );

    // A forged Host, sent instead of the honest one.
    use std::io::{BufReader, Write};
    let mut s = std::net::TcpStream::connect(("127.0.0.1", d.port())).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let req = format!(
        "GET /health HTTP/1.1\r\nHost: evil.com\r\nAuthorization: Bearer {}\r\n\
         Connection: close\r\n\r\n",
        d.token()
    );
    s.write_all(req.as_bytes()).unwrap();
    let mut r = BufReader::new(s);
    assert_eq!(rote::http::read_head(&mut r).unwrap().0, 403);
}

#[test]
fn state_warms_up_and_then_serves_the_engines_view() {
    let cli = session();
    let d = Daemon::start(&cli);

    // The engine publishes on its very first step, so the warming-up window is
    // sub-second — but it exists, and 503 is the honest answer inside it.
    wait_until("the engine to publish a snapshot", || {
        d.get("/state").status == 200
    });

    let snap: state::Snapshot = d.get("/state").json().unwrap();
    assert_eq!(snap.wire_version, state::WIRE_VERSION);
    assert_eq!(snap.counts.pending, 1);
    let active = snap.active.expect("an active hunk");
    assert_eq!(active.hunk.new_lines, vec!["    work();"]);
    // Resolved by the engine against the real file, so a browser front end
    // needs no filesystem of its own.
    assert_eq!(active.anchor_line, 2);
    assert!(active.real_path.ends_with("a.rs"));
}

#[test]
fn the_snapshot_carries_drift_which_no_stateless_handler_could_produce() {
    // drift lives only in the engine — it comes out of a recompute report and
    // is stored nowhere. A handler that re-derived a snapshot per request would
    // have to shell out to git to know it, and could disagree with what every
    // subscriber was just told.
    let cli = session();
    let d = Daemon::start(&cli);
    wait_until("a first snapshot", || d.get("/state").status == 200);
    let before: state::Snapshot = d.get("/state").json().unwrap();
    assert!(!before.drift);

    // Move HEAD under the session.
    cli.fx.write("other.rs", "fn other() {}\n");
    cli.fx.commit_all("unrelated work");

    wait_until("the engine to notice the repository moved", || {
        d.get("/state")
            .json::<state::Snapshot>()
            .map(|s| s.drift)
            .unwrap_or(false)
    });
}

#[test]
fn a_hunk_can_be_fetched_by_id_with_its_anchor() {
    let cli = session();
    let d = Daemon::start(&cli);
    wait_until("a first snapshot", || d.get("/state").status == 200);
    let snap: state::Snapshot = d.get("/state").json().unwrap();
    let id = snap.queue[0].id.clone();

    let detail: state::HunkDetail = d.get(&format!("/hunk/{id}")).json().unwrap();
    assert_eq!(detail.hunk.id, id);
    assert_eq!(detail.hunk.new_lines, vec!["    work();"]);
    assert_eq!(detail.anchor_line, 2);
    assert!(detail.real_path.ends_with("a.rs"));

    // The queue entry deliberately carries no line bodies; this is the other
    // half of that trade.
    assert_eq!(d.get("/hunk/h-nosuchhunk").status, 404);
}

#[test]
fn unknown_paths_and_methods_are_refused_in_the_conventional_way() {
    let cli = session();
    let d = Daemon::start(&cli);

    assert_eq!(d.get("/nope").status, 404);

    use std::io::{BufReader, Write};
    let mut s = std::net::TcpStream::connect(("127.0.0.1", d.port())).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let req = format!(
        "DELETE /state HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nAuthorization: Bearer {}\r\n\
         Connection: close\r\n\r\n",
        d.port(),
        d.token()
    );
    s.write_all(req.as_bytes()).unwrap();
    let mut r = BufReader::new(s);
    let (status, headers) = rote::http::read_head(&mut r).unwrap();
    assert_eq!(status, 405);
    assert!(rote::http::header_of(&headers, "Allow").is_some());
}

#[test]
fn every_response_carries_a_content_length() {
    // The daemon never chunk-encodes anything but the event stream, which is
    // what lets the hand-rolled client refuse chunked outright.
    let cli = session();
    let d = Daemon::start(&cli);
    for path in ["/health", "/state", "/nope"] {
        let r = d.get(path);
        assert!(
            r.header("Content-Length").is_some(),
            "{path} had no Content-Length"
        );
        assert!(
            r.header("Transfer-Encoding").is_none(),
            "{path} was chunked"
        );
    }
}

#[test]
fn a_second_daemon_refuses_because_the_first_owns_the_engine() {
    // The single-engine invariant, from the daemon's side. Two engines do not
    // duplicate work, they corrupt each other.
    let cli = session();
    let _d = Daemon::start(&cli);

    let out = cli.run(&["daemon", "--foreground", "--timeout", "3000"]);
    assert!(!out.status.success());
    let err = stderr(&out);
    assert!(
        err.contains("already owns this project's queue"),
        "and says what is holding it: {err}"
    );
}

#[test]
fn the_daemon_exits_when_the_session_is_archived() {
    // `rote done` deletes session.json and then storms both watchers with a
    // `git clean`. That must read as a clean exit, not a crash.
    let cli = session();
    let mut d = Daemon::start(&cli);
    wait_until("a first snapshot", || d.get("/state").status == 200);

    let out = cli.run_with_input(&["done", "--force", "--no-checks", "--no-review"], "y\n");
    assert!(out.status.success(), "{}", stderr(&out));

    wait_until("the daemon to notice and exit", || {
        d.child.try_wait().ok().flatten().is_some()
    });
    let status = d.child.try_wait().unwrap().unwrap();
    assert!(
        status.success(),
        "an archived session is a clean exit, not a failure"
    );
    assert!(
        Endpoint::read(&cli.fx.project()).is_none(),
        "and it takes its address book with it"
    );
}
