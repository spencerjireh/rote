# rote

Use Claude Code at full capability, and type every line in yourself.

`rote` runs Claude Code inside a shadow clone of your repository. The agent works
normally: editing files, installing dependencies, running tests. It is never told
anything unusual is happening, because from its perspective nothing is. Your real
working tree is never touched by it. What the session produces is a *diff*, which
rote serves back to you hunk by hunk, anchored in your editor, for you to type in
by hand.

The idea comes from Ankur Sethi's essay on cognitive debt: retyping generated code
is what keeps it in your head. The problem with doing that by hand is that it
usually means crippling the agent. This doesn't.

## Install

Via Homebrew:

```
brew tap spencerjireh/rote https://github.com/spencerjireh/rote
brew install spencerjireh/rote/rote
```

The tap points straight at this repository. Homebrew requires formulae to live in
a tap rather than a loose file, but it does not need a separate `homebrew-rote`
repo when you give it the URL.

Or from source:

```
cargo install --path .          # or: just install
```

Requires `git`, plus `claude` on PATH for real use. Linux and macOS only.

## First run

```
rote setup      # writes ~/.config/rote/config.toml
rote doctor     # confirms this machine is ready
```

`doctor` checks git, claude, the reviewer's tool-restriction flag, your editor,
the global config, and (when you are in a repo) the repository, the shadow
location, and the project config. Every line either passes or names the command
that fixes it, and it exits non-zero if anything needs attention, so it works as
a script gate. It also runs outside a repository, which is where you are right
after installing.

`rote doctor --deep` also makes one real `claude` call, which proves the reviewer
path works end to end. It is opt-in because that costs an API call, but it is the
only check that catches a flag that exists and behaves differently than expected.

## Quickstart

```
cd your-repo
rote init                       # writes .rote.toml, shaped to your project
rote start "add tagging to posts"
```

`rote init` detects what kind of project this is and prefills `[checks]` with
commands it has verified will run here, so `rote done` checks something from the
start instead of passing silently. It prints what it chose.

`start` syncs the shadow and hands your pane to claude. Argue with it and run its
tests, all inside the shadow. When you have what you want, quit claude (or flip
to a second pane) and start transcribing:

```
rote next      # shows one hunk, opens your editor on it
rote next      # ...and the next
rote status    # where you are
rote done      # checks, review, close
```

### The two-pane workflow

The intended shape, in ghostty or any splittable terminal: claude in the left
pane, rote in the right.

```
┌─────────────────────────┬─────────────────────────┐
│ rote start "add tags"   │ rote next               │
│ → claude, in the shadow │ → hunk 3/11, nvim opens │
│                         │                         │
│ (argue, iterate, test)  │ (type it in)            │
└─────────────────────────┴─────────────────────────┘
```

You can go back to the agent at any point: `rote talk --attach` resumes the
session, so you can ask for rework and carry on. The next `rote next` absorbs
whatever changed. Hunks you already typed stay typed; hunks the agent reworked
are re-offered as new ones.

## Commands

| Command | What it does |
|---|---|
| `rote init` | Write a commented `.rote.toml`. `--force` overwrites. |
| `rote start [TASK…]` | Sync the shadow, open a session, exec claude in it. |
| `rote status` | State, task, age, shadow path, hunk counts. |
| `rote next` | Present the next hunk and open your editor on it. |
| `rote back` | Re-print the last hunk. Display only — changes nothing. |
| `rote skip` | Leave a hunk untyped. Durable: it will not come back. |
| `rote talk` | Print the shadow path; `--attach` resumes the agent there. |
| `rote done` | Run checks, review the session, close it. |
| `rote abort` | Discard the session. Your real tree is untouched. |
| `rote doctor` | Check this machine is set up. `--deep` proves the reviewer path. |
| `rote setup` | Write the global config. The only command that does. |

Global flags: `--project <path>`, `-q/--quiet`, `--no-color`.

