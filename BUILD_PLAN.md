# rote — Build Plan

Sequenced milestones for implementation. Each milestone ends in a compiling, tested, demonstrable state. Read ARCHITECTURE.md and DESIGN.md fully before writing any code. Do not skip ahead: later milestones depend on the exact contracts (Hunk schema, manifest schema, sync algorithm) established earlier. When a design question arises that the docs do not answer, prefer the simplest behavior consistent with the four principles in ARCHITECTURE.md and leave a `// DESIGN:` comment.

Conventions for all milestones:
- Rust stable, edition 2021. `cargo clippy -- -D warnings` and `cargo fmt --check` must pass.
- Errors: `anyhow::Result` at the app layer; every shelled git failure must surface git's stderr in the error message.
- All git invocations go through `git.rs` — no raw `Command::new("git")` elsewhere.
- All manifest writes are atomic (temp file + rename) and lock-guarded.
- No writes to the real repo tree anywhere in the codebase except: `.rote.toml` in `rote init`. Add a debug assertion helper `assert_not_in_real_tree(path)` used by every file-writing utility, so this invariant is mechanically enforced. This survives the byte-compare gate intact: binaries and lockfiles are *compared*, never copied — the user still runs `cp` or `npm install` themselves.

---

## M0 — Scaffold

**Goal:** project skeleton, CLI shape, config, paths.

- `cargo new rote`; deps: `clap` (derive), `anyhow`, `serde`, `serde_json`, `toml`, `directories`, `sha2`, and a terminal color crate (`anstyle`/`owo-colors`) — M4 renders colored hunks and no later milestone adds it.
- Implement `main.rs` with all subcommands from DESIGN §1 as stubs printing "not implemented" (exit 2), with real help text.
- `paths.rs`: repo-root discovery (walk up to `.git`; error message if absent), project hash (SHA-256 of canonicalized root, first 12 hex), XDG dir resolution, lock-file acquire/release with stale-PID detection.
- `config.rs`: load + merge global and project config per DESIGN §7 — including `strict_whitespace`, `[shadow] preserve`, and `[transcribe] verbatim`, all three of which later milestones depend on. Both claude commands parse as argument vectors, never shell strings. Defaults when files absent. Unit tests for merge precedence.
- `rote init` fully working (writes commented `.rote.toml`, `--force` behavior).

**Acceptance:** `rote --help` shows all commands; `rote init` produces the documented file; tests pass.

## M1 — GitPlumbing + ShadowManager

**Goal:** shadow lifecycle and sync, byte-for-byte.

- `git.rs`: typed wrappers for exactly the invocations DESIGN needs (clone, fetch, reset, clean with `-e` exclusions, diff HEAD --binary, apply, ls-files --others, status --porcelain, diff --no-index, rev-parse). Each returns stdout bytes or a rich error.
- `shadow.rs`: create (clone `--no-hardlinks`, remove **all** remotes), sync per DESIGN §3 steps 1–8, in-progress-operation detection (merge/rebase/etc.) exposed for both `start` and `done` to call.
- Sync step 7 writes `baseline.patch`. It is not optional and not deferrable to M5: §8's session diff has no other source for the session-start state, and M5 will read a file only M1 can produce.
- Integration test (`tests/shadow.rs`): build a fixture repo in a temp dir with committed files + staged changes + unstaged changes + untracked file + gitignored `.env` on the allowlist + a `target/` directory on the preserve list; run sync; assert shadow tree ≡ real tree byte-wise (walk both trees; compare), including the allowlisted file, excluding non-allowlisted ignored files. Assert `target/` survived and `baseline.patch` matches the real tree's `git diff HEAD --binary` byte for byte.

**Acceptance:** integration test passes; re-running sync after mutating the real fixture converges again (idempotence test); preserved directories survive repeated syncs while non-preserved ignored files do not.

## M2 — Diff parsing + Hunk model

**Goal:** trees → structured, split, content-addressed hunks.

