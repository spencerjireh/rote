use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use rote::config::{self, Config};
use rote::curator;
use rote::daemon;
use rote::detect;
use rote::doctor;
use rote::git;
use rote::hunks::Status;
use rote::lockfile::Lock;
use rote::pane;
use rote::paths::{self, ProjectPaths};
use rote::present::{self, Classification};
use rote::review;
use rote::session::{self, Manifest, Terminal};
use rote::setup;
use rote::shadow;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

/// How a hunk's content arrived, on the command line.
///
/// No `unknown`: that is the absence of an assertion, not something to assert.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
enum Arrival {
    /// You typed it.
    Typed,
    /// You pasted it.
    Pasted,
}

/// How to answer a divergence question, on the command line.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
enum Resolution {
    /// Record what you typed; the proposal is not offered again.
    Keep,
    /// Withdraw the question and carry on typing.
    Retry,
}

#[derive(Parser, Debug)]
#[command(
    name = "rote",
    version,
    about = "Use Claude Code at full capability; type every line in yourself.",
    long_about = "rote runs Claude Code inside a shadow clone of this repository and serves \
                  the resulting diff back to you hunk by hunk, so the agent stays fully \
                  capable while every line entering the real tree is typed by hand."
)]
struct Cli {
    /// Override repo discovery instead of walking up from the current directory.
    #[arg(long, global = true, value_name = "PATH")]
    project: Option<PathBuf>,

    /// Suppress non-essential output.
    #[arg(short, long, global = true)]
    quiet: bool,

    /// Disable colored output.
    #[arg(long, global = true)]
    no_color: bool,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Write a commented .rote.toml in the repo root.
    Init {
        /// Overwrite an existing .rote.toml.
        #[arg(long)]
        force: bool,
    },

    /// Sync the shadow, open a session, and hand the pane to claude.
    Start {
        /// What you're about to work on. Printed for you to paste; never passed to claude.
        task: Vec<String>,

        /// Prepare the session but do not launch claude.
        #[arg(long, hide = true)]
        no_launch: bool,
    },

    /// Show session state, task, and hunk counts.
    Status,

    /// Print the hunk at the head of the queue. Display only.
    Next {
        /// Emit the next pending hunk as one JSON line instead of presenting it.
        #[arg(long, hide = true)]
        json: bool,
    },

    /// Re-print a hunk, defaulting to the last one presented. Display only.
    Show {
        /// Which hunk. Omit for the most recently presented one.
        hunk_id: Option<String>,
    },

    /// Answer an open divergence question.
    Resolve {
        hunk_id: String,
        #[arg(value_enum)]
        choice: Resolution,
    },

    /// Say how a hunk's content arrived. Changes no status.
    Report {
        hunk_id: String,
        #[arg(value_enum)]
        input: Arrival,
    },

    /// Where the daemon is listening, for a front end that wants to attach.
    Endpoint {
        /// Machine-readable, including the token.
        #[arg(long)]
        json: bool,
        /// Print the bare token and nothing else.
        #[arg(long)]
        token: bool,
        /// Start a daemon if none is running.
        #[arg(long)]
        ensure: bool,
    },

    /// Watch your tree and classify as you type. The transcription loop.
    Watch {
        /// Run the engine in this process instead of attaching to a daemon.
        /// For debugging, and for a machine where a daemon cannot start.
        #[arg(long)]
        local: bool,
        /// Print a URL for the browser front end instead of taking the
        /// terminal. The daemon keeps watching either way.
        #[arg(long, conflicts_with = "local")]
        web: bool,
        /// Print frames instead of taking the terminal. Inferred off a pipe.
        #[arg(long)]
        headless: bool,
        /// Exit once the queue is empty, instead of waiting for more work.
        #[arg(long, hide = true)]
        exit_when_empty: bool,
        /// Give up after this many milliseconds. A wedged watcher should fail
        /// a test rather than hang the machine running it.
        #[arg(long, hide = true)]
        timeout: Option<u64>,
    },

    /// Own the queue and serve it. Started for you by `rote start`.
    #[command(hide = true)]
    Daemon {
        /// Stay in the foreground. What tests and debugging use.
        #[arg(long)]
        foreground: bool,
        /// Exit after this many milliseconds no matter what.
        #[arg(long, hide = true)]
        timeout: Option<u64>,
    },

    /// Leave a hunk untyped. Defaults to the active one.
    Skip {
        /// Which hunk. Omit for the active hunk.
        hunk_id: Option<String>,
    },

    /// Print the shadow path, or resume the session agent with --attach.
    Talk {
        /// Exec `claude --continue` in the shadow.
        #[arg(long)]
        attach: bool,
    },

    /// Run checks, review the session, and close it.
    Done {
        /// Close even with pending hunks (they become skipped).
        #[arg(long)]
        force: bool,
        /// Skip the [checks] commands.
        #[arg(long)]
        no_checks: bool,
        /// Skip the reviewer pass.
        #[arg(long)]
        no_review: bool,
    },

    /// Discard the session. The real tree is untouched by definition.
    Abort {
        /// Skip the confirmation prompt.
        #[arg(long)]
        yes: bool,
    },

    /// Check that this machine is set up to run rote.
    Doctor {
        /// Also invoke claude once to prove the reviewer path really works.
        #[arg(long)]
        deep: bool,
    },

    /// Write the global config at ~/.config/rote/config.toml.
    Setup {
        /// Overwrite an existing global config.
        #[arg(long)]
        force: bool,
    },
}

impl Command {
    /// Whether this command needs to be inside a git repository.
    ///
    /// `doctor` and `setup` are about machine-level state — claude, the editor,
    /// the global config — and `doctor` is the command you reach for when
    /// something is wrong, which may well be before you have a repo.
    fn needs_repo(&self) -> bool {
        !matches!(self, Command::Doctor { .. } | Command::Setup { .. })
    }
}

