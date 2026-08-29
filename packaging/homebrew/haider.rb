class Haider < Formula
  desc "Provider-agnostic coding-agent TUI and runtime"
  homepage "https://github.com/Rizzist/haider-agent"
  license "LicenseRef-KOA-P-1.0"
  version "0.0.964"

  on_macos do
    if Hardware::CPU.arm?
      url "https://github.com/Rizzist/haider-agent/releases/download/v0.0.964/haider-v0.0.964-aarch64-apple-darwin.tar.xz"
      sha256 "47125b4d3e7dd5169f4e2d7b4e5a3a34b8bc7e84b4bbc8135211dc99c38665d9"
    else
      url "https://github.com/Rizzist/haider-agent/releases/download/v0.0.964/haider-v0.0.964-x86_64-apple-darwin.tar.xz"
      sha256 "a9e5262a1bb5cb7802ccfdecd30290dc803eab6d04d86c2e0df90ca2cba132f1"
    end
  end

  on_linux do
    if Hardware::CPU.arm?
      url "https://github.com/Rizzist/haider-agent/releases/download/v0.0.964/haider-v0.0.964-aarch64-unknown-linux-gnu.tar.xz"
      sha256 "d618db44d7bcf7df014569ce7a8552c6402b4d83e5233538018eb100f343a531"
    else
      url "https://github.com/Rizzist/haider-agent/releases/download/v0.0.964/haider-v0.0.964-x86_64-unknown-linux-gnu.tar.xz"
      sha256 "dba3814352dd8806b9f19c84361d557db1b74040f11a7dbdcdf9c307cbe3ea95"
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
