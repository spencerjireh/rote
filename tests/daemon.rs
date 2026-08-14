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

// ------------------------------------------------------------ POST /command

fn skip(id: &str, generation: Option<u64>) -> serde_json::Value {
    let mut v = serde_json::json!({
        "wire_version": state::WIRE_VERSION,
        "verb": "skip",
        "hunk_id": id,
    });
    if let Some(g) = generation {
        v["generation"] = serde_json::json!(g);
    }
    v
}

#[test]
fn a_command_applies_with_or_without_a_generation() {
    let cli = session();
    let d = Daemon::start(&cli);
    wait_until("a first snapshot", || d.get("/state").status == 200);
    let snap: state::Snapshot = d.get("/state").json().unwrap();
    let id = snap.queue[0].id.clone();

    // Carrying the current generation is the careful form.
    let r = d.post("/command", &skip(&id, Some(snap.generation)));
    assert_eq!(r.status, 200, "an outcome is a successful conversation");
    let body: state::Response = r.json().unwrap();
    assert_eq!(body.outcome, state::Outcome::Applied);
    assert!(body.generation > snap.generation, "the write moved it");

    wait_until("the skip to reach the snapshot", || {
        d.get("/state")
            .json::<state::Snapshot>()
            .map(|s| s.counts.skipped == 1)
            .unwrap_or(false)
    });
}

#[test]
fn a_stale_generation_is_refused_with_the_current_one_attached() {
    let cli = session();
    let d = Daemon::start(&cli);
    wait_until("a first snapshot", || d.get("/state").status == 200);
    let snap: state::Snapshot = d.get("/state").json().unwrap();
    let id = snap.queue[0].id.clone();

    let stale = snap.generation.saturating_sub(1);
    let r = d.post("/command", &skip(&id, Some(stale)));
    assert_eq!(r.status, 200);
    let body: state::Response = r.json().unwrap();
    match body.outcome {
        state::Outcome::Stale { current } => {
            assert_eq!(
                current, snap.generation,
                "the current generation comes back, so a client can re-issue \
                 without a round trip to discover it"
            );
        }
        other => panic!("expected stale, got {other:?}"),
    }

    // Nothing was written, and the generation did not move — which is what
    // makes retrying exactly once safe.
    let after: state::Snapshot = d.get("/state").json().unwrap();
    assert_eq!(after.generation, snap.generation);
    assert_eq!(after.counts.skipped, 0);
}

#[test]
fn answering_a_question_about_a_hunk_the_agent_has_since_reworked_does_not_land() {
    // The case the whole generation mechanism exists for. A front end renders a
    // question, the agent reworks that region, and the user answers what they
    // are still looking at — which is no longer what is there.
    let cli = session();
    let d = Daemon::start(&cli);
    wait_until("a first snapshot", || d.get("/state").status == 200);

    // Raise a question: type something of your own and let it settle.
    cli.fx.write("a.rs", "fn a() {\n    my_own_way();\n}\n");
    let asked = common::daemon::wait_for("a question to be raised", || {
        d.get("/state")
            .json::<state::Snapshot>()
            .ok()
            .filter(|s| s.queue.iter().any(|q| q.has_question))
    });
    let id = asked
        .queue
        .iter()
        .find(|q| q.has_question)
        .unwrap()
        .id
        .clone();

    // The agent reworks the region while the question is on screen.
    std::fs::write(
        cli.shadow().join("a.rs"),
        "fn a() {\n    something_else();\n}\n",
    )
    .unwrap();
    wait_until("the rework to move the generation", || {
        d.get("/state")
            .json::<state::Snapshot>()
            .map(|s| s.generation > asked.generation)
            .unwrap_or(false)
    });

    // The user answers what they were looking at.
    let r = d.post(
        "/command",
        &serde_json::json!({
            "wire_version": state::WIRE_VERSION,
            "generation": asked.generation,
            "verb": "resolve",
            "hunk_id": id,
            "choice": "keep",
        }),
    );
    let body: state::Response = r.json().unwrap();
    assert!(
        matches!(body.outcome, state::Outcome::Stale { .. }),
        "an answer to a question that has moved must not land: {:?}",
        body.outcome
    );

    let after: state::Snapshot = d.get("/state").json().unwrap();
    assert_eq!(
        after.counts.diverged, 0,
        "and nothing was recorded as a kept divergence"
    );
}

