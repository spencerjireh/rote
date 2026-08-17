# rote development tasks.
#
# Two spellings of the same tools, and no single one that works everywhere: a
# Homebrew Rust ships `cargo-clippy` and `cargo-fmt` as binaries with no rustup
# shim, so `cargo clippy` does not resolve there; a rustup machine has only the
# subcommand and no hyphenated binary. `lint` and `fmt-check` below pick at run
# time, which is what lets .rote.toml and CI both point here and be right.
# `rote init` does the same detection when it writes a project's [checks].

owner := "spencerjireh"
repo  := "rote"

# List available tasks.
default:
    @just --list

# Build and install into ~/.cargo/bin.
install:
    cargo install --path .

# The full gate: exactly what CI would run, and what .rote.toml checks.
gate: test lint fmt-check

test:
    cargo test

# Clippy, by whichever spelling this machine has.
#
# CI is the authority for this one, not your terminal. A Homebrew clippy and a
# rustup clippy report the same version and enforce different lint sets: this
# machine's 0.1.97 does not fire `field_reassign_with_default` or
# `items_after_test_module`, and the 0.1.97 on a runner does. CI caught eight of
# those on its first run, in code that had been "clippy clean" locally for
# months. There is no fix short of installing rustup alongside the Homebrew
# Rust, which is not worth it — so treat a local pass here as necessary and not
# sufficient, and let CI settle it.
lint:
    @just _cargo clippy --all-targets -- -D warnings

fmt-check:
    @just _cargo fmt --check

fmt:
    @just _cargo fmt

# Run a cargo subcommand by whichever spelling this machine has.
#
# Resolved rather than attempted: running `cargo clippy` and falling back on
# failure would treat a genuine lint failure as a missing shim and then run the
# whole thing twice.
_cargo subcommand *args:
    #!/usr/bin/env sh
    set -eu
    if cargo "{{subcommand}}" --version > /dev/null 2>&1; then
        exec cargo "{{subcommand}}" {{args}}
    elif command -v "cargo-{{subcommand}}" > /dev/null 2>&1; then
        exec "cargo-{{subcommand}}" {{args}}
    else
        echo "neither \`cargo {{subcommand}}\` nor \`cargo-{{subcommand}}\` is available." >&2
        echo "install it: rustup component add {{subcommand}}" >&2
        exit 1
    fi

# Compute the sha256 for the tagged release tarball, for Formula/rote.rb.
#
# Requires the tag to exist on the remote. Usage:
#     just formula-sha            # defaults to v0.1.0
#     just formula-sha v0.2.0
formula-sha version="v0.1.0":
    #!/usr/bin/env bash
    set -euo pipefail
    url="https://github.com/{{owner}}/{{repo}}/archive/refs/tags/{{version}}.tar.gz"
    echo "fetching $url" >&2
    sha=$(curl -fsSL "$url" | shasum -a 256 | cut -d' ' -f1)
    echo "$sha"
    echo >&2
    echo "paste into Formula/rote.rb:" >&2
    echo "  sha256 \"$sha\"" >&2

# Tap this repo and install through Homebrew.
#
# Homebrew 6 rejects `brew install --formula ./path.rb`: formulae must come
# from a tap. Tapping by URL avoids needing a separate homebrew-rote repo.
brew-tap:
    brew tap {{owner}}/{{repo}} https://github.com/{{owner}}/{{repo}}

brew-install: brew-tap
    brew install {{owner}}/{{repo}}/{{repo}}

# Pick up a formula change after pushing it.
brew-reinstall:
    brew update --quiet
    brew reinstall {{owner}}/{{repo}}/{{repo}}

# Syntax-check the formula. Deliberately NOT `brew audit`.
#
# `brew audit` is a developer command: it silently enables Homebrew developer
# mode and installs a set of dev gems into Homebrew's vendored bundle. On this
# machine that pulled in a json gem that conflicts with the one portable-ruby
# already provides, which broke `brew info` until the gem was removed again.
# Not worth it to lint one formula. If you do run it, expect to clean up after.
brew-check:
    ruby -c Formula/rote.rb

# Lint the nvim plugin, if the tools are installed.
#
# Deliberately not part of `gate`, and never in .rote.toml's [checks] — those
# run at `rote done` on other people's machines, where stylua is not a
# reasonable thing to require.
lua-lint:
    #!/usr/bin/env sh
    if command -v stylua > /dev/null; then stylua --check lua plugin; \
    else echo "stylua not installed — skipping"; fi
    if command -v luacheck > /dev/null; then luacheck lua plugin; \
    else echo "luacheck not installed — skipping"; fi
