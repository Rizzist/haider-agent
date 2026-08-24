class Haider < Formula
  desc "Provider-agnostic coding-agent TUI and runtime"
  homepage "https://github.com/Rizzist/haider-agent"
  license "LicenseRef-KOA-P-1.0"
  version "0.0.951"

  on_macos do
    if Hardware::CPU.arm?
      url "https://github.com/Rizzist/haider-agent/releases/download/v0.0.951/haider-v0.0.951-aarch64-apple-darwin.tar.xz"
      sha256 "78970edc6f7531d31ba6074e14b2412a1fab10ec92544813f6d2a51e89c5dade"
    else
      url "https://github.com/Rizzist/haider-agent/releases/download/v0.0.951/haider-v0.0.951-x86_64-apple-darwin.tar.xz"
      sha256 "bc2cd6bb5c6190f3682d14aeaf60e3664680e29965b6556fa1e7aa3f7316386f"
    end
  end

  on_linux do
    if Hardware::CPU.arm?
      url "https://github.com/Rizzist/haider-agent/releases/download/v0.0.951/haider-v0.0.951-aarch64-unknown-linux-gnu.tar.xz"
      sha256 "71b8fd5c5aab23284127cb02aa51d1f0e6e4c46ca9aefb6201e6f75eb6884ba5"
    else
      url "https://github.com/Rizzist/haider-agent/releases/download/v0.0.951/haider-v0.0.951-x86_64-unknown-linux-gnu.tar.xz"
      sha256 "0f8a45b04e89bf9bfc6bd988d7181e78ade368d184ff19c12c2bd1e6cdb30c19"
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
