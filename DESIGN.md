# rote — Detailed Design

Read ARCHITECTURE.md first. This document specifies component behavior, data schemas, CLI contracts, and edge cases precisely enough to implement from. Where this doc and ARCHITECTURE.md conflict, this doc wins.

---

## 1. CLI contract

Binary name: `rote`. All commands run from anywhere inside the real repo (repo root discovered by walking up to `.git`). Running outside a git repo is an error with a clear message.

Two commands — `doctor` and `setup` — deliberately do **not** require a repository. They concern machine-level state (the claude binary, the global config), and `doctor` is the command you reach for when something is wrong, which may well be before you have a repo. Repo discovery is therefore per-command, not a precondition of dispatch.

### `rote doctor [--deep]`
Read-only diagnosis of whether this machine can run rote. One aligned line per check — git, claude, the reviewer's tool-restriction flag, the global config, the repository, the shadow location, the project config — each either passing or naming the command that fixes it. Repository-scoped lines report `not in a git repository` rather than failing when run outside one. **Exits non-zero if any check fails**, so it can gate a script.

The reviewer check parses `claude --help` for the flag in `REVIEWER_TOOL_FLAGS` (§8): free, offline, and enough to catch a rename. `--deep` additionally runs one real `claude -p` invocation, which is the only check that catches a flag that exists but behaves differently than assumed. It is opt-in because it costs a token spend.

`doctor` never writes anything.

### `rote setup [--force]`
The only writer of `~/.config/rote/config.toml`, which nothing else creates. Runs the same detection as `doctor`, prompts only where detection is ambiguous, and refuses an existing file without `--force`. It never prompts about the editor: rote does not launch one as part of the loop, so blocking on that answer would block on something nothing needs. On a non-tty it takes the detected values without prompting, so it stays scriptable.

### `rote init`
Creates `.rote.toml` in the repo root (see §7). Idempotent; refuses to overwrite an existing file without `--force`.

**Checks are prefilled from project detection (§11) and left active, not commented out.** A `[checks]` block that is empty by default means `rote done` silently verifies nothing — the wrong default for the one safety net in the close-out pipeline. `init` prints one line naming what it detected and what it chose; an unrecognized project gets `commands = []` with a comment explaining what to add.

### `rote start [TASK...]`
- Errors if a session is already active (message: show `rote status`, suggest `done` or `abort`).
- Errors if the real repo is in a merge/rebase/cherry-pick/bisect state (detect via `.git/MERGE_HEAD`, `.git/rebase-merge`, `.git/rebase-apply`, `.git/CHERRY_PICK_HEAD`, `.git/BISECT_LOG`).
- **Preflights `claude_cmd` only**, and only when it is actually going to launch: if the binary does not resolve on PATH, fail naming it and suggesting `rote doctor`. Deliberately narrow — the reviewer is not needed until `done`, so each command validates its own prerequisites at the point it needs them rather than every command running a full diagnostic.
- Creates the shadow clone if absent; syncs real → shadow (§3).
- Writes the manifest: state `working`, `task` = joined TASK args (may be empty), `baseline` = snapshot info (§4).
- **Releases the manifest lock before exec.** The lock records a PID and treats a live PID as held (§9.7); after `exec` that PID belongs to the claude process, which lives for the whole session. Failing to release here wedges every subsequent `rote` invocation until the user quits claude.
- **Execs `claude` with cwd = shadow directory, replacing the rote process in the current pane** (use `exec`-style process replacement: `std::os::unix::process::CommandExt::exec`). Pass no extra args, no prompt injection. If TASK was given, print it once before exec so the user can paste/say it — do NOT pass it to claude (that would require choosing between `-p` and interactive; the user drives the conversation).
- The claude command is configurable (`config.toml: claude_cmd`, default `["claude"]`; an argument vector, not a shell string — see §7).

### `rote status`
Prints: state, task, session age, shadow path, and if `transcribing`/`working`: files changed, hunk counts by status (pending / typed / diverged / skipped). Exit code 0 always (informational).

### `rote watch [--local]`
The transcription loop, and the only command in it. By default it **attaches to
the daemon** (§13), starting one if none is running; `--local` runs the engine in
this process instead, for debugging and for a machine where a daemon cannot
start. Two panes attached to one daemon are ordinary; two `--local` panes are
refused, because each would run an engine. Watches the real tree and the shadow, reclassifies on save (§6), advances the queue, and redraws a full-screen pane. There is no command between a keystroke and the queue moving.

Refuses while the repository is mid-merge or mid-rebase: every classification would be nonsense and the pane would report it confidently.

Losing the daemon is not fatal: the pane shows a warning notice and reconnects with backoff, re-reading `daemon.json` each attempt because a restarted daemon has a different port. It gives up after 30s and names the log. A `closed` frame is a clean exit instead, with the reason — "session closed" or "session aborted".

Keys: `s` skip, `o` open the configured editor at the anchor, `g` refresh, `q` quit; `k`/`r` answer an open question. Hidden flags `--exit-when-empty` and `--timeout` exist for tests and scripts, so a wedged watcher fails rather than hangs.

