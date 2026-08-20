class Haider < Formula
  desc "Provider-agnostic coding-agent TUI and runtime"
  homepage "https://github.com/Rizzist/haider-agent"
  license "LicenseRef-KOA-P-1.0"
  version "0.0.934"

  on_macos do
    if Hardware::CPU.arm?
      url "https://github.com/Rizzist/haider-agent/releases/download/v0.0.934/haider-v0.0.934-aarch64-apple-darwin.tar.xz"
      sha256 "f9ce7d1f8ef46e551b252f77cc76be28f2c40737d8b059584d6aaed54ef859db"
    else
      url "https://github.com/Rizzist/haider-agent/releases/download/v0.0.934/haider-v0.0.934-x86_64-apple-darwin.tar.xz"
      sha256 "7583e458a89de0c75fe82e64e1a8dbbc7bf08654e0165eda4adf9c544f289b77"
    end
  end

  on_linux do
    if Hardware::CPU.arm?
      url "https://github.com/Rizzist/haider-agent/releases/download/v0.0.934/haider-v0.0.934-aarch64-unknown-linux-gnu.tar.xz"
      sha256 "788ee3f5edd8399264892a30a60361f8edb3882cf8a5a8798e15ec548545410d"
    else
      url "https://github.com/Rizzist/haider-agent/releases/download/v0.0.934/haider-v0.0.934-x86_64-unknown-linux-gnu.tar.xz"
      sha256 "dcb2a8da0cf7c00c92e0f359c3ab4dac30e4d656115fd3c27ade99a8cf5eb13c"
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
