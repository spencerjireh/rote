//! The daemon, over a real socket.
//!
//! Every test bounds its daemon with `--timeout` and reaps it on drop, so a
//! wedged one fails the suite rather than outliving the machine running it.

mod common;

use common::cli::{stderr, stdout, Cli};
use common::daemon::{wait_for, wait_until, Daemon};
use common::Fixture;
use rote::daemon::Endpoint;
use rote::http;
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
    for path in ["/", "/health", "/state", "/nope"] {
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
    // Both halves asserted deliberately: `cause` is what a client branches on,
    // and `reason` is the human text, which older clients read and which must
    // not have moved when the variant was introduced.
    match body.outcome {
        state::Outcome::Rejected { reason, cause } => {
            assert!(reason.contains("no open question"));
            assert_eq!(cause, Some(state::Cause::NoOpenQuestion));
        }
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
        state::Outcome::Rejected { reason, cause } => {
            assert!(reason.contains("wire version"));
            assert_eq!(cause, Some(state::Cause::WireVersion));
        }
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
fn the_stream_takes_its_token_either_way() {
    // The nvim plugin writes its own request line over a raw socket, so it sends
    // the header — a secret in a request line is what proxies and access logs
    // record. The query spelling stays for the two clients that cannot set a
    // header at all: an `EventSource` and a browser address bar (§13).
    let cli = session();
    let d = Daemon::start(&cli);

    let status = |auth_line: &str, query: &str| -> u16 {
        use std::io::{BufReader, Write};
        let mut s = std::net::TcpStream::connect(("127.0.0.1", d.port())).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let req = format!(
            "GET /events{query} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\n{auth_line}\
             Connection: close\r\n\r\n",
            d.port()
        );
        s.write_all(req.as_bytes()).unwrap();
        let mut r = BufReader::new(s);
        rote::http::read_head(&mut r).unwrap().0
    };

    // Exactly the request `lua/rote/stream.lua` builds.
    assert_eq!(
        status(&format!("Authorization: Bearer {}\r\n", d.token()), ""),
        200,
        "the header spelling, which the plugin uses"
    );
    assert_eq!(
        status("", &format!("?token={}", d.token())),
        200,
        "the query spelling, which a browser needs"
    );
    assert_eq!(status("", ""), 401, "and neither is not enough");
}

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

// ------------------------------------------------------------ spawn and reap

#[test]
fn start_leaves_a_daemon_watching_in_the_background() {
    let fx = Fixture::new();
    fx.write("a.rs", "fn a() {\n}\n");
    fx.commit_all("initial");
    let cli = Cli::with_fixture(fx);
    // A claude that exits immediately, so `start` runs its whole path
    // including the exec.
    let (stub, _) = common::cli::stub_claude(cli.fx.root.path(), "claude-stub", "", 0);
    cli.use_claude_stub(&stub);

    let out = cli.run(&["start", "add work"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(
        stdout(&out).contains("watching in the background"),
        "{}",
        stdout(&out)
    );

    let ep = common::daemon::wait_for("a live daemon", || {
        rote::daemon::discover(&cli.fx.project())
    });
    // And it survived the process that started it exec'ing away.
    assert!(rote::lockfile::pid_is_live(ep.pid));

    // Clean up: nothing else in this test reaps it.
    let _ = cli.run_with_input(&["abort", "--yes"], "");
    common::daemon::wait_until("the daemon to be reaped", || {
        !rote::lockfile::pid_is_live(ep.pid)
    });
}

#[test]
fn daemon_without_foreground_detaches_and_returns() {
    // `foreground` was destructured and thrown away, so `rote daemon` always
    // served inline — while `spawn_detached` invokes `rote daemon --foreground`,
    // which is the evidence the flag always meant this.
    let cli = session();
    let project = cli.fx.project();

    let out = cli.run(&["daemon", "--timeout", "30000"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(
        stdout(&out).contains("rote daemon listening on http://127.0.0.1:"),
        "{}",
        stdout(&out)
    );

    let ep = wait_for("the detached daemon to answer", || {
        rote::daemon::discover(&project)
    });
    assert_ne!(ep.pid, std::process::id(), "it is not this process");

    // Cleaned up the way anything else would: through the session ending.
    assert!(cli.run(&["abort", "--yes"]).status.success());
}

#[test]
fn no_launch_does_not_spawn_a_daemon() {
    // Every existing test uses `--no-launch`. Spawning there would leak a
    // background process into the whole suite.
    let cli = session();
    assert!(
        rote::daemon::Endpoint::read(&cli.fx.project()).is_none(),
        "a session prepared without an agent needs no daemon"
    );
}

#[test]
fn done_reaps_the_daemon_and_tells_it_why_before_the_shadow_is_reset() {
    let cli = session();
    let mut d = Daemon::start(&cli);
    let mut stream = d.events();
    stream.next_named("snapshot");

    let out = cli.run_with_input(&["done", "--force", "--no-checks", "--no-review"], "y\n");
    assert!(out.status.success(), "{}", stderr(&out));

    // The pane learns *how* it ended, which is what separates "session closed"
    // from "the daemon vanished".
    let ev: state::Event = serde_json::from_str(&stream.next_named("closed").data).unwrap();
    match ev {
        state::Event::Closed { terminal } => assert_eq!(
            terminal,
            Some(rote::session::Terminal::Done),
            "the reaper is the only thing that knows the reason"
        ),
        other => panic!("expected closed, got {other:?}"),
    }

    // And it had already exited before `done` returned, so the `git clean` that
    // resets the shadow landed on a tree nobody was watching.
    //
    // Asserted through `try_wait` rather than `pid_is_live`: here the daemon is
    // a direct child of the test process, so after exiting it is a zombie until
    // this reaps it, and `kill(pid, 0)` cannot tell a zombie from a live
    // process. In use the daemon is `setsid`-detached and reparented, so the
    // process running `rote done` sees a real exit.
    let status = d
        .child
        .try_wait()
        .expect("waitable")
        .expect("the daemon should already have exited");
    assert!(status.success(), "and exited cleanly: {status:?}");
    assert!(rote::daemon::Endpoint::read(&cli.fx.project()).is_none());
}

#[test]
fn a_subscriber_that_is_behind_is_still_told_the_session_closed() {
    // "Tell everyone before going, so a pane can say 'session closed' rather than
    // 'the daemon vanished'" was best-effort in two ways: the terminal frame went
    // out through the same `try_send` that drops a subscriber for being behind,
    // and the writer threads were detached, so the process could exit with the
    // frame still sitting in a channel.
    //
    // This subscribes, then reads nothing at all while the session is closed
    // underneath it — a pane whose terminal is scrolled, or simply descheduled.
    let cli = session();
    cli.fx.write("b.rs", "fn b() {\n}\n");
    cli.fx.commit_all("second file");
    std::fs::write(cli.shadow().join("b.rs"), "fn b() {\n    more();\n}\n").unwrap();

    let d = Daemon::start(&cli);
    let mut stream = d.events();
    wait_until("both hunks to be queued", || {
        d.get("/state")
            .json::<state::Snapshot>()
            .map(|s| s.counts.pending == 2)
            .unwrap_or(false)
    });

    // Not read until after `done` has been and gone.
    let out = cli.run_with_input(&["done", "--force", "--no-checks", "--no-review"], "y\n");
    assert!(out.status.success(), "{}", stderr(&out));

    let ev: state::Event = serde_json::from_str(&stream.next_named("closed").data).unwrap();
    assert!(
        matches!(
            ev,
            state::Event::Closed {
                terminal: Some(rote::session::Terminal::Done)
            }
        ),
        "got {ev:?}"
    );
}

#[test]
fn abort_reaps_with_the_reason_it_was_aborted() {
    let cli = session();
    let d = Daemon::start(&cli);
    let mut stream = d.events();
    stream.next_named("snapshot");

    let out = cli.run(&["abort", "--yes"]);
    assert!(out.status.success(), "{}", stderr(&out));

    let ev: state::Event = serde_json::from_str(&stream.next_named("closed").data).unwrap();
    match ev {
        state::Event::Closed { terminal } => {
            assert_eq!(terminal, Some(rote::session::Terminal::Aborted))
        }
        other => panic!("expected closed, got {other:?}"),
    }
}

#[test]
fn declining_to_close_leaves_the_daemon_alone() {
    // The reap sits below every early return, so "left the session open" never
    // takes the daemon with it. That is satisfied by placement rather than by a
    // flag someone can forget.
    let cli = session();
    let d = Daemon::start(&cli);
    let pid = d.endpoint.pid;

    let out = cli.run_with_input(&["done", "--no-checks", "--no-review"], "n\n");
    assert!(
        stdout(&out).contains("left the session open"),
        "{}",
        stdout(&out)
    );

    assert!(
        rote::lockfile::pid_is_live(pid),
        "the daemon should still be up"
    );
    assert_eq!(d.get("/health").status, 200);
}

#[test]
fn a_stale_endpoint_from_a_dead_daemon_is_not_believed() {
    let cli = session();
    let project = cli.fx.project();
    project.ensure_state_dir().unwrap();

    // A pid that is genuinely gone, on a port nothing is listening to.
    let mut child = std::process::Command::new("true").spawn().unwrap();
    let dead = child.id();
    child.wait().unwrap();
    Endpoint::new(&project, dead, 1, "deadbeef".into())
        .write(&project)
        .unwrap();

    assert!(
        rote::daemon::discover(&project).is_none(),
        "an address book entry is not evidence that anything is there"
    );
}

#[test]
fn discover_ignores_a_port_answering_for_another_project() {
    // The gate the hash in the *file* cannot provide, since that one is true by
    // construction. A daemon that died frees its port and anything may take it,
    // so only the hash in the reply proves the listener is ours.
    let mine = session();
    let theirs = session();
    let project = mine.fx.project();
    project.ensure_state_dir().unwrap();
    let other = Daemon::start(&theirs);

    // My address book, pointing at their live daemon.
    Endpoint::new(
        &project,
        other.endpoint.pid,
        other.port(),
        other.token().into(),
    )
    .write(&project)
    .unwrap();

    assert!(
        rote::daemon::discover(&project).is_none(),
        "something answered, but not for this project"
    );
}

#[test]
fn reap_refuses_to_signal_a_pid_belonging_to_another_project() {
    // After a kill -9 and a reboot, a stale file can name a pid that now
    // belongs to something else entirely.
    let cli = session();
    let project = cli.fx.project();
    project.ensure_state_dir().unwrap();

    let mut victim = std::process::Command::new("sleep")
        .arg("30")
        .spawn()
        .unwrap();
    let pid = victim.id();

    let mut ep = Endpoint::new(&project, pid, 1, "deadbeef".into());
    ep.project_hash = "0123456789ab".into(); // a different project
    ep.write(&project).unwrap();

    rote::daemon::reap(&project, Some(rote::session::Terminal::Done));

    // Through `try_wait`, not `pid_is_live`. A signalled child of this process
    // becomes a zombie until it is waited on, and a zombie still answers
    // `kill(pid, 0)` — so `pid_is_live` cannot tell "survived" from "killed a
    // moment ago", and this assertion held either way.
    assert!(
        victim.try_wait().unwrap().is_none(),
        "rote must not kill a process it cannot prove is its own"
    );
    let _ = victim.kill();
    let _ = victim.wait();
    let _ = pid;
}

#[test]
fn reap_does_not_signal_a_recycled_pid() {
    // The hazard §13 names, and the case the project-hash check cannot reach: the
    // file names *this* project, so it is entirely plausible. What makes it stale
    // is that no engine holds the token — the endpoint file survives a `kill -9`
    // and the flock does not, which is precisely what tells the two apart.
    let cli = session();
    let project = cli.fx.project();
    project.ensure_state_dir().unwrap();

    let mut victim = std::process::Command::new("sleep")
        .arg("30")
        .spawn()
        .unwrap();
    let pid = victim.id();

    Endpoint::new(&project, pid, 1, "deadbeef".into())
        .write(&project)
        .unwrap();

    rote::daemon::reap(&project, Some(rote::session::Terminal::Done));

    assert!(
        victim.try_wait().unwrap().is_none(),
        "nothing held the engine token, so that pid is a stranger"
    );
    let _ = victim.kill();
    let _ = victim.wait();
    let _ = pid;
}

#[test]
fn reap_still_stops_an_owner_that_answers_nothing() {
    // Why the proof is the flock and not `/health`. An owner that holds the engine
    // token but answers no HTTP is the `Owner::Opaque` case, and it has to stay
    // stoppable: `done` is about to `git clean -fdx` the shadow underneath it.
    // Requiring a reply would leave it running, which is worse than the bug the
    // guard above fixes.
    let cli = session();
    let project = cli.fx.project();
    let mut owner = cli.spawn(&["watch", "--local", "--headless", "--timeout", "30000"]);

    let holder = wait_for("the pane to take the engine token", || {
        rote::lockfile::read_lock_holder(&project.watch_lock_path())
    });
    // An endpoint naming that pid, on a port nothing is listening to.
    Endpoint::new(&project, holder, 1, "deadbeef".into())
        .write(&project)
        .unwrap();

    rote::daemon::reap(&project, Some(rote::session::Terminal::Done));

    // `reap` waits for the exit itself, so this returns at once. Asserted through
    // `wait` rather than `pid_is_live`, which cannot see the difference: a signalled
    // child of this process is a zombie until it is reaped, and a zombie still
    // answers `kill(pid, 0)`.
    let status = owner
        .try_wait()
        .unwrap()
        .expect("the token holder must be stopped even though it never answered");
    assert!(
        std::os::unix::process::ExitStatusExt::signal(&status).is_some(),
        "and stopped by a signal, not by its own timeout: {status:?}"
    );
}

// ------------------------------------------------------------ verb routing

#[test]
fn skip_routes_through_the_daemon_when_one_owns_the_queue() {
    let cli = session();
    let d = Daemon::start(&cli);
    let mut stream = d.events();
    stream.next_named("snapshot");

    let out = cli.run(&["skip"]);
    assert!(out.status.success(), "{}", stderr(&out));

    // The proof that it went through the daemon rather than around it: an
    // attached subscriber saw the result.
    for _ in 0..10 {
        let snap: state::Snapshot =
            serde_json::from_str(&stream.next_named("snapshot").data).unwrap();
        if snap.counts.skipped == 1 {
            return;
        }
    }
    panic!("the skip never reached a subscriber, so it did not go through the daemon");
}

#[test]
fn skip_mutates_directly_when_nothing_owns_the_queue() {
    // No daemon, no pane: the CLI takes the engine token itself and writes.
    let cli = session();
    let out = cli.run(&["skip"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(stdout(&out).contains("skipped — a.rs"), "{}", stdout(&out));

    let status = stdout(&cli.run(&["status"]));
    assert!(status.contains("1 skipped"), "{status}");
}

#[test]
fn a_verb_refuses_when_something_owns_the_queue_but_answers_nothing() {
    // A `rote watch --local` pane, or a wedged daemon. Writing around it is
    // exactly what corrupts an engine's view of the world.
    let cli = session();
    let project = cli.fx.project();
    project.ensure_state_dir().unwrap();
    let _held = rote::lockfile::Lock::try_acquire(&project.watch_lock_path())
        .unwrap()
        .expect("the token should be free");

    let out = cli.run(&["skip"]);
    assert!(!out.status.success());
    let err = stderr(&out);
    assert!(
        err.contains("owns this project's queue") && err.contains("not answering"),
        "{err}"
    );
}

/// The on-disk generation, which a bypassing writer moves and the daemon does not
/// learn about — nothing under the state directory reaches the watcher.
fn disk_generation(cli: &Cli) -> u64 {
    let m: serde_json::Value =
        serde_json::from_slice(&std::fs::read(cli.fx.project().session_json()).unwrap()).unwrap();
    m["generation"].as_u64().unwrap()
}

#[test]
fn next_routes_its_write_through_the_daemon() {
    // It called `with_session_recomputed` directly: a second recompute racing the
    // engine's, and a `last_presented` write behind its back (§13). `last_presented`
    // is not on the wire, so what proves the routing is that the engine *published*
    // the write — a direct writer moves the file and leaves `/state` behind.
    let cli = session();
    let d = Daemon::start(&cli);
    let engines_view: state::Snapshot = d.get("/state").json().unwrap();
    let id = engines_view.active.expect("an active hunk").hunk.id;

    let out = cli.run(&["next"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(stdout(&out).contains("+     work();"), "{}", stdout(&out));

    let after: state::Snapshot = d.get("/state").json().unwrap();
    assert_eq!(
        after.generation,
        disk_generation(&cli),
        "the engine and the file must agree: a write it did not make leaves it behind"
    );
    let manifest: serde_json::Value =
        serde_json::from_slice(&std::fs::read(cli.fx.project().session_json()).unwrap()).unwrap();
    assert_eq!(manifest["last_presented"], id, "and the write did land");

    // Which is also what makes `show` with no id work afterwards.
    let shown = stdout(&cli.run(&["show"]));
    assert!(shown.contains("+     work();"), "{shown}");
}

#[test]
fn done_skips_pending_hunks_through_the_daemon() {
    // The bulk skip wrote every pending hunk to `Skipped` with `with_session`,
    // while the daemon was alive — `reap` is at step 6, below every early return.
    // A subscriber seeing the skips is what proves they went through the engine.
    let cli = session();
    cli.fx.write("b.rs", "fn b() {\n}\n");
    cli.fx.commit_all("second file");
    std::fs::write(cli.shadow().join("b.rs"), "fn b() {\n    more();\n}\n").unwrap();

    let d = Daemon::start(&cli);
    let mut stream = d.events();
    wait_until("both hunks to be queued", || {
        d.get("/state")
            .json::<state::Snapshot>()
            .map(|s| s.counts.pending == 2)
            .unwrap_or(false)
    });

    let out = cli.run_with_input(&["done", "--force", "--no-checks", "--no-review"], "y\n");
    assert!(out.status.success(), "{}", stderr(&out));

    // Read to the end of the stream, tracking the most the engine ever published.
    let mut high_water = 0;
    while let Some(f) = stream.next_any() {
        match f.event.as_deref() {
            Some("snapshot") => {
                let s: state::Snapshot = serde_json::from_str(&f.data).unwrap();
                high_water = high_water.max(s.counts.skipped);
            }
            Some("closed") => {
                let ev: state::Event = serde_json::from_str(&f.data).unwrap();
                assert!(
                    matches!(
                        ev,
                        state::Event::Closed {
                            terminal: Some(rote::session::Terminal::Done)
                        }
                    ),
                    "the reap must still happen, and still carry the reason: {ev:?}"
                );
                break;
            }
            _ => {}
        }
    }
    assert_eq!(
        high_water, 2,
        "both skips must reach a subscriber, or they went around the engine"
    );

    // And the archive records them, which is what `done` is for.
    let manifest = std::fs::read_dir(cli.fx.project().archive_dir())
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .find(|p| {
            p.extension().is_some_and(|x| x == "json") && !p.to_string_lossy().contains("curator")
        })
        .expect("an archived manifest");
    let m: serde_json::Value = serde_json::from_slice(&std::fs::read(manifest).unwrap()).unwrap();
    let skipped = m["hunks"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|h| h["status"] == "skipped")
        .count();
    assert_eq!(skipped, 2, "{m:#}");
}

#[test]
fn done_refuses_when_something_owns_the_queue_but_answers_nothing() {
    let cli = session();
    let project = cli.fx.project();
    project.ensure_state_dir().unwrap();
    let _held = rote::lockfile::Lock::try_acquire(&project.watch_lock_path())
        .unwrap()
        .expect("the token should be free");

    let out = cli.run_with_input(&["done", "--no-checks", "--no-review"], "y\ny\n");
    assert!(!out.status.success());
    let err = stderr(&out);
    assert!(
        err.contains("owns this project's queue") && err.contains("not answering"),
        "{err}"
    );
}

#[test]
fn next_refuses_when_something_owns_the_queue_but_answers_nothing() {
    let cli = session();
    let project = cli.fx.project();
    project.ensure_state_dir().unwrap();
    let _held = rote::lockfile::Lock::try_acquire(&project.watch_lock_path())
        .unwrap()
        .expect("the token should be free");

    let out = cli.run(&["next"]);
    assert!(!out.status.success());
    let err = stderr(&out);
    assert!(
        err.contains("owns this project's queue") && err.contains("not answering"),
        "{err}"
    );
}

#[test]
fn answering_retry_through_the_daemon_does_not_get_re_asked() {
    // The Stage-1 bug this step closes. Clearing pending_divergence is only
    // half of answering: a live engine also has to disarm its watchdog, or it
    // re-raises the identical question a couple of seconds later.
    let cli = session();
    let d = Daemon::start(&cli);
    cli.fx.write("a.rs", "fn a() {\n    my_own_way();\n}\n");
    let asked = common::daemon::wait_for("a question", || {
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

    let out = cli.run(&["resolve", &id, "retry"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(
        stdout(&out).contains("withdrew the question"),
        "{}",
        stdout(&out)
    );

    // Well past the grace window: it must stay withdrawn.
    std::thread::sleep(Duration::from_millis(3500));
    let snap: state::Snapshot = d.get("/state").json().unwrap();
    assert!(
        !snap.queue.iter().any(|q| q.has_question),
        "an answered question must not come back"
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

// ------------------------------------------------------------------ curator

/// A session whose queue is worth curating, with a stub that will answer.
///
/// Two files so the model's order is visibly different from file order, and
/// `enable_curator` because every fixture in this suite runs with the curator
/// off — the default `claude_cmd` would otherwise resolve to the developer's
/// real binary and every daemon test would spend tokens.
fn curated_session(reply: &str) -> Cli {
    let fx = Fixture::new();
    fx.write("a.rs", "fn a() {\n}\n");
    fx.write("b.rs", "fn b() {\n}\n");
    fx.commit_all("initial");
    let mut cli = Cli::with_fixture(fx);

    let stub = cli.bin.join("curator-stub");
    common::cli::write_exec(
        &stub,
        &format!("#!/bin/sh\ncat > /dev/null\nprintf '%s' '{reply}'\n"),
    );
    cli.enable_curator(&stub);

    let out = cli.run(&["start", "--no-launch", "add work"]);
    assert!(out.status.success(), "{}", stderr(&out));
    std::fs::write(cli.shadow().join("a.rs"), "fn a() {\n    one();\n}\n").unwrap();
    std::fs::write(cli.shadow().join("b.rs"), "fn b() {\n    two();\n}\n").unwrap();
    cli
}

#[test]
fn the_curators_order_and_notes_reach_the_snapshot() {
    // The whole point, seen from where a front end sees it: the queue arrives
    // already ordered, and each entry carries its line.
    let cli = curated_session(
        r#"{"order":[{"hunk":2,"note":"needed by a"},{"hunk":1,"note":"uses b"}]}"#,
    );
    let d = Daemon::start(&cli);

    wait_until("the curation to reach the snapshot", || {
        d.get("/state")
            .json::<state::Snapshot>()
            .map(|s| s.queue.iter().any(|q| q.curator_note.is_some()))
            .unwrap_or(false)
    });

    let snap: state::Snapshot = d.get("/state").json().unwrap();
    let files: Vec<&str> = snap.queue.iter().map(|q| q.file.as_str()).collect();
    assert_eq!(
        files,
        vec!["b.rs", "a.rs"],
        "teaching order, not file order"
    );
    assert_eq!(snap.queue[0].curator_note.as_deref(), Some("needed by a"));
    assert_eq!(snap.queue[1].curator_note.as_deref(), Some("uses b"));
}

#[test]
fn a_daemon_with_the_curator_off_never_runs_the_model() {
    // `session()` leaves it off, which is the default for every fixture here.
    // If that ever stops being true, this suite starts spending money quietly,
    // so the assertion is on the stub never having been invoked at all.
    let cli = session();
    let marker = cli.bin.join("claude");
    common::cli::write_exec(
        &marker,
        &format!(
            "#!/bin/sh\ntouch {}\n",
            cli.fx.root.path().join("model-was-called").display()
        ),
    );

    let d = Daemon::start(&cli);
    wait_until("a first snapshot", || d.get("/state").status == 200);
    std::thread::sleep(Duration::from_millis(600));

    assert!(
        !cli.fx.root.path().join("model-was-called").exists(),
        "a test suite must never reach a model on its own"
    );
}

// ------------------------------------------------------------- how it came

#[test]
fn a_reported_paste_reaches_the_manifest_through_the_socket() {
    let cli = session();
    let d = Daemon::start(&cli);
    wait_until("a first snapshot", || d.get("/state").status == 200);

    let snap: state::Snapshot = d.get("/state").json().unwrap();
    let id = snap.active.expect("an active hunk").hunk.id;

    let r = d.post(
        "/command",
        &serde_json::json!({
            "wire_version": state::WIRE_VERSION,
            "verb": "report",
            "hunk_id": id,
            "input": "pasted",
        }),
    );
    assert_eq!(r.status, 200);
    let body: state::Response = r.json().unwrap();
    assert!(matches!(body.outcome, state::Outcome::Applied));

    wait_until("the report to reach a snapshot", || {
        d.get("/state")
            .json::<state::Snapshot>()
            .ok()
            .and_then(|s| s.active)
            .map(|p| p.hunk.input == rote::hunks::Input::Pasted)
            .unwrap_or(false)
    });

    // And it moved nothing else.
    let after: state::Snapshot = d.get("/state").json().unwrap();
    assert_eq!(after.counts.pending, snap.counts.pending);
    assert_eq!(after.counts.typed, 0);
}

// ------------------------------------------------------------ the browser app

#[test]
fn the_root_path_serves_the_browser_app() {
    let cli = session();
    let d = Daemon::start(&cli);

    let r = d.get("/");
    assert_eq!(r.status, 200);
    assert_eq!(
        r.header("Content-Type"),
        Some("text/html; charset=utf-8"),
        "so a browser renders it rather than downloading it"
    );
    assert!(String::from_utf8_lossy(&r.body).contains("<title>"));
}

#[test]
fn the_page_is_refused_without_a_token_and_served_with_a_query_one() {
    // A browser address bar cannot set an Authorization header, so the page
    // takes the token the way the event stream does.
    let cli = session();
    let d = Daemon::start(&cli);

    assert_eq!(d.get_with_token("/", "wrong").status, 401);

    let r = http::send(
        d.port(),
        "",
        "GET",
        &format!("/?token={}", d.token()),
        None,
        Duration::from_secs(5),
    )
    .unwrap();
    assert_eq!(r.status, 200, "the query token opens the page");
}

#[test]
fn the_query_token_still_buys_nothing_on_any_other_path() {
    // The guard that the widening did not leak. Everything except the page and
    // the event stream still needs a real Authorization header.
    let cli = session();
    let d = Daemon::start(&cli);

    for path in ["/state", "/health", "/hunk/h-1"] {
        let r = http::send(
            d.port(),
            "",
            "GET",
            &format!("{path}?token={}", d.token()),
            None,
            Duration::from_secs(5),
        )
        .unwrap();
        assert_eq!(r.status, 401, "{path} accepted a query token");
    }
}

#[test]
fn the_page_is_served_with_a_policy_that_forbids_loading_anything_off_this_origin() {
    let cli = session();
    let d = Daemon::start(&cli);

    let r = d.get("/");
    let csp = r.header("Content-Security-Policy").expect("a CSP");
    assert!(csp.contains("default-src 'none'"), "{csp}");
    assert!(csp.contains("connect-src 'self'"), "{csp}");
    assert_eq!(r.header("Referrer-Policy"), Some("no-referrer"));
    assert_eq!(r.header("X-Content-Type-Options"), Some("nosniff"));
}