fn main() -> ExitCode {
    match run() {
        Ok(code) => code,
        Err(e) => {
            eprintln!("rote: {e:#}");
            ExitCode::FAILURE
        }
    }
}

/// Locate the repository, if we are in one.
fn find_repo(cli: &Cli) -> Result<PathBuf> {
    let cwd = std::env::current_dir().context("cannot determine current directory")?;
    match &cli.project {
        Some(p) => paths::discover_repo_root(p),
        None => paths::discover_repo_root(&cwd),
    }
}

/// Environment overrides, applied at the process boundary.
///
/// Deliberately not inside `Config::load`: that stays a pure function of two
/// file paths, and its unit tests run in parallel, where process-global
/// environment would make them lie to each other.
///
/// `ROTE_CURATOR=off` is the one-invocation escape hatch for the only thing
/// rote does that spends money without being asked. A detached daemon inherits
/// the variable, so turning it off for a `rote start` also turns it off for the
/// session that start leaves running.
fn apply_env_overrides(cfg: &mut Config) {
    if let Some(v) = std::env::var_os("ROTE_CURATOR") {
        let v = v.to_string_lossy().to_ascii_lowercase();
        if matches!(v.as_str(), "0" | "off" | "no" | "false") {
            cfg.curator_enabled = false;
        }
    }
}

fn run() -> Result<ExitCode> {
    let cli = Cli::parse();

    // Most commands operate on a repo; `doctor` and `setup` do not require one,
    // so resolution is per-command rather than unconditional.
    let resolved = if cli.command.needs_repo() {
        let repo_root = find_repo(&cli)?;
        let project = ProjectPaths::resolve(&repo_root)?;
        let cfg = Config::load(&paths::global_config_path()?, &project.project_config())?;
        Some((project, cfg))
    } else {
        None
    };

    // Applied before anything reads a config, and to both branches below, so no
    // command can be the one that forgot.
    let mut resolved = resolved;
    if let Some((_, cfg)) = resolved.as_mut() {
        apply_env_overrides(cfg);
    }

    // Fall back to a repo-less config for the two commands that allow it.
    let cfg = match &resolved {
        Some((_, cfg)) => cfg.clone(),
        None => {
            let mut cfg = Config::load(&paths::global_config_path()?, Path::new("/nonexistent"))?;
            apply_env_overrides(&mut cfg);
            cfg
        }
    };
    // --no-color wins over config; NO_COLOR is honored as the de facto standard.
    let color = cfg.color && !cli.no_color && std::env::var_os("NO_COLOR").is_none();

    // Commands that do not need a repo run before the unwrap below.
    match &cli.command {
        // `.ok()` rather than `?`: being outside a repository is one of the
        // states doctor exists to report, not a reason for it to fail.
        Command::Doctor { deep } => return doctor::run(find_repo(&cli).ok(), &cfg, *deep, color),
        Command::Setup { force } => {
            setup::run(&cfg, *force, cli.quiet)?;
            return Ok(ExitCode::SUCCESS);
        }
        _ => {}
    }

    let (project, cfg) = resolved.expect("every other command requires a repo");

    match cli.command {
        Command::Init { force } => {
            cmd_init(&project, force, cli.quiet)?;
            Ok(ExitCode::SUCCESS)
        }
        Command::Start { task, no_launch } => {
            cmd_start(&project, &cfg, task.join(" "), no_launch, cli.quiet)?;
            Ok(ExitCode::SUCCESS)
        }
        Command::Status => {
            cmd_status(&project)?;
            Ok(ExitCode::SUCCESS)
        }
        Command::Abort { yes } => {
            cmd_abort(&project, &cfg, yes, cli.quiet)?;
            Ok(ExitCode::SUCCESS)
        }
        Command::Next { json } => {
            cmd_next(&project, &cfg, json, color)?;
            Ok(ExitCode::SUCCESS)
        }
        Command::Show { hunk_id } => {
            cmd_show(&project, hunk_id.as_deref(), color)?;
            Ok(ExitCode::SUCCESS)
        }
        Command::Resolve { hunk_id, choice } => {
            cmd_resolve(&project, &hunk_id, choice)?;
            Ok(ExitCode::SUCCESS)
        }
        Command::Report { hunk_id, input } => {
            cmd_report(&project, &hunk_id, input)?;
            Ok(ExitCode::SUCCESS)
        }
        Command::Endpoint {
            json,
            token,
            ensure,
        } => cmd_endpoint(&project, json, token, ensure),
        Command::Watch {
            local,
            web,
            headless,
            exit_when_empty,
            timeout,
        } => {
            if web {
                cmd_watch_web(&project, cli.quiet)?;
                return Ok(ExitCode::SUCCESS);
            }
            cmd_watch(
                &project,
                &cfg,
                local,
                pane::Options {
                    headless,
                    exit_when_empty,
                    timeout_ms: timeout,
                },
            )?;
            Ok(ExitCode::SUCCESS)
        }
        Command::Daemon {
            foreground: _,
            timeout,
        } => {
            daemon::serve(
                &project,
                &cfg,
                daemon::ServeOptions {
                    timeout_ms: timeout,
                },
            )?;
            Ok(ExitCode::SUCCESS)
        }
        Command::Skip { hunk_id } => {
            cmd_skip(&project, &cfg, hunk_id.as_deref())?;
            Ok(ExitCode::SUCCESS)
        }
        Command::Talk { attach } => {
            cmd_talk(&project, &cfg, attach)?;
            Ok(ExitCode::SUCCESS)
        }
        // Both are dispatched above, before the repo is required.
        Command::Doctor { .. } | Command::Setup { .. } => unreachable!(),
        Command::Done {
            force,
            no_checks,
            no_review,
        } => {
            cmd_done(&project, &cfg, force, no_checks, no_review, cli.quiet)?;
            Ok(ExitCode::SUCCESS)
        }
    }
}

