//! Driving the built `rote` binary from an integration test.
//!
//! This was three near-identical harnesses — one each in `transcribe.rs`,
//! `done.rs` and `setup.rs` — that had already drifted apart: only one of them
//! cleared `ROTE_EDITOR`, only one prepended a stub directory to `PATH`, and
//! each had its own copy of `rote_bin`. A test's behaviour depended on which
//! file it happened to live in.
//!
//! One harness, one environment. Every invocation is hermetic: isolated XDG
//! roots from `Fixture`, no colour, no inherited `ROTE_EDITOR`, and a stub
//! directory at the front of `PATH` whether or not the test puts anything in it.

#![allow(dead_code)]

use super::Fixture;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

/// The binary under test. The test binary lives in `target/<profile>/deps/`,
/// so `rote` is two levels up.
pub fn rote_bin() -> PathBuf {
    let mut p = std::env::current_exe().unwrap();
    p.pop();
    p.pop();
    p.join("rote")
}

/// Write a file and make it executable.
pub fn write_exec(path: &Path, body: &str) {
    std::fs::write(path, body).unwrap();
    let mut perms = std::fs::metadata(path).unwrap().permissions();
    std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o755);
    std::fs::set_permissions(path, perms).unwrap();
}

/// A `$ROTE_EDITOR` stand-in that applies a canned edit.
///
/// It ignores the `+LINE` argument and rewrites the whole file, which is all a
/// test needs: what matters is what lands on disk before rote reads it back.
pub fn scripted_editor(dir: &Path, name: &str, body: &str) -> PathBuf {
    let path = dir.join(name);
    write_exec(
        &path,
        &format!(
            "#!/bin/sh\n\
             # last argument is the file; earlier ones may include +LINE\n\
             for a in \"$@\"; do f=\"$a\"; done\n\
             cat > \"$f\" <<'ROTE_EOF'\n{body}ROTE_EOF\n"
        ),
    );
    path
}

/// A stand-in for `claude` that records its argv and the payload on stdin.
///
/// Returns the script path and the path the payload is captured to.
pub fn stub_claude(dir: &Path, name: &str, reply: &str, exit_code: i32) -> (PathBuf, PathBuf) {
    let script = dir.join(name);
    let capture = dir.join(format!("{name}.payload"));
    let args_log = dir.join(format!("{name}.args"));
    write_exec(
        &script,
        &format!(
            "#!/bin/sh\n\
             printf '%s\\n' \"$@\" > {args}\n\
             cat > {cap}\n\
             printf '%s' '{reply}'\n\
             exit {exit_code}\n",
            args = args_log.display(),
            cap = capture.display(),
        ),
    );
    (script, capture)
}

/// A `claude` stand-in whose `--help` advertises the tool-restriction flag,
/// so `rote doctor` sees a reviewer path it believes in.
pub fn claude_stub_with_help(dir: &Path) -> PathBuf {
    let p = dir.join("claude-stub");
    write_exec(
        &p,
        "#!/bin/sh\n\
         case \"$1\" in --help) echo '  --tools <tools...>  Use \"\" to disable all tools';; esac\n\
         exit 0\n",
    );
    p
}

pub struct Cli {
    pub fx: Fixture,
    /// Prepended to `PATH`, for stubbing binaries rote looks up by name.
    pub bin: PathBuf,
    editor: Option<PathBuf>,
}

impl Cli {
    pub fn new() -> Self {
        Self::with_fixture(Fixture::new())
    }

    pub fn with_fixture(fx: Fixture) -> Self {
        let bin = fx.root.path().join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        Self {
            fx,
            bin,
            editor: None,
        }
    }

    pub fn with_editor(mut self, editor: PathBuf) -> Self {
        self.editor = Some(editor);
        self
    }

    pub fn set_editor(&mut self, editor: PathBuf) {
        self.editor = Some(editor);
    }

    /// Run in the repository root.
    pub fn run(&self, args: &[&str]) -> Output {
        self.run_in_with_input(args, &self.fx.repo, "")
    }

    pub fn run_with_input(&self, args: &[&str], stdin_text: &str) -> Output {
        self.run_in_with_input(args, &self.fx.repo, stdin_text)
    }

    /// Run somewhere else — `doctor` and `setup` work outside a repository.
    pub fn run_in(&self, args: &[&str], cwd: &Path) -> Output {
        self.run_in_with_input(args, cwd, "")
    }

    pub fn run_in_with_input(&self, args: &[&str], cwd: &Path, stdin_text: &str) -> Output {
        use std::io::Write as _;
        let path = format!(
            "{}:{}",
            self.bin.display(),
            std::env::var("PATH").unwrap_or_default()
        );
        let mut cmd = Command::new(rote_bin());
        cmd.args(args)
            .current_dir(cwd)
            .env("PATH", path)
            .env("XDG_CACHE_HOME", &self.fx.xdg_cache)
            .env("XDG_DATA_HOME", &self.fx.xdg_data)
            .env("XDG_CONFIG_HOME", &self.fx.xdg_config)
            .env("NO_COLOR", "1")
            // Never inherit the developer's editor: a test that depends on the
            // machine it runs on is worse than no test.
            .env_remove("ROTE_EDITOR")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(e) = &self.editor {
            cmd.env("ROTE_EDITOR", e);
        }
        let mut child = cmd.spawn().unwrap();
        child
            .stdin
            .as_mut()
            .unwrap()
            .write_all(stdin_text.as_bytes())
            .unwrap();
        drop(child.stdin.take());
        child.wait_with_output().unwrap()
    }

    /// Start a command and leave it running.
    ///
    /// The first thing in this suite that outlives a single call — `rote watch`
    /// is a loop, so a test has to run alongside it rather than after it. Stdin
    /// is null so the pane never takes raw mode.
    pub fn spawn(&self, args: &[&str]) -> std::process::Child {
        let path = format!(
            "{}:{}",
            self.bin.display(),
            std::env::var("PATH").unwrap_or_default()
        );
        Command::new(rote_bin())
            .args(args)
            .current_dir(&self.fx.repo)
            .env("PATH", path)
            .env("XDG_CACHE_HOME", &self.fx.xdg_cache)
            .env("XDG_DATA_HOME", &self.fx.xdg_data)
            .env("XDG_CONFIG_HOME", &self.fx.xdg_config)
            .env("NO_COLOR", "1")
            .env_remove("ROTE_EDITOR")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap()
    }

    pub fn shadow(&self) -> PathBuf {
        self.fx.project().shadow_dir
    }

    pub fn global_config(&self) -> PathBuf {
        self.fx.xdg_config.join("rote/config.toml")
    }

    /// Point `claude_cmd` at a stub via the global config file.
    pub fn use_claude_stub(&self, stub: &Path) {
        let dir = self.fx.xdg_config.join("rote");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("config.toml"),
            format!("claude_cmd = [\"{}\"]\n", stub.display()),
        )
        .unwrap();
    }

    pub fn write_project_config(&self, body: &str) {
        std::fs::write(self.fx.repo.join(".rote.toml"), body).unwrap();
    }
}

impl Default for Cli {
    fn default() -> Self {
        Self::new()
    }
}

pub fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

pub fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}