### `rote next`
- Errors if state is `idle`.
- Runs **recompute** (§5), takes the first `pending` hunk, renders it (§6), prints a `file:line` jump target, and exits. A printer, not a step in the loop: it classifies nothing and launches nothing. It exists so a plain shell, a script, or a `--json` consumer can see what is outstanding without a pane.
- Untypeable hunks keep their byte gate here, because `classify_by_bytes` is two file reads and no subprocess — the one honest verdict a printer can still reach on its own.
- Progress is position within the queue *as it stands after this recompute*, not a fixed target. The denominator moves when the agent reworks something or adds new work; that is expected, and the display should not pretend otherwise.
- If the queue is empty: prints "nothing to transcribe" and, if all hunks are terminal, suggests `rote done`.
- Hidden flag `--json`: instead of rendering, emit the next pending hunk as one JSON line (schema = the Hunk object in §4) and exit. This is the front-end seam. `--json` performs no classification.

  There is deliberately **no verb that sets a hunk's status to `typed`**. The hidden `rote mark` did exactly that and has been removed. `Typed` is producible only by the classifier, whose input is the hunk's own `new_lines` — which came from the shadow. Three layers hold the line: no verb exists, the engine is the only producer, and reconcile rule 3 returns a `typed` hunk to the queue the moment the trees disagree again. A front end cannot assert its way to a finished session.

  Status changes a front end *may* make are the ones that record a human decision rather than a fact about the trees: `rote skip [HUNK_ID]`, and (from Stage 1) resolving a divergence. Both are addressed by hunk id, never by queue position — the head of the queue can move between the moment a front end renders a hunk and the moment the user acts on it.

### `rote show [HUNK_ID]`
Re-prints a hunk, defaulting to the most recently presented one (`last_presented`). **It changes nothing.** This is a display command, not an undo: rote never writes the real tree, so it cannot un-type keystrokes, and a correctly typed hunk has already vanished from the fresh diff (§5 rule 7) — there is nothing to restore. To re-edit a region, open it yourself; the watcher will notice.

### `rote resolve <HUNK_ID> keep|retry`
Answers an open divergence question (§6). `keep` records both versions and makes the decision final; `retry` withdraws the question and lets the watcher carry on classifying as the user keeps typing. The id is required for both: unlike `skip` there is no sensible default, because the queue moves on past an unanswered question rather than blocking on it, so the hunk carrying one is usually not the active hunk.

### `rote skip [HUNK_ID]`
Marks a hunk `skipped`, defaulting to the active one. Skip is **durable**: a skipped hunk stays skipped across recomputes (§5 rule 4), even though its region still differs between the trees. Skipped hunks are reported at `done` time and require explicit confirmation to close the session.

### `rote report <HUNK_ID> typed|pasted`
Records how a hunk's content arrived (§6). Changes no status and moves nothing in the queue; it exists so a user with no editor plugin can correct what the engine could not observe, and so one whose plugin guessed wrong can correct it back. Last write wins.

### `rote endpoint [--json] [--token] [--ensure]`
Prints where the daemon is listening, for a front end that is not the pane. The human form withholds the token — that is the form that ends up in scrollback and on a shared screen — and `--json` / `--token` print it, because the only consumer of a token is a program capturing stdout. Gating on a tty instead would break `rote endpoint --json | jq` in exactly the configuration where someone is debugging. `--json` prints an object on failure too (`no_session`, `no_daemon`, `opaque_owner`) so a caller can parse stdout unconditionally. `--ensure` starts a daemon, explicitly overriding `[daemon] autostart = false`, which is about *implicit* spawning at `start`.

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
  "version": 2,
  "state": "transcribing",
  "task": "add tagging to posts",
  "created_at": "2026-08-13T10:22:00Z",
  "session_id": "9f2c1a4e7b03d581",
  "project_root": "/home/user/proj",
  "shadow_dir": "/home/user/.cache/rote/ab12…/shadow",
  "baseline": {
    "head": "c0ffee…",
    "uncommitted_digest": "9a3f…",
    "synced_at": "2026-08-13T10:22:01Z"
  },
  "hunks": [ Hunk, … ],
  "generation": 41,
  "last_presented": "h-0007"
}
```

`generation` is monotonic and bumped by every write that changes something (§13). `session_id` is 8 bytes of kernel entropy minted at `start`; it exists because `created_at` is second-resolution and so cannot identify a session — two opened in the same second would share it, and anything keyed off a session would believe the previous one's file. `Manifest::session_stamp` falls back to `created_at` for a manifest written before the field existed.

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
  "key": "k-3f9a1c7e0b52d846",      // content identity WITHOUT context — see below
  "status": "pending",              // pending | typed | diverged | skipped
  "divergence": null,               // when diverged: {"proposed": [...], "actual": [...]}
  "pending_divergence": null,       // a question asked but not yet answered (§6)
  "note": null,                     // optional one-liner rote attaches (e.g. "file is new")
  "curator_note": null,             // one line of ordering rationale (§8b)
  "curator_rank": null,             // teaching order; absent sorts after everything ranked
  "input": "typed"                  // unknown | typed | pasted; omitted when unknown
}
```

Every field after `note` is optional and omitted when unset, which is what keeps this additive: `WIRE_VERSION` is unchanged by all of them.

Hunk IDs are content-addressed: `h-` + first 16 hex of SHA-256 over (`file`, `old_lines`, `new_lines`, `context_before`, `context_after`). This makes reconciliation across recomputes natural: an unchanged proposal keeps its ID; a reworked one appears as a new hunk. Colliding IDs are disambiguated with a `.2`, `.3` ordinal.

`key` is the same hash **without the two context groups**, and the difference is the point. Because `id` folds in surrounding context, typing anywhere within `CONTEXT_LINES` of a hunk changes its *neighbour's* id — so anything stored against an id evaporates while the user works. `key` survives exactly that churn, and is what the curator cache (§8b) and the classifier's compare-and-swap key off. Two hunks in one file with the same `old → new` pair deliberately share a key: they are the same change and deserve the same note. So `key` is content identity, never a lookup handle — `find`/`find_mut` are id-only.

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
- Sub-hunks of one parent are ordered and must be presented in order.

