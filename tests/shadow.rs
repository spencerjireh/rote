//! Acceptance: the shadow syncs to a byte-identical twin of the real tree,
//! preserves build output, produces the baseline patch, and converges on re-run.
//!
//! Parallel-safe: each fixture resolves rote state against its own temp roots,
//! so no test touches the real `~/.local/share/rote` or another test's state.

mod common;

use common::{snapshot_tree, Fixture};
use rote::config::Config;
use rote::shadow;

/// The fixture every test here builds on: committed files, a staged change, an
/// unstaged change, an untracked file, a gitignored `.env` on the copy
/// allowlist, and a `target/` on the preserve list.
fn build_fixture() -> (Fixture, Config) {
    let fx = Fixture::new();
    fx.write("src/main.rs", "fn main() {\n    println!(\"hi\");\n}\n");
    fx.write("README.md", "# fixture\n");
    fx.write(".gitignore", ".env\ntarget/\n");
    fx.commit_all("initial");

    // Staged change.
    fx.write("src/main.rs", "fn main() {\n    println!(\"staged\");\n}\n");
    fx.git(&["add", "src/main.rs"]);

    // Unstaged change on top of the staged one.
    fx.write("README.md", "# fixture\n\nunstaged edit\n");

    // Untracked, not ignored.
    fx.write("notes.txt", "untracked\n");

    // Gitignored but needed by the agent's builds.
    fx.write(".env", "SECRET=1\n");

    // Ignored build output that must survive the shadow's clean.
    fx.write("target/debug/artifact", "prebuilt\n");

    let mut cfg = Config::default();
    cfg.shadow_copy = vec![".env".into()];
    cfg.shadow_preserve = vec!["target/".into()];
    (fx, cfg)
}

#[test]
fn sync_produces_a_byte_identical_shadow() {
    let (fx, cfg) = build_fixture();
    let project = fx.project();

    let baseline = shadow::sync(&project, &cfg).unwrap();

    // Every source file, the untracked file, and the allowlisted .env match.
    // target/ is excluded from the comparison: it is preserved in the shadow by
    // policy, not synced, so the trees are expected to differ there.
    let real = snapshot_tree(&project.repo_root, &["target", ".rote.toml"]);
    let shad = snapshot_tree(&project.shadow_dir, &["target"]);
    assert_eq!(real, shad, "shadow tree must equal real tree byte for byte");

    // The staged change made it across.
    let shadow_main = std::fs::read_to_string(project.shadow_dir.join("src/main.rs")).unwrap();
    assert!(
        shadow_main.contains("staged"),
        "staged change must carry over"
    );

    // The unstaged change made it across.
    let shadow_readme = std::fs::read_to_string(project.shadow_dir.join("README.md")).unwrap();
    assert!(shadow_readme.contains("unstaged edit"));

    // The gitignored allowlist file is present despite the clean.
    assert_eq!(
        std::fs::read_to_string(project.shadow_dir.join(".env")).unwrap(),
        "SECRET=1\n"
    );

    // Baseline reports the real HEAD and a non-empty digest (there were changes).
    let real_head = fx.git(&["rev-parse", "HEAD"]).trim().to_string();
    assert_eq!(baseline.head, real_head);
    assert_eq!(baseline.uncommitted_digest.len(), 64);
}

#[test]
fn preserved_build_output_survives_sync() {
    let (fx, cfg) = build_fixture();
    let project = fx.project();

    // Seed the shadow, then plant build output in it as the agent would.
    shadow::sync(&project, &cfg).unwrap();
    let artifact = project.shadow_dir.join("target/debug/artifact");
    std::fs::create_dir_all(artifact.parent().unwrap()).unwrap();
    std::fs::write(&artifact, "agent build\n").unwrap();

    // A second sync must not wipe it.
    shadow::sync(&project, &cfg).unwrap();
    assert_eq!(
        std::fs::read_to_string(&artifact).unwrap(),
        "agent build\n",
        "preserve list must survive `git clean -fdx`"
    );
}

#[test]
fn non_preserved_ignored_files_are_cleaned() {
    let (fx, cfg) = build_fixture();
    let project = fx.project();
    shadow::sync(&project, &cfg).unwrap();

    // An ignored file that is neither preserved nor on the copy allowlist.
    fx.write(".gitignore", ".env\ntarget/\nscratch.tmp\n");
    fx.commit_all("ignore scratch");
    let junk = project.shadow_dir.join("scratch.tmp");
    std::fs::write(&junk, "agent scratch\n").unwrap();

    shadow::sync(&project, &cfg).unwrap();
    assert!(!junk.exists(), "unpreserved ignored files must be cleaned");
}

#[test]
fn baseline_patch_matches_real_diff_byte_for_byte() {
    let (fx, cfg) = build_fixture();
    let project = fx.project();
    shadow::sync(&project, &cfg).unwrap();

    // Step 7 of sync must produce exactly what §8 will later reconstruct from.
    let written = std::fs::read(project.baseline_patch()).unwrap();
    let expected = fx.git(&["diff", "HEAD", "--binary", "--no-color", "--no-ext-diff"]);
    assert_eq!(
        String::from_utf8_lossy(&written),
        expected,
        "baseline.patch must equal `git diff HEAD --binary`"
    );
    assert!(!written.is_empty(), "fixture has uncommitted changes");
}

