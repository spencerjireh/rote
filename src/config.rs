//! Global (`~/.config/rote/config.toml`) and per-project (`.rote.toml`) config,
//! merged with project values winning. DESIGN.md §7.

use anyhow::{Context, Result};
use globset::{Glob, GlobSet, GlobSetBuilder};
use serde::Deserialize;
use std::path::Path;

pub const DEFAULT_MAX_HUNK_LINES: usize = 20;

fn default_editor() -> String {
    "nvim".into()
}

fn default_claude_cmd() -> Vec<String> {
    vec!["claude".into()]
}

fn default_claude_continue_cmd() -> Vec<String> {
    vec!["claude".into(), "--continue".into()]
}

fn default_preserve() -> Vec<String> {
    ["target/", "node_modules/", ".venv/", "dist/", "build/"]
        .iter()
        .map(|s| s.to_string())
        .collect()
}

fn default_verbatim() -> Vec<String> {
    [
        "Cargo.lock",
        "package-lock.json",
        "pnpm-lock.yaml",
        "yarn.lock",
        "poetry.lock",
        "uv.lock",
        "*.lock",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

/// Raw global file. Every field optional so absence means "use the default".
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct GlobalFile {
    editor: Option<String>,
    claude_cmd: Option<Vec<String>>,
    claude_continue_cmd: Option<Vec<String>>,
    color: Option<bool>,
    max_hunk_lines: Option<usize>,
    strict_whitespace: Option<bool>,
}

/// Raw project file.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProjectFile {
    max_hunk_lines: Option<usize>,
    strict_whitespace: Option<bool>,
    #[serde(default)]
    shadow: Option<ShadowFile>,
    #[serde(default)]
    transcribe: Option<TranscribeFile>,
    #[serde(default)]
    checks: Option<ChecksFile>,
    #[serde(default)]
    review: Option<ReviewFile>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct ShadowFile {
    copy: Option<Vec<String>>,
    preserve: Option<Vec<String>>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct TranscribeFile {
    verbatim: Option<Vec<String>>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct ChecksFile {
    commands: Option<Vec<String>>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReviewFile {
    enabled: Option<bool>,
    model_args: Option<Vec<String>>,
}

/// Fully resolved configuration: defaults, overlaid by global, overlaid by project.
#[derive(Debug, Clone)]
pub struct Config {
    pub editor: String,
    pub claude_cmd: Vec<String>,
    pub claude_continue_cmd: Vec<String>,
    pub color: bool,
    pub max_hunk_lines: usize,
    pub strict_whitespace: bool,
    pub shadow_copy: Vec<String>,
    pub shadow_preserve: Vec<String>,
    pub verbatim: Vec<String>,
    pub check_commands: Vec<String>,
    pub review_enabled: bool,
    pub review_model_args: Vec<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            editor: default_editor(),
            claude_cmd: default_claude_cmd(),
            claude_continue_cmd: default_claude_continue_cmd(),
            color: true,
            max_hunk_lines: DEFAULT_MAX_HUNK_LINES,
            strict_whitespace: false,
            shadow_copy: vec![".env".into()],
            shadow_preserve: default_preserve(),
            verbatim: default_verbatim(),
            check_commands: Vec::new(),
            review_enabled: true,
            review_model_args: Vec::new(),
        }
    }
}

impl Config {
    /// Load and merge. A missing file at either level is not an error.
    pub fn load(global_path: &Path, project_path: &Path) -> Result<Self> {
        let mut cfg = Config::default();

        if let Some(text) = read_if_exists(global_path)? {
            let g: GlobalFile = toml::from_str(&text)
                .with_context(|| format!("cannot parse {}", global_path.display()))?;
            if let Some(v) = g.editor {
                cfg.editor = v;
            }
            if let Some(v) = g.claude_cmd {
                cfg.claude_cmd = v;
            }
            if let Some(v) = g.claude_continue_cmd {
                cfg.claude_continue_cmd = v;
            }
            if let Some(v) = g.color {
                cfg.color = v;
            }
            if let Some(v) = g.max_hunk_lines {
                cfg.max_hunk_lines = v;
            }
            if let Some(v) = g.strict_whitespace {
                cfg.strict_whitespace = v;
            }
        }

        if let Some(text) = read_if_exists(project_path)? {
            let p: ProjectFile = toml::from_str(&text)
                .with_context(|| format!("cannot parse {}", project_path.display()))?;
            if let Some(v) = p.max_hunk_lines {
                cfg.max_hunk_lines = v;
            }
            if let Some(v) = p.strict_whitespace {
                cfg.strict_whitespace = v;
            }
            if let Some(s) = p.shadow {
                if let Some(v) = s.copy {
                    cfg.shadow_copy = v;
                }
                if let Some(v) = s.preserve {
                    cfg.shadow_preserve = v;
                }
            }
            if let Some(t) = p.transcribe {
                if let Some(v) = t.verbatim {
                    cfg.verbatim = v;
                }
            }
            if let Some(c) = p.checks {
                if let Some(v) = c.commands {
                    cfg.check_commands = v;
                }
            }
            if let Some(r) = p.review {
                if let Some(v) = r.enabled {
                    cfg.review_enabled = v;
                }
                if let Some(v) = r.model_args {
                    cfg.review_model_args = v;
                }
            }
        }

        if cfg.max_hunk_lines == 0 {
            anyhow::bail!("max_hunk_lines must be at least 1 (found 0)");
        }
        if cfg.claude_cmd.is_empty() {
            anyhow::bail!("claude_cmd must name a command");
        }
        if cfg.claude_continue_cmd.is_empty() {
            anyhow::bail!("claude_continue_cmd must name a command");
        }

        Ok(cfg)
    }

    /// Compiled matcher for `[transcribe] verbatim`.
    pub fn verbatim_set(&self) -> Result<GlobSet> {
        build_globset(&self.verbatim, "transcribe.verbatim")
    }
}

/// Build a glob matcher. Directory-style entries (`target/`) also match
/// everything beneath them, which is how `preserve` is meant to read.
pub fn build_globset(patterns: &[String], what: &str) -> Result<GlobSet> {
    let mut builder = GlobSetBuilder::new();
    for pat in patterns {
        let trimmed = pat.trim_end_matches('/');
        if trimmed.is_empty() {
            continue;
        }
        builder.add(Glob::new(trimmed).with_context(|| format!("invalid glob in {what}: {pat}"))?);
        if pat.ends_with('/') || !pat.contains('*') {
            builder.add(
                Glob::new(&format!("{trimmed}/**"))
                    .with_context(|| format!("invalid glob in {what}: {pat}"))?,
            );
        }
    }
    builder
        .build()
        .with_context(|| format!("cannot build glob set for {what}"))
}

fn read_if_exists(path: &Path) -> Result<Option<String>> {
    match std::fs::read_to_string(path) {
        Ok(s) => Ok(Some(s)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("cannot read {}", path.display())),
    }
}

/// The commented `.rote.toml` written by `rote init`.
pub const INIT_TEMPLATE: &str = r#"# rote per-project configuration.
# Values here override the global ~/.config/rote/config.toml.

# Maximum lines per transcription hunk. Larger raw diffs are split.
max_hunk_lines = 20

# true = trailing whitespace differences count as divergence when classifying.
strict_whitespace = false

[shadow]
# Gitignored files the agent needs; copied real -> shadow on every sync.
copy = [".env"]

# Paths that survive the shadow's clean, so the agent's builds stay warm.
# Build output ONLY -- never source, never anything the diff reads.
preserve = ["target/", "node_modules/", ".venv/", "dist/", "build/"]

[transcribe]
# Generated files. Presented whole and gated on a byte comparison rather than
# typed line by line -- run the generating command instead.
verbatim = [
    "Cargo.lock",
    "package-lock.json",
    "pnpm-lock.yaml",
    "yarn.lock",
    "poetry.lock",
    "uv.lock",
    "*.lock",
]

[checks]
# Run in the REAL tree at `rote done`, in order, stopping on the first failure.
commands = []

[review]
# Headless reviewer pass over the session diff at `rote done`.
enabled = true
# Extra args appended to `claude -p`, e.g. ["--model", "claude-haiku-4-5"]
model_args = []
"#;

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn write(dir: &Path, name: &str, body: &str) -> std::path::PathBuf {
        let p = dir.join(name);
        fs::write(&p, body).unwrap();
        p
    }

    #[test]
    fn defaults_when_both_files_absent() {
        let dir = tempfile::tempdir().unwrap();
        let cfg =
            Config::load(&dir.path().join("nope.toml"), &dir.path().join("nah.toml")).unwrap();
        assert_eq!(cfg.editor, "nvim");
        assert_eq!(cfg.claude_cmd, vec!["claude".to_string()]);
        assert_eq!(cfg.max_hunk_lines, 20);
        assert!(!cfg.strict_whitespace);
        assert!(cfg.review_enabled);
        assert!(cfg.verbatim.contains(&"Cargo.lock".to_string()));
        assert!(cfg.shadow_preserve.contains(&"target/".to_string()));
    }

    #[test]
    fn project_overrides_global() {
        let dir = tempfile::tempdir().unwrap();
        let g = write(
            dir.path(),
            "config.toml",
            "editor = \"hx\"\nmax_hunk_lines = 50\nstrict_whitespace = true\n",
        );
        let p = write(dir.path(), ".rote.toml", "max_hunk_lines = 5\n");
        let cfg = Config::load(&g, &p).unwrap();
        // Project wins where they overlap...
        assert_eq!(cfg.max_hunk_lines, 5);
        // ...global still supplies what the project file omits.
        assert_eq!(cfg.editor, "hx");
        assert!(cfg.strict_whitespace);
    }

    #[test]
    fn global_only_leaves_project_tables_at_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let g = write(dir.path(), "config.toml", "color = false\n");
        let cfg = Config::load(&g, &dir.path().join("absent.toml")).unwrap();
        assert!(!cfg.color);
        assert_eq!(cfg.shadow_copy, vec![".env".to_string()]);
    }

    #[test]
    fn claude_commands_parse_as_vectors() {
        let dir = tempfile::tempdir().unwrap();
        let g = write(
            dir.path(),
            "config.toml",
            "claude_cmd = [\"claude\", \"--foo\"]\nclaude_continue_cmd = [\"claude\", \"-c\"]\n",
        );
        let cfg = Config::load(&g, &dir.path().join("absent.toml")).unwrap();
        assert_eq!(cfg.claude_cmd, vec!["claude", "--foo"]);
        assert_eq!(cfg.claude_continue_cmd, vec!["claude", "-c"]);
    }

    #[test]
    fn project_tables_override_lists() {
        let dir = tempfile::tempdir().unwrap();
        let p = write(
            dir.path(),
            ".rote.toml",
            "[shadow]\ncopy = [\".env\", \".envrc\"]\npreserve = [\"out/\"]\n\
             [transcribe]\nverbatim = [\"*.lock\"]\n\
             [checks]\ncommands = [\"cargo test\"]\n\
             [review]\nenabled = false\nmodel_args = [\"--model\", \"claude-haiku-4-5\"]\n",
        );
        let cfg = Config::load(&dir.path().join("absent.toml"), &p).unwrap();
        assert_eq!(cfg.shadow_copy, vec![".env", ".envrc"]);
        assert_eq!(cfg.shadow_preserve, vec!["out/"]);
        assert_eq!(cfg.verbatim, vec!["*.lock"]);
        assert_eq!(cfg.check_commands, vec!["cargo test"]);
        assert!(!cfg.review_enabled);
        assert_eq!(cfg.review_model_args, vec!["--model", "claude-haiku-4-5"]);
    }

    #[test]
    fn init_template_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let p = write(dir.path(), ".rote.toml", INIT_TEMPLATE);
        let cfg = Config::load(&dir.path().join("absent.toml"), &p).unwrap();
        assert_eq!(cfg.max_hunk_lines, 20);
        assert!(cfg.review_enabled);
        assert_eq!(cfg.check_commands, Vec::<String>::new());
    }

    #[test]
    fn unknown_key_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let p = write(dir.path(), ".rote.toml", "max_hunk_lnies = 5\n");
        assert!(Config::load(&dir.path().join("absent.toml"), &p).is_err());
    }

    #[test]
    fn zero_max_hunk_lines_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let p = write(dir.path(), ".rote.toml", "max_hunk_lines = 0\n");
        assert!(Config::load(&dir.path().join("absent.toml"), &p).is_err());
    }

    #[test]
    fn verbatim_globs_match_lockfiles_not_source() {
        let cfg = Config::default();
        let set = cfg.verbatim_set().unwrap();
        assert!(set.is_match("Cargo.lock"));
        assert!(set.is_match("package-lock.json"));
        assert!(!set.is_match("Cargo.toml"));
        assert!(!set.is_match("src/main.rs"));
    }

    #[test]
    fn preserve_globs_match_directory_contents() {
        let set = build_globset(&default_preserve(), "shadow.preserve").unwrap();
        assert!(set.is_match("target"));
        assert!(set.is_match("target/debug/rote"));
        assert!(set.is_match("node_modules/left-pad/index.js"));
        assert!(!set.is_match("src/main.rs"));
    }
}
