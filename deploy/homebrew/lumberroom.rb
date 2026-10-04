# The published copy of this file lives at Formula/lumberroom.rb in the tap
# lumberroom/homebrew-lumberroom, and that copy is what brew installs. This one is the staging
# copy: prepare a version here alongside the release it targets, then copy it across. brew resolves
# formula lookups by path inside a tap, not by where the source happens to live before that.
class Lumberroom < Formula
  desc "CLI client for lumberroom, a personal memory control plane"
  homepage "https://lumberroom.cloud"
  license "Apache-2.0"

  on_macos do
    on_arm do
      url "https://github.com/lumberroom/lumberroom/releases/download/v0.5.0/lumberroom-0.5.0-aarch64-apple-darwin.tar.gz"
      sha256 "04db75b9bf8ce25d44faf873868f3c351ac179ec647894d078d42f5da32dda42"
    end
    on_intel do
      url "https://github.com/lumberroom/lumberroom/releases/download/v0.5.0/lumberroom-0.5.0-x86_64-apple-darwin.tar.gz"
      sha256 "bcff7ef7b1210aaee12d8694ee9a4ae5ff8d9e654306b71de868c25cbb0d3b79"
    end
  end

  on_linux do
    on_arm do
      url "https://github.com/lumberroom/lumberroom/releases/download/v0.5.0/lumberroom-0.5.0-aarch64-unknown-linux-musl.tar.gz"
      sha256 "eb40a94a0347a4e6babf49572de1d128d6c47d12339b97a7394c17f833eb0c92"
    end
    on_intel do
      url "https://github.com/lumberroom/lumberroom/releases/download/v0.5.0/lumberroom-0.5.0-x86_64-unknown-linux-musl.tar.gz"
      sha256 "fc8e11245f98c50b9613065311fc1d3acebade35cd69a781e6b79af055d9edcf"
    end
  end

  def install
    bin.install "lumberroom"
    doc.install "README.md"
  end

  def caveats
    <<~EOS
      Point this binary at a running lumberroom server before using it:
        lumberroom doctor

      To wire up Claude Code on this machine (MCP server, SessionStart hook, CLAUDE.md rule),
      use the wiring script from the source repository:
        client/wire-mac.sh --url https://your-lumberroom-host
    EOS
  end

  test do
    # Both checks run offline. `version` is the one subcommand that answers without a server and
    # exits zero, which is why it exists as a command and not only as a flag. The unknown command
    # covers the other half: argument parsing and dispatch reaching a fixed message and exit 1.
    assert_match "lumberroom #{version}", shell_output("#{bin}/lumberroom version")

    output = shell_output("#{bin}/lumberroom not-a-real-command 2>&1", 1)
    assert_match "unknown command not-a-real-command", output
    assert_match "doctor", output
  end
end