fn cmd_talk(project: &ProjectPaths, cfg: &Config, attach: bool) -> Result<()> {
    println!("{}", project.shadow_dir.display());
    if !attach {
        return Ok(());
    }
    // Coming back to a session in a new pane. Same window as `start`: this
    // execs, so anything worth reporting has to be reported first.
    if cfg.daemon_autostart && Manifest::load(project)?.is_some() {
        if let Err(e) = daemon::ensure_running(project) {
            eprintln!("warning: could not start the rote daemon ({e:#}).");
        }
    }
    use std::os::unix::process::CommandExt;
    let (program, args) = cfg
        .claude_continue_cmd
        .split_first()
        .context("claude_continue_cmd is empty")?;
    let err = std::process::Command::new(program)
        .args(args)
        .current_dir(&project.shadow_dir)
        .exec();
    Err(err).with_context(|| {
        format!(
            "cannot launch `{}` (configured as claude_continue_cmd).",
            cfg.claude_continue_cmd.join(" ")
        )
    })
}

/// The close-out pipeline. DESIGN.md §1 `rote done`, in order.
fn cmd_done(
    project: &ProjectPaths,
    cfg: &Config,
    force: bool,
    no_checks: bool,
    no_review: bool,
    quiet: bool,
) -> Result<()> {
    // 1. Refuse mid-operation, before anything expensive runs.
    shadow::ensure_no_operation_in_progress(project)?;

    // 2. Ask whoever owns the queue what is still pending; then let go before
    // prompting, because a confirmation waits on a human and no lock is ever held
    // across that.
    //
    // Routed like every other mutation (§13). The daemon is alive right through
    // here — `reap` is at step 6, below every early return, so that "left the
    // session open" never takes it down — and recomputing here ourselves would be
    // a second engine for as long as this runs.
    //
    // Under a daemon nothing forces a recompute, and that loses nothing: its
    // classifier has already marked correctly typed hunks `Typed`, which is a
    // better answer than a bare recompute merely dropping them under rule 5.
    let pending: Vec<(String, String)> = match daemon::owner(project)? {
        daemon::Owner::Daemon(ep) => daemon::fetch_state(&ep)?
            .queue
            .iter()
            .map(|q| (q.id.clone(), format!("  {}:{}", q.file, q.anchor_hint)))
            .collect(),
        daemon::Owner::Nobody(_guard) => {
            session::with_session_recomputed(project, cfg, |m, report| {
                for w in &report.warnings {
                    eprintln!("warning: {w}");
                }
                Ok(m.pending()
                    .map(|h| (h.id.clone(), format!("  {}:{}", h.file, h.anchor_hint)))
                    .collect::<Vec<_>>())
            })?
        }
        daemon::Owner::Opaque { pid } => return Err(daemon::opaque_owner_error(project, pid)),
    };

    if !pending.is_empty() {
        println!("{} hunk(s) still pending:", pending.len());
        for (_, line) in &pending {
            println!("{line}");
        }
        if !force && !confirm("close anyway, marking them skipped? [y/N] ")? {
            println!("left the session open. `rote watch` continues.");
            return Ok(());
        }
        // One `skip` per hunk rather than a bulk verb. Each has to disarm the
        // watchdog and retake the baseline too, which is engine-private state — a
        // bulk variant would have to reimplement that inside the engine to be
        // correct, and would buy nothing: there is no atomicity to protect with
        // `reap` and `archive_and_clear` next, and the extra snapshots go to panes
        // about to be told the session closed.
        match daemon::owner(project)? {
            daemon::Owner::Daemon(ep) => {
                for (id, _) in &pending {
                    let command = rote::state::Command::Skip {
                        hunk_id: id.clone(),
                    };
                    match daemon::apply(&ep, command)? {
                        daemon::Applied::Yes => {}
                        // It left the queue between the listing and here — typed,
                        // or reworked away. Nothing left to skip either way.
                        daemon::Applied::Rejected {
                            cause: Some(rote::state::Cause::NoSuchHunk),
                            ..
                        } => {}
                        daemon::Applied::Rejected { reason, .. } => bail!("{reason} ({id})"),
                    }
                }
            }
            daemon::Owner::Nobody(_guard) => {
                session::with_session(project, |m| {
                    let ids: Vec<String> = m.pending().map(|h| h.id.clone()).collect();
                    for id in ids {
                        if let Some(h) = m.find_mut(&id) {
                            h.status = Status::Skipped;
                        }
                    }
                    Ok(())
                })?;
            }
            daemon::Owner::Opaque { pid } => return Err(daemon::opaque_owner_error(project, pid)),
        }
    }

    // Re-read once the mutations above have landed. Everything from here is
    // read-only until teardown, which takes the lock again and reloads.
    let manifest = Manifest::require(project)?;
    let empty_session = manifest.hunks.is_empty();
    if empty_session && !quiet {
        println!("no changes this session.");
    }

    // 3. Checks, in the REAL tree.
    if !no_checks && !empty_session {
        review::run_checks(project, cfg)?;
    }

    // 4. Reviewer. Advisory throughout.
    if !no_review && cfg.review_enabled && !empty_session {
        let diff = review::session_diff(project, &manifest)?;
        let payload = review::build_payload(&manifest, &diff);
        if let Some(findings) = review::run_reviewer(cfg, &payload) {
            println!("\n── Reviewer findings ──────────────────────────────────────");
            println!("{}", findings.trim_end());
            println!("───────────────────────────────────────────────────────────\n");
        }
    }

    // 5. Confirm, naming what is about to be discarded.
    let skipped = manifest.count(Status::Skipped);
    let diverged = manifest.count(Status::Diverged);
    if !force {
        if skipped > 0 || diverged > 0 {
            println!(
                "closing discards the agent's version of {skipped} skipped and {diverged} diverged hunk(s)."
            );
        }
        if !confirm("close session? [y/N] ")? {
            println!("left the session open.");
            return Ok(());
        }
    }

    // 6. Stop the daemon first. Before the lock rather than merely before the
    //    sync: it may be mid-write, and waiting five seconds behind a process we
    //    are about to stop is five seconds of nothing. Telling it the reason is
    //    what lets it say "session closed" to every attached pane — after the
    //    archive there is no live manifest left for it to read.
    daemon::reap(project, Some(Terminal::Done));

    //    Residue first, then archive, then the sync that destroys the shadow's
    //    copy. Reloaded under the lock: the copy above predates the prompts, and
    //    archiving a stale manifest would file the wrong record.
    let _lock = Lock::acquire(&project.lock_path())?;
    let mut manifest = Manifest::require(project)?;
    // Read before archiving: `archive_and_clear` sets the terminal state, and
    // this is a statement about the session that just happened.
    let arrival = describe_arrival(&manifest);
    let archived = session::archive_and_clear(&mut manifest, project, cfg, Terminal::Done)?;
    shadow::sync(project, cfg)?;

    if !quiet {
        println!("session closed. state: idle");
        if let Some(line) = arrival {
            println!("{line}");
        }
        if let Some(msg) = review::describe_residue(&archived.patch, archived.residue_bytes) {
            println!("{msg}");
        }
    }
    Ok(())
}

