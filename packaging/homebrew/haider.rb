class Haider < Formula
  desc "Provider-agnostic coding-agent TUI and runtime"
  homepage "https://github.com/Rizzist/haider-agent"
  license "LicenseRef-KOA-P-1.0"
  version "0.0.946"

  on_macos do
    if Hardware::CPU.arm?
      url "https://github.com/Rizzist/haider-agent/releases/download/v0.0.946/haider-v0.0.946-aarch64-apple-darwin.tar.xz"
      sha256 "53dd2bc44b011216ff2df4eb706349d79b9694e3ee222fca71f04a221a630a97"
    else
      url "https://github.com/Rizzist/haider-agent/releases/download/v0.0.946/haider-v0.0.946-x86_64-apple-darwin.tar.xz"
      sha256 "8e7630d53a7f5cab9102f535fe33fe27228dcb4ba40fde15d61f8c8260fd70a4"
    end
  end

  on_linux do
    if Hardware::CPU.arm?
      url "https://github.com/Rizzist/haider-agent/releases/download/v0.0.946/haider-v0.0.946-aarch64-unknown-linux-gnu.tar.xz"
      sha256 "8c0d2c7b2daa36e9e96f504f2f9e532237366fb705a7b8ca37b3490e0929b2bc"
    else
      url "https://github.com/Rizzist/haider-agent/releases/download/v0.0.946/haider-v0.0.946-x86_64-unknown-linux-gnu.tar.xz"
      sha256 "fb2152a3cc01dfdf9a45297ed091364152c041c3388c4f3ae0950f5c85657d63"
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
