class Haider < Formula
  desc "Provider-agnostic coding-agent TUI and runtime"
  homepage "https://github.com/Rizzist/haider-agent"
  license "LicenseRef-KOA-P-1.0"
  version "0.0.941"

  on_macos do
    if Hardware::CPU.arm?
      url "https://github.com/Rizzist/haider-agent/releases/download/v0.0.941/haider-v0.0.941-aarch64-apple-darwin.tar.xz"
      sha256 "ebfa8b905467a5bfa50524e995e45e1a8debd09ca5479729c7723673183b7b81"
    else
      url "https://github.com/Rizzist/haider-agent/releases/download/v0.0.941/haider-v0.0.941-x86_64-apple-darwin.tar.xz"
      sha256 "01021b51e4f6bda5abfc1154e67d7242af6b72db73463bd0a77ab7ca96c25534"
    end
  end

  on_linux do
    if Hardware::CPU.arm?
      url "https://github.com/Rizzist/haider-agent/releases/download/v0.0.941/haider-v0.0.941-aarch64-unknown-linux-gnu.tar.xz"
      sha256 "7a8548d4ae4a5a42f0b82153ef8da8314b693b959d571428ad144444f3d04b64"
    else
      url "https://github.com/Rizzist/haider-agent/releases/download/v0.0.941/haider-v0.0.941-x86_64-unknown-linux-gnu.tar.xz"
      sha256 "9b730cf5c4817a2c5e20e0aec44fef564b8b203f036706a0aa054a38eed2d541"
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