/// Print a URL for the browser front end, and return.
///
/// Deliberately does not take the terminal: you are going to a browser, and the
/// detached daemon watches until the session closes whether or not this process
/// is alive.
///
/// **Prints, never opens.** Auto-opening would put a bearer token into whichever
/// browser happens to be default — into its history and its session restore —
/// without being asked.
fn cmd_watch_web(project: &ProjectPaths, quiet: bool) -> Result<()> {
    shadow::ensure_no_operation_in_progress(project)?;
    Manifest::require(project)?;

    let endpoint = daemon::ensure_running(project)?;
    // The token rides in the query because a browser address bar cannot set an
    // Authorization header. Same-origin from here on, which is why the daemon
    // needs no CORS headers at all.
    println!("{}/?token={}", endpoint.base_url(), endpoint.token);
    if !quiet {
        println!("open that in a browser. The daemon keeps watching whether or not you do.");
    }
    Ok(())
}

/// How the typed hunks arrived, or `None` when there is nothing honest to say.
///
/// Over `typed` hunks only — a skipped or diverged hunk was not typed, and
/// counting it would make the denominator a different question.
///
/// **Silence when nothing was observed**, which is the no-plugin case with no
/// autosave. "0 of 12 observed" is technically true and reads as an accusation,
/// and it would be the line almost everyone saw. The phrasing everywhere else
/// puts the limitation on rote rather than on the user, because it *is* rote's:
/// a careful typist who saves once is byte-identical to a paste.
fn describe_arrival(manifest: &Manifest) -> Option<String> {
    use rote::hunks::Input;

    let typed: Vec<Input> = manifest
        .hunks
        .iter()
        .filter(|h| h.status == Status::Typed)
        .map(|h| h.input)
        .collect();
    if typed.is_empty() {
        return None;
    }

    let observed = typed.iter().filter(|i| **i == Input::Typed).count();
    let pasted = typed.iter().filter(|i| **i == Input::Pasted).count();
    if observed == 0 && pasted == 0 {
        return None;
    }

    let total = typed.len();
    let unknown = total - observed - pasted;
    let mut line = if observed == total {
        format!("all {total} typed hunks were observed being typed.")
    } else {
        format!("{observed} of {total} typed hunks were observed being typed")
    };
    if observed != total {
        if pasted > 0 {
            line.push_str(&format!(", {pasted} reported pasted"));
        }
        if unknown > 0 {
            line.push_str(&format!("; {unknown} could not be told apart"));
        }
        line.push('.');
    }
    Some(line)
}

/// The transcription loop: watch, classify, advance.
fn cmd_watch(project: &ProjectPaths, cfg: &Config, local: bool, opts: pane::Options) -> Result<()> {
    // A rebase or merge in flight makes every classification nonsense, and the
    // pane would report it confidently. Refuse before taking the terminal.
    shadow::ensure_no_operation_in_progress(project)?;
    Manifest::require(project)?;

    if local {
        return pane::run_local(project, cfg, opts);
    }

    // Attach to whoever owns the queue, starting one if nobody does. Falling
    // back to a local engine on failure keeps the tool usable on a machine
    // where the daemon cannot start — and it is not a second engine, because
    // it takes the same token the daemon would have held.
    match daemon::ensure_running(project) {
        Ok(endpoint) => pane::run_client(project, cfg, endpoint, opts),
        Err(e) => {
            eprintln!("warning: no rote daemon ({e:#}).\nWatching in this process instead.");
            pane::run_local(project, cfg, opts)
        }
    }
}

