class Haider < Formula
  desc "Provider-agnostic coding-agent TUI and runtime"
  homepage "https://github.com/Rizzist/haider-agent"
  license "LicenseRef-KOA-P-1.0"
  version "0.0.959"

  on_macos do
    if Hardware::CPU.arm?
      url "https://github.com/Rizzist/haider-agent/releases/download/v0.0.959/haider-v0.0.959-aarch64-apple-darwin.tar.xz"
      sha256 "7ecf20a6502a3a4020ffff9e164feb6a7df659f22e26f2d98eb5e6b1138e7baa"
    else
      url "https://github.com/Rizzist/haider-agent/releases/download/v0.0.959/haider-v0.0.959-x86_64-apple-darwin.tar.xz"
      sha256 "194021c5865ffe1fcb010eb1c8ac51fdc678d33faaee5d91f6f3dc501fdf3e4d"
    end
  end

  on_linux do
    if Hardware::CPU.arm?
      url "https://github.com/Rizzist/haider-agent/releases/download/v0.0.959/haider-v0.0.959-aarch64-unknown-linux-gnu.tar.xz"
      sha256 "6ec4c4296f97ce30b87135f82ac5583dc642ef1fb26c1272fe7ada7d37705467"
    else
      url "https://github.com/Rizzist/haider-agent/releases/download/v0.0.959/haider-v0.0.959-x86_64-unknown-linux-gnu.tar.xz"
      sha256 "52910ba229039dbd0e48fe20b9e6721c5b771a7abf7f8eac5e7e78ed6eff2d4a"
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
