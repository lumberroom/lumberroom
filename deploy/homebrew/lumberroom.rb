# The published copy of this file lives at Formula/lumberroom.rb in the tap
# the-cybersapien/homebrew-lumberroom, and that copy is what brew installs. This one is the staging
# copy: prepare a version here alongside the release it targets, then copy it across. brew resolves
# formula lookups by path inside a tap, not by where the source happens to live before that.
class Lumberroom < Formula
  desc "CLI client for lumberroom, a personal memory control plane"
  homepage "https://lumberroom.cloud"
  license "Apache-2.0"

  on_macos do
    on_arm do
      url "https://github.com/lumberroom/lumberroom/releases/download/v0.4.0/lumberroom-0.4.0-aarch64-apple-darwin.tar.gz"
      sha256 "240f95d8cf49426db97d3af25c409fe460b0c7557028060600082062fa74d9f5"
    end
    on_intel do
      url "https://github.com/lumberroom/lumberroom/releases/download/v0.4.0/lumberroom-0.4.0-x86_64-apple-darwin.tar.gz"
      sha256 "6bcff41a9ec1ddb0a1ae202b45828cbff9ac81c068c50d33db2db5de625fdff4"
    end
  end

  on_linux do
    on_arm do
      url "https://github.com/lumberroom/lumberroom/releases/download/v0.4.0/lumberroom-0.4.0-aarch64-unknown-linux-musl.tar.gz"
      sha256 "c18668c25161639182ab3e5f25e11d7bcf5cb5b91964c4b70e12622668e24800"
    end
    on_intel do
      url "https://github.com/lumberroom/lumberroom/releases/download/v0.4.0/lumberroom-0.4.0-x86_64-unknown-linux-musl.tar.gz"
      sha256 "21453b74c966ec779e8368aa4f82a519ef474564b0796018df0b61cfad0ad4bd"
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