/// Print the hunk at the head of the queue.
///
/// A printer, not a step in the loop. `rote watch` is what advances the queue;
/// this exists so a plain shell, a script, or a `--json` consumer can see what
/// is outstanding without a pane. It classifies nothing and launches nothing.
///
/// Routed, because it writes: `last_presented` is what `rote show` with no id
/// re-prints, and it used to recompute and save straight past a live engine
/// (§13). Under a daemon it reads the engine's published view and asks the engine
/// to record what it showed. It deliberately does *not* force a recompute — the
/// engine does that on save, on any unknown-file change, and at least every floor
/// sweep, and forcing one is the only part of this that would need the engine's
/// permission. Asking what it currently thinks does not, and its answer is the
/// authoritative one anyway: it carries `drift`, curator order, and an anchor
/// resolved against the real file, all of which the CLI would otherwise re-derive
/// and could disagree about.
fn cmd_next(project: &ProjectPaths, cfg: &Config, json: bool, color: bool) -> Result<()> {
    // §1: "Errors if state is idle." A precondition, read before anything else —
    // but the routing decision still comes before any mutation.
    Manifest::require(project)?;

    let picked = match daemon::owner(project)? {
        daemon::Owner::Daemon(ep) => {
            let snapshot = daemon::fetch_state(&ep)?;
            match snapshot.active {
                None => None,
                Some(p) => {
                    // The same write the local arm makes, made by the engine so
                    // its private view cannot end up disagreeing with the file.
                    let command = rote::state::Command::Show {
                        hunk_id: Some(p.hunk.id.clone()),
                    };
                    match daemon::apply(&ep, command)? {
                        daemon::Applied::Yes => {}
                        // It left the queue between the fetch and the show. The
                        // hunk is still worth printing; nothing is worth failing.
                        daemon::Applied::Rejected {
                            cause: Some(rote::state::Cause::NoSuchHunk),
                            ..
                        } => {}
                        daemon::Applied::Rejected { reason, .. } => {
                            bail!("{reason} ({})", p.hunk.id)
                        }
                    }
                    Some((p.hunk, p.position, p.total))
                }
            }
        }
        daemon::Owner::Nobody(_guard) => {
            session::with_session_recomputed(project, cfg, |m, report| {
                for w in &report.warnings {
                    eprintln!("warning: {w}");
                }
                let Some(hunk) = m.active().cloned() else {
                    return Ok(None);
                };
                let position = m.queue_position(&hunk.id).unwrap_or(1);
                let total = m.pending().count();
                m.last_presented = Some(hunk.id.clone());
                Ok(Some((hunk, position, total)))
            })?
        }
        daemon::Owner::Opaque { pid } => return Err(daemon::opaque_owner_error(project, pid)),
    };

    let Some((hunk, position, total)) = picked else {
        println!("nothing to transcribe.");
        let manifest = Manifest::require(project)?;
        if !manifest.hunks.is_empty() {
            println!("every hunk is accounted for — `rote done` closes the session.");
        }
        return Ok(());
    };

    if json {
        // The front-end seam: emit and exit, classifying nothing.
        println!("{}", serde_json::to_string(&hunk)?);
        return Ok(());
    }

    let real_file = project.repo_root.join(&hunk.file);

    // Untypeable hunks are gated on bytes rather than typing, and that gate is
    // two file reads and no subprocess — the one honest verdict a printer can
    // still reach on its own.
    if hunk.is_untypeable() {
        let anchor = present::Anchor {
            line: 1,
            via: present::AnchorVia::NoContext,
        };
        print!(
            "{}",
            present::render(&hunk, position, total, &anchor, color)
        );
        let shadow_file = project.shadow_dir.join(&hunk.file);
        if present::classify_by_bytes(&real_file, &shadow_file) == Classification::Untouched {
            println!("still differs from the shadow — do that, and `rote watch` will notice.");
        }
        return Ok(());
    }

    let located = present::locate(&project.repo_root, &hunk);
    print!(
        "{}",
        present::render(&hunk, position, total, &located.anchor, color)
    );
    println!("{}:{}", hunk.file, located.anchor.line);
    if let Some(d) = &hunk.pending_divergence {
        print!(
            "{}",
            present::render_divergence(&d.proposed, &d.actual, color)
        );
        println!("answer with `rote resolve {} keep|retry`.", hunk.id);
    }
    Ok(())
}

/// Re-print a hunk. Display only — it changes nothing.
fn cmd_show(project: &ProjectPaths, hunk_id: Option<&str>, color: bool) -> Result<()> {
    let manifest = Manifest::require(project)?;
    let id = match hunk_id {
        Some(id) => id.to_string(),
        None => match manifest.last_presented.clone() {
            Some(id) => id,
            None => {
                println!("nothing presented yet in this session.");
                return Ok(());
            }
        },
    };
    let Some(hunk) = manifest.find(&id) else {
        println!("no hunk {id} in this session (the agent may have reworked it).");
        return Ok(());
    };

    let located = present::locate(&project.repo_root, hunk);
    print!("{}", present::render(hunk, 0, 0, &located.anchor, color));
    println!("status: {}", hunk.status);
    if let Some(d) = hunk
        .divergence
        .as_ref()
        .or(hunk.pending_divergence.as_ref())
    {
        print!(
            "{}",
            present::render_divergence(&d.proposed, &d.actual, color)
        );
    }
    println!("\n(display only — `rote show` changes nothing)");
    Ok(())
}

