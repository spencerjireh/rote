//! `rote setup`: write the global config at ~/.config/rote/config.toml.
//!
//! The only command that writes there, and one of the two (with `doctor`) that
//! are about this machine rather than about a repository. It prompts, so its
//! helpers — `prompt_line` and the three TOML emitters — live here rather than
//! being shared: nothing else in rote writes TOML by hand.

use crate::config::Config;
use crate::detect;
use crate::paths;
use anyhow::{bail, Context, Result};

/// Write the global config. The only command that does.
pub fn run(cfg: &Config, force: bool, quiet: bool) -> Result<()> {
    let target = paths::global_config_path()?;
    if target.exists() && !force {
        bail!(
            "{} already exists.\nRe-run with --force to overwrite it, or edit it by hand.",
            target.display()
        );
    }

    let claude = detect::probe_claude(cfg);
    let editor = detect::probe_editor(cfg);

    // Prompt only where detection is ambiguous, and only when someone is there
    // to answer — a non-tty takes the detected values.
    let interactive = std::io::IsTerminal::is_terminal(&std::io::stdin());

    // Written unconditionally, and never prompted for. rote does not launch an
    // editor as part of the loop any more, so stopping setup to ask about one
    // would be blocking on an answer nothing needs.
    let editor_cmd = editor.command.join(" ");

    let claude_cmd = if claude.found() || !interactive {
        cfg.claude_cmd.join(" ")
    } else {
        println!("`{}` was not found on PATH.", cfg.claude_cmd.join(" "));
        let answer = prompt_line("path to the claude binary? [claude] ")?;
        if answer.trim().is_empty() {
            "claude".into()
        } else {
            answer.trim().to_string()
        }
    };

    let body = format!(
        r#"# rote global configuration.
# Per-project .rote.toml files override anything set here.

# Optional. rote never opens this on its own; it is what the `o` key in
# `rote watch` runs, as: editor +LINE FILE
editor = {editor}

# The session agent. An argument vector, not a shell string.
claude_cmd = {claude}
claude_continue_cmd = {claude_continue}

color = true

# Defaults for every project; override per-project in .rote.toml.
max_hunk_lines = 20
strict_whitespace = false
"#,
        editor = toml_str(&editor_cmd),
        claude = toml_vec(&split_ws(&claude_cmd)),
        claude_continue = toml_vec(&{
            let mut v = split_ws(&claude_cmd);
            v.push("--continue".into());
            v
        }),
    );

    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("cannot create {}", parent.display()))?;
    }
    paths::write_atomic(&target, body.as_bytes())?;

    if !quiet {
        println!("wrote {}", target.display());
        if !claude.found() {
            println!("note: claude still does not resolve — `rote doctor` will tell you.");
        }
        println!("next: rote doctor");
    }
    Ok(())
}

fn split_ws(s: &str) -> Vec<String> {
    s.split_whitespace().map(String::from).collect()
}

fn toml_str(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

fn toml_vec(items: &[String]) -> String {
    let inner: Vec<String> = items.iter().map(|s| toml_str(s)).collect();
    format!("[{}]", inner.join(", "))
}

fn prompt_line(prompt: &str) -> Result<String> {
    use std::io::Write as _;
    print!("{prompt}");
    std::io::stdout().flush().ok();
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    Ok(line)
}