**Queue order** is a *read-time view*, never storage order — `reconcile` appends to the tail and its positional invariants are what the reconcile tests assert against, so nothing may sort `manifest.hunks` itself. The view is: `curator_rank` where it exists, then file path, then position within the file, then `id` so the sort is total. The last three are the entire ordering when no curator has run (§8b), and the tie-break when one has.

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

### The open action (optional)
rote does not launch an editor as part of the loop. It offers to, from the `o` key in the watch pane, and that is the only place an editor is ever invoked.

Resolution is `$ROTE_EDITOR`, else `config.toml editor`, else `nvim`. **`$EDITOR` and `$VISUAL` are deliberately not consulted** — the invocation uses a specific `+LINE` calling convention, and silently inheriting whatever `$EDITOR` happens to be (including `ed`, or a pager, or something with no `+LINE` support) is a worse failure than the explicit chain.

Invocation: `<editor> +<anchor_line> <real_file_abs_path>`. The pane leaves the alternate screen and restores cooked mode first, and the exit status is ignored — rote is not waiting on the editor to decide anything. Events queue while it has the terminal, so the user returns to a hunk that has already been classified. A machine with no editor is fully usable; `doctor` reports this as informational, never a failure.

### How the content arrived

Orthogonal to classification, and reported at `done` rather than enforced anywhere. `Hunk.input` is `unknown | typed | pasted`, omitted from the wire when unknown, and has two producers.

The **engine** infers, from the save history rather than from the bytes: a hunk observed *in progress* — the region a line-wise prefix of the proposal — had a human in the loop at that moment, and a single paste of the whole hunk cannot produce that, because it classifies `typed` on the first save and never passes through the prefix rule. The inference is deliberately weaker than "every character was typed": pasting the first half and then the second is recorded as `typed`, and so is accepting a completion over a typed prefix. What the engine will never do is assert `pasted` — a careful typist who writes a whole hunk into the buffer and saves once is byte-identical to a paste, which is why `unknown` is a first-class answer rather than a null.

A **front end** reports, from something the filesystem cannot see: nvim knows a bracketed paste from `TextChangedI`. A report always lands; an inference only ever fills in `unknown`. So an inference can never overwrite what a front end saw, and a wrong report is correctable by another one.

The evidence lives in the engine's memory, keyed by hunk `key` (§4) so it survives the id churn of typing near a hunk, and pruned to the pending queue on every recompute. It is deliberately not persisted: a daemon restart degrades one hunk to `unknown`, and a fourth on-disk artifact with its own version and session stamps is not worth an advisory counter that gates nothing.

`rote done` reports the tally over `typed` hunks, and **says nothing at all when it observed nothing** — that is the no-plugin, no-autosave case, and "0 of 12 observed" is technically true and reads as an accusation. `rote status` deliberately does not carry it: mid-session the number only grows and is not actionable, and a retrospective is not a scoreboard.

### Classification (on save)
Re-locate the region via context matching as above, extract the lines between the context blocks, compare to `new_lines`:
- Exact match (modulo trailing whitespace per line — configurable `strict_whitespace = false` default) → `typed`.
- **The region unchanged since the last baseline → `untouched`.** Per region, not per file: a file usually holds several hunks, so an edit to any one of them would otherwise make every other hunk in it look touched.
- **A line-wise prefix of the proposal, last line allowed to be a character prefix → in progress.** No question is ever raised for it, however long the pause. A prefix cannot be a disagreement; it can only be an unfinished agreement. This is the contract that makes a watcher usable at all: without it, every keystroke-flush mid-hunk reads as a divergence.
- Anything else → a **question**, but only after the file has been still for `watch.divergence_grace_ms` (default 2000). Any further save to that file restarts the window: it measures stillness, not time since the first mistake. The two layers are both needed — the prefix rule handles typing top to bottom, which is most transcription, and the timer handles pasting the middle or typing bottom-up, where no prefix relationship ever holds.

A raised question sets `pending_divergence` and leaves the hunk `pending`; **the queue advances past it** rather than blocking. Mechanically: an open question is the first term of `queue_view`'s sort, so such a hunk sorts *last* among the pending ones — still in the queue, still answerable, no longer the head. The curator's "freeze current" pin therefore does not fire on it either; a hunk the user has deliberately been moved past is not the hunk they are looking at.

It is answered by `rote resolve` or by `k`/`r` in a front end, and those keys address the questioned hunk rather than the active one — which is why `Snapshot` carries it as `question`, separately from `active`. `QueueItem.has_question` marks it in a list; answering it honestly needs the proposed and actual lines, so the whole `Presented` goes on the wire. A front end draws it alongside the hunk being typed, naming the file, because it is about something else.

`keep` → status `diverged`, both versions stored in `divergence`. `retry` → withdraw the question and keep watching. Typing the proposal correctly while a question is open withdraws it too — the question is moot.

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

[curator]                        # the one table the global file carries
enabled = true
model_args = []
```

`[curator]` is global as well as per-project, and it is the only table that is. Every other one describes a repository; this one answers "may rote spend tokens in the background without being asked", which is a property of the person and their plan. Requiring it once per repo is how it ends up unsaid somewhere. Project values still win.

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

[watch]
divergence_grace_ms = 2000   # stillness before a disagreement becomes a question (§6)
debounce_ms = 400            # trailing-edge debounce before a full re-diff
ignore = []                  # extra globs the watcher never wakes for

[daemon]
autostart = true             # `rote start` leaves a daemon watching (§13)

[curator]
enabled = true               # the teaching-order pass (§8b)
model_args = []
```