/// Answer an open divergence question.
///
/// The id is required for both choices. Unlike `skip` there is no sensible
/// default: the hunk carrying the question is often not the active one, because
/// the queue moves on past an unanswered question rather than blocking on it.
fn cmd_resolve(project: &ProjectPaths, hunk_id: &str, choice: Resolution) -> Result<()> {
    let wire_choice = match choice {
        Resolution::Keep => rote::state::Resolution::Keep,
        Resolution::Retry => rote::state::Resolution::Retry,
    };

    let file = match daemon::owner(project)? {
        // This one matters most. Answering "retry" clears the question, but a
        // live engine also has to disarm its watchdog and retake its baseline —
        // otherwise it re-raises the identical question seconds later.
        daemon::Owner::Daemon(ep) => {
            let command = rote::state::Command::Resolve {
                hunk_id: hunk_id.to_string(),
                choice: wire_choice,
            };
            match daemon::apply(&ep, command)? {
                daemon::Applied::Yes => Some(hunk_id.to_string()),
                // The one refusal that is not an error: the user answered a
                // question that was already gone.
                daemon::Applied::Rejected {
                    cause: Some(rote::state::Cause::NoOpenQuestion),
                    ..
                } => None,
                daemon::Applied::Rejected { reason, .. } => bail!("{reason} ({hunk_id})"),
            }
        }
        daemon::Owner::Nobody(_guard) => {
            session::with_session_maybe(project, |m| {
                let h = m
                    .find_mut(hunk_id)
                    .with_context(|| format!("no hunk {hunk_id} in this session"))?;
                let Some(d) = h.pending_divergence.take() else {
                    return Ok(None);
                };
                match choice {
                    Resolution::Keep => {
                        h.status = Status::Diverged;
                        h.divergence = Some(d);
                    }
                    // There is no editor to reopen. Retry withdraws the question
                    // and lets `rote watch` keep classifying as the user types.
                    Resolution::Retry => {}
                }
                let file = h.file.clone();
                m.last_presented = Some(hunk_id.to_string());
                Ok(Some(file))
            })?
            .0
        }
        daemon::Owner::Opaque { pid } => return Err(daemon::opaque_owner_error(project, pid)),
    };

    match (file, choice) {
        (None, _) => println!("hunk {hunk_id} has no open question."),
        (Some(f), Resolution::Keep) => {
            println!("kept your version — {f}");
            println!("the proposal will not be offered again.");
        }
        (Some(f), Resolution::Retry) => {
            println!("withdrew the question — {f}");
            println!("`rote watch` keeps classifying as you type.");
        }
    }
    Ok(())
}
/// Print where a daemon is listening, so a front end can attach to it.
///
/// The token is withheld from the human form and printed by the machine ones,
/// which is the opposite of the usual instinct and is deliberate: the only
/// consumer of a token is a program capturing stdout, and the exposure worth
/// defending against is a terminal someone is looking at or sharing. Gating on
/// a tty instead would break `rote endpoint --json | jq` in exactly the
/// configuration where someone is debugging.
///
/// `--json` always prints a JSON object, including on failure, so a caller can
/// parse stdout unconditionally rather than switching on the exit code first.
fn cmd_endpoint(project: &ProjectPaths, json: bool, token: bool, ensure: bool) -> Result<ExitCode> {
    let fail = |error: &str, pid: Option<u32>| -> Result<ExitCode> {
        if json {
            let mut body = serde_json::json!({ "ok": false, "error": error });
            if let Some(p) = pid {
                body["pid"] = serde_json::json!(p);
            }
            println!("{body}");
        } else {
            eprintln!("{}", endpoint_advice(error, pid));
        }
        Ok(ExitCode::FAILURE)
    };

    if Manifest::load(project)?.is_none() {
        return fail("no_session", None);
    }

    // `--ensure` overrides `[daemon] autostart = false`, which is about
    // *implicit* spawning at `start`. Asking for an address is explicit.
    let found = if ensure {
        daemon::ensure_running(project).ok()
    } else {
        daemon::discover(project)
    };

    let Some(ep) = found else {
        // A `rote watch --local` pane owns the engine and publishes no address
        // at all, so "no daemon" would be a lie. Say which it is.
        return match daemon::owner(project)? {
            daemon::Owner::Opaque { pid } => fail("opaque_owner", pid),
            _ => fail("no_daemon", None),
        };
    };

    if token {
        println!("{}", ep.token);
        return Ok(ExitCode::SUCCESS);
    }

    if json {
        println!(
            "{}",
            serde_json::json!({
                "ok": true,
                "url": ep.base_url(),
                "port": ep.port,
                "token": ep.token,
                "project_hash": ep.project_hash,
                "repo_root": ep.repo_root,
                "pid": ep.pid,
                "wire_version": ep.wire_version,
                "rote_version": ep.rote_version,
            })
        );
        return Ok(ExitCode::SUCCESS);
    }

    // The human form. No token: this is the one that ends up in scrollback and
    // on a shared screen.
    let state = Manifest::load(project)?
        .map(|m| m.state.to_string())
        .unwrap_or_else(|| "idle".into());
    println!("url:    {}", ep.base_url());
    println!("pid:    {}", ep.pid);
    println!("hash:   {}", ep.project_hash);
    println!("state:  {state}");
    println!("\ntoken withheld. `rote endpoint --token` prints it.");
    Ok(ExitCode::SUCCESS)
}

fn endpoint_advice(error: &str, pid: Option<u32>) -> String {
    match error {
        "no_session" => "no active session.\nRun `rote start \"what you're working on\"` to begin one.".into(),
        "opaque_owner" => format!(
            "something owns this project's queue but does not serve HTTP{}.\n\
             A `rote watch --local` pane does that. Quit it, or attach to that pane instead.",
            pid.map(|p| format!(" (pid {p})")).unwrap_or_default()
        ),
        _ => "no daemon is running for this project.\nRun `rote endpoint --ensure` to start one, or `rote watch`.".into(),
    }
}

/// Record how a hunk's content arrived.
///
/// The manual half of the input signal: the engine infers `typed` only when it
/// watched the region pass through a partial state, and anything else stays
/// `unknown`. This is how a user with no editor plugin corrects the record, and
/// how one whose plugin guessed wrong corrects it back.
fn cmd_report(project: &ProjectPaths, hunk_id: &str, input: Arrival) -> Result<()> {
    let reported = match input {
        Arrival::Typed => rote::state::Reported::Typed,
        Arrival::Pasted => rote::state::Reported::Pasted,
    };
    let value = rote::hunks::Input::from(reported);

    match daemon::owner(project)? {
        // An engine owns the queue: it must make the write, or its next
        // classification would publish a snapshot that disagrees with the file.
        daemon::Owner::Daemon(ep) => {
            let command = rote::state::Command::Report {
                hunk_id: hunk_id.to_string(),
                input: reported,
            };
            if let daemon::Applied::Rejected { reason, .. } = daemon::apply(&ep, command)? {
                bail!("{reason} ({hunk_id})");
            }
        }
        daemon::Owner::Nobody(_guard) => {
            session::with_session_maybe(project, |m| {
                let h = m
                    .find_mut(hunk_id)
                    .with_context(|| format!("no hunk {hunk_id} in this session"))?;
                Ok(h.input.set(value).then_some(()))
            })?;
        }
        daemon::Owner::Opaque { pid } => return Err(daemon::opaque_owner_error(project, pid)),
    }

    println!("recorded {hunk_id} as {value}.");
    Ok(())
}

