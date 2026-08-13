use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use rote::config::{Config, INIT_TEMPLATE};
use rote::git;
use rote::hunks::{Divergence, Op, Status};
use rote::paths::{self, Lock, ProjectPaths};
use rote::present::{self, Classification};
use rote::review;
use rote::session::{self, Manifest, State, Terminal};
use rote::shadow;
use std::path::PathBuf;
use std::process::ExitCode;

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

    /// Present the next pending hunk and open your editor on it.
    Next {
        /// Emit the next pending hunk as one JSON line instead of presenting it.
        #[arg(long, hide = true)]
        json: bool,
    },

    /// Re-print the most recently presented hunk. Display only; changes nothing.
    Back,

    /// Mark the head-of-queue hunk skipped without opening the editor.
    Skip,

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

    /// Set a hunk's status directly (companion to `next --json`).
    #[command(hide = true)]
    Mark { hunk_id: String, status: String },
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

fn run() -> Result<ExitCode> {
    let cli = Cli::parse();

    // Resolved for every command: all of them operate on a repo.
    let cwd = std::env::current_dir().context("cannot determine current directory")?;
    let repo_root = match &cli.project {
        Some(p) => paths::discover_repo_root(p)?,
        None => paths::discover_repo_root(&cwd)?,
    };
    let project = ProjectPaths::resolve(&repo_root)?;
    let cfg = Config::load(&paths::global_config_path()?, &project.project_config())?;
    // --no-color wins over config; NO_COLOR is honored as the de facto standard.
    let color = cfg.color && !cli.no_color && std::env::var_os("NO_COLOR").is_none();

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
        Command::Back => {
            cmd_back(&project, color)?;
            Ok(ExitCode::SUCCESS)
        }
        Command::Skip => {
            cmd_skip(&project, &cfg)?;
            Ok(ExitCode::SUCCESS)
        }
        Command::Mark { hunk_id, status } => {
            cmd_mark(&project, &hunk_id, &status)?;
            Ok(ExitCode::SUCCESS)
        }
        Command::Talk { attach } => {
            cmd_talk(&project, &cfg, attach)?;
            Ok(ExitCode::SUCCESS)
        }
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
    let mut manifest = Manifest::require(project)?;

    // 1. Refuse mid-operation, before anything expensive runs.
    shadow::ensure_no_operation_in_progress(project)?;

    // 2. Recompute; pending hunks need consent before they become skipped.
    let report = session::recompute(&mut manifest, project, cfg)?;
    for w in &report.warnings {
        eprintln!("warning: {w}");
    }
    let pending: Vec<String> = manifest
        .pending()
        .map(|h| format!("  {}:{}", h.file, h.anchor_hint))
        .collect();
    if !pending.is_empty() {
        println!("{} hunk(s) still pending:", pending.len());
        for p in &pending {
            println!("{p}");
        }
        if !force && !confirm("close anyway, marking them skipped? [y/N] ")? {
            println!("left the session open. `rote next` continues.");
            return Ok(());
        }
        let ids: Vec<String> = manifest.pending().map(|h| h.id.clone()).collect();
        for id in ids {
            if let Some(h) = manifest.find_mut(&id) {
                h.status = Status::Skipped;
            }
        }
    }

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

    // 6. Residue first, then archive, then the sync that destroys the shadow's copy.
    let _lock = Lock::acquire(&project.lock_path())?;
    let archived = session::archive_and_clear(&mut manifest, project, cfg, Terminal::Done)?;
    shadow::sync(project, cfg)?;

    if !quiet {
        println!("session closed. state: idle");
        if let Some(msg) = review::describe_residue(&archived.patch, archived.residue_bytes) {
            println!("{msg}");
        }
    }
    Ok(())
}

