# rote — Detailed Design

Read ARCHITECTURE.md first. This document specifies component behavior, data schemas, CLI contracts, and edge cases precisely enough to implement from. Where this doc and ARCHITECTURE.md conflict, this doc wins.

---

## 1. CLI contract

Binary name: `rote`. All commands run from anywhere inside the real repo (repo root discovered by walking up to `.git`). Running outside a git repo is an error with a clear message.

Two commands — `doctor` and `setup` — deliberately do **not** require a repository. They concern machine-level state (the claude binary, the editor, the global config), and `doctor` is the command you reach for when something is wrong, which may well be before you have a repo. Repo discovery is therefore per-command, not a precondition of dispatch.

### `rote doctor [--deep]`
Read-only diagnosis of whether this machine can run rote. One aligned line per check — git, claude, the reviewer's tool-restriction flag, the editor, the global config, the repository, the shadow location, the project config — each either passing or naming the command that fixes it. Repository-scoped lines report `not in a git repository` rather than failing when run outside one. **Exits non-zero if any check fails**, so it can gate a script.

The reviewer check parses `claude --help` for the flag in `REVIEWER_TOOL_FLAGS` (§8): free, offline, and enough to catch a rename. `--deep` additionally runs one real `claude -p` invocation, which is the only check that catches a flag that exists but behaves differently than assumed. It is opt-in because it costs a token spend.

`doctor` never writes anything.

### `rote setup [--force]`
The only writer of `~/.config/rote/config.toml`, which nothing else creates. Runs the same detection as `doctor`, prompts only where detection is ambiguous, and refuses an existing file without `--force`. On a non-tty it takes the detected values without prompting, so it stays scriptable.

### `rote init`
Creates `.rote.toml` in the repo root (see §7). Idempotent; refuses to overwrite an existing file without `--force`.

**Checks are prefilled from project detection (§11) and left active, not commented out.** A `[checks]` block that is empty by default means `rote done` silently verifies nothing — the wrong default for the one safety net in the close-out pipeline. `init` prints one line naming what it detected and what it chose; an unrecognized project gets `commands = []` with a comment explaining what to add.

### `rote start [TASK...]`
- Errors if a session is already active (message: show `rote status`, suggest `done` or `abort`).
- Errors if the real repo is in a merge/rebase/cherry-pick/bisect state (detect via `.git/MERGE_HEAD`, `.git/rebase-merge`, `.git/rebase-apply`, `.git/CHERRY_PICK_HEAD`, `.git/BISECT_LOG`).
- **Preflights `claude_cmd` only**, and only when it is actually going to launch: if the binary does not resolve on PATH, fail naming it and suggesting `rote doctor`. Deliberately narrow — the editor is not needed until `next` and the reviewer not until `done`, so each command validates its own prerequisites at the point it needs them rather than every command running a full diagnostic.
- Creates the shadow clone if absent; syncs real → shadow (§3).
- Writes the manifest: state `working`, `task` = joined TASK args (may be empty), `baseline` = snapshot info (§4).
- **Releases the manifest lock before exec.** The lock records a PID and treats a live PID as held (§9.7); after `exec` that PID belongs to the claude process, which lives for the whole session. Failing to release here wedges every subsequent `rote` invocation until the user quits claude.
- **Execs `claude` with cwd = shadow directory, replacing the rote process in the current pane** (use `exec`-style process replacement: `std::os::unix::process::CommandExt::exec`). Pass no extra args, no prompt injection. If TASK was given, print it once before exec so the user can paste/say it — do NOT pass it to claude (that would require choosing between `-p` and interactive; the user drives the conversation).
- The claude command is configurable (`config.toml: claude_cmd`, default `["claude"]`; an argument vector, not a shell string — see §7).

### `rote status`
Prints: state, task, session age, shadow path, and if `transcribing`/`working`: files changed, hunk counts by status (pending / typed / diverged / skipped). Exit code 0 always (informational).

