class Haider < Formula
  desc "Provider-agnostic coding-agent TUI and runtime"
  homepage "https://github.com/Rizzist/haider-agent"
  license "LicenseRef-KOA-P-1.0"
  version "0.0.962"

  on_macos do
    if Hardware::CPU.arm?
      url "https://github.com/Rizzist/haider-agent/releases/download/v0.0.962/haider-v0.0.962-aarch64-apple-darwin.tar.xz"
      sha256 "5c99d272a29957ebee72fd02d174dc409996930247aaf6444362656c86a12e18"
    else
      url "https://github.com/Rizzist/haider-agent/releases/download/v0.0.962/haider-v0.0.962-x86_64-apple-darwin.tar.xz"
      sha256 "d63911a8d727dccdcc4a0606a925ca31e8d73d4cff7ee5b85208372e81c5b4ba"
    end
  end

  on_linux do
    if Hardware::CPU.arm?
      url "https://github.com/Rizzist/haider-agent/releases/download/v0.0.962/haider-v0.0.962-aarch64-unknown-linux-gnu.tar.xz"
      sha256 "1051fd3fdaabe5b3013fedcf5329f15650e846fd21924a5f2365daa1b67dbc92"
    else
      url "https://github.com/Rizzist/haider-agent/releases/download/v0.0.962/haider-v0.0.962-x86_64-unknown-linux-gnu.tar.xz"
      sha256 "9ebf2ae5266033d982486290b71c3d098751152eba1bfd6bd9f70f3e45cf9b6e"
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
