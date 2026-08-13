//! M2 acceptance: real + shadow trees with a known set of edits produce an
//! exact hunk list — files, ops, line contents, and ordering — with stable IDs.

mod common;

use common::Fixture;
use rote::config::Config;
use rote::hunks::{compute_hunks, Op, Status};
use rote::shadow;

/// Set up a synced pair, then let `agent` mutate the shadow as Claude would.
fn with_agent_edits(
    setup: impl Fn(&Fixture),
    agent: impl Fn(&std::path::Path),
) -> (Fixture, Config, Vec<rote::hunks::Hunk>) {
    let fx = Fixture::new();
    setup(&fx);
    fx.commit_all("initial");
    let project = fx.project();
    let cfg = Config::default();
    shadow::sync(&project, &cfg).unwrap();

    agent(&project.shadow_dir);

    let hunks = compute_hunks(&project, &cfg).unwrap();
    (fx, cfg, hunks)
}

#[test]
fn identical_trees_produce_no_hunks() {
    let (_fx, _cfg, hunks) = with_agent_edits(
        |fx| fx.write("a.rs", "fn main() {}\n"),
        |_shadow| { /* the agent changed nothing */ },
    );
    assert!(hunks.is_empty(), "got {hunks:#?}");
}

#[test]
fn a_single_edit_produces_one_exact_hunk() {
    let (_fx, _cfg, hunks) = with_agent_edits(
        |fx| {
            fx.write(
                "src/models.py",
                "class Post:\n    title = CharField()\n    body = TextField()\n\n    def __str__(self):\n        return self.title\n",
            )
        },
        |shadow| {
            std::fs::write(
                shadow.join("src/models.py"),
                "class Post:\n    title = CharField()\n    body = TextField()\n    tags = Manager()\n\n    def __str__(self):\n        return self.title\n",
            )
            .unwrap()
        },
    );

    assert_eq!(hunks.len(), 1, "got {hunks:#?}");
    let h = &hunks[0];
    assert_eq!(h.file, "src/models.py");
    assert_eq!(h.op, Op::Insert);
    assert_eq!(h.old_lines, Vec::<String>::new());
    assert_eq!(h.new_lines, vec!["    tags = Manager()"]);
    assert_eq!(
        h.context_before,
        vec![
            "class Post:",
            "    title = CharField()",
            "    body = TextField()"
        ]
    );
    assert_eq!(
        h.context_after,
        vec!["", "    def __str__(self):", "        return self.title"]
    );
    assert_eq!(h.status, Status::Pending);
    assert_eq!(
        h.anchor_hint, 4,
        "line in the REAL file where typing starts"
    );
    assert!(h.divergence.is_none());
}

#[test]
fn ids_are_stable_across_recomputes() {
    let fx = Fixture::new();
    fx.write("a.rs", "fn a() {}\n");
    fx.commit_all("initial");
    let project = fx.project();
    let cfg = Config::default();
    shadow::sync(&project, &cfg).unwrap();
    std::fs::write(project.shadow_dir.join("a.rs"), "fn a() { work(); }\n").unwrap();

    let first = compute_hunks(&project, &cfg).unwrap();
    let second = compute_hunks(&project, &cfg).unwrap();
    let ids1: Vec<_> = first.iter().map(|h| h.id.clone()).collect();
    let ids2: Vec<_> = second.iter().map(|h| h.id.clone()).collect();
    assert_eq!(ids1, ids2, "identical trees must yield identical hunk IDs");
    assert!(!ids1.is_empty());
}

#[test]
fn reworking_a_region_changes_its_id() {
    let fx = Fixture::new();
    fx.write("a.rs", "fn a() {}\n");
    fx.commit_all("initial");
    let project = fx.project();
    let cfg = Config::default();
    shadow::sync(&project, &cfg).unwrap();

    std::fs::write(project.shadow_dir.join("a.rs"), "fn a() { first(); }\n").unwrap();
    let before = compute_hunks(&project, &cfg).unwrap();

    // The agent reconsiders — different content, therefore a different hunk.
    std::fs::write(project.shadow_dir.join("a.rs"), "fn a() { second(); }\n").unwrap();
    let after = compute_hunks(&project, &cfg).unwrap();

    assert_ne!(before[0].id, after[0].id, "rework must produce a new ID");
}

#[test]
fn hunks_are_ordered_by_file_then_position() {
    let (_fx, _cfg, hunks) = with_agent_edits(
        |fx| {
            fx.write("z_last.rs", "one\ntwo\n");
            fx.write(
                "a_first.rs",
                "alpha\nbeta\ngamma\ndelta\nepsilon\nzeta\neta\ntheta\n",
            );
        },
        |shadow| {
            std::fs::write(shadow.join("z_last.rs"), "one\nTWO\n").unwrap();
            // Two edits far enough apart to stay separate hunks.
            std::fs::write(
                shadow.join("a_first.rs"),
                "ALPHA\nbeta\ngamma\ndelta\nepsilon\nzeta\neta\nTHETA\n",
            )
            .unwrap();
        },
    );

    let order: Vec<(&str, usize)> = hunks
        .iter()
        .map(|h| (h.file.as_str(), h.anchor_hint))
        .collect();
    assert_eq!(
        order,
        vec![("a_first.rs", 1), ("a_first.rs", 8), ("z_last.rs", 2)],
        "file path first, then position within the file"
    );
}