**Environment.** `NO_COLOR` disables colour. `ROTE_EDITOR` overrides `editor`. `ROTE_CURATOR=off` (also `0`, `no`, `false`) disables the curator for one invocation — the escape hatch for the only thing rote does that spends money without being asked. It is read at the process boundary rather than inside config loading, so config loading stays a pure function of two files. A detached daemon inherits it, so turning it off for a `rote start` also turns it off for the session that start leaves running.

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

The flag names are the one place rote touches Claude Code's surface, so they live in a single named constant — `model::TOOL_FLAGS`, now shared by the reviewer, the curator, and `doctor --deep` — and expect to update them across CLI versions. This does not weaken Principle 1: that principle governs the *session* agent, which stays entirely unconfigured. The reviewer is a separate, headless invocation that rote fully owns.

### The model call itself

All three headless invocations go through `model::run`, and nothing else in the tree spawns `claude` for an answer. That is not tidiness: the obvious implementation writes the payload to the child's stdin and then reads its output, which deadlocks the moment both pipes fill — the parent blocked in `write_all` because the child is not reading, the child blocked in `write` because the parent is not reading. Pipe buffers are 16 KiB on macOS and 64 KiB on Linux, so a session diff clears the threshold on an ordinary session.

So: stdin gets its own thread, stdout and stderr get one each, and the calling thread polls `try_wait` against the deadline — which is what puts the timeout around the *whole* call rather than only the part after the payload has landed. Killing the child is what unblocks the other three, by closing the pipe ends. Results are collected over a channel rather than by joining, because a grandchild that inherits the pipe keeps `read_to_end` from returning and an unconditional join would reintroduce the same hang one level down.

---

## 8b. Curator Claude payload

A headless pass that puts the queue in **teaching order** and writes one line per hunk saying why it comes where it does. Advisory in every direction: a curator that cannot run leaves the deterministic order (§5) exactly as it was.

Payload piped to stdin of `claude -p "$CURATOR_PROMPT" [tool flags] [curator.model_args…]`:

```
== TASK ==
<task string or "(unspecified)">

== HOW TO ANSWER ==
{"order":[{"hunk":3,"note":"..."},...]} — every hunk number below
exactly once, best first. JSON only.

== HUNKS ==
[1] src/models.py:42  replace
    in: class Post(models.Model):
    note: <mechanical note, if any>
    previously: <the note a earlier pass gave this hunk, if any>
    -     body = models.TextField()
    +     body = models.TextField()
    +     tags = TaggableManager()
[2] …
```

Hunks are labelled with **small integers, never keys**. A model asked to echo `k-3f9a1c…` will eventually mangle one, and a mangled key is a hunk silently placed where it does not belong; index → key is a mapping rote owns and can check. The list is deduplicated by `key` (§4), capped at `MAX_HUNKS` (60), and each side of a hunk is capped at `MAX_LINES_PER_HUNK` (30) so one enormous hunk cannot crowd out the other fifty. Untypeable hunks (§5's byte-compare gate) go in with their `note` as their body — a lockfile belongs somewhere in the order too, usually last.

`CURATOR_PROMPT` (constant in source) asks for an order in which each hunk makes sense to a reader who has seen everything above it and nothing below it — definitions before uses, a data model before the code that reads it, the core change before the knock-ons, tests and generated files last — plus one short line of **ordering rationale** per hunk. It explicitly rules two things out: not a summary of what the hunk does (the reader is about to read it, and a description becomes the thing they read instead of the code), and not warnings about typos or tricky syntax (noticing those is the exercise).

**Reading the reply** is forgiving in bounded ways, because the failure mode of strictness is discarding a good ordering over a code fence: fenced JSON, JSON wrapped in prose, a bare top-level array, unknown keys, out-of-range or repeated hunk numbers (first position wins). Brace matching tracks string literals and their escapes rather than scanning for the last `}` — the material being described is code, so a note containing a brace or a quote is ordinary. Notes are reduced to one trimmed line and capped at 160 characters. A reply with nothing usable is an **error**, not an empty ordering: the caller has to be able to tell "the model declined" from "the model chose this order".

### Where the result lives

`~/.local/share/rote/<hash>/curator.json`, keyed by hunk `key`, re-applied after every `reconcile` (§5). This is the source of truth and the fields on `Hunk` are a projection of it — it has to be, because `reconcile` copies nothing onto a fresh hunk and ids move whenever the user types near one (§4).

Two properties are load-bearing:

- **An entry with neither rank nor note is a tombstone**, meaning "offered to the model, nothing came back". Without it a failed pass is retried on every keystroke: the uncached set would still be the pending set, and typing a hunk *shrinks* it, moving the fingerprint the trigger compares against. Tombstones gate **triggering**, never **inclusion** — the payload is always the whole pending set, because a new hunk cannot be ordered against hunks the model has not seen.
- **The file is stamped with `session_id`.** Clearing it at `done`/`abort` is not enough: `abort` reaps the daemon best-effort, and a `rote watch --local` pane publishes no `daemon.json` to be reaped at all, so its engine can write the file *after* the teardown removed it. The next session would then inherit a teaching order built for a different set of hunks — and never re-curate, because every key would look considered.

### When it runs

In the engine, on its own thread; `Engine::run` is single-threaded and a blocking `claude -p` inside `step` would freeze keystroke classification for minutes. A pass starts when all of: the curator is enabled, none is in flight, the shadow has been still for `CURATOR_SETTLE_MS` (1.5 s) with no re-diff owed, at least two hunks are pending, at least one pending key has never been considered, and the fingerprint of the pending set differs from the last attempt. At most `MAX_PASSES_PER_SESSION` (8) in one session, in memory — a daemon restart resets it, which is right, because a restart is not a loop.

Quiescence is its own signal rather than the recompute debounce, which the user's typing arms as well: gating on that would delay curation for as long as somebody is working, and under steady typing a comparable window may never elapse.

### Freeze current, reorder ahead

A curation landing mid-session must not move the hunk you are looking at. The pin is **baked into the stored ranks** (rank 0) rather than applied when writing — it has to be, because the cache is re-applied after every `reconcile`, so a pin held anywhere else would be overwritten by the next recompute a second later and the screen would jump after all.

The head is pinned only once the user is *in* the session: any hunk terminal, or the head carrying an open question, or the head's region differing from its baseline. With nothing typed and nothing started there is no place to lose, and the curator's opinion about what to do first is the entire point of asking. That last test goes through `engine::observe`, not `present::is_in_progress` — the latter returns true for an empty region ("the state before the first keystroke") and so cannot tell started from not-started.

### Curator isolation

Identical to the reviewer's: `model::TOOL_FLAGS`, an empty temp cwd, and a hard timeout (`CURATOR_TIMEOUT`, 120 s — shorter than the reviewer's, since this ranks material already summarized while a user waits to see a queue). Every failure becomes a `Notice::warn` and never a propagated error: an `Err` out of the curation path reaches `Engine::run`, which reports "the engine stopped" and takes the daemon with it, so a curator that could not write its cache would kill the thing that watches you type.

