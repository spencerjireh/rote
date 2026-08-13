//! Filesystem events, filtered down to the two trees rote cares about.
//!
//! Two decisions carry this module.
//!
//! **Directories are watched, never individual files.** Editors that save via
//! write-to-temp-and-rename — vim with `backupcopy=no`, VS Code, and most
//! others — replace the inode, and a watch registered on the old file goes
//! permanently deaf while reporting no error at all. rote exists to watch files
//! an editor writes, so this is the failure mode that matters most.
//!
//! **The event kind is discarded.** rote trusts only "something under this path
//! may have changed" and then re-reads from disk. That is what makes atomic
//! renames, metadata storms, and macOS fsevent's directory-granular coalescing
//! all harmless: a re-read that finds identical bytes classifies identically
//! and writes nothing. Acting on `EventKind` would mean being right about every
//! platform's notion of what a write is.

use crate::config::Config;
use crate::engine::{EngineEvent, Origin};
use crate::paths::ProjectPaths;
use crate::shadow;
use anyhow::{Context, Result};
use globset::GlobSet;
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;

/// The compiled filter, kept beside the watcher so a path is classified without
/// recompiling a glob set per event.
pub struct Filter {
    repo_root: PathBuf,
    shadow_dir: PathBuf,
    state_dir: PathBuf,
    preserve: GlobSet,
    ignore: GlobSet,
}

impl Filter {
    pub fn new(project: &ProjectPaths, cfg: &Config) -> Result<Self> {
        Ok(Self {
            repo_root: project.repo_root.clone(),
            shadow_dir: project.shadow_dir.clone(),
            state_dir: project.state_dir.clone(),
            preserve: shadow::preserve_matcher(cfg)?,
            ignore: cfg.watch_ignore_set()?,
        })
    }

    /// Which tree this path belongs to, and its path relative to that tree —
    /// or `None` when the engine should never hear about it.
    ///
    /// Ordered cheapest first: a `cargo build` under the user's own repo
    /// generates thousands of events per second, and every one of them reaches
    /// here.
    pub fn classify(&self, path: &Path) -> Option<(Origin, PathBuf)> {
        // The shadow is checked first only for clarity; `ProjectPaths::resolve_in`
        // already refuses a layout where one tree contains the other.
        let (origin, rel) = if let Ok(rel) = path.strip_prefix(&self.shadow_dir) {
            (Origin::Shadow, rel)
        } else if let Ok(rel) = path.strip_prefix(&self.repo_root) {
            (Origin::Real, rel)
        } else {
            return None;
        };

        // Every component, not just the first: submodules and nested worktrees
        // put a `.git` well below the root.
        if rel.components().any(|c| c.as_os_str() == ".git") {
            return None;
        }
        if path.starts_with(&self.state_dir) {
            return None;
        }
        if self.preserve.is_match(rel) || self.ignore.is_match(rel) {
            return None;
        }
        // `write_atomic` writes `.<name>.tmp` beside its target, so rote's own
        // writes would otherwise wake the engine.
        if let Some(name) = rel.file_name().and_then(|n| n.to_str()) {
            if name.starts_with('.') && name.ends_with(".tmp") {
                return None;
            }
        }
        if rel.as_os_str().is_empty() {
            return None; // the tree root itself
        }
        Some((origin, rel.to_path_buf()))
    }
}

/// A running watcher. Dropping it stops the threads.
pub struct Watch {
    _watcher: RecommendedWatcher,
    _pump: std::thread::JoinHandle<()>,
}