- `diffparse.rs`: unified-diff parser per DESIGN §5. Fixture-driven tests: modify, new file (`/dev/null` old side), deleted file, no-newline-at-EOF, multiple hunks, empty diff.
- `hunks.rs`: `Hunk` struct exactly matching the JSON schema in DESIGN §4 (serde round-trip test, lowercase enum values), content-addressed IDs, `op` derivation, splitting algorithm (blank-line preference, indentation fallback, hard cut, synthesized context) with tests at limits (hunk of exactly `max`, `max+1`, giant single block with no blank lines, **and a pure deletion over `max` lines** — splitting counts `max(old, new)`, so this must split).
- File enumeration + per-file diffing (DESIGN §5) wired: a function `compute_hunks(real, shadow, cfg) -> Vec<Hunk>` using `status --porcelain=v1 -z` as the single enumeration source, `--no-textconv` on every diff, the binary-file heuristic, and lockfile/verbatim-glob handling per §9.4. Untypeable hunks are tagged for the byte-compare gate here; M4 consumes that tag.
- Integration test: fixture real+shadow trees with a known set of edits → assert exact hunk list (files, ops, line contents, ordering).

**Acceptance:** all fixtures produce exact expected hunks; IDs stable across runs.

## M3 — SessionEngine

**Goal:** state machine, manifest, recompute/reconciliation.