#[test]
fn a_command_that_cannot_be_carried_out_is_rejected_rather_than_applied() {
    let cli = session();
    let d = Daemon::start(&cli);
    wait_until("a first snapshot", || d.get("/state").status == 200);
    let snap: state::Snapshot = d.get("/state").json().unwrap();
    let id = snap.queue[0].id.clone();

    // No such hunk.
    let body: state::Response = d.post("/command", &skip("h-nope", None)).json().unwrap();
    assert!(matches!(body.outcome, state::Outcome::Rejected { .. }));

    // Resolving a hunk with no open question.
    let body: state::Response = d
        .post(
            "/command",
            &serde_json::json!({
                "wire_version": state::WIRE_VERSION,
                "verb": "resolve",
                "hunk_id": id,
                "choice": "keep",
            }),
        )
        .json()
        .unwrap();
    match body.outcome {
        state::Outcome::Rejected { reason } => assert!(reason.contains("no open question")),
        other => panic!("expected a rejection, got {other:?}"),
    }

    // A wire version this daemon does not speak.
    let body: state::Response = d
        .post(
            "/command",
            &serde_json::json!({"wire_version": 99, "verb": "refresh"}),
        )
        .json()
        .unwrap();
    match body.outcome {
        state::Outcome::Rejected { reason } => assert!(reason.contains("wire version")),
        other => panic!("expected a rejection, got {other:?}"),
    }
}

#[test]
fn a_post_must_be_json_and_must_not_be_enormous() {
    let cli = session();
    let d = Daemon::start(&cli);

    // The three content types a browser form can send are all refused, which is
    // what stops a hostile page firing one at every port on the loopback.
    use std::io::{BufReader, Write};
    let post = |content_type: &str, body: &str| -> u16 {
        let mut s = std::net::TcpStream::connect(("127.0.0.1", d.port())).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let req = format!(
            "POST /command HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nAuthorization: Bearer {}\r\n\
             Content-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            d.port(),
            d.token(),
            body.len()
        );
        s.write_all(req.as_bytes()).unwrap();
        let mut r = BufReader::new(s);
        rote::http::read_head(&mut r).unwrap().0
    };

    assert_eq!(post("application/x-www-form-urlencoded", "{}"), 415);
    assert_eq!(post("text/plain", "{}"), 415);
    assert_eq!(post("application/json", "not json"), 400);
    assert_eq!(
        post("application/json", &"x".repeat(70 * 1024)),
        413,
        "a verb is a few hundred bytes; refuse rather than allocate on trust"
    );
}

// ------------------------------------------------------------ GET /events

#[test]
fn a_small_event_reaches_the_client_immediately() {
    // The regression test for the chunked-buffer trap. tiny_http's chunked path
    // is Encoder::new + io::copy: the encoder buffers 8 KiB, io::copy never
    // flushes, and for an infinite reader it never returns either. A frame this
    // size would sit invisible until eight kilobytes had accumulated. Without
    // this test that bug returns the first time someone "simplifies" the writer
    // back to a Response.
    let cli = session();
    let d = Daemon::start(&cli);
    let mut stream = d.events();

    let started = std::time::Instant::now();
    let frame = stream.next_named("snapshot");
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "a few hundred bytes took {:?} to arrive — it is being buffered",
        started.elapsed()
    );
    assert!(frame.data.len() < 8192, "and it is well under a chunk");

    let snap: state::Snapshot = serde_json::from_str(&frame.data).unwrap();
    assert_eq!(snap.wire_version, state::WIRE_VERSION);
}

