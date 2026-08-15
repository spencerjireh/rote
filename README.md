# rote

Use Claude Code at full capability, and type every line in yourself.

`rote` runs Claude Code inside a shadow clone of your repository. The agent works
normally: editing files, installing dependencies, running tests. It is never told
anything unusual is happening, because from its perspective nothing is. Your real
working tree is never touched by it. What the session produces is a *diff*, which
rote serves back to you hunk by hunk, in a pane beside your editor, for you to
type in by hand. It watches you type and keeps score itself.

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

`doctor` checks git, claude, the reviewer's tool-restriction flag, the global
config, and (when you are in a repo) the repository, the shadow location, and
the project config. It reports your editor too, but never fails over it: rote
does not launch one as part of the loop. Every line either passes or names the
command that fixes it, and it exits non-zero if anything needs attention, so it works as
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

`start` syncs the shadow, leaves a small daemon watching in the background, and
hands your pane to claude. Argue with it and run its tests, all inside the
shadow. When you have what you want, quit claude (or flip to a second pane) and
open the watcher:

```
rote watch     # the loop: shows a hunk, and watches you type it
```

That is the whole loop. There is no command between typing a line and the queue
moving: rote watches your tree, notices the save, checks what you typed against
the proposal, and advances to the next hunk on its own.

The watching is done by the background daemon, not by the pane, so it keeps
going whether or not a pane is open — and you can have as many panes as you
like, on the same queue. `rote done` and `rote abort` stop it. `[daemon]
autostart = false` in `.rote.toml` turns the automatic start off; `rote watch`
then starts one when you need it.

### The order you get them in

Sorting a diff by file path and line number gives you the order a filesystem
happens to be in, which is rarely the order a change makes sense in — you end up
typing a caller before the thing it calls, and a test for something you have not
written yet.

So once the agent's work settles, rote makes one headless call that puts the
queue in **teaching order** and writes a line above each hunk saying why it comes
where it does:

```
the Post.tags field everything below reads
── hunk 1/3 ── models.py:3 ── insert ───────────────────────
   class Post:
       title = ""
 +     tags = []
```

It runs in the background and takes a few seconds, so you will usually see file
order first and watch it rearrange. **The hunk you are already typing never
moves** — only the queue behind it. Ask the agent for more work and it re-orders
around what arrived, without paying to re-examine what it has already seen.

It is one call per session and it fails soft: if it cannot run, you get the plain
file order and one line saying so. `ROTE_CURATOR=off rote start` skips it
entirely, and `[curator] enabled = false` in either config file turns it off for
good — in `~/.config/rote/config.toml` if you would rather rote never spent
tokens on its own.

### Inside nvim

If you would rather not have a separate pane, the plugin in `lua/` is the same
front end without leaving the editor:

```lua
{ "spencerjireh/rote", config = function() require("rote").setup({}) end }
```

`:Rote` opens a panel showing the current hunk with its curator note. The queue
advances as you type and the panel follows, and the cursor moves to each new
hunk — but never while you are in insert mode, and never when it is already
inside the hunk you are typing. `s`, `k`, `r` and `g` do what they do in the
pane. `:help rote` has the rest.

The plugin also tells rote when content arrived by paste, which is the one thing
nothing outside your editor can see. That is the whole of what it adds over the
terminal pane, and it is why the count at `done` means anything.

Note the binary is installed separately — a plugin manager only takes the Lua.

### In a browser

```
rote watch --web
```

Prints a URL. The daemon serves the page itself, so it is same-origin and the
token it already uses covers it — there is no separate server and nothing to
install. It shows the same hunk the pane does, updating as you type, with `s`,
`k`, `r` and `g` on the keyboard.

It prints rather than opens, deliberately: the URL carries a bearer token, and
putting that into whichever browser happens to be default — into its history and
its session restore — is not something to do without being asked.

One asymmetry worth knowing: if the daemon restarts, the page cannot follow it.
A new daemon has a new port *and* a new token, and a page has no way to re-read
either. It will say so and ask you to re-run the command. The pane and the nvim
plugin both reconnect on their own.

### The three-pane workflow

The intended shape, in ghostty or any splittable terminal: claude on the left,
`rote watch` in the middle, your editor on the right.

```
┌───────────────────┬────────────────────┬───────────────────┐
│ rote start        │ rote watch         │ nvim              │
│ → claude, in the  │ → hunk 3/11        │                   │
│   shadow          │   src/posts.py:48  │ (you type)        │
│                   │   - tags = Mgr()   │                   │
│ (argue, iterate)  │   + tags = TagMgr()│                   │
└───────────────────┴────────────────────┴───────────────────┘
```

Nothing about that layout is enforced. The pane prints `file:line`, so most
terminals will take you there on a click, and `o` in the pane opens your editor
if you have one configured.