#[test]
fn baseline_patch_is_empty_for_a_clean_tree() {
    let fx = Fixture::new();
    fx.write("a.txt", "committed\n");
    fx.commit_all("only commit");
    let project = fx.project();

    let baseline = shadow::sync(&project, &Config::default()).unwrap();
    assert_eq!(std::fs::read(project.baseline_patch()).unwrap(), b"");
    assert_eq!(baseline.uncommitted_digest, "");
}

#[test]
fn sync_converges_after_the_real_tree_moves() {
    let (fx, cfg) = build_fixture();
    let project = fx.project();
    shadow::sync(&project, &cfg).unwrap();

    // Mutate the real tree in every way that matters: new commit, new
    // uncommitted edit, a new untracked file, and a deleted file.
    fx.write("src/lib.rs", "pub fn added() {}\n");
    fx.commit_all("second commit");
    fx.write("README.md", "# fixture\n\nrewritten\n");
    fx.write("fresh.txt", "new untracked\n");
    std::fs::remove_file(fx.repo.join("notes.txt")).unwrap();

    shadow::sync(&project, &cfg).unwrap();

    let real = snapshot_tree(&project.repo_root, &["target", ".rote.toml"]);
    let shad = snapshot_tree(&project.shadow_dir, &["target"]);
    assert_eq!(real, shad, "re-sync must converge on the new reality");
    assert!(
        !project.shadow_dir.join("notes.txt").exists(),
        "a file deleted in real must not linger in the shadow"
    );
}

#[test]
fn sync_is_idempotent() {
    let (fx, cfg) = build_fixture();
    let project = fx.project();

    let first = shadow::sync(&project, &cfg).unwrap();
    let after_first = snapshot_tree(&project.shadow_dir, &["target"]);
    let second = shadow::sync(&project, &cfg).unwrap();
    let after_second = snapshot_tree(&project.shadow_dir, &["target"]);

    assert_eq!(after_first, after_second, "repeat sync must not drift");
    assert_eq!(first.head, second.head);
    assert_eq!(first.uncommitted_digest, second.uncommitted_digest);
}

#[test]
fn agent_work_in_the_shadow_is_discarded_by_the_next_sync() {
    let (fx, cfg) = build_fixture();
    let project = fx.project();
    shadow::sync(&project, &cfg).unwrap();

    // The agent edits, adds, and even commits in the shadow.
    std::fs::write(
        project.shadow_dir.join("src/main.rs"),
        "fn main() { agent(); }\n",
    )
    .unwrap();
    std::fs::write(project.shadow_dir.join("agent_new.rs"), "// agent\n").unwrap();
    common::git(&project.shadow_dir, &["add", "-A"]);
    common::git(
        &project.shadow_dir,
        &["commit", "--quiet", "-m", "agent work"],
    );

    // Sync resets history and tree regardless (DESIGN.md §9.3).
    shadow::sync(&project, &cfg).unwrap();
    let real = snapshot_tree(&project.repo_root, &["target", ".rote.toml"]);
    let shad = snapshot_tree(&project.shadow_dir, &["target"]);
    assert_eq!(
        real, shad,
        "the agent's shadow commits must not survive sync"
    );
}

#[test]
fn symlinks_are_reproduced_as_symlinks() {
    let fx = Fixture::new();
    fx.write("real.txt", "target contents\n");
    fx.commit_all("initial");
    std::os::unix::fs::symlink("real.txt", fx.repo.join("link.txt")).unwrap();
    let project = fx.project();

    shadow::sync(&project, &Config::default()).unwrap();

    let link = project.shadow_dir.join("link.txt");
    let meta = std::fs::symlink_metadata(&link).unwrap();
    assert!(
        meta.file_type().is_symlink(),
        "must stay a symlink, not be followed"
    );
    assert_eq!(
        std::fs::read_link(&link).unwrap().to_string_lossy(),
        "real.txt"
    );
}

#[test]
fn shadow_has_no_remotes() {
    let fx = Fixture::new();
    fx.write("a.txt", "x\n");
    fx.commit_all("initial");
    let project = fx.project();

    shadow::sync(&project, &Config::default()).unwrap();

    // With no remote configured, a stray `git push` fails harmlessly.
    let remotes = common::git(&project.shadow_dir, &["remote"]);
    assert!(
        remotes.trim().is_empty(),
        "shadow must have no remotes, got: {remotes}"
    );
}

#[test]
fn sync_refuses_while_the_real_repo_is_mid_merge() {
    let fx = Fixture::new();
    fx.write("f.txt", "base\n");
    fx.commit_all("base");

    // Two branches touching the same line, merged to conflict.
    fx.git(&["checkout", "--quiet", "-b", "other"]);
    fx.write("f.txt", "other\n");
    fx.commit_all("other side");
    fx.git(&["checkout", "--quiet", "main"]);
    fx.write("f.txt", "main\n");
    fx.commit_all("main side");
    let (ok, stderr) = common::git_allow_fail(&fx.repo, &["merge", "other"]);
    assert!(!ok, "the fixture merge should conflict");
    assert!(
        fx.repo.join(".git/MERGE_HEAD").exists(),
        "the merge must actually be in progress, not merely failed: {stderr}"
    );

    let project = fx.project();
    let err = shadow::sync(&project, &Config::default())
        .unwrap_err()
        .to_string();
    assert!(err.contains("middle of a merge"), "got: {err}");
}

#[test]
fn repo_without_commits_gets_a_clear_error() {
    let fx = Fixture::new();
    let project = fx.project();
    let err = shadow::sync(&project, &Config::default())
        .unwrap_err()
        .to_string();
    assert!(err.contains("no commits yet"), "got: {err}");
}