### What happens when you type something different

If what you type doesn't match the proposal, rote shows both and asks:

```
your version differs from the proposal:
  proposal │     tags = Manager()
  yours    │     tags = TaggableManager()
[k]eep mine   [r]etry (reopen editor)   [s]how full hunk again
```

`k` records both versions and moves on. The proposal will not be offered again,
this recompute or any later one. `r` reopens your
editor to try again.

Files you can't meaningfully type (binaries, lockfiles) are never opened in an
editor. rote shows the path and a note, then checks whether the file matches. Run
`cargo add` or `npm install` yourself and it clears.

## Configuration

Global, `~/.config/rote/config.toml`:

```toml
editor = "nvim"                  # invoked as: editor +LINE FILE
claude_cmd = ["claude"]
claude_continue_cmd = ["claude", "--continue"]
color = true
max_hunk_lines = 20
strict_whitespace = false
```

Per-project `.rote.toml` overrides those and adds:

```toml
[shadow]
copy = [".env"]        # gitignored files the agent needs, copied real -> shadow
preserve = ["target/"] # build output that survives the shadow's clean

[transcribe]
verbatim = ["Cargo.lock", "*.lock"]   # gated on bytes, never typed by hand

[checks]
commands = ["cargo test"]             # run in the REAL tree at `rote done`

[review]
enabled = true
model_args = []
```

`$ROTE_EDITOR` overrides `editor`. `$EDITOR` is deliberately *not* consulted —
transcription needs a specific `+LINE` calling convention, and silently
inheriting a pager is a worse failure than an explicit setting.

## Where things live

```
~/.cache/rote/<hash>/shadow/          the shadow clone
~/.local/share/rote/<hash>/
    session.json                      the active session
    baseline.patch                    the real tree at session start
    archive/<timestamp>.json          finished sessions
    archive/<timestamp>.patch         the agent's work you never typed
~/.config/rote/config.toml
<repo>/.rote.toml
```

XDG paths on both Linux and macOS, so the escape hatch below works as written
everywhere. rote refuses to run if those directories would land inside the
repository it is shadowing — which happens by default when the repository *is*
your home directory, as with a dotfiles repo. Point `XDG_CACHE_HOME` elsewhere
in that case.

### If you skip something and change your mind

Closing a session discards the agent's version of anything you skipped or
diverged from — but not before writing it to `archive/<timestamp>.patch`. That
patch is repo-relative and applies cleanly:

```
git apply ~/.local/share/rote/<hash>/archive/20260813T104500Z.patch
```

### Escape hatch

The shadow is disposable by design. If it ever gets into a state you don't like:

```
rm -rf ~/.cache/rote/<hash>
```

The next `rote start` rebuilds it. Nothing in your real repository is affected:
rote never writes source into it.

## What it will not do

No hooks, no MCP servers, no SDK, no prompt injection. rote's entire coupling to
Claude Code is the working directory it launches the agent in, plus tool-
restriction flags on the separate headless reviewer at `done`. The session agent
is unconfigured and cannot tell it is being shadowed.

Also out of scope for v0: multiple concurrent sessions, non-git projects, the
nvim ghost-text plugin (`rote next --json` is the seam it will use), paste
prevention (honor system), Windows, and telemetry of any kind.

## Development

```
just gate      # test + lint + fmt-check
```

Or individually:

```
cargo test
cargo-clippy --all-targets -- -D warnings
cargo-fmt --check
```

Note the hyphens: a Homebrew Rust ships `cargo-clippy` and `cargo-fmt` as
binaries but has no rustup shim, so `cargo clippy` does not resolve. `rote init`
detects the same thing when writing a project's checks.

Design documents, in reading order: `ARCHITECTURE.md` for the shape and the four
non-negotiable principles, `DESIGN.md` for the contracts and edge cases,
`BUILD_PLAN.md` for how it was sequenced.