/// Recompute, then present the head of the queue.
fn cmd_next(project: &ProjectPaths, cfg: &Config, json: bool, color: bool) -> Result<()> {
    let mut manifest = Manifest::require(project)?;

    // First `next` is what turns a working session into a transcribing one.
    if manifest.state == State::Working {
        manifest.state = State::Transcribing;
    }

    let report = session::recompute(&mut manifest, project, cfg)?;
    for w in &report.warnings {
        eprintln!("warning: {w}");
    }

    let Some(hunk) = manifest.head_of_queue().cloned() else {
        save_locked(&manifest, project)?;
        println!("nothing to transcribe.");
        if !manifest.hunks.is_empty() {
            println!("every hunk is accounted for — `rote done` closes the session.");
        }
        return Ok(());
    };

    if json {
        // The v2 seam: emit and exit, classifying nothing. `rote mark` is how a
        // front end reports back.
        println!("{}", serde_json::to_string(&hunk)?);
        save_locked(&manifest, project)?;
        return Ok(());
    }

    let total = manifest.pending().count();
    let position = manifest.queue_position(&hunk.id).unwrap_or(1);
    let real_file = project.repo_root.join(&hunk.file);

    let outcome = present_one(&hunk, project, cfg, position, total, color, &real_file)?;

    let label = match &outcome {
        Classification::Typed => "typed",
        Classification::Untouched => "untouched",
        Classification::Diverged { .. } => "diverged",
    };

    {
        let entry = manifest
            .find_mut(&hunk.id)
            .context("the hunk vanished from the manifest mid-command")?;
        match &outcome {
            Classification::Typed => entry.status = Status::Typed,
            Classification::Untouched => {} // stays pending
            Classification::Diverged { actual } => {
                entry.status = Status::Diverged;
                entry.divergence = Some(Divergence {
                    proposed: hunk.new_lines.clone(),
                    actual: actual.clone(),
                });
            }
        }
    }
    manifest.last_presented = Some(hunk.id.clone());
    save_locked(&manifest, project)?;

    println!("[{position}/{total}] {label} — {}", hunk.file);
    if matches!(outcome, Classification::Untouched) {
        println!("run `rote next` to try again, or `rote skip` to leave it.");
    }
    Ok(())
}

/// Render a hunk, get the user's version into the file, and classify it.
fn present_one(
    hunk: &rote::hunks::Hunk,
    project: &ProjectPaths,
    cfg: &Config,
    position: usize,
    total: usize,
    color: bool,
    real_file: &std::path::Path,
) -> Result<Classification> {
    // Untypeable hunks never reach an editor: the gate is a byte comparison.
    if hunk.is_untypeable() {
        let anchor = present::Anchor {
            line: 1,
            via: present::AnchorVia::NoContext,
        };
        print!("{}", present::render(hunk, position, total, &anchor, color));
        let shadow_file = project.shadow_dir.join(&hunk.file);
        let outcome = present::classify_by_bytes(real_file, &shadow_file);
        if outcome == Classification::Untouched {
            println!("still differs from the shadow — do that, then run `rote next` again.");
        }
        return Ok(outcome);
    }

    let editor = present::editor_command(&cfg.editor);
    let is_new = hunk.op == Op::CreateFile;

    loop {
        let file_lines = present::read_lines(real_file)?;
        let anchor = present::find_anchor(&file_lines, hunk);
        print!("{}", present::render(hunk, position, total, &anchor, color));
        println!("opening {} at {}:{} …", editor[0], hunk.file, anchor.line);

        if !present::launch_editor(&editor, real_file, anchor.line, is_new)? {
            // A nonzero exit means the user bailed out (DESIGN.md §9.6).
            return Ok(Classification::Untouched);
        }

        let after = present::read_lines(real_file)?;
        match present::classify(&after, hunk, cfg.strict_whitespace) {
            Classification::Diverged { actual } => {
                print!(
                    "{}",
                    present::render_divergence(&hunk.new_lines, &actual, color)
                );
                match prompt_divergence()? {
                    Divergent::Keep => return Ok(Classification::Diverged { actual }),
                    Divergent::Retry => continue,
                    Divergent::Show => continue,
                }
            }
            other => return Ok(other),
        }
    }
}

enum Divergent {
    Keep,
    Retry,
    Show,
}

fn prompt_divergence() -> Result<Divergent> {
    use std::io::Write as _;
    loop {
        print!("[k]eep mine   [r]etry (reopen editor)   [s]how full hunk again: ");
        std::io::stdout().flush().ok();
        let mut line = String::new();
        if std::io::stdin().read_line(&mut line).unwrap_or(0) == 0 {
            // No one is there to answer; keeping what is already on disk is the
            // only choice that does not discard the user's work.
            return Ok(Divergent::Keep);
        }
        match line.trim().to_ascii_lowercase().as_str() {
            "k" | "keep" => return Ok(Divergent::Keep),
            "r" | "retry" => return Ok(Divergent::Retry),
            "s" | "show" => return Ok(Divergent::Show),
            _ => continue,
        }
    }
}

