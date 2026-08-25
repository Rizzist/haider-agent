class Haider < Formula
  desc "Provider-agnostic coding-agent TUI and runtime"
  homepage "https://github.com/Rizzist/haider-agent"
  license "LicenseRef-KOA-P-1.0"
  version "0.0.958"

  on_macos do
    if Hardware::CPU.arm?
      url "https://github.com/Rizzist/haider-agent/releases/download/v0.0.958/haider-v0.0.958-aarch64-apple-darwin.tar.xz"
      sha256 "ee9440e828b7e7ba0758aecdbcab9e2448615c6c8fc83c42153334bf5592ac77"
    else
      url "https://github.com/Rizzist/haider-agent/releases/download/v0.0.958/haider-v0.0.958-x86_64-apple-darwin.tar.xz"
      sha256 "3c17345599ed1b30420f7897c3e5f324d08882c35e534d943654e1b19084f7a1"
    end
  end

  on_linux do
    if Hardware::CPU.arm?
      url "https://github.com/Rizzist/haider-agent/releases/download/v0.0.958/haider-v0.0.958-aarch64-unknown-linux-gnu.tar.xz"
      sha256 "82238f95a543450410931818a23b572944bc2191331cd0d81efb8bebeb70ed25"
    else
      url "https://github.com/Rizzist/haider-agent/releases/download/v0.0.958/haider-v0.0.958-x86_64-unknown-linux-gnu.tar.xz"
      sha256 "61c0c1bebfe8fe6f28aa61f1f51f5aa2f80c5305d4a7e453d8cf93837285c978"
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