You can go back to the agent at any point: `rote talk --attach` resumes the
session, so you can ask for rework and carry on. The pane notices without being
told. Hunks you already typed stay typed; hunks the agent reworked are
re-offered as new ones.

## Commands

| Command | What it does |
|---|---|
| `rote init` | Write a commented `.rote.toml`. `--force` overwrites. |
| `rote start [TASK…]` | Sync the shadow, open a session, exec claude in it. |
| `rote watch` | The loop. Attaches to the background daemon; `--local` runs the engine in this process instead. |
| `rote status` | State, task, age, shadow path, hunk counts. |
| `rote next` | Print the hunk at the head of the queue. Display only. |
| `rote show [HUNK_ID]` | Re-print a hunk, defaulting to the last presented. Display only. |
| `rote skip [HUNK_ID]` | Leave a hunk untyped, defaulting to the active one. Durable: it will not come back. |
| `rote resolve <HUNK_ID> keep\|retry` | Answer an open divergence question. |
| `rote talk` | Print the shadow path; `--attach` resumes the agent there. |
| `rote done` | Run checks, review the session, close it. |
| `rote abort` | Discard the session. Your real tree is untouched. |
| `rote doctor` | Check this machine is set up. `--deep` proves the reviewer path. |
| `rote setup` | Write the global config. The only command that does. |

Global flags: `--project <path>`, `-q/--quiet`, `--no-color`.

In the pane: `s` skip, `o` open your editor, `g` refresh, `q` quit. When a
question is open, `k` keeps your version and `r` withdraws the question.

### What happens when you type something different

Half-typed lines are not disagreements. While what you have written is still on
its way to the proposal, rote says nothing, however long you pause — a prefix
can only be an unfinished agreement.

When you stop somewhere else, and the file has been still for a couple of
seconds, the pane asks:

```
your version differs from the proposal:
  proposal │     tags = Manager()
  yours    │     tags = TaggableManager()
[k]eep mine  [r]etry  [s]kip  [o]pen  [q]uit
```

`k` records both versions and moves on; the proposal will not be offered again,
this recompute or any later one. `r` withdraws the question and keeps watching,
so you can simply carry on typing. The queue does not block on an unanswered
question — it moves to the next hunk and leaves the question open.

Files you can't meaningfully type (binaries, lockfiles) are gated on a byte
comparison instead. rote shows the path and a note; run `cargo add` or
`npm install` yourself and it clears.

## Configuration

Global, `~/.config/rote/config.toml`:

```toml
editor = "nvim"                  # optional; what the `o` key opens
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

[watch]
divergence_grace_ms = 2000       # stillness before rote asks about a difference
debounce_ms = 400                # before re-diffing the trees
ignore = []                      # extra globs the watcher never wakes for

[daemon]
autostart = true                 # whether `rote start` leaves one watching
```

`[review]`, `[watch]` and `[daemon]` are project-only sections: the global config
takes flat keys only, so a table there is a parse error.

The daemon listens on 127.0.0.1 on a port the kernel picks, behind a token
generated per session. It is what the nvim plugin and the browser front end will
talk to; `rote watch` already does.

`$ROTE_EDITOR` overrides `editor`. `$EDITOR` is deliberately *not* consulted —
the open action uses a specific `+LINE` calling convention, and silently
inheriting a pager is a worse failure than an explicit setting.

## Where things live

```
~/.cache/rote/<hash>/shadow/          the shadow clone
~/.local/share/rote/<hash>/
    session.json                      the active session
    watch.lock                        held by whoever owns the queue
    daemon.json                       where the daemon listens (mode 0600)
    daemon.log                        its output, if you need to see why
    curator.json                      the teaching order and its notes
    baseline.patch                    the real tree at session start
    archive/<timestamp>.json          finished sessions
    archive/<timestamp>.patch         the agent's work you never typed
    archive/<timestamp>.curator.json  the order you transcribed it in
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
rote never writes source into it. If a session's own state is the problem rather
than the shadow, `rm -rf ~/.local/share/rote/<hash>` clears the manifest, the
daemon's address book and the teaching order together — after which the project
is simply idle.

## What it will not do

No hooks, no MCP servers, no SDK, no prompt injection. rote's entire coupling to
Claude Code is the working directory it launches the agent in, plus tool-
restriction flags on the two separate headless calls — the curator, and the
reviewer at `done`. The session agent is unconfigured and cannot tell it is being
shadowed.

Also out of scope for now: multiple concurrent sessions, non-git projects,
Windows, and telemetry of any kind. Paste *prevention* is not planned either —
rote records how a hunk arrived and says so when the session closes, but it will
never withhold text or refuse a paste.

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
