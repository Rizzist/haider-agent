class Haider < Formula
  desc "Provider-agnostic coding-agent TUI and runtime"
  homepage "https://github.com/Rizzist/haider-agent"
  license "LicenseRef-KOA-P-1.0"
  version "0.0.953"

  on_macos do
    if Hardware::CPU.arm?
      url "https://github.com/Rizzist/haider-agent/releases/download/v0.0.953/haider-v0.0.953-aarch64-apple-darwin.tar.xz"
      sha256 "7927fac2649336b0f37f5739e0faab4c991172b70b580b2e43fb0d86db0cfaa6"
    else
      url "https://github.com/Rizzist/haider-agent/releases/download/v0.0.953/haider-v0.0.953-x86_64-apple-darwin.tar.xz"
      sha256 "71cb74760d00409e98b3e1bde49ff8225bd42bfa586a79552e62913a6511ec21"
    end
  end

  on_linux do
    if Hardware::CPU.arm?
      url "https://github.com/Rizzist/haider-agent/releases/download/v0.0.953/haider-v0.0.953-aarch64-unknown-linux-gnu.tar.xz"
      sha256 "3a8d23719fd0cdfaf072b89072f94b4706f232ebeed0444d4ac4be15fb1942a2"
    else
      url "https://github.com/Rizzist/haider-agent/releases/download/v0.0.953/haider-v0.0.953-x86_64-unknown-linux-gnu.tar.xz"
      sha256 "917374ddd0c9f93f20ed70bc7750d3123f10890d1790ddbf476c936da250bc46"
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