#[test]
fn a_new_file_in_the_shadow_is_a_create_file_hunk() {
    let (_fx, _cfg, hunks) = with_agent_edits(
        |fx| fx.write("existing.rs", "fn a() {}\n"),
        |shadow| std::fs::write(shadow.join("added.rs"), "fn added() {\n    work();\n}\n").unwrap(),
    );

    assert_eq!(hunks.len(), 1);
    assert_eq!(hunks[0].file, "added.rs");
    assert_eq!(hunks[0].op, Op::CreateFile);
    assert_eq!(hunks[0].note.as_deref(), Some("file is new"));
    assert_eq!(hunks[0].new_lines, vec!["fn added() {", "    work();", "}"]);
}

#[test]
fn a_file_deleted_in_the_shadow_is_a_delete_file_hunk() {
    let (_fx, _cfg, hunks) = with_agent_edits(
        |fx| {
            fx.write("keep.rs", "fn keep() {}\n");
            fx.write("remove.rs", "fn gone() {}\n");
        },
        |shadow| std::fs::remove_file(shadow.join("remove.rs")).unwrap(),
    );

    assert_eq!(hunks.len(), 1);
    assert_eq!(hunks[0].file, "remove.rs");
    assert_eq!(hunks[0].op, Op::DeleteFile);
    assert_eq!(hunks[0].note.as_deref(), Some("delete this file"));
    assert_eq!(hunks[0].old_lines, vec!["fn gone() {}"]);
}

#[test]
fn binary_files_become_untypeable_hunks() {
    let (_fx, _cfg, hunks) = with_agent_edits(
        |fx| fx.write("src.rs", "fn a() {}\n"),
        |shadow| {
            std::fs::write(shadow.join("logo.png"), b"\x89PNG\r\n\x1a\n\x00\x00binary").unwrap()
        },
    );

    assert_eq!(hunks.len(), 1);
    let h = &hunks[0];
    assert_eq!(h.file, "logo.png");
    assert!(h.is_untypeable(), "no line arrays to type");
    assert_eq!(h.note.as_deref(), Some("binary — copy it across yourself"));
    assert_eq!(h.op, Op::CreateFile);
}

#[test]
fn lockfiles_become_untypeable_hunks() {
    let (_fx, _cfg, hunks) = with_agent_edits(
        |fx| {
            fx.write("Cargo.toml", "[package]\nname = \"x\"\n");
            fx.write("Cargo.lock", "# autogenerated\nversion = 3\n");
        },
        |shadow| {
            // The agent adds a dependency: both files change, but only one is typed.
            std::fs::write(
                shadow.join("Cargo.toml"),
                "[package]\nname = \"x\"\n\n[dependencies]\nserde = \"1\"\n",
            )
            .unwrap();
            std::fs::write(
                shadow.join("Cargo.lock"),
                "# autogenerated\nversion = 3\n\n[[package]]\nname = \"serde\"\n",
            )
            .unwrap();
        },
    );

    let lock = hunks
        .iter()
        .find(|h| h.file == "Cargo.lock")
        .expect("lockfile hunk");
    assert!(lock.is_untypeable());
    assert_eq!(
        lock.note.as_deref(),
        Some("generated file — run the generating command instead")
    );

    // Cargo.toml is hand-edited and stays an ordinary typed hunk.
    let toml = hunks
        .iter()
        .find(|h| h.file == "Cargo.toml")
        .expect("manifest hunk");
    assert!(!toml.is_untypeable());
    assert!(toml.new_lines.iter().any(|l| l.contains("serde")));
}

#[test]
fn a_large_agent_rewrite_splits_into_typeable_pieces() {
    let cfg = Config::default();
    let (_fx, _cfg, hunks) = with_agent_edits(
        |fx| fx.write("big.rs", "fn main() {}\n"),
        |shadow| {
            let body: String = (0..50).map(|i| format!("    step{i}();\n")).collect();
            std::fs::write(shadow.join("big.rs"), format!("fn main() {{\n{body}}}\n")).unwrap();
        },
    );

    assert!(hunks.len() > 1, "50 new lines must not arrive as one hunk");
    for h in &hunks {
        assert!(
            h.new_lines.len() <= cfg.max_hunk_lines,
            "hunk of {} lines exceeds max_hunk_lines",
            h.new_lines.len()
        );
    }
    // Nothing is lost across the split. The original was `fn main() {}` on one
    // line, so the reopened brace line is new too: 1 + 50 steps + 1 closing.
    let typed: usize = hunks.iter().map(|h| h.new_lines.len()).sum();
    assert_eq!(typed, 52);
}

#[test]
fn user_typing_removes_the_hunk_from_the_next_computation() {
    // The self-truing property recompute depends on (DESIGN.md §5 rule 7).
    let fx = Fixture::new();
    fx.write("a.rs", "fn a() {\n}\n");
    fx.commit_all("initial");
    let project = fx.project();
    let cfg = Config::default();
    shadow::sync(&project, &cfg).unwrap();
    std::fs::write(
        project.shadow_dir.join("a.rs"),
        "fn a() {\n    work();\n}\n",
    )
    .unwrap();

    let before = compute_hunks(&project, &cfg).unwrap();
    assert_eq!(before.len(), 1);

    // The user types it into the real tree, exactly.
    fx.write("a.rs", "fn a() {\n    work();\n}\n");

    let after = compute_hunks(&project, &cfg).unwrap();
    assert!(
        after.is_empty(),
        "a correctly typed hunk vanishes: {after:#?}"
    );
}

#[test]
fn missing_shadow_gives_an_actionable_error() {
    let fx = Fixture::new();
    fx.write("a.rs", "fn a() {}\n");
    fx.commit_all("initial");
    let project = fx.project();
    let cfg = Config::default();
    shadow::sync(&project, &cfg).unwrap();
    std::fs::remove_dir_all(&project.shadow_dir).unwrap();

    let err = compute_hunks(&project, &cfg).unwrap_err().to_string();
    assert!(
        err.contains("rote abort"),
        "must say what to do next: {err}"
    );
}