/// Re-print the last presented hunk. Display only — it changes nothing.
fn cmd_back(project: &ProjectPaths, color: bool) -> Result<()> {
    let manifest = Manifest::require(project)?;
    let Some(id) = manifest.last_presented.clone() else {
        println!("nothing presented yet in this session.");
        return Ok(());
    };
    let Some(hunk) = manifest.hunks.iter().find(|h| h.id == id) else {
        println!("the last presented hunk is no longer in the queue (the agent reworked it).");
        return Ok(());
    };

    let file_lines = present::read_lines(&project.repo_root.join(&hunk.file))?;
    let anchor = present::find_anchor(&file_lines, hunk);
    print!("{}", present::render(hunk, 0, 0, &anchor, color));
    println!("status: {}", hunk.status);
    if let Some(d) = &hunk.divergence {
        print!(
            "{}",
            present::render_divergence(&d.proposed, &d.actual, color)
        );
    }
    println!("\n(display only — `rote back` changes nothing)");
    Ok(())
}

fn cmd_skip(project: &ProjectPaths, cfg: &Config) -> Result<()> {
    let mut manifest = Manifest::require(project)?;
    session::recompute(&mut manifest, project, cfg)?;

    let Some(id) = manifest.head_of_queue().map(|h| h.id.clone()) else {
        println!("nothing to skip — the queue is empty.");
        return Ok(());
    };
    let file = {
        let h = manifest.find_mut(&id).expect("just found it");
        h.status = Status::Skipped;
        h.file.clone()
    };
    manifest.last_presented = Some(id);
    save_locked(&manifest, project)?;
    println!("skipped — {file}");
    println!("it will not come back. `rote done` reports it before closing.");
    Ok(())
}

/// Hidden companion to `next --json`: set a hunk's status from outside.
fn cmd_mark(project: &ProjectPaths, hunk_id: &str, status: &str) -> Result<()> {
    let mut manifest = Manifest::require(project)?;
    let parsed: Status = status.parse()?;
    let hunk = manifest
        .find_mut(hunk_id)
        .with_context(|| format!("no hunk {hunk_id} in this session"))?;
    hunk.status = parsed;
    manifest.last_presented = Some(hunk_id.to_string());
    save_locked(&manifest, project)?;
    Ok(())
}

fn save_locked(manifest: &Manifest, project: &ProjectPaths) -> Result<()> {
    let _lock = Lock::acquire(&project.lock_path())?;
    manifest.save(project)
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
    }

    if !task.is_empty() && !quiet {
        println!("\ntask: {task}\n");
    }

    if no_launch {
        if !quiet {
            println!("session open (not launching claude).");
            println!("shadow: {}", project.shadow_dir.display());
        }
        return Ok(());
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

    if manifest.hunks.is_empty() {
        println!("\nno hunks computed yet — `rote next` builds the queue.");
        return Ok(());
    }

    let pending = manifest.count(Status::Pending);
    let typed = manifest.count(Status::Typed);
    let diverged = manifest.count(Status::Diverged);
    let skipped = manifest.count(Status::Skipped);
    println!(
        "\nhunks:  {} total — {pending} pending, {typed} typed, {diverged} diverged, {skipped} skipped",
        manifest.hunks.len()
    );

    let mut files: Vec<&str> = manifest
        .hunks
        .iter()
        .filter(|h| h.status == Status::Pending)
        .map(|h| h.file.as_str())
        .collect();
    files.dedup();
    if !files.is_empty() {
        println!("files:  {}", files.join(", "));
    }

    println!(
        "\nnext:   {}",
        if pending > 0 {
            "rote next"
        } else {
            "rote done"
        }
    );
    Ok(())
}

fn cmd_abort(project: &ProjectPaths, cfg: &Config, yes: bool, quiet: bool) -> Result<()> {
    let mut manifest = Manifest::require(project)?;

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

    let _lock = Lock::acquire(&project.lock_path())?;
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
    // The one documented write into the real tree (ARCHITECTURE.md, Principle 2),
    // so it deliberately does not route through the guarded writers.
    std::fs::write(&target, INIT_TEMPLATE)
        .with_context(|| format!("cannot write {}", target.display()))?;
    if !quiet {
        println!("wrote {}", target.display());
        println!("next: rote start \"what you're working on\"");
    }
    Ok(())
}
