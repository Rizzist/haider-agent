class Haider < Formula
  desc "Provider-agnostic coding-agent TUI and runtime"
  homepage "https://github.com/Rizzist/haider-agent"
  license "LicenseRef-KOA-P-1.0"
  version "0.0.944"

  on_macos do
    if Hardware::CPU.arm?
      url "https://github.com/Rizzist/haider-agent/releases/download/v0.0.944/haider-v0.0.944-aarch64-apple-darwin.tar.xz"
      sha256 "ecd20c6e2e8dd5be775e636c16e1e11aaea41e221999cb2030833c631ad85482"
    else
      url "https://github.com/Rizzist/haider-agent/releases/download/v0.0.944/haider-v0.0.944-x86_64-apple-darwin.tar.xz"
      sha256 "796635911b60395bad38c243089b0157cd75465442602c03f54aa22d345e5d04"
    end
  end

  on_linux do
    if Hardware::CPU.arm?
      url "https://github.com/Rizzist/haider-agent/releases/download/v0.0.944/haider-v0.0.944-aarch64-unknown-linux-gnu.tar.xz"
      sha256 "064155f942b028a0b41c62d79050bf68981f0e1b60e8e176a848817d492c3a3f"
    else
      url "https://github.com/Rizzist/haider-agent/releases/download/v0.0.944/haider-v0.0.944-x86_64-unknown-linux-gnu.tar.xz"
      sha256 "379d2fe2b95549890fad888ad97799222ff442e1b1b021fbe597dc2aa31d928c"
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
