//! `rote endpoint`: how a front end that is not the pane finds the daemon.
//!
//! This is the whole of what `cargo test` can prove about the nvim plugin. The
//! plugin itself is Lua and is verified by hand — inventing a Lua harness for
//! it in a Rust repo would be a test nobody runs.

mod common;

use common::cli::{stderr, stdout, Cli};
use common::daemon::Daemon;
use common::Fixture;

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

fn parse(text: &str) -> serde_json::Value {
    serde_json::from_str(text.trim()).unwrap_or_else(|e| panic!("not JSON: {e}\n{text}"))
}

#[test]
fn endpoint_prints_a_url_and_withholds_the_token() {
    // The human form is the one that ends up in scrollback and on a shared
    // screen, so it is the one that must not carry a bearer token.
    let cli = session();
    let d = Daemon::start(&cli);

    let out = cli.run(&["endpoint"]);
    let text = stdout(&out);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(text.contains(&format!("127.0.0.1:{}", d.port())), "{text}");
    assert!(text.contains(&d.child.id().to_string()), "the pid: {text}");
    assert!(
        !text.contains(d.token()),
        "the token must not be in the human form: {text}"
    );
    assert!(text.contains("--token"), "and it says where to get it");
}

#[test]
fn endpoint_json_carries_everything_a_front_end_needs_to_connect() {
    let cli = session();
    let d = Daemon::start(&cli);

    let out = cli.run(&["endpoint", "--json"]);
    assert!(out.status.success(), "{}", stderr(&out));
    let v = parse(&stdout(&out));

    assert_eq!(v["ok"], true);
    assert_eq!(v["port"], d.port());
    assert_eq!(v["token"], d.token());
    assert_eq!(v["url"], format!("http://127.0.0.1:{}", d.port()));
    assert_eq!(v["project_hash"], cli.fx.project().hash);
    assert_eq!(v["wire_version"], rote::state::WIRE_VERSION);
    assert!(v["repo_root"].is_string());
}

#[test]
fn endpoint_token_prints_the_bare_token_and_nothing_else() {
    // So `curl -H "Authorization: Bearer $(rote endpoint --token)"` works.
    let cli = session();
    let d = Daemon::start(&cli);

    let out = cli.run(&["endpoint", "--token"]);
    assert!(out.status.success());
    assert_eq!(stdout(&out).trim(), d.token());
}

#[test]
fn endpoint_without_a_session_says_so_in_json_and_exits_non_zero() {
    // `--json` prints an object even when it fails, so a caller can parse
    // stdout unconditionally rather than switching on the exit code first.
    let fx = Fixture::new();
    fx.write("a.rs", "fn a() {\n}\n");
    fx.commit_all("initial");
    let cli = Cli::with_fixture(fx);

    let out = cli.run(&["endpoint", "--json"]);
    assert!(!out.status.success());
    let v = parse(&stdout(&out));
    assert_eq!(v["ok"], false);
    assert_eq!(v["error"], "no_session");
}

#[test]
fn endpoint_without_a_daemon_says_which_kind_of_nothing_it_found() {
    let cli = session();
    let out = cli.run(&["endpoint", "--json"]);
    assert!(!out.status.success());
    assert_eq!(parse(&stdout(&out))["error"], "no_daemon");
}

#[test]
fn endpoint_ensure_starts_a_daemon_and_finds_the_same_one_twice() {
    let cli = session();

    let first = cli.run(&["endpoint", "--json", "--ensure"]);
    assert!(first.status.success(), "{}", stderr(&first));
    let a = parse(&stdout(&first));
    assert_eq!(a["ok"], true);

    let second = cli.run(&["endpoint", "--json"]);
    assert!(second.status.success());
    let b = parse(&stdout(&second));
    assert_eq!(a["port"], b["port"], "the same daemon, not a second one");
    assert_eq!(a["pid"], b["pid"]);

    // Leave nothing running for the next test.
    let _ = cli.run_with_input(&["abort"], "y\n");
}

#[test]
fn endpoint_reports_an_owner_that_will_not_talk_rather_than_a_stale_address() {
    // A `rote watch --local` pane holds the engine token and publishes no
    // address at all, so "no daemon" would be a lie — and `--ensure` must not
    // try to start one, because it could not take the token anyway.
    let cli = session();
    let project = cli.fx.project();
    project.ensure_state_dir().unwrap();
    let _held = rote::lockfile::Lock::acquire(&project.watch_lock_path()).unwrap();

    let out = cli.run(&["endpoint", "--json", "--ensure"]);
    assert!(!out.status.success());
    assert_eq!(parse(&stdout(&out))["error"], "opaque_owner");

    let human = cli.run(&["endpoint"]);
    assert!(
        stderr(&human).contains("--local"),
        "and it names what does that: {}",
        stderr(&human)
    );
}

// ------------------------------------------------------- the plugin's seam

/// Repo-relative, from the test binary's location.
fn repo_root() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

#[test]
fn the_plugin_pins_the_wire_version_the_daemon_speaks() {
    // The one thing standing between a protocol change and a plugin that
    // silently misreads it. Everything else about the Lua is hand-verified;
    // this is cheap and catches the failure that would be hardest to see.
    let lua = std::fs::read_to_string(repo_root().join("lua/rote/init.lua")).unwrap();
    let pinned: u32 = lua
        .lines()
        .find_map(|l| l.trim().strip_prefix("M.WIRE_VERSION = "))
        .and_then(|v| v.trim().parse().ok())
        .expect("lua/rote/init.lua must pin M.WIRE_VERSION");

    assert_eq!(
        pinned,
        rote::state::WIRE_VERSION,
        "the plugin pins wire version {pinned}, the daemon speaks {}",
        rote::state::WIRE_VERSION
    );
}

#[test]
fn every_rote_command_the_plugin_defines_is_documented() {
    let root = repo_root();
    let plugin = std::fs::read_to_string(root.join("plugin/rote.lua")).unwrap();
    let doc = std::fs::read_to_string(root.join("doc/rote.txt")).unwrap();

    let mut found = 0;
    for line in plugin.lines() {
        let Some(rest) = line.trim().strip_prefix("cmd(\"") else {
            continue;
        };
        let Some(name) = rest.split('"').next() else {
            continue;
        };
        found += 1;
        assert!(
            doc.contains(&format!(":{name}")),
            "{name} is not in doc/rote.txt"
        );
    }
    assert!(found >= 8, "expected the command set, found {found}");
}