### `rote next`
- Errors if state is `idle`.
- If state is `working`, transitions to `transcribing` on first call.
- Runs **recompute** (§5), takes the first `pending` hunk, renders it (§6), launches the editor, classifies the outcome, updates the manifest, prints one-line progress (`[4/11] typed — src/models.py`).
- Progress is position within the queue *as it stands after this recompute*, not a fixed target. The denominator moves when the agent reworks something or adds new work; that is expected, and the display should not pretend otherwise.
- If the queue is empty: prints "nothing to transcribe" and, if all hunks are terminal, suggests `rote done`.
- Hidden flag `--json`: instead of rendering + launching the editor, emit the next pending hunk as one JSON line (schema = the Hunk object in §4) and exit. This is the front-end seam. `--json` performs no classification.

  There is deliberately **no verb that sets a hunk's status to `typed`**. The hidden `rote mark` did exactly that and has been removed. `Typed` is producible only by the classifier, whose input is the hunk's own `new_lines` — which came from the shadow. Three layers hold the line: no verb exists, the engine is the only producer, and reconcile rule 3 returns a `typed` hunk to the queue the moment the trees disagree again. A front end cannot assert its way to a finished session.

  Status changes a front end *may* make are the ones that record a human decision rather than a fact about the trees: `rote skip [HUNK_ID]`, and (from Stage 1) resolving a divergence. Both are addressed by hunk id, never by queue position — the head of the queue can move between the moment a front end renders a hunk and the moment the user acts on it.

### `rote back`
Re-prints the most recently presented hunk (`last_presented`) for reference. **It does not change the hunk's status and does not open the editor.** This is a display command, not an undo: rote never writes the real tree, so it cannot un-type keystrokes, and a correctly typed hunk has already vanished from the fresh diff (§5 rule 7) — there is nothing to restore. To re-edit a region, open it yourself; the next recompute will notice. One level of history is sufficient for v0.

### `rote skip`
Marks the current head-of-queue hunk `skipped` without opening the editor. Skip is **durable**: a skipped hunk stays skipped across recomputes (§5 rule 4), even though its region still differs between the trees. Skipped hunks are reported at `done` time and require explicit confirmation to close the session.

### `rote talk`
Prints the shadow path and, with `--attach`, execs `claude --continue` (configurable) in the shadow so the user can resume arguing with the session agent.

