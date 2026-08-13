# rote — Architecture

## What this is

`rote` is a Rust CLI that lets a developer use Claude Code at full capability while guaranteeing that every line of code entering the real repository is typed in by hand. It implements the "manually retype LLM-generated code" workflow (per Ankur Sethi's essay on cognitive debt) without degrading the agent.

Core idea: **the shadow workspace.** Claude Code runs inside a sandbox twin of the repository and works completely normally — editing files, installing dependencies, running tests. It is never told anything unusual is happening, because from its perspective nothing is. The user's real working tree is never touched by the agent. The deliverable of an agent session is the *diff* between shadow and real, which `rote` serves to the user hunk by hunk as a transcription queue anchored in their editor (nvim).

## Principles (non-negotiable)

1. **Agent unawareness.** No hooks, no restricted tools, no special instructions injected into the Claude Code session. Claude must behave exactly as it would in a normal repo. `rote` has zero coupling to Claude Code internals; it only sets the working directory Claude is launched in.
2. **Real tree is sacred.** No code path in `rote` ever writes source files into the real repository. The only writes to the real tree are the user's keystrokes in their editor. (`rote` may write its own per-project config `.rote.toml` if the user runs `rote init`, and nothing else.)
3. **Git is plumbing, never surface.** Git provides snapshots, diffs, and file enumeration. The user never sees or types a git command through this tool. All git usage is shelled out and hidden. `rote`'s own state lives outside git.
4. **The interface is the product.** Sessions, hunks, progress, divergence tracking — all first-class concepts owned by `rote`'s own state, so the plumbing (git) or the frontend (terminal today, an nvim ghost-text plugin later) can be swapped without changing the workflow contract.

## Tech stack

| Concern | Choice |
|---|---|
| Language | Rust (edition 2021, stable toolchain) |
| CLI | `clap` (derive API) |
| Errors | `anyhow` (application), `thiserror` for library-ish modules if useful |
| State serialization | `serde` + `serde_json` |
| Config | `toml` crate |
| Diffing | Shell out to system `git` via `std::process::Command`. No libgit2 / gix. |
| Unified diff parsing | Hand-rolled parser in `diffparse.rs` (the format is small and stable). A crate may be substituted if it demonstrably covers all edge cases in the spec. |
| Hashing | `sha2` — project identity, content-addressed hunk IDs, baseline digest |
| Terminal color | `anstyle`/`owo-colors` or plain ANSI codes — keep it light |
| Paths | `directories` crate for XDG dirs |

Rationale for Rust: the tool is subprocess orchestration + state bookkeeping; distribution as a single static binary matters for a daily-driver PATH tool; the strict compiler acts as a reviewer for agent-generated code.

## Filesystem layout

Given a real repo at `/home/user/proj` (project identity = hash of canonicalized repo root path):

```
~/.cache/rote/<hash>/shadow/          # the shadow repo (a full local clone)
~/.local/share/rote/<hash>/
    session.json                       # active session manifest
    session.json.lock                  # PID lock, held only across manifest writes
    baseline.patch                     # real tree's uncommitted diff at last sync
    archive/<timestamp>.json           # completed/aborted sessions
    archive/<timestamp>.patch          # the agent's work that was never typed
~/.config/rote/config.toml            # global config (editor cmd, defaults)
<repo>/.rote.toml                     # per-project config (check commands, env allowlist, hunk size)
```

The shadow lives entirely outside the real repo. Nothing rote-related is ever created inside the real repo except the optional `.rote.toml`.

## Shadow mechanism: full local clone, not a worktree

The shadow is a `git clone` (local, `--no-hardlinks`) of the real repo — **not** a `git worktree`. Rationale: worktrees share `.git` state with the main repo, so an agent freely running git commands (branch deletion, gc, config changes, hooks) could corrupt or mutate the real repository's metadata. A separate clone gives hard isolation: the worst the agent can do is destroy the shadow, which is disposable by design.

Sync (real → shadow), performed at session start and at `done`:

1. In shadow: fetch from the real repo path, hard reset to the real repo's current `HEAD` commit.
2. Clean the shadow of untracked and ignored files, except a configured `preserve` list of build-output directories (`target/`, `node_modules/`, …). The shadow is where the agent installs dependencies and runs tests; wiping those on every sync would buy source purity with a cold rebuild at the top of every session.
3. Apply the real repo's uncommitted changes (staged + unstaged) to the shadow working tree (`git diff HEAD` in real, `git apply` in shadow), and save those diff bytes as the session baseline patch for the reviewer.
4. Copy untracked-but-not-ignored files from real to shadow.
5. Copy ignored-but-required files listed in `.rote.toml` (e.g. `.env`) so the agent's builds work.

After sync, shadow working tree ≡ real working tree, byte for byte, across all source (modulo ignored files not on the allowlist, and the preserved build directories). DESIGN.md §3 is the authoritative step-by-step.

## Components

```
┌─────────────────────────────────────────────────────────┐
│ CLI layer (clap)                                        │
│ init | start | status | next | back | skip | talk |     │
│ done | abort                                            │
└───────┬────────────────┬───────────────┬────────────────┘
        │                │               │
┌───────▼──────┐ ┌───────▼───────┐ ┌─────▼──────────┐
│ ShadowManager│ │ SessionEngine │ │ HunkPresenter  │
│ create/sync/ │ │ state machine │ │ render, editor │
│ teardown     │ │ manifest,     │ │ launch,        │
│              │ │ diff/recompute│ │ divergence     │
└───────┬──────┘ └───────┬───────┘ └─────┬──────────┘
        │                │               │
┌───────▼────────────────▼───────────────▼────────────────┐
│ GitPlumbing (subprocess wrapper) + DiffParser           │
└─────────────────────────────────────────────────────────┘
```

- **ShadowManager** — owns the clone lifecycle and the sync algorithm above. Refuses to operate if the real repo is mid-merge/rebase/cherry-pick.
- **SessionEngine** — the state machine (`idle → working → transcribing → idle`; `done`/`aborted` are labels on archived sessions, not live states) and the manifest. Its key operation is **recompute**: re-diff shadow vs. real and reconcile the hunk queue against hunks already typed/diverged/skipped, which is what allows the user to flip back to the Claude session mid-transcription, ask for rework, and continue. Terminal statuses are sticky by hunk ID (DESIGN.md §5) — the trees are the source of truth about content, the manifest about the user's decisions.
- **HunkPresenter** — renders one hunk to the terminal, launches the editor at a context-derived anchor, and on editor exit classifies the outcome (typed exactly / diverged / untouched). Divergence policy: **prompt every time** — show a mini-diff of user version vs. proposal and ask keep / retry. Files that cannot meaningfully be typed (binaries, lockfiles) skip the editor entirely and are classified by byte comparison against the shadow.
- **GitPlumbing + DiffParser** — thin `Command` wrappers over the ~8 git invocations rote needs, and a unified-diff parser producing structured hunks.

## Data flow (one session)

```
rote start "add tagging"
  → ShadowManager.sync(real → shadow)
  → SessionEngine: idle → working (manifest written)
  → exec claude (cwd = shadow dir, current pane taken over)   [user argues/steers]

user exits claude (or flips to a second pane)

rote next
  → SessionEngine.recompute(): enumerate files, per-file git diff --no-index
    real/<f> shadow/<f>, parse, split into ≤ max_hunk_lines units, reconcile
    with manifest
  → HunkPresenter: print hunk, spawn editor +<anchor_line> <real_file>
  → on exit: re-read region, classify, prompt on divergence, update manifest
  (repeat until queue empty)

rote done
  → run check commands from .rote.toml in the REAL tree
  → headless review: pipe session diff + divergence report into
    `claude -p <review prompt>` (text in, text out; tool-restricted, so
    genuinely no filesystem access)
  → show findings, ask user to confirm
  → archive residue patch + manifest    # the agent's untyped work, recoverable
  → ShadowManager.sync(real → shadow)   # user's typed version becomes baseline
  → state → idle
```

## The two Claudes

1. **Session Claude** — interactive, launched by `rote start` inside the shadow. Fully capable, fully unaware. rote never communicates with it.
2. **Reviewer Claude** — headless `claude -p`, invoked by `rote done`. Receives only text on stdin (the session diff against the pre-session baseline, plus the divergence report). It never gets filesystem access — enforced by tool-restriction flags on the invocation, not merely by the empty temp cwd it runs in — and never runs inside either tree. Its output is advisory; the user confirms before the session closes. This is the one place rote touches Claude Code's flag surface; Principle 1 governs the session agent, which stays entirely unconfigured.

These are independent invocations; nothing links them.

## Explicit non-goals (v0)

- Multiple concurrent sessions per project
- Non-git projects
- The nvim ghost-text plugin (v2; it will consume the same hunk stream `next` produces — keep the hunk model serializable with that in mind)
- Paste prevention (honor system in v0)
- Windows support (Linux/macOS only)
- Any Claude Code hooks, MCP servers, or SDK usage

## v2 seam

The HunkPresenter's terminal rendering must be separated from hunk selection/classification logic so a future nvim plugin (ghost-text typing surface) can drive the same SessionEngine over a JSON-lines protocol (`rote next --json` is specced in DESIGN.md as a hidden flag from day one).