---

## 9. Edge cases the implementation must handle

1. **Real repo dirty at `start`** — supported and normal (sync carries uncommitted changes). Not an error.
2. **Real repo changes commits mid-session** (user commits, pulls, switches branches while a session is open) — detect at recompute by comparing real `HEAD` against `baseline.head`. If they differ, print a prominent warning and continue best-effort; `rote status` shows a `baseline drift` flag. Do not block — recompute against current reality is the defined behavior.

   **`HEAD` is the only drift signal.** `baseline.uncommitted_digest` must not be used for this: it is a SHA-256 over the entire uncommitted diff, so it changes the instant the user types the first character of the first hunk. Transcription *is* a change to the uncommitted diff, which makes the digest a perfect detector of the one thing that is not drift. It stays in the manifest for archive and debugging only.
3. **Agent commits in the shadow** — irrelevant; diffs compare working trees, not history. Sync at `done`/`start` resets shadow history regardless.
4. **Agent adds dependencies** (`Cargo.toml`, `package-lock.json`, etc.) — these are ordinary file hunks; lockfiles will produce huge diffs. Mitigation: `[transcribe] verbatim` in `.rote.toml` (§7, defaulting to the common lockfiles) — files matching these globs are presented as a single hunk with `note: "generated file — run the generating command instead"` and classified by the byte-compare gate (§5), not by typing and not by a prompt. The expected workflow is that the user runs `cargo add` / `npm install` themselves in the real tree; the gate stays pending until the resulting lockfile matches the shadow's byte for byte, which is what catches the case where the user forgets to run it at all. Note that `Cargo.toml` itself is a hand-edited file and stays an ordinary typed hunk — only the generated lockfile is gated.
5. **File renames** — appear as delete_file + create_file pairs (no rename detection in `--no-index` mode). Acceptable for v0; note in output when a deleted and created file have >80% identical lines: `note: "possible rename from X"`.
6. **Half-typed saves** — a watcher sees the file mid-hunk, and the region matches neither the original nor the proposal. Treat a line-wise prefix of the proposal (last line allowed to be a character prefix) as *in progress*, never as a disagreement, and require `watch.divergence_grace_ms` of stillness before raising a question about anything else. Both layers are load-bearing: without the prefix rule the user is interrupted on every keystroke-flush, and without the timer someone who pastes the middle of a hunk can never reach a question at all. See §6.
7. **Concurrent `rote` invocations** — `session.json.lock` is an `flock(2)`, taken across a whole read-modify-write cycle and released at its end. The kernel drops it when a holder dies, so there is no stale-lock machinery to get wrong; a contended acquirer waits rather than failing, because a daemon and a CLI invocation contend routinely and failing fast on a lock held for three milliseconds would make the CLI unusable. `rote start` must still release it before `exec`ing claude (§1). A separate `watch.lock` is held for a whole pane's lifetime, which is why it cannot be the same file.

   The lock guards the *cycle*, not just the write. Loading outside it and saving inside is a TOCTOU: two processes both load, both mutate different hunks, both save, and the second silently discards the first's work.
