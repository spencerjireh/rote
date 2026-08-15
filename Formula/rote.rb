# Homebrew formula for rote.
#
# Builds from source rather than shipping a bottle: no release artifacts to
# maintain, no checksums to chase, and it works the moment the tag exists.
#
# Homebrew 6 refuses formulae given as a loose file path — they must live in a
# tap. Tapping this repo by URL works without a separate homebrew-rote repo:
#
#     brew tap spencerjireh/rote https://github.com/spencerjireh/rote
#     brew install spencerjireh/rote/rote
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
    # One binary, no runtime assets: the browser front end is compiled in with
    # include_str!, and the nvim plugin is installed by a plugin manager rather
    # than from here — see the caveats.
    system "cargo", "install", *std_cargo_args
  end

  def caveats
    <<~EOS
      rote needs the `claude` CLI to run a session:
        https://docs.claude.com/en/docs/claude-code

      Then, once:
        rote setup     # writes ~/.config/rote/config.toml
        rote doctor    # confirms this machine is ready

      Front ends, all speaking the same protocol:
        rote watch         a terminal pane
        rote watch --web   prints a URL; the daemon serves the page itself

      For nvim, point a plugin manager at the same repository — this formula
      installs only the binary:
        { "spencerjireh/rote", config = function() require("rote").setup({}) end }
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