- `session.rs`: manifest load/validate/atomic-save, state enum `idle | working | transcribing` (terminal `done`/`aborted` labels only on archived manifests), state transitions with clear errors on invalid ones, `recompute()` implementing reconciliation rules DESIGN §5 exactly (all seven numbered rules).
- `rote start` (without the claude exec — behind `--no-launch`, a **permanently retained hidden flag**: M5's acceptance test drives a full lifecycle without a real agent and needs it), `rote status`, `rote abort` fully working, including the residue patch written before abort's sync.
- Reconciliation unit tests, one per scenario: (a) user typed a hunk → vanishes from fresh set, terminal status preserved; (b) agent reworked a pending hunk → old dropped, new appended under a new ID; (c) `typed` hunk unexpectedly reappears → reset to pending with warning; (d) new agent work mid-session → appended; (e) baseline drift (real HEAD moved) → warning path exercised.
- **Stickiness tests — these guard the rule the whole queue rests on:** (f) a `skipped` hunk whose region still differs stays skipped across ten consecutive recomputes and never re-enters the queue; (g) same for a `diverged` hunk after `keep mine`; (h) a hunk skipped, then reworked by the agent, appears as a *new* pending hunk while the old skipped entry stays terminal in the history.
- Drift test: typing hunks changes the uncommitted diff on every keystroke, so assert that transcription alone never raises the drift flag — only a moved `HEAD` does.

**Acceptance:** scenario tests pass; `start --no-launch` → `status` → `abort` round-trip works on the fixture repo; manifest survives kill -9 between operations (lock staleness test).

## M4 — HunkPresenter

**Goal:** the transcription loop.

- `present.rs`: terminal rendering per DESIGN §6 (respect `--no-color`), context-based anchoring with the full fallback chain and nearest-to-hint disambiguation, editor launch (blocking, `+line file` at the anchor line itself, resolution chain `$ROTE_EDITOR` → config → `nvim` with `$EDITOR` deliberately absent, no file creation for `create_file` hunks), classification (exact / untouched / divergence), the divergence prompt loop (`k`/`r`/`s`), and the byte-compare gate for binary and verbatim hunks (no editor, no prompt).
- `rote next`, `rote back`, `rote skip` wired end to end. `back` re-prints only — no status change, no editor. `rote next --json` and hidden `rote mark` per DESIGN §1. *(Superseded by M8: `back` became `show`, `mark` was deleted, and `next` stopped launching an editor.)*
- Anchoring unit tests: file with lines inserted above the target (drift), ambiguous context (two matches, hint disambiguates), context missing entirely (falls back to hint with warning).
- Classification tests: exact match, trailing-whitespace tolerance under both `strict_whitespace` settings, divergence storage, untouched detection, and a verbatim-glob hunk that stays pending until the fixture's lockfile is made to match byte for byte. (Editor interaction tested by substituting `editor = "true"` / a test script that applies a scripted edit.)

**Acceptance:** on a fixture session, a scripted "editor" that types hunks correctly drives the queue to empty; a scripted editor that types a variant triggers the divergence path and records both versions; a session containing a skipped hunk and a kept divergence still reaches an empty queue rather than looping.

## M5 — `done` pipeline + claude launch

**Goal:** session close, checks, reviewer; wire the real agent launch.

- `review.rs`: in-progress-operation gate (before anything expensive runs), pending-hunk gate, `[checks]` command execution in the real tree (streamed output, stop on failure, `--no-checks`), baseline materialization from `baseline.head` + `baseline.patch` + session-diff computation per DESIGN §8, payload assembly (task / session diff / divergences / skipped), `claude -p` invocation with tool-restriction flags in one named constant, timeout, and fail-soft behavior on unsupported flags, confirmation, residue patch + archive, then final sync — in that order, since sync destroys what the patch records.
- `rote start` claude exec (DESIGN §1: lock released first, process replacement, cwd = shadow, task printed not passed); `rote talk` with `--attach`.
- Integration test for the payload builder (assert exact payload text from a fixture session; stub the claude binary with a script that echoes stdin to a file). A second stub that exits nonzero must leave the pipeline running to completion.

**Acceptance:** full lifecycle on the fixture repo with stubbed `claude` and scripted editor: `init → start --no-launch → (mutate shadow as the "agent") → next×N → done` ends `idle`, shadow ≡ real, manifest and residue patch both archived, reviewer stub received the documented payload. Re-running with a skipped hunk asserts the residue patch actually contains that hunk's change.

## M6 — Polish

- Error-message pass: every user-facing error names the problem and the next command to run.
- `rote status` output design (the one screen the user sees most).
- README.md: install (`cargo install --path .`), quickstart, the ghostty two-pane workflow, the `rm -rf ~/.cache/rote/<hash>` escape hatch, config reference, and a short note on where a session's untyped residue lands (`archive/<timestamp>.patch`) for the user who skips something and changes their mind.
- Edge-case sweep against DESIGN §9: add a test or a manual-verification note for each of the 11 items. Where each landed:

| §9 | Case | Covered by |
|---|---|---|
| 1 | Real repo dirty at `start` | `tests/shadow.rs::sync_produces_a_byte_identical_shadow` (staged + unstaged + untracked fixture) |
| 2 | Repo changes commits mid-session | `tests/session.rs::a_commit_mid_session_raises_drift_but_does_not_block`, and `transcription_alone_never_raises_the_drift_flag` for the inverse |
| 3 | Agent commits in the shadow | `tests/shadow.rs::agent_work_in_the_shadow_is_discarded_by_the_next_sync` |
| 4 | Agent adds dependencies | `tests/transcribe.rs::a_generated_file_is_gated_on_bytes_not_typing`, `tests/hunks.rs::lockfiles_become_untypeable_hunks` |
| 5 | File renames | `src/hunks.rs::a_moved_file_is_noted_as_a_possible_rename` (and `an_unrelated_new_file_is_not_called_a_rename`) |
| 6 | Editor exits nonzero | `tests/transcribe.rs::an_editor_that_exits_nonzero_is_treated_as_untouched` |
| 7 | Concurrent invocations | `src/paths.rs::lock_is_exclusive_while_held`, `tests/session.rs::the_lock_survives_a_killed_process` |
| 8 | Shadow deleted mid-session | `tests/hunks.rs::missing_shadow_gives_an_actionable_error` |
| 9 | Symlinks | `tests/shadow.rs::symlinks_are_reproduced_as_symlinks` |
| 10 | Empty session | `tests/done.rs::an_empty_session_closes_without_checks_or_review` |
| 11 | CRLF | `src/present.rs::a_stray_carriage_return_is_absorbed` |
- Dogfood note: the intended first real use is rote's own repository — building rote's next milestone through rote.

**Acceptance:** clippy/fmt clean, all tests green, README accurate against actual behavior.

## M7 — Install and setup UX

**Goal:** a first run that diagnoses itself, and the release-mode hole closed.

- **The guard, first — it is a bug.** `assert_not_in_real_tree` is a `debug_assert`, and `cargo install` builds in release, so the guard protecting Principle 2 is absent from exactly the binary people use. `ProjectPaths::resolve` gains a real runtime check refusing a shadow or state directory inside the repository (DESIGN §11). The realistic trigger is ordinary: a dotfiles repo at `$HOME` puts `~/.cache/rote/…` inside the real tree by definition.
- `detect.rs`: one shared detection surface for `doctor`, `setup`, and `init` — PATH resolution, the claude/reviewer-flag probe, editor resolution, project kind, and check commands *verified to run on this machine* before being written.
- `rote doctor [--deep]` per DESIGN §1: read-only, works outside a repo with repository-scoped lines degrading, exits non-zero on failure.
- `rote setup [--force]`: the only writer of the global config, which nothing previously created.
- `rote init`: prefills `[checks]` and `[transcribe] verbatim` from detection and says what it chose. Empty checks meant `rote done` verified nothing by default.
- `rote start`: preflights `claude_cmd` only, and only when launching.
- Dispatch restructure so repo discovery is per-command rather than a precondition.
- `Formula/rote.rb` (build from source, tagged) and a `justfile` (`install`, `gate`, `formula-sha`).

**Acceptance:** `doctor` passes on a correctly configured machine and fails with an actionable line on a broken one; it runs outside a repository without erroring. `setup` writes a config the real loader round-trips, and refuses without `--force`. `init` in a Rust fixture writes checks whose commands exist on PATH; an unrecognized project gets empty checks and says so. `start` refuses with `run rote doctor` when claude is missing, but `--no-launch` still works without it. A repo whose XDG dirs sit inside itself is refused through the CLI.

---

---

## Out of scope — do not build

Multi-session, non-git backends, paste *prevention* (M11 records and reports; it never withholds), Windows, any Claude Code hooks/MCP/SDK integration, `rote gc`, telemetry of any kind. From M7: a TUI wizard, publishing the tap, prebuilt binaries, GitHub Releases, or release CI.

---

## M8 — the watch engine

Retire the editor launch. rote watches the real tree, reclassifies on save, and
advances the queue itself; a full-screen `rote watch` pane is the reading
surface. Wire types are defined now so a daemon and a browser front end are a
transport swap rather than a rewrite.

Scope:
- `present::is_in_progress` / `is_subrun`, and `region_of` for per-region
  comparison.
- `reconcile` gains `strict_whitespace` plus two suppression rules: an
  unanswered divergence survives recompute, and a whitespace-tolerated `typed`
  hunk stops reappearing as a phantom.
- `src/state.rs` — the wire contract (DESIGN §12), field names pinned by tests.
- `src/engine.rs` — injected clock, pure `Watchdog`, baselines, fast path.
- `src/watcher.rs` — `notify`, both trees, directories not files, event kind
  discarded.
- `src/pane.rs` — alternate screen, termios raw mode via `libc`, pure
  `render_frame`.
- Verbs: `next` becomes a printer, `back` becomes `show`, new `resolve`.
- `[watch]` config; doctor's editor row becomes informational.

Acceptance: with a session open and `rote watch` running, writing the proposal
into the real file marks the hunk typed and advances the queue with no command
issued. Half-typing raises no question however long the pause; a settled
difference raises one after the grace window and the queue moves on past it.

Edge cases this milestone adds to §9: half-typed saves (6), and the per-region
untouched rule — an edit anywhere in a file must not accuse the other hunks in
it.

---

## M9 — the daemon and the protocol

Move the engine into a long-lived daemon serving localhost HTTP + SSE, and make
`rote watch` a client of it. Merged with what was going to be a separate
"pane becomes a client" milestone, because shipping a daemon while the pane
still ran its own engine would have been actively broken — see the
single-engine invariant in DESIGN §13.

Scope:
- `src/http.rs` — transport as pure functions over bytes; `src/daemon.rs` —
  identity, server, spawn, reap, routing.
- `watch.lock` becomes the engine token, and the routing decision for every
  mutation.
- `POST /command` with the generation check inside the `with_session` closure.
- SSE via `Request::into_writer` with a flush per frame.
- `rote start` spawns detached and health-waits; `done`/`abort` reap before the
  shadow sync.
- `rote skip` / `rote resolve` route to the daemon; `rote watch [--local]`.

Acceptance: `rote start` leaves a daemon watching; two panes attach to it and
see the same hunk; typing advances the queue in both; `rote skip` from another
shell moves them; `done` reaps it and the panes say the session closed.

Edge cases this milestone adds to §9: two engines (11) and a question raised
then re-identified (12).


---

## M10 — the curator

The queue was sorted by file path and line number, which is the order a
filesystem happens to be in and not the order a change makes sense in: it hands
you a caller before the thing it calls, and a test for something you have not
written yet. `curator_note` and `curator_rank` have existed since the v2
manifest, fully plumbed through the sort, the wire and the pane, with nothing
writing them. This milestone supplies the producer.

It opens with a prerequisite rather than a feature. Every headless `claude -p`
wrote its whole payload to stdin before reading a byte of output, which
deadlocks past a pipe buffer — 16 KiB on macOS — and the reviewer's timeout was
armed only after that write returned, so the hang had no ceiling and no
diagnostic. A curator payload of every pending hunk hits it routinely.

Scope:
- `src/model.rs` — the one headless call, with a writer thread, two reader
  threads, and the deadline around the whole thing. `review.rs` and
  `detect::deep_probe_claude` move onto it; the latter had no timeout at all.
- `src/curator.rs` — prompt, payload, tolerant JSON parser, the cache, and the
  pure policy (`should_curate`, `assign_ranks`).
- `curator.json`, keyed by hunk `key` and stamped with `session_id`, re-applied
  after every `reconcile` — the source of truth, because ids move as the user
  types and `reconcile` copies nothing onto a fresh hunk.
- The engine gains a curation channel, a settle timer of its own, and a
  writeback whose every failure is a notice rather than an error.
- `[curator]` on both config files, and `ROTE_CURATOR=off`.

Acceptance: with a session open and the agent's work landed, the queue reorders
itself into teaching order within seconds and each hunk carries one line saying
why it comes where it does; the hunk being typed does not move; `rote talk`
adding work re-curates and only the tail changes; a curator that cannot run
leaves the file order, warns once, and is not asked again.

Edge cases this milestone adds to §9: a curation landing mid-transcription (13),
a daemon restarted mid-curation (14), and a curator child outliving the session
(15).

---

## M11 — how the content arrived

The tool's whole premise is that you type the code rather than paste it, and it
could not say a word about whether you did. `Hunk.input` has existed since the
v2 manifest with one write — `Input::default()` — and zero reads anywhere.

The old doc comment said the value was "reported by the front end, not
inferred", arguing that a careful typist who saves once is byte-identical to a
paste. That is correct *about the final bytes* and wrong as a general claim: the
engine does not only see the final bytes, it sees the save history. A hunk
observed *in progress* — the region a line-wise prefix of the proposal — had a
human in the loop, and one paste of a whole hunk never produces that.

Scope:
- `Input::fill` (the engine's write, only ever into `unknown`) and `Input::set`
  (a front end's, always lands), both reporting whether anything moved.
  `skip_serializing_if` so the field is finally omitted when unset, as §4 has
  claimed all along.
- The engine records in-progress observations by hunk `key`, in memory, pruned
  to the pending queue on every recompute. The verdict is written inside
  `commit_typed`'s compare-and-swap; the byte-compare gate passes `false`,
  because a file that matched byte for byte was verified, not typed.
- `Command::Report` and `state::Reported`, plus `Decision::Nothing` so a repeat
  writes nothing and moves no generation.
- `reconcile` rule 3 clears `input`: the field describes how the content
  *currently in the tree* arrived, and that rule fires because it is not there.
- `rote report <HUNK_ID> typed|pasted`, and a line at `done`.

Acceptance: typing a hunk a few characters at a time and saving as you go leaves
`typed` on it; pasting the whole hunk in one save leaves `unknown` and never
`pasted`; `rote report` overrides either and moves no status; `rote done` names
how the typed hunks arrived, and says nothing at all in a session where it
observed nothing.

Edge cases this milestone adds to §9: a hunk half-typed then pasted (16), a
daemon restarted mid-hunk (17), and a hunk re-identified mid-typing (18).