/// Leave a hunk untyped, by id or by "whatever is active".
///
/// Addressed by id rather than by position. Resolving "the active hunk" and then
/// mutating it are two steps, and between them the queue can move — the daemon
/// reorders after a curator pass, and a concurrent recompute can retire the head
/// entirely. Resolving to an id first means the thing skipped is the thing the
/// user was looking at, or nothing at all.
fn cmd_skip(project: &ProjectPaths, cfg: &Config, hunk_id: Option<&str>) -> Result<()> {
    let file = match daemon::owner(project)? {
        // An engine owns the queue, so it has to make this change. Writing
        // around it would leave its private view of the world — baselines,
        // armed questions, recompute schedule — silently disagreeing with the
        // manifest, which is how a hunk nobody touched ends up being asked
        // about.
        daemon::Owner::Daemon(ep) => {
            // One read, for two reasons: to resolve "the active hunk" into an
            // id, and to have a file name to print. The daemon's reply is an
            // outcome, not a description of what it acted on.
            let snapshot = daemon::fetch_state(&ep)?;
            let (id, file) = match hunk_id {
                Some(id) => {
                    let file = snapshot
                        .queue
                        .iter()
                        .find(|q| q.id == id)
                        .map(|q| q.file.clone())
                        .unwrap_or_else(|| id.to_string());
                    (id.to_string(), file)
                }
                None => match snapshot.queue.first() {
                    Some(q) => (q.id.clone(), q.file.clone()),
                    None => {
                        println!("nothing to skip — the queue is empty.");
                        return Ok(());
                    }
                },
            };
            let command = rote::state::Command::Skip {
                hunk_id: id.clone(),
            };
            if let daemon::Applied::Rejected { reason, .. } = daemon::apply(&ep, command)? {
                bail!("{reason} ({id})");
            }
            file
        }
        // No engine exists, so a direct mutation is safe — for exactly as long
        // as the guard is held, which is why it lives across the whole cycle.
        daemon::Owner::Nobody(_guard) => {
            let skipped = session::with_session_recomputed(project, cfg, |m, _report| {
                let target = match hunk_id {
                    Some(id) => id.to_string(),
                    None => match m.active().map(|h| h.id.clone()) {
                        Some(id) => id,
                        None => return Ok(None),
                    },
                };
                let h = m
                    .find_mut(&target)
                    .with_context(|| format!("no hunk {target} in this session"))?;
                h.status = Status::Skipped;
                let file = h.file.clone();
                m.last_presented = Some(target);
                Ok(Some(file))
            })?;
            let Some(file) = skipped else {
                println!("nothing to skip — the queue is empty.");
                return Ok(());
            };
            file
        }
        daemon::Owner::Opaque { pid } => return Err(daemon::opaque_owner_error(project, pid)),
    };

    println!("skipped — {file}");
    println!("it will not come back. `rote done` reports it before closing.");
    Ok(())
}

fn cmd_start(
    project: &ProjectPaths,
    cfg: &Config,
    task: String,
    no_launch: bool,
    quiet: bool,
) -> Result<()> {
    if let Some(existing) = Manifest::load(project)? {
        bail!(
            "a session is already active (state {}).\n\
             See `rote status`, then finish it with `rote done` or discard it with `rote abort`.",
            existing.state
        );
    }
    shadow::ensure_no_operation_in_progress(project)?;

    // Preflight only what `start` needs. The editor is not required until
    // `next`, nor the reviewer until `done`, so each command checks its own
    // prerequisites where it needs them rather than running a full diagnostic.
    if !no_launch && detect::find_on_path(&cfg.claude_cmd[0]).is_none() {
        bail!(
            "`{}` is not on PATH, so there is nothing to hand the session to.\n\
             Run `rote doctor` to see what is missing.",
            cfg.claude_cmd.join(" ")
        );
    }

    if !quiet {
        println!("syncing shadow …");
    }
    let baseline = shadow::sync(project, cfg)?;
    let manifest = Manifest::new(project, task.clone(), baseline);

    {
        // Scoped so the lock is released before the exec below. The lock treats
        // a live PID as held, and after exec that PID belongs to claude for the
        // whole session — holding it here would wedge every later invocation.
        let _lock = Lock::acquire(&project.lock_path())?;
        project.ensure_state_dir()?;
        manifest.save(project)?;
        // Tidiness only. A curation left by a pane that outlived the last
        // teardown is already inert, because it is stamped with that session's
        // created_at and this one's will not match.
        curator::Cache::remove(project);
    }

    if !task.is_empty() && !quiet {
        println!("\ntask: {task}\n");
    }

    if no_launch {
        if !quiet {
            println!("session open (not launching claude).");
            println!("shadow: {}", project.shadow_dir.display());
        }
        // Deliberately no daemon here. This is the "prepare a session with no
        // agent" path that every test uses, and leaking a background process
        // into a whole suite is how a suite becomes unreliable.
        return Ok(());
    }

    // The only window: after the manifest is written and the lock released,
    // before `exec` replaces this process. Nothing after `launch_claude` runs,
    // so a failure has to be reportable here or not at all.
    if cfg.daemon_autostart {
        match daemon::ensure_running(project) {
            Ok(ep) => {
                if !quiet {
                    println!("watching in the background (port {}).", ep.port);
                }
            }
            // Warn and carry on. A session without a daemon is still a session,
            // and refusing to launch the agent over a background process is the
            // wrong trade.
            Err(e) => eprintln!(
                "warning: could not start the rote daemon ({e:#}).
                 `rote watch` will start one when you need it."
            ),
        }
    }

    launch_claude(project, cfg)
}

