# rote development tasks.
#
# Note the hyphenated `cargo-clippy` / `cargo-fmt`: a Homebrew Rust ships those
# binaries directly but has no rustup shim, so `cargo clippy` does not resolve.
# `rote init` detects the same thing when it writes a project's [checks].

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

lint:
    cargo-clippy --all-targets -- -D warnings

fmt-check:
    cargo-fmt --check

fmt:
    cargo-fmt

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

# Install the local formula through Homebrew.
brew-install:
    brew install --formula ./Formula/rote.rb

# Syntax-check the formula. Deliberately NOT `brew audit`.
#
# `brew audit` is a developer command: it silently enables Homebrew developer
# mode and installs a set of dev gems into Homebrew's vendored bundle. On this
# machine that pulled in a json gem that conflicts with the one portable-ruby
# already provides, which broke `brew info` until the gem was removed again.
# Not worth it to lint one formula. If you do run it, expect to clean up after.
brew-check:
    ruby -c Formula/rote.rb