### `rote done`
Pipeline, in order; abort at any failed/declined step leaves the session in `transcribing`:
1. Refuse if the real repo is in a merge/rebase/cherry-pick/bisect state (same detection as `start`). Checked **first**, before any work: step 5's sync cannot run mid-operation, and discovering that after the checks and the reviewer have already run wastes the expensive part of the pipeline.
2. Recompute. If pending hunks remain, list them and require `--force` or interactive confirmation to proceed (proceeding marks them `skipped`).
3. Run each command in `.rote.toml [checks] commands` sequentially **in the real tree**, streaming output. Any non-zero exit stops the pipeline (override: `--no-checks`).
4. **Reviewer Claude** (skippable with `--no-review`, or if `[review] enabled = false`): build the review payload (§8), run `claude -p <REVIEW_PROMPT>` with the payload on stdin, print the response verbatim under a "Reviewer findings" header. This invocation gets a clean temp cwd and tool restriction (§8).
5. Interactive confirmation: "Close session? [y/N]". If any hunks are `skipped` or `diverged`, say so in the prompt — step 6 discards the agent's version of that work.
6. **Archive the residue**, then sync. In order: write the current shadow-vs-real diff (everything the agent produced that never made it into the real tree) to `archive/<iso-timestamp>.patch`; archive the manifest to `archive/<iso-timestamp>.json`; sync real → shadow (user's typed version becomes the new baseline); state → `idle`. The patch is written **before** the sync, which is what destroys the shadow's copy. Same basename as the manifest so the pair is obvious.

### `rote abort`
Confirms, then: write the residue patch to `archive/<iso-timestamp>.patch` (as in `done` step 6 — abort discards strictly more agent work, so the patch matters more here), archive the manifest with terminal label `aborted`, sync real → shadow (discarding the agent's shadow work), state → `idle`. The user's real tree is untouched by definition.

### Global flags
`--project <path>` (override repo discovery), `-q/--quiet`, `--no-color`.

---

## 2. State machine

```
idle ──start──▶ working ──first `next`──▶ transcribing ──done──▶ idle
                  │                            │
                  └────────abort───────────────┴──abort──▶ idle
```

- **The live state enum is exactly `idle | working | transcribing`.** There is no live `Done` or `Aborted` state: both `done` and `abort` return the session to `idle`. `done` / `aborted` exist only as the `terminal` label written into an archived manifest, recording how that session ended. (ARCHITECTURE.md's sketch of the machine lists them as states; this document wins.)
- **Serialized enum values are lowercase throughout** — `state` and hunk `status` alike. Both cross the `--json` seam (§1, §4), so the casing is a public contract; pin it now rather than after the nvim plugin exists.
- `working` and `transcribing` differ only in that `transcribing` implies a hunk queue exists. The user may return to the Claude session (`rote talk --attach`) at any time in either state; recompute at the next `next` absorbs whatever changed.
- The manifest on disk is the single source of truth. Every command loads it, validates state, acts, and atomically rewrites it (write temp file + rename).
- A `manifest.version` integer field guards against future schema changes.

---

## 3. ShadowManager

### Creation
```
git clone --no-hardlinks <real_repo_root> <shadow_dir>
```
(`--no-hardlinks` trades disk for safety: an agent running `git gc` or rewriting objects in the shadow must not be able to affect the real repo's object store even in edge cases. Accept the cost.)

Then **remove every remote** from the shadow (`git remote remove origin`, plus any others the clone inherited). Sync fetches by explicit path instead (`git fetch <real_repo_root> …`), so no remote is needed. With none configured, an agent absent-mindedly running `git push` fails harmlessly instead of pushing the shadow's work somewhere real.

### Sync (real → shadow)
All steps run with explicit `-C <dir>`:
1. Real: resolve `HEAD` commit `H` and current branch name.
2. Shadow: `git fetch <real_repo_root> +HEAD:refs/rote/baseline`, then `git reset --hard refs/rote/baseline` with the working tree detached at that ref.
3. Shadow: `git clean -fdx -e <pattern>…`, passing one `-e` per entry in `[shadow] preserve` (§7). Everything else ignored or untracked goes. **Preserve exists to keep the agent's build usable:** the shadow is where the agent installs dependencies and runs tests, and a bare `-fdx` deletes `target/`, `node_modules/`, and every other artifact at the top of every session, buying byte-for-byte purity of *source* with a cold rebuild each time. Preserved paths are build output only — never source, never anything the diff reads. The allowlist of §5 is **not** preserved here; it is cleaned and re-copied fresh, which is simpler than excepting it and guarantees it matches the real tree.
4. Real: `git diff HEAD --binary` → apply in shadow via `git apply --whitespace=nowarn` (empty diff → skip). This carries staged + unstaged changes. **Retain these diff bytes**; steps 6 and 7 both need them.
5. Untracked files: in real, `git ls-files --others --exclude-standard -z`; copy each to shadow (create parent dirs).
6. Allowlist files (`[shadow] copy` in `.rote.toml`, e.g. `[".env", ".envrc"]`): copy from real to shadow if they exist. These are typically gitignored.
7. **Write the step-4 diff bytes to `~/.local/share/rote/<hash>/baseline.patch`** (truncate to empty if there was no diff). Together with `baseline.head` this is the complete description of the real tree at session start, and it is what §8 reconstructs the session diff from. Writing it is part of sync, not of `done` — `done` only reads it. Note the ordering consequence: `done` computes the session diff in step 4 of its pipeline and re-syncs in step 6, so the read always precedes the overwrite.
8. Record in the manifest: `baseline.head = H`, `baseline.uncommitted_digest` = SHA-256 of the step-4 diff bytes (empty string if none), `baseline.synced_at`. The digest is retained for archive and debugging only — it is deliberately **not** a drift signal (§9.2).

### Failure handling
Any git failure during sync aborts the command with the git stderr surfaced. A half-synced shadow is fine — it is disposable; the next `start` re-syncs or the user can delete `~/.cache/rote/<hash>` entirely (document this as the universal "reset rote" escape hatch).

### Teardown
No automatic teardown. `rote abort`/`done` re-sync rather than delete. (A `rote gc` command is out of scope for v0.)

---

## 4. Manifest schema (`session.json`)

```json
{
  "version": 1,
  "state": "transcribing",
  "task": "add tagging to posts",
  "created_at": "2026-08-13T10:22:00Z",
  "project_root": "/home/user/proj",
  "shadow_dir": "/home/user/.cache/rote/ab12…/shadow",
  "baseline": {
    "head": "c0ffee…",
    "uncommitted_digest": "9a3f…",
    "synced_at": "2026-08-13T10:22:01Z"
  },
  "hunks": [ Hunk, … ],
  "last_presented": "h-0007"
}
```

There is no `session_start_snapshot` field. The session-start baseline is `baseline.head` plus the patch at the fixed path `~/.local/share/rote/<hash>/baseline.patch`, written by sync step 7 — one representation, derivable from the project hash, so nothing can disagree with itself. Archived manifests carry one extra field, `"terminal": "done" | "aborted"` (§2).

**Hunk object** — this schema is a public contract (the `--json` seam):

```json
{
  "id": "h-0007",
  "file": "src/models.py",
  "op": "replace",                  // insert | replace | delete | create_file | delete_file
  "context_before": ["class Post(models.Model):", "    title = …"],
  "old_lines": ["    body = models.TextField()"],
  "new_lines": ["    body = models.TextField()", "    tags = TaggableManager()"],
  "context_after": ["", "    def __str__(self):"],
  "anchor_hint": 42,                // line number in the REAL file at last recompute; advisory only
  "status": "pending",              // pending | typed | diverged | skipped
  "divergence": null,               // when diverged: {"proposed": [...], "actual": [...]}
  "note": null                      // optional one-liner rote attaches (e.g. "file is new")
}
```

Hunk IDs are content-addressed: `h-` + first 8 hex of SHA-256 over (`file`, `old_lines`, `new_lines`, `context_before`, `context_after`). This makes reconciliation across recomputes natural: an unchanged proposal keeps its ID; a reworked one appears as a new hunk.

`op` is derived, not free-form: empty `old_lines` → `insert`; empty `new_lines` → `delete`; both populated → `replace`. The whole-file ops `create_file` / `delete_file` come from the `/dev/null` side of the diff (§5) and win over the line-level derivation. For `create_file`, `anchor_hint` is `1` — the real file does not exist yet, so there is nothing to anchor against.

---

## 5. SessionEngine: diff and recompute

### File enumeration
Changed-file candidates = union of:
- shadow: `git -C shadow status --porcelain=v1 -z`, parsed — every path git reports as staged, unstaged, or untracked-not-ignored relative to the synced baseline, and
- files that exist in one tree but not the other among (tracked ∪ untracked-not-ignored).

Use `status --porcelain=v1 -z` and nothing else. (An earlier draft offered `ls-files -mo --exclude-standard` as an equivalent; it is not one — it reports a different set and no status codes. One command, one code path.)

For each candidate relative path `p`, compare `real/p` vs `shadow/p` byte-wise; skip identical.

**Binary files** (heuristic: NUL byte in first 8 KiB): record as a hunk with `op` `create_file`/`replace`/`delete_file`, `note: "binary — copy it across yourself"`, and empty line arrays. Classification is the byte-compare gate below, not a prompt.

### The byte-compare gate (untypeable hunks)
Binary files and `[transcribe] verbatim` matches (§9.4) share a problem: there is no meaningful "type this in." A PNG cannot be typed and a lockfile should not be — it should be regenerated by the command that owns it. Both are therefore classified by **comparing bytes, not keystrokes**:

- The presenter shows the path, the `note`, and nothing else. No line rendering, no editor launch.
- On each `next`, rote compares `real/p` to `shadow/p`. Identical → `typed`. Different → stays `pending`, printing the note again as a reminder of the command to run.
- `rote skip` remains available and is durable, exactly as for ordinary hunks.

This keeps the honor system out of the loop — a forgotten `npm install` leaves the hunk pending and blocks a clean `done` instead of silently closing the session against a stale lockfile — and it costs nothing to implement, because byte comparison is already how enumeration decides a file is a candidate at all. rote still writes nothing: the user runs `cargo add`, `npm install`, or `cp` themselves.

### Per-file diff
```
git diff --no-index --no-textconv --unified=3 --histogram -- real/p shadow/p
```
`--no-textconv` is required, not optional: any configured textconv filter would diff a *rendered* view of the file rather than its bytes, and the whole classification model rests on byte truth (see §9.11 for the CRLF case).
Exit code 1 with output = normal. Parse with the hand-rolled unified-diff parser (spec: headers `--- ` / `+++ `, hunk headers `@@ -a,b +c,d @@`, line prefixes ` `, `-`, `+`, `\ No newline at end of file` handling). New file: old side is `/dev/null` → `op: create_file`. Deleted in shadow → `op: delete_file`.

### Splitting
Raw git hunks larger than `max_hunk_lines` (default **20**, counting `max(old_lines.len(), new_lines.len())`) are split into sub-hunks. Counting `new_lines` alone would leave every large deletion unsplittable — a 500-line removal has no new lines at all, and arrives as one hunk regardless of the limit:
- Prefer split points at blank lines in `new_lines`; else at lines with lower indentation than their successor (crude block boundary); else hard-cut at the limit.
- Each sub-hunk gets ≥ 2 lines of context on each side, synthesized from the neighboring sub-hunk's lines where the original context is out of reach.
- Sub-hunks of one parent are ordered and must be presented in order (the queue is globally ordered: file path, then position).

### Reconciliation (the heart of recompute)
**Terminal statuses are sticky, keyed by hunk ID.** Once a hunk is `typed`, `diverged`, or `skipped`, it stays that way and stays out of the queue — with exactly one exception, rule 3. This is the load-bearing rule; get it wrong and `skip` and `diverged` both stop working (see the note below).

1. Compute the fresh hunk list `F` from the current trees.
2. For each manifest hunk `M` with terminal status, if no `F` hunk shares its ID: the change was absorbed into the real tree (`typed`) or reworked away by the agent. Keep `M` in the manifest for history; it is already out of the queue.
3. For each manifest hunk `M` with status **`typed`**, if an `F` hunk shares its ID: the region the user typed differs from the shadow again — their work was overwritten, or never landed as recorded. Reset `M` to `pending` and warn. **This applies to `typed` only.**
4. For each manifest hunk `M` with status **`diverged`** or **`skipped`**, an `F` hunk sharing its ID is *expected* and is ignored — the hunk stays terminal and stays out of the queue. This is the normal steady state for both: the user kept their own version or declined the change outright, so that region differs from the shadow permanently and reappears in `F` on every single recompute. Resetting on reappearance (as an earlier draft of this rule did, by applying rule 3 to all terminal statuses) would resurrect every skipped and diverged hunk on every `next`, making `skip` mean "suppress until the next recompute" and making `keep mine` an infinite loop.
5. `pending` manifest hunks not present in `F` are dropped (agent reworked them).
6. `F` hunks with no manifest counterpart are appended as new `pending` hunks — **except** a fresh hunk that restates a divergence the user already resolved, which is suppressed. Note that a reworked region arrives here rather than at rule 4: rework changes the content, the content changes the ID, and a new ID is simply a new hunk. A stale `skipped` entry for the old version stays in the history and correctly does not suppress the new proposal.

   **Why divergence needs more than an ID match.** Rule 4's stickiness is keyed by hunk ID, and for `skipped` that is enough: declining a hunk leaves the real tree untouched, so the region keeps producing a byte-identical diff and therefore the same ID, recompute after recompute. `diverged` is different, and the difference is easy to miss. Keeping your own version *writes to the real tree*. The next diff of that region is no longer (nothing → proposal) but (what you typed → proposal) — different content, different hash, different ID. Rule 4 never sees it, rule 6 appends it as new work, and the proposal you just rejected returns on every single `next`, forever.

   So a resolved divergence is matched by **content**, not ID: suppress a fresh hunk when some `diverged` manifest hunk for the same file has `divergence.actual == fresh.old_lines` and `divergence.proposed == fresh.new_lines`. In words — a hunk that wants to replace exactly what you typed with exactly what you already turned down is the same decision, already made. The match is deliberately narrow: if the agent proposes something *different* for that region, the contents differ, nothing is suppressed, and the new idea is offered as it should be.
7. Natural consequence: when the user types a hunk correctly, that region becomes identical across trees and the hunk vanishes from `F` — recompute is self-truing for `typed`. `diverged` and `skipped` are the cases where the manifest, not the trees, holds the decision; that is precisely why they must be sticky. The manifest's job is history, ordering, and the user's declared intent — not truth about file contents.

### Deletions
`op: delete` (lines removed, nothing added) and `op: delete_file` are presented as instructions ("delete these N lines / delete this file"), the editor is opened at the anchor, and classification checks the lines are gone. The user still performs the deletion by hand.

---

## 6. HunkPresenter

### Rendering (terminal)
```
── hunk 4/11 ── src/models.py:42 ── replace ─────────────
   context lines (dim)
 -  old lines (red)
 +  new lines (green, this is what you type)
   context lines (dim)
──────────────────────────────────────────────────────────
opening nvim at src/models.py:42 …
```
No prose, no explanation. `note` (if any) shown as a single dim line. The editor opens at the anchor line itself — the first line the user is about to type — so the header's `:42` and the launch line's `:42` always agree.

Untypeable hunks (binary, `[transcribe] verbatim`) render as path + `note` only, with no line block and no editor launch; see the byte-compare gate in §5.

### Anchoring
`anchor_hint` is advisory. Real anchor at presentation time = search the current real file for the `context_before` block (exact line match); if found, anchor = line after it. Fallbacks, in order: search for `context_after` (anchor = line before it); fuzzy search (strip leading/trailing whitespace per line); finally fall back to `anchor_hint` with a warning line. Ambiguity (context matches at multiple positions): choose the match nearest `anchor_hint`.

### Editor launch
`$ROTE_EDITOR`, else `config.toml editor`, else `nvim`. **`$EDITOR` and `$VISUAL` are deliberately not consulted** — transcription wants a specific editor with a specific `+LINE` calling convention, and silently inheriting whatever `$EDITOR` happens to be (including `ed`, or a pager, or something with no `+LINE` support) is a worse failure than the explicit chain. ARCHITECTURE.md's data-flow sketch says `$EDITOR`; this document wins.

Invocation: `<editor> +<anchor_line> <real_file_abs_path>`. Blocking wait on exit. (For `create_file` hunks: touch nothing; open `<editor> <path>` and let the user create it — the editor invocation must not create the file itself.)

### Classification (on editor exit)
Re-locate the region via context matching as above, extract the lines between the context blocks, compare to `new_lines`:
- Exact match (modulo trailing whitespace per line — configurable `strict_whitespace = false` default) → `typed`.
- Region unchanged from before (still equals `old_lines`) → remains `pending`; print "untouched — run `rote next` to retry or `rote skip`".
- Anything else → **divergence prompt** (user chose prompt-every-time):
  ```
  your version differs from the proposal:
    proposal │ yours
    (side-by-side or unified mini-diff of new_lines vs actual)
  [k]eep mine   [r]etry (reopen editor)   [s]how full hunk again
  ```
  `keep` → status `diverged`, store both versions in `divergence`. `retry` → reopen editor, reclassify. The prompt loops until `k` or a successful retry.

---

## 7. Configuration

### Global `~/.config/rote/config.toml`
```toml
editor = "nvim"                  # invoked as: editor +LINE FILE
claude_cmd = ["claude"]          # session agent launch command
claude_continue_cmd = ["claude", "--continue"]   # for `rote talk --attach`
color = true
max_hunk_lines = 20              # project value overrides this
strict_whitespace = false        # true = trailing whitespace differences count as divergence
```

Both claude commands are **argument vectors, not shell strings.** They are the same kind of thing and must have the same shape; a bare binary name in one and a space-separated string with flags in the other invites splitting one and not the other, and any `exec` path that shell-splits a config string is a quoting bug waiting for a path with a space in it. Vectors are passed to `exec` verbatim, with no shell involved.

### Per-project `.rote.toml` (created by `rote init`)
```toml
max_hunk_lines = 20
strict_whitespace = false

[shadow]
copy = [".env"]          # gitignored files the agent needs; copied real → shadow each sync
preserve = ["target/", "node_modules/", ".venv/", "dist/", "build/"]
                         # survive `git clean -fdx` in the shadow (§3 step 3).
                         # Build output ONLY — never source, never anything the diff reads.

[transcribe]
verbatim = ["Cargo.lock", "package-lock.json", "pnpm-lock.yaml", "yarn.lock", "poetry.lock", "uv.lock", "*.lock"]
                         # presented whole, byte-compare gated, never typed line by line (§5, §9.4)

[checks]
commands = ["cargo test", "cargo clippy -- -D warnings"]   # run in REAL tree at `done`

[review]
enabled = true
model_args = []          # extra args appended to `claude -p`, e.g. ["--model", "claude-haiku-4-5"]
```

Per-project values override globals. Missing file → defaults. `copy` and `preserve` are separate keys doing different jobs and must not be merged: `copy` moves files real → shadow on every sync (so the agent's `.env` matches yours), while `preserve` merely exempts paths from the shadow's clean (and is never copied from anywhere).

---

## 8. Reviewer Claude payload (`rote done` step 3)

The session-start snapshot is `baseline.head` + `~/.local/share/rote/<hash>/baseline.patch`, both produced by sync (§3 steps 7–8). §8 only reads them.

At `done`, the **session diff** = diff between (baseline.head + baseline.patch) and the current real tree, computed by: create a temporary detached snapshot of the baseline state in the shadow (`git stash`-free approach: reset a temp worktree of the *shadow* clone to baseline and apply baseline.patch), then `git diff --no-index` against the real tree per file, concatenated. Implementation may instead use a temp directory materialization — correctness over elegance; this runs once per session.

Payload piped to stdin of `claude -p "$REVIEW_PROMPT" [model_args…]`:

```
== TASK ==
<task string or "(unspecified)">

== SESSION DIFF (baseline → current working tree) ==
<unified diff>

== MANUAL DIVERGENCES (proposal vs. what was typed) ==
file src/models.py hunk h-0007:
  proposal:
    …
  typed:
    …
<or "(none)">

== SKIPPED HUNKS ==
<list with file:line and the skipped lines, or "(none)">
```

`REVIEW_PROMPT` (constant in source):
> You are reviewing code changes that were typed in manually. Review the session diff for bugs, inconsistencies, and incomplete changes. Pay special attention to the manual divergences: distinguish deliberate refactors from likely transcription typos (single-character differences, transposed identifiers, wrong operators) and flag the typos explicitly with file and line. Note anything a skipped hunk leaves broken. Be terse. Do not explain concepts. Output findings as a flat list ordered by severity; if nothing is wrong, say so in one line.

### Reviewer isolation

ARCHITECTURE.md claims the reviewer "never gets filesystem access." A temp cwd alone does not deliver that — `claude -p` keeps its normal file tools and can read anything the user can, cwd notwithstanding. The claim is kept and the mechanism is brought up to it:

- **Tool restriction on the invocation.** Pass the Claude Code CLI's tool-restriction flags so the reviewer has no file, shell, or network tools — genuinely text in, text out. Everything it needs is already on stdin.
- **Plus** the temp empty cwd, `--no-color`-safe output handling, and a 5-minute timeout.
- **Fail soft.** A nonzero exit, a timeout, or an unsupported flag on the installed CLI prints a warning and continues the pipeline. Review is advisory (§1 step 4), so a reviewer that cannot run must never block a session from closing.

The flag names are the one place rote touches Claude Code's surface, so keep them in a single named constant in `review.rs` and expect to update them across CLI versions. This does not weaken Principle 1: that principle governs the *session* agent, which stays entirely unconfigured. The reviewer is a separate, headless invocation that rote fully owns.

---

## 9. Edge cases the implementation must handle

1. **Real repo dirty at `start`** — supported and normal (sync carries uncommitted changes). Not an error.
2. **Real repo changes commits mid-session** (user commits, pulls, switches branches while a session is open) — detect at recompute by comparing real `HEAD` against `baseline.head`. If they differ, print a prominent warning and continue best-effort; `rote status` shows a `baseline drift` flag. Do not block — recompute against current reality is the defined behavior.

   **`HEAD` is the only drift signal.** `baseline.uncommitted_digest` must not be used for this: it is a SHA-256 over the entire uncommitted diff, so it changes the instant the user types the first character of the first hunk. Transcription *is* a change to the uncommitted diff, which makes the digest a perfect detector of the one thing that is not drift. It stays in the manifest for archive and debugging only.
3. **Agent commits in the shadow** — irrelevant; diffs compare working trees, not history. Sync at `done`/`start` resets shadow history regardless.
4. **Agent adds dependencies** (`Cargo.toml`, `package-lock.json`, etc.) — these are ordinary file hunks; lockfiles will produce huge diffs. Mitigation: `[transcribe] verbatim` in `.rote.toml` (§7, defaulting to the common lockfiles) — files matching these globs are presented as a single hunk with `note: "generated file — run the generating command instead"` and classified by the byte-compare gate (§5), not by typing and not by a prompt. The expected workflow is that the user runs `cargo add` / `npm install` themselves in the real tree; the gate stays pending until the resulting lockfile matches the shadow's byte for byte, which is what catches the case where the user forgets to run it at all. Note that `Cargo.toml` itself is a hand-edited file and stays an ordinary typed hunk — only the generated lockfile is gated.
5. **File renames** — appear as delete_file + create_file pairs (no rename detection in `--no-index` mode). Acceptable for v0; note in output when a deleted and created file have >80% identical lines: `note: "possible rename from X"`.
6. **Editor exits nonzero** — treat as untouched; do not classify.
7. **Concurrent `rote` invocations** — a lock file (`session.json.lock`, PID + timestamp, stale after crash detection via PID liveness) guards manifest writes; second invocation errors politely. The lock is held for the duration of a manifest write, never for the duration of a session: `rote start` must release it before `exec`ing claude (§1), or the recorded PID becomes the long-lived claude process and every later invocation sees a live holder.
8. **Shadow deleted or corrupted mid-session** — `next` detects missing shadow dir → error instructing `rote abort` then `rote start` again.
9. **Symlinks in the repo** — copy as symlinks during untracked-file sync; diffing follows git's behavior (link target as content).
10. **Empty session** (agent changed nothing) — `next` reports queue empty; `done` short-circuits checks/review with "no changes".
11. **CRLF** — rely on byte diffs (`--no-textconv` is already in the §5 invocation); classification's whitespace tolerance covers trailing `\r` when `strict_whitespace = false` (§7).

---

## 10. Module layout (suggested)

```
src/
  main.rs          # clap definitions, dispatch
  config.rs        # global + project config load/merge
  paths.rs         # XDG dirs, project hashing, lock file
  git.rs           # GitPlumbing: thin Command wrappers, error surfacing
  shadow.rs        # ShadowManager
  diffparse.rs     # unified diff parser → Vec<RawHunk>
  hunks.rs         # splitting, IDs, Hunk model (serde)
  session.rs       # SessionEngine: manifest, state machine, recompute
  detect.rs        # environment + project detection (doctor/setup/init)
  present.rs       # HunkPresenter: render, anchor, editor, classify
  review.rs        # done-pipeline: checks, reviewer payload + invocation
```

Unit-test targets (minimum): diffparse (fixture diffs incl. new/deleted/binary/no-newline), hunk splitting, anchor matching (drifted files), reconciliation scenarios (typed/diverged/reworked/new), sync round-trip on a fixture repo (integration test using a temp git repo).

---

## 11. Project detection and the path-collision guard

### Detection (`detect.rs`)

One shared surface, so `doctor`, `setup`, and `init` cannot disagree about what
they found. It probes and returns values; it never prints and never writes.

Project kind is decided by manifest file, in a fixed order so a polyglot repo
gets one answer rather than an arbitrary one: `Cargo.toml` → Rust,
`package.json` → Node, `pyproject.toml` → Python, `go.mod` → Go, else unknown.

Check commands are **verified to run on this machine before being written**,
which is the whole reason detection probes rather than hardcodes. A Homebrew
Rust with no rustup has `cargo-clippy` and `cargo-fmt` on PATH but no
`cargo clippy` subcommand, so the obvious defaults would be written into every
`.rote.toml` and fail at the first `rote done`. The Node case reads the scripts
actually declared in `package.json` before falling back to `npm test`. An
unrecognized project gets no commands at all — rote does not invent checks.

`verbatim` globs follow the same ecosystem mapping, so a Rust project is not
told to watch for `pnpm-lock.yaml`.

### The path-collision guard

`ProjectPaths::resolve` refuses, **at runtime in every build**, when the shadow
or state directory would land inside the repository being shadowed. The error
names the collision and the environment variable that fixes it.

This is not the same thing as `assert_not_in_real_tree`, which is a
`debug_assert` and therefore compiled out of exactly the build `cargo install`
produces. The realistic trigger is not a misconfigured `XDG_CACHE_HOME` but an
ordinary one: when `$HOME` is itself a git repository — a dotfiles repo —
`~/.cache/rote/<hash>/shadow` is inside the real tree by definition, and rote
would clone the home directory into a subdirectory of itself. The debug
assertions remain as a second layer, for a future writer that constructs a
target without going through `ProjectPaths` at all.

Comparison detail: the XDG roots routinely do not exist on first run, so
canonicalize the deepest ancestor that *does* exist and re-attach the remainder.
Canonicalizing the whole path fails on a missing directory; comparing raw
strings misses macOS's `/var` → `/private/var` symlink.

---