8. **Shadow deleted or corrupted mid-session** — recompute detects a missing shadow dir → error instructing `rote abort` then `rote start` again.
9. **Symlinks in the repo** — copy as symlinks during untracked-file sync; diffing follows git's behavior (link target as content).
10. **Empty session** (agent changed nothing) — the pane and `next` report the queue empty; `done` short-circuits checks/review with "no changes".
11. **Two engines** — impossible by construction, and the reason the daemon exists in the shape it does. See §13: `watch.lock` is the engine token, and every mutation either holds it or routes to whoever does. Getting this wrong does not merely duplicate work; it fabricates divergence questions about hunks the user never touched, and `reconcile` keeps them forever.
12. **A question raised, then re-identified** — once the user's own text is in the tree, a recompute re-diffs that region as (theirs → proposal), a different id. A question that has been *written down* survives by content match (§5); one that is still only an armed candidate follows its content onto the new hunk. Without that the disagreement becomes permanently unaskable, because the baseline has already refreshed to include their text and nothing can re-arm.
13. **A curation landing mid-transcription** — the pass runs unlocked and takes seconds, so the queue moves underneath it. No compare-and-swap is needed, unlike the classifier's: the cache is keyed by content, so a hunk typed while the model was thinking is terminal and its rank is irrelevant, and one the agent reworked away simply has no entry. The *pin* does need care and is computed inside the lock, because the head moves too. A hunk that appears during the call has no entry and so sorts after every ranked hunk rather than in file position; the next pass picks it up. See §8b.
14. **A daemon restarted mid-curation** — the job thread dies with the process and no tombstones were written, so the fingerprint is `None` and the next engine re-fires: one duplicate call per crash, which is correct. Deliberately **no on-disk in-flight marker** — after a `kill -9` a stale one would disable the curator permanently, which is the exact failure `watch.lock`'s doctrine exists to avoid (§13).
15. **A curator child outliving the session** — `serve` joins the engine thread within `JOIN_CEILING` and exits, but a `claude` spawned by a curation can outlive `rote done` by up to `CURATOR_TIMEOUT`. It runs in an empty temp cwd with no tools, so it can do nothing; reaping it would mean holding a child handle somewhere with no business holding one.
16. **A hunk half-typed then pasted** — the in-progress observation stands and the verdict is `typed`. Deliberate: the inference proves a human was in the loop, not that every character was typed, and it is stated that way in §6 rather than quietly overclaimed.
17. **A daemon restarted mid-hunk** — the typing evidence is in memory and goes with the process, so that hunk degrades to `unknown`, never to `pasted`. Persisting it would be a fourth on-disk artifact with its own version and session stamps for an advisory counter that gates nothing.
18. **A hunk re-identified mid-typing** — a floor sweep re-diffs the region as (half-typed → proposal), which changes `old_lines` and therefore the `key` too, so the evidence is lost and the next save re-creates it. The window is one save and the cost is `unknown`, which is why the record is keyed by `key` and not by a proposal-only hash: the latter would survive this but would alias two hunks in one file with identical `new_lines`, turning "typed the first, pasted the second" into a false `typed`. A false negative is the right failure.
19. **A paste outside the active hunk's region** — not reported, and the verdict stays `unknown`. The active hunk is the only one whose id a front end holds reliably: `QueueItem` carries `anchor_hint`, which is where a hunk was at the last recompute rather than where it is now. Attributing a paste to the wrong hunk is worse than attributing it to none.
20. **A browser page whose daemon restarted** — the new one has a new port *and* a new token, and a page cannot re-read `daemon.json` from inside a browser. `EventSource` would retry against a dead address forever, so the page closes the stream on a terminal error and says to re-run `rote watch --web`. This is a real asymmetry with the pane and the plugin, which both re-read the address book on every attempt.
21. **CRLF** — rely on byte diffs (`--no-textconv` is already in the §5 invocation), and carry the `\r` all the way through: `relativize_no_index_diff` and the parser's line split both work on bytes, so a proposal for a CRLF file ends its lines the way the file does. Both used `str::lines`, which strips a trailing `\r`, and the two sides of the comparison then agreed only through the whitespace tolerance — which meant `strict_whitespace = true` (§7) made a CRLF file permanently unclassifiable, diverging on a byte that had been thrown away. The tolerance is not what makes CRLF work; it just hid this.

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
  doctor.rs        # `rote doctor`: is this machine ready? (§1)
  setup.rs         # `rote setup`: write the global config (§1)
  present.rs       # HunkPresenter: render, anchor, classify
  model.rs         # the one headless `claude -p` call (§8)
  review.rs        # done-pipeline: checks, reviewer payload + invocation
  curator.rs       # teaching order: payload, parser, cache, policy (§8b)
  state.rs         # the wire types (§12)
  engine.rs        # the watch engine: classify on save, advance, publish
  watcher.rs       # filesystem events, filtered and origin-tagged
  daemon.rs        # the daemon: endpoint, threads, HTTP surface (§13)
  http.rs          # transport as pure functions over bytes
  pane.rs          # the terminal front end, local or client
  web.rs           # the browser front end, compiled in with include_str!