/// Replace this process with claude, running in the shadow.
///
/// Process replacement rather than a child process: the user gets their pane
/// back exactly as if they had run claude themselves. No prompt is injected and
/// no extra arguments are passed — the agent must not be able to tell.
fn launch_claude(project: &ProjectPaths, cfg: &Config) -> Result<()> {
    use std::os::unix::process::CommandExt;

    let (program, args) = cfg
        .claude_cmd
        .split_first()
        .context("claude_cmd is empty")?;
    let err = std::process::Command::new(program)
        .args(args)
        .current_dir(&project.shadow_dir)
        .exec();

    // exec only returns on failure.
    Err(err).with_context(|| {
        format!(
            "cannot launch `{}` (configured as claude_cmd).\n\
             Check it is installed and on PATH, or set claude_cmd in ~/.config/rote/config.toml.",
            cfg.claude_cmd.join(" ")
        )
    })
}

fn cmd_status(project: &ProjectPaths) -> Result<()> {
    let Some(manifest) = Manifest::load(project)? else {
        println!("state:  idle");
        println!("\nno active session. `rote start \"what you're working on\"` begins one.");
        return Ok(());
    };

    let age = rote::age_since(&manifest.created_at);
    println!("state:  {}{}", manifest.state, age);
    println!(
        "task:   {}",
        if manifest.task.is_empty() {
            "(unspecified)"
        } else {
            &manifest.task
        }
    );
    println!("shadow: {}", project.shadow_dir.display());

    // Cheap enough to check on every status, and worth knowing about.
    if let Ok(head) = git::head_commit(&project.repo_root) {
        if head != manifest.baseline.head {
            println!("\nbaseline drift: the repository has moved since this session started.");
        }
    }

    let pending = manifest.count(Status::Pending);
    let typed = manifest.count(Status::Typed);
    let diverged = manifest.count(Status::Diverged);
    let skipped = manifest.count(Status::Skipped);
    println!(
        "\nhunks:  {} total — {pending} pending, {typed} typed, {diverged} diverged, {skipped} skipped",
        manifest.hunks.len()
    );

    // Sorted before deduping, because `dedup` only drops *adjacent* repeats and
    // `manifest.hunks` is storage order: reconcile appends a re-identified hunk
    // to the tail, so typing near one file's hunk moves it behind another's and
    // the file is listed twice.
    let mut files: Vec<&str> = manifest
        .hunks
        .iter()
        .filter(|h| h.status == Status::Pending)
        .map(|h| h.file.as_str())
        .collect();
    files.sort_unstable();
    files.dedup();
    if !files.is_empty() {
        println!("files:  {}", files.join(", "));
    }

    // `rote watch` is the loop now, so that is what "next" means — including
    // when the queue is empty, because the agent may still be working and the
    // pane picks that up on its own.
    println!(
        "\nnext:   {}",
        if manifest.hunks.is_empty() {
            "rote watch  (waiting for the agent's work)"
        } else if pending > 0 {
            "rote watch"
        } else {
            "rote done"
        }
    );
    Ok(())
}

fn cmd_abort(project: &ProjectPaths, cfg: &Config, yes: bool, quiet: bool) -> Result<()> {
    // Read only, and only to shape the prompt. The authoritative copy is
    // reloaded under the lock below.
    let manifest = Manifest::require(project)?;

    if !yes {
        let untyped = manifest.count(Status::Pending) + manifest.count(Status::Skipped);
        println!(
            "abort this session? the agent's work is discarded ({untyped} hunk(s) not typed)."
        );
        println!("your real tree is untouched either way.");
        if !confirm("abort? [y/N] ")? {
            println!("left the session open.");
            return Ok(());
        }
    }

    daemon::reap(project, Some(Terminal::Aborted));

    // Reloaded under the lock — the copy read before the prompt may be stale.
    let _lock = Lock::acquire(&project.lock_path())?;
    let mut manifest = Manifest::require(project)?;
    // Residue first, then the shadow reset that destroys the agent's copy.
    let archived = session::archive_and_clear(&mut manifest, project, cfg, Terminal::Aborted)?;
    shadow::sync(project, cfg)?;

    if !quiet {
        println!("session aborted. state: idle");
        if archived.residue_bytes > 0 {
            println!("the agent's unabsorbed work: {}", archived.patch.display());
        }
    }
    Ok(())
}

/// Read a yes/no answer. A closed stdin means "no" — never assume consent.
fn confirm(prompt: &str) -> Result<bool> {
    use std::io::Write as _;
    print!("{prompt}");
    std::io::stdout().flush().ok();
    let mut line = String::new();
    if std::io::stdin().read_line(&mut line).unwrap_or(0) == 0 {
        return Ok(false);
    }
    Ok(matches!(
        line.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}

fn cmd_init(project: &ProjectPaths, force: bool, quiet: bool) -> Result<()> {
    let target = project.project_config();
    if target.exists() && !force {
        bail!(
            "{} already exists.\nRe-run with --force to overwrite it.",
            target.display()
        );
    }
    let detected = detect::detect_project(&project.repo_root);
    // The one documented write into the real tree (ARCHITECTURE.md, Principle 2),
    // so it deliberately does not route through the guarded writers.
    std::fs::write(&target, config::init_template(&detected))
        .with_context(|| format!("cannot write {}", target.display()))?;

    if !quiet {
        println!("wrote {}", target.display());
        if detected.checks.is_empty() {
            println!(
                "no project type detected — add your own commands to [checks] so \
                 `rote done` can verify the session."
            );
        } else {
            println!(
                "detected a {} project; checks set to: {}",
                detected.kind.label(),
                detected.checks.join(", ")
            );
        }
        println!("next: rote start \"what you're working on\"");
    }
    Ok(())
}