#[test]
fn the_first_frame_after_connecting_is_the_whole_world() {
    // Which is why the protocol needs no Last-Event-ID and no resume: a client
    // that attaches late, or reconnects, is never behind.
    let cli = session();
    let d = Daemon::start(&cli);
    wait_until("a first snapshot", || d.get("/state").status == 200);

    let mut stream = d.events();
    let snap: state::Snapshot = serde_json::from_str(&stream.next_named("snapshot").data).unwrap();
    assert_eq!(snap.counts.pending, 1);
    assert!(snap.active.is_some());
}

#[test]
fn typing_the_proposal_pushes_a_frame_without_being_asked() {
    let cli = session();
    let d = Daemon::start(&cli);
    let mut stream = d.events();
    stream.next_named("snapshot"); // the cached one

    cli.fx.write("a.rs", "fn a() {\n    work();\n}\n");

    // Keep reading until the queue drains — there may be intermediate frames.
    for _ in 0..10 {
        let snap: state::Snapshot =
            serde_json::from_str(&stream.next_named("snapshot").data).unwrap();
        if snap.counts.typed == 1 && snap.counts.pending == 0 {
            return;
        }
    }
    panic!("typing the proposal never produced a typed snapshot");
}

#[test]
fn two_clients_see_the_same_thing() {
    let cli = session();
    let d = Daemon::start(&cli);
    let mut a = d.events();
    let mut b = d.events();

    cli.fx.write("a.rs", "fn a() {\n    work();\n}\n");

    let drained = |s: &mut common::daemon::Stream| {
        for _ in 0..10 {
            let snap: state::Snapshot =
                serde_json::from_str(&s.next_named("snapshot").data).unwrap();
            if snap.counts.typed == 1 {
                return snap;
            }
        }
        panic!("no typed snapshot");
    };
    let sa = drained(&mut a);
    let sb = drained(&mut b);
    assert_eq!(sa.generation, sb.generation);
    assert_eq!(sa.counts, sb.counts);

    // And the daemon knows how many are listening.
    let health: state::Health = d.get("/health").json().unwrap();
    assert_eq!(health.subscribers, 2);
}

#[test]
fn a_closing_session_says_so_on_the_stream_before_the_daemon_goes() {
    // A pane must be able to tell "the session ended" from "the daemon
    // vanished" — they call for different sentences and different exit codes.
    let cli = session();
    let d = Daemon::start(&cli);
    let mut stream = d.events();
    stream.next_named("snapshot");

    let out = cli.run_with_input(&["done", "--force", "--no-checks", "--no-review"], "y\n");
    assert!(out.status.success(), "{}", stderr(&out));

    let frame = stream.next_named("closed");
    let ev: state::Event = serde_json::from_str(&frame.data).unwrap();
    assert!(matches!(ev, state::Event::Closed { .. }), "got {ev:?}");
}

#[test]
fn a_reader_that_goes_away_is_pruned() {
    let cli = session();
    let d = Daemon::start(&cli);
    {
        let mut s = d.events();
        s.next_named("snapshot");
        let health: state::Health = d.get("/health").json().unwrap();
        assert_eq!(health.subscribers, 1);
    } // the socket closes here

    // Pruning is deliberately lazy: there is no unsubscribe message, so the hub
    // only learns a client is gone when a send to it fails. That takes two
    // published frames — the first wakes the stream thread, which discovers the
    // broken socket on its write and drops the receiver; the second finds the
    // channel closed. One mechanism, and no bookkeeping that can leak.
    //
    // It also means the count in /health is as of the last publish, not as of
    // now: an idle daemon publishes nothing, so nothing prunes.
    wait_until("the subscriber to be pruned", || {
        d.post(
            "/command",
            &serde_json::json!({"wire_version": state::WIRE_VERSION, "verb": "refresh"}),
        );
        d.get("/health")
            .json::<state::Health>()
            .map(|h| h.subscribers == 0)
            .unwrap_or(false)
    });
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