```

Plus `lua/rote/` and `plugin/rote.lua`: the nvim front end, and the repository's
first non-Rust artifacts. It reads the event stream over a raw `vim.uv` socket
and issues every mutation by running `rote`, which already decides for itself
whether to mutate directly or send a verb (§13) — so the plugin never holds a
token, never builds a request body, and cannot disagree with the CLI about what
a verb means. The binary is unaffected: it still ships as one file with no
runtime assets, and a plugin manager installs the Lua.

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
unrecognized project gets no commands at all — rote does not invent checks — and
neither does a recognized one whose toolchain is absent: verification failing is
what "verified before being written" *means*, so the command is simply not
written. What gets probed is the program a command names, never the check itself;
running `cargo test` to decide whether to write `cargo test` would cost `rote
init` a full test suite. Each spelling is probed on its own, because clippy and
fmt are separate rustup components and either can be missing while the other is
present.

`verbatim` globs follow the same ecosystem mapping, so a Rust project is not
told to watch for `pnpm-lock.yaml`.

Detection is right for the machine that ran `rote init`, and `.rote.toml` is
usually committed — so a project with contributors on both kinds of Rust
install will have one of them failing `rote done` on a spelling the other
needed. rote's own `.rote.toml` hit exactly this and answers it by pointing
`[checks]` at a `just` recipe that resolves the spelling at run time. That is a
fix a repository chooses, not something `init` can write: it cannot assume a
task runner is installed.

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

---

## 12. The wire types

`src/state.rs` is a public contract in the same sense as the `Hunk` schema in §4,
and for the same reason: three front ends will be written against it — the
terminal pane, an nvim plugin, a browser — and they will not all be updated on
the same day. Field names and variant tags are pinned by tests so that changing
one is a decision rather than an accident.

It is a thin envelope over types that are *already* contracts rather than a
parallel view schema. `Hunk`, `Status`, `Op` and `State` all serialize lowercase
and already cross the `--json` seam; a second set of types would be two schemas
to keep in agreement forever, and they would disagree.

- `Snapshot` — `wire_version`, `generation`, session state, counts, the active
  `Presented` hunk, the pending queue as summaries, and notices.
- `Presented` — the hunk, its position, and **`anchor_line`, resolved by the
  engine against the real file**. That field is what lets a client be a pure
  client: without it, rendering a jump target means reading the file, and a
  browser front end cannot.
- `QueueItem` — a summary with no line bodies, so a snapshot stays small at two
  hundred pending hunks. Full text is fetched by id. Carries `curator_note`, so
  a front end can show the whole queue with its reasons without fetching
  anything. The *order* needs no field: `queue` is already in teaching order,
  because it is built from `queue_view` (§5).
- `Event` — `snapshot` | `notice` | `heartbeat` | `closed`, tagged on `type`.
  **Every change carries a whole snapshot, never a delta.** A delta protocol
  needs a resync story, and a client that misses one frame is silently wrong
  from then on, which is the worst failure mode available because nothing looks
  broken. `generation` is already the resync token.
- `Command` — `skip` | `resolve` | `show` | `report` | `refresh`, tagged on
  `verb`. Deliberately excludes `done`, `abort` and `start`: those confirm
  interactively and `exec`, so they stay CLI-only. And there is no verb that sets
  a hunk to `typed` (§1) — `report` does not weaken that, because it writes
  `input` and nothing else: not status, not the open question, not
  `last_presented`. It is accepted on a hunk that has already gone terminal,
  precisely so a paste report cannot lose a race against the classifier.
- `Reported` — `typed` | `pasted`, what a front end may say about how content
  arrived. Deliberately not `Input`: `unknown` is the *absence* of an assertion,
  and making it unrepresentable on the wire beats rejecting it at runtime.
- `Health` — the handshake: wire and manifest versions, pid, port, project hash,
  and the session's state. A front end reads this first and can refuse politely
  rather than misinterpreting a payload it does not understand. `project_hash`
  is the field a client must check.
- `HunkDetail` — one hunk in full, from `GET /hunk/<id>`, with its anchor
  resolved. The other half of `QueueItem` carrying no line bodies. Not a
  `Presented`: `position` and `total` are meaningless for a typed hunk, and zero
  is a lie a front end will render.
- `Request`/`Response` — a request may carry the `generation` it was looking at;
  when present and stale, the response is `stale` with the current generation
  attached, so a client can re-read and re-issue without a round trip to
  discover it. This matters most for `resolve`: answering a question about a
  hunk that has since been reworked must not land.
- `Cause` — why a `rejected` outcome was rejected, as a variant beside the human
  `reason`. Not redundant with it: `rote resolve` treats one refusal as benign
  (you answered a question that had already gone) and every other one as an
  error, and it used to tell them apart with a substring match on prose composed
  in `engine.rs`, with nothing pinning the two together. `reason` is for a human
  and stays free to be reworded; `cause` is what a client may branch on. Optional
  and additive under the rule below, so introducing it was not a bump.

`WIRE_VERSION` bumps only when a change would break a client written against the
old shape. New optional fields and new event variants are additive.

---

## 13. The daemon

### The single-engine invariant

**An `Engine` may be constructed only by a process holding the `watch.lock`
flock, and it holds that lock unbroken for the engine's whole lifetime.**

Two engines do not duplicate work, they corrupt each other, and the damage is
user-visible and durable. An engine's authority lives in private in-process
state — its baselines, its watchdog, its recompute schedule — that no other
process can see or invalidate. Both classify the same keystroke; one wins the
compare-and-swap in `commit_typed`; the loser's `typed_any` stays false, so it
never retakes its baseline, and it goes on to raise a divergence question about
a hunk the user never touched. `reconcile` keeps hunks with an open question
across every recompute, so only answering clears it.

The lock is therefore also the routing decision for every mutation:

```
Lock::try_acquire(watch.lock):
  acquired -> no engine exists. Mutate directly, holding the guard across
              the whole cycle.
  refused  -> an engine exists. Send it a verb over the protocol.
              Nothing answers -> refuse, naming the pid.
