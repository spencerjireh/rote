# Homebrew formula for rote.
#
# Builds from source rather than shipping a bottle: no release artifacts to
# maintain, no checksums to chase, and it works the moment the tag exists.
#
# Install locally, without publishing a tap:
#
#     brew install --formula ./Formula/rote.rb
#
# BEFORE THIS WORKS: push the repo to github.com/spencerjireh/rote, tag v0.1.0,
# then run `just formula-sha` and paste the result over the placeholder below.
# The sha256 cannot be computed until the tag's tarball exists.
class Rote < Formula
  desc "Use Claude Code at full capability; type every line in yourself"
  homepage "https://github.com/spencerjireh/rote"
  url "https://github.com/spencerjireh/rote/archive/refs/tags/v0.1.0.tar.gz"
  # PLACEHOLDER — replace with the output of `just formula-sha`.
  sha256 "0000000000000000000000000000000000000000000000000000000000000000"
  license "MIT"
  head "https://github.com/spencerjireh/rote.git", branch: "main"

  depends_on "rust" => :build
  depends_on "git"

  def install
    system "cargo", "install", *std_cargo_args
  end

  def caveats
    <<~EOS
      rote needs the `claude` CLI to run a session:
        https://docs.claude.com/en/docs/claude-code

      Then, once:
        rote setup     # writes ~/.config/rote/config.toml
        rote doctor    # confirms this machine is ready
    EOS
  end

  test do
    # `rote doctor` runs outside a repository by design and exits non-zero when
    # something is missing, so assert on what it prints rather than its status.
    output = shell_output("#{bin}/rote doctor 2>&1", 1)
    assert_match "git", output

    assert_match version.to_s, shell_output("#{bin}/rote --version")

    # Outside a repo, commands that need one must say so clearly.
    assert_match "not inside a git repository", shell_output("#{bin}/rote status 2>&1", 1)
  end
end
