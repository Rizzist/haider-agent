class Haider < Formula
  desc "Provider-agnostic coding-agent TUI and runtime"
  homepage "https://github.com/Rizzist/haider-agent"
  license "LicenseRef-KOA-P-1.0"
  version "0.0.966"

  on_macos do
    if Hardware::CPU.arm?
      url "https://github.com/Rizzist/haider-agent/releases/download/v0.0.966/haider-v0.0.966-aarch64-apple-darwin.tar.xz"
      sha256 "69735f821cb4406f12baad5d2e10981182a260edf1e52d35081d1e964b30dd6e"
    else
      url "https://github.com/Rizzist/haider-agent/releases/download/v0.0.966/haider-v0.0.966-x86_64-apple-darwin.tar.xz"
      sha256 "f9344439adce5837e58e87cd2e7ae12ce3bd855c9482bddf9fa78a56b68f4277"
    end
  end

  on_linux do
    if Hardware::CPU.arm?
      url "https://github.com/Rizzist/haider-agent/releases/download/v0.0.966/haider-v0.0.966-aarch64-unknown-linux-gnu.tar.xz"
      sha256 "db128ea2eda39364b1fe28caf04c33e5787fb30f47f344084734db7aa6e9cbdc"
    else
      url "https://github.com/Rizzist/haider-agent/releases/download/v0.0.966/haider-v0.0.966-x86_64-unknown-linux-gnu.tar.xz"
      sha256 "53cb601611b02253f30993ea172c39f8f19ffa6ab230b462ef7b5f0418e15c04"
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
