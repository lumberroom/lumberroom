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
      url "https://github.com/lumberroom/lumberroom/releases/download/v0.6.0/lumberroom-0.6.0-aarch64-apple-darwin.tar.gz"
      sha256 "2952a2e90f59a422dae4e1cfb16cc505894f629148fac59c5de01ef498afe1eb"
    end
    on_intel do
      url "https://github.com/lumberroom/lumberroom/releases/download/v0.6.0/lumberroom-0.6.0-x86_64-apple-darwin.tar.gz"
      sha256 "143af7e0e42847177e58d5b34dbc7f34e14823b9402180b25cb06db94c575299"
    end
  end

  on_linux do
    on_arm do
      url "https://github.com/lumberroom/lumberroom/releases/download/v0.6.0/lumberroom-0.6.0-aarch64-unknown-linux-musl.tar.gz"
      sha256 "fe54c36d21404b2b1b2f2c92cffaff1878d407caa131207e822cef15ce73b8c4"
    end
    on_intel do
      url "https://github.com/lumberroom/lumberroom/releases/download/v0.6.0/lumberroom-0.6.0-x86_64-unknown-linux-musl.tar.gz"
      sha256 "58b65ca3d8bcde49e70373a3d04ac84b5d2b26c8586c49e2e5ebe4daba5b873b"
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