/// Watch both trees and forward filtered changes to the engine.
///
/// The shadow is watched as well as the real tree so that flipping back to the
/// agent, asking for rework, and flipping away again shows up in the pane
/// without anyone running a command — the "argue with it and carry on" story in
/// DESIGN.md §5, which was only true on paper while recompute was manual.
pub fn spawn(project: &ProjectPaths, cfg: &Config, out: Sender<EngineEvent>) -> Result<Watch> {
    let filter = Filter::new(project, cfg)?;
    let (tx, rx) = std::sync::mpsc::channel::<notify::Result<notify::Event>>();

    // notify implements EventHandler for a plain Sender, so the watcher thread
    // needs no adapter.
    let mut watcher = notify::recommended_watcher(tx).context("cannot start a file watcher")?;
    watcher
        .watch(&project.repo_root, RecursiveMode::Recursive)
        .with_context(|| format!("cannot watch {}", project.repo_root.display()))?;
    if project.shadow_dir.exists() {
        watcher
            .watch(&project.shadow_dir, RecursiveMode::Recursive)
            .with_context(|| format!("cannot watch {}", project.shadow_dir.display()))?;
    }

    let pump = std::thread::spawn(move || {
        for ev in rx {
            let sent = match ev {
                Ok(ev) => {
                    let mut ok = true;
                    for path in ev.paths {
                        if let Some((origin, rel)) = filter.classify(&path) {
                            if out.send(EngineEvent::Changed(origin, rel)).is_err() {
                                ok = false;
                                break;
                            }
                        }
                    }
                    ok
                }
                // A watch-limit exhaustion or an unreadable directory. Report it
                // and let the engine fall back to polling rather than dying:
                // a session with a broken watcher is still a session.
                Err(e) => out.send(EngineEvent::WatchError(e.to_string())).is_ok(),
            };
            if !sent {
                return; // the engine hung up
            }
        }
    });

    Ok(Watch {
        _watcher: watcher,
        _pump: pump,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn filter(root: &Path) -> Filter {
        let mut cfg = Config::default();
        cfg.shadow_preserve = vec!["target/".into()];
        cfg.watch_ignore = vec!["*.log".into()];
        Filter {
            repo_root: root.join("repo"),
            shadow_dir: root.join("shadow"),
            state_dir: root.join("state"),
            preserve: shadow::preserve_matcher(&cfg).unwrap(),
            ignore: cfg.watch_ignore_set().unwrap(),
        }
    }

    #[test]
    fn a_repo_file_and_its_shadow_twin_are_distinguished() {
        let root = Path::new("/tmp/x");
        let f = filter(root);
        assert_eq!(
            f.classify(&root.join("repo/src/a.rs")),
            Some((Origin::Real, PathBuf::from("src/a.rs")))
        );
        assert_eq!(
            f.classify(&root.join("shadow/src/a.rs")),
            Some((Origin::Shadow, PathBuf::from("src/a.rs")))
        );
    }

    #[test]
    fn a_dot_git_component_at_any_depth_is_ignored() {
        let root = Path::new("/tmp/x");
        let f = filter(root);
        // Git writes constantly; every one of these would wake the engine.
        assert_eq!(f.classify(&root.join("repo/.git/index")), None);
        assert_eq!(f.classify(&root.join("shadow/.git/HEAD")), None);
        // Not only at the top: submodules and nested worktrees.
        assert_eq!(f.classify(&root.join("repo/vendor/dep/.git/index")), None);
        // But a file merely *named* like it is fine.
        assert!(f.classify(&root.join("repo/.gitignore")).is_some());
    }

    #[test]
    fn preserved_build_output_is_ignored() {
        // Otherwise a `cargo build` in the user's own repo floods the engine.
        let root = Path::new("/tmp/x");
        let f = filter(root);
        assert_eq!(f.classify(&root.join("repo/target/debug/rote")), None);
        assert_eq!(f.classify(&root.join("repo/target")), None);
        assert!(f.classify(&root.join("repo/src/target.rs")).is_some());
    }

    #[test]
    fn the_configured_ignore_globs_apply() {
        let root = Path::new("/tmp/x");
        let f = filter(root);
        assert_eq!(f.classify(&root.join("repo/debug.log")), None);
        assert!(f.classify(&root.join("repo/debug.txt")).is_some());
    }

    #[test]
    fn rotes_own_writes_are_ignored() {
        let root = Path::new("/tmp/x");
        let f = filter(root);
        // `write_atomic` writes this beside its target.
        assert_eq!(f.classify(&root.join("repo/.session.json.tmp")), None);
        assert_eq!(f.classify(&root.join("state/session.json")), None);
    }

    #[test]
    fn a_path_outside_both_trees_is_ignored() {
        let root = Path::new("/tmp/x");
        let f = filter(root);
        assert_eq!(f.classify(Path::new("/etc/passwd")), None);
        assert_eq!(f.classify(&root.join("elsewhere/a.rs")), None);
        // The tree root itself is not a file anyone typed in.
        assert_eq!(f.classify(&root.join("repo")), None);
    }
}
