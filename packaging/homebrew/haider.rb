class Haider < Formula
  desc "Provider-agnostic coding-agent TUI and runtime"
  homepage "https://github.com/Rizzist/haider-agent"
  license "LicenseRef-KOA-P-1.0"
  version "0.0.954"

  on_macos do
    if Hardware::CPU.arm?
      url "https://github.com/Rizzist/haider-agent/releases/download/v0.0.954/haider-v0.0.954-aarch64-apple-darwin.tar.xz"
      sha256 "0bc5705f1502ba291d5905fbf286869a448a3110b78c1563fe4eb699d04ea454"
    else
      url "https://github.com/Rizzist/haider-agent/releases/download/v0.0.954/haider-v0.0.954-x86_64-apple-darwin.tar.xz"
      sha256 "b2580c28d575e041ee08d53a0cf321eed292d9bfe293a9c592be5c38c85df6a6"
    end
  end

  on_linux do
    if Hardware::CPU.arm?
      url "https://github.com/Rizzist/haider-agent/releases/download/v0.0.954/haider-v0.0.954-aarch64-unknown-linux-gnu.tar.xz"
      sha256 "e6e9822d5895e6ae1d2f51e9d70ece107a863cdd429c7277fbe5e29b7f5fff4e"
    else
      url "https://github.com/Rizzist/haider-agent/releases/download/v0.0.954/haider-v0.0.954-x86_64-unknown-linux-gnu.tar.xz"
      sha256 "d1177621d8cc4b0421e636e51b4c69db7924962618275b10928641d726f8171d"
    end
  end

  def install
    bundle = Dir["haider-v#{version}-*"].first
    source = bundle || "."
    bin.install "#{source}/haider", "#{source}/haiderd"
    portal = "#{source}/haider-wayland-portal"
    bin.install portal if OS.linux? && File.exist?(portal)
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/haider --version")
  end
end
