# Homebrew formula for rote.
#
# Builds from source rather than shipping a bottle: no release artifacts to
# maintain, no checksums to chase, and it works the moment the tag exists.
#
# Install locally, without publishing a tap:
#
#     brew install --formula ./Formula/rote.rb
#
# On a new release: tag it, push the tag, then `just formula-sha <tag>` and
# update both the url and the sha256 below. The hash covers GitHub's generated
# tarball for that tag, so it cannot be computed before the tag exists.
class Rote < Formula
  desc "Use Claude Code at full capability; type every line in yourself"
  homepage "https://github.com/spencerjireh/rote"
  url "https://github.com/spencerjireh/rote/archive/refs/tags/v0.1.0.tar.gz"
  sha256 "4a8a86efe1698ce79acb66cc8fcde075f7bb8149bd23c25d0931a957edf79fbe"
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