```

The check *is* the exclusion, which is why it beats consulting `daemon.json`
first. That file is an **address book, never an authority**: it outlives a
`kill -9`, and a `--local` pane owns the engine without writing one at all.

### Lifecycle

`rote start` spawns one detached (`setsid`, so it survives the pty that started
it) and waits for `/health` before `exec`ing claude — the only window in which a
failure is reportable, since nothing after the exec runs. Failure warns and
continues. `--no-launch` deliberately does not spawn. `[daemon] autostart =
false` turns it off; front ends then start one on demand.

`done` and `abort` reap **before** taking the manifest lock, which is before
`archive_and_clear` and the `shadow::sync` that resets the shadow. Reaping asks
over HTTP first, so the daemon can broadcast `closed` with the reason — the
reaper is the only thing that knows it, and it knows it before the archive
exists. It escalates to SIGTERM then SIGKILL, which is safe because every write
is an atomic rename and the kernel releases the flock. It refuses to signal a
pid whose endpoint does not name this project, **and one that is not holding the
engine token**: the endpoint file outlives its author, so after a `kill -9` and
enough pid churn the pid in it belongs to a stranger, and the project hash cannot
tell — it is true by construction. The flock can. An engine holds it unbroken for
its whole life and stamps it with its pid after acquiring, and the kernel drops it
on any death, so a held lock naming that pid is the only proof rote has.

Deliberately the flock rather than a `/health` reply. A wedged daemon holds the
token and answers nothing — the `Opaque` case above — and it must stay stoppable,
because `done` is about to `git clean -fdx` the shadow underneath it. Requiring a
reply would leave it running, which is a worse failure than the one being fixed.

The daemon lives until the session closes. The queue advancing while you type
with no pane open is most of what it is for.

### The HTTP surface

`127.0.0.1` only, ephemeral port, per-session bearer token; both in
`daemon.json`, mode 0600, in a state directory that is 0700.

`Authorization: Bearer <token>` on everything, compared in constant time.
`GET /events` and `GET /` also accept `?token=`, because neither an
`EventSource` nor a browser address bar can set a header. Exactly those two
paths: the page is *authorized*, not exempted, because an exempt route would be
the first unauthenticated one in a daemon whose whole doctrine is that `reject`
runs before routing — and it would make the port fingerprintable by any page on
the machine.
`Origin` and `Host`, when present, must be loopback — the second is the
DNS-rebinding defence, and it costs four lines now versus a CVE the week a
browser front end ships. **No CORS headers at all**: the webapp will be served
by this daemon, same-origin. POSTs must be `application/json` (a browser form
can only send three content types, none of them this one) and cap at 64 KiB.

| Method + path | Success | Failures |
|---|---|---|
| `GET /` | `200 text/html` (the browser app) | `401`, `403` |
| `GET /health` | `200` `Health` | `401`, `403` |
| `GET /state` | `200` `Snapshot` | `401`, `403`, `503 warming_up`, `503 hub_timeout` |
| `GET /hunk/<id>` | `200` `HunkDetail` | `401`, `403`, `404`, `503 no_session` |
| `GET /events` | `200 text/event-stream` | `401`, `403` |
| `POST /command` | `200` `Response` | `400`, `401`, `403`, `413`, `415`, `503 engine_timeout` |
| `POST /shutdown` | `202` | `400`, `401`, `403`, `415` |

`applied`, `stale` and `rejected` all return **200**. The outcome is the payload
of a successful conversation, and a client that reads only status codes must not
confuse a race with a transport failure.

**The generation check happens inside the `with_session` closure**, in the same
read-modify-write cycle as the mutation. Checking it in the handler and then
locking to write is a TOCTOU that would make the mechanism decorative. `stale`
writes nothing and does not move the generation, which is what makes retrying
exactly once safe. It applies to every verb that writes nothing as well as to
every verb that does — `show` with no id moves no state, but a client that sent a
stale generation with it still needs to be told so rather than told `applied`.

`refresh` is the one exception, and deliberately. It asserts nothing about the
queue's contents, so there is nothing for a generation to be stale against; and
since the pane attaches the generation it is looking at to every command, checking
it would refuse the client that has fallen behind the one verb that recovers from
that, at the moment it asks. It also republishes even when the recompute finds
nothing, because a client sending it is saying it thinks it is out of sync, and a
heartbeat does not answer that.

`GET /state` is the engine's **last published view**, not a fresh read: `drift`
is engine-only state, so a stateless handler would either omit it or shell out to
git per request, and could disagree with what every subscriber was just told.
`generation` is how a client knows how fresh it is.

### The event stream

`event:` mirrors the `type` tag, and a browser **must** use
`addEventListener("snapshot", …)`: because the daemon always writes an `event:`
line, `onmessage` — which handles only the default `message` type — never fires
at all. The pane gets away with ignoring the name because its parser is
hand-rolled. `data:` is the whole event, so a hand-rolled client
and parses is equally correct. The first frame after connecting is always the
cached snapshot — which is why there is **no `Last-Event-ID`, no resume and no
deltas**: a reconnecting client is never behind. A bare `:` comment every 15s of
silence proves the socket is alive, and is the only thing that makes a vanished
client detectable, since without a write there is nothing to fail.

**The response is written by hand through `Request::into_writer`, with an
explicit flush after every frame.** tiny_http's chunked path is `Encoder::new`
followed by `io::copy`: the encoder buffers 8 KiB, `io::copy` never flushes, and
for an infinite reader it never returns either. A small frame would sit invisible
until eight kilobytes had accumulated.

There is no unsubscribe message — a stream thread that exits drops its receiver
and the hub prunes on the next send. One mechanism, no bookkeeping that can leak,
and the consequence is that `Health.subscribers` is as of the last published
frame rather than as of now.

### Threads

Five kinds, all `std::thread` + `mpsc`, and **no `Arc`, `Mutex`, `RwLock` or
atomics anywhere**: every piece of state has one owning thread, every interaction
is a message, and every message that expects an answer has a deadline.

- **main** — the `tiny_http` accept loop, the lock guard, `daemon.json`.
- **engine** — owns the `Engine` (`Send`, not `Sync`) and its receiver.
- **hub** — a fan-out actor caching the last snapshot. Separate so `/state` and
  `/health` are answerable without touching the engine, and so an HTTP request
  can never block the thread classifying keystrokes.
- **watcher pump** — unchanged from Stage 1.
- **event streams** — one per client, blocking.

Shutdown needs no flag: everything that ends the session ends the hub loop.
`SIGTERM` gets no handler, because atomic renames and a kernel-released flock
make abrupt death safe by construction.

