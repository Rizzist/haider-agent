class Haider < Formula
  desc "Provider-agnostic coding-agent TUI and runtime"
  homepage "https://github.com/Rizzist/haider-agent"
  license "LicenseRef-KOA-P-1.0"
  version "0.0.955"

  on_macos do
    if Hardware::CPU.arm?
      url "https://github.com/Rizzist/haider-agent/releases/download/v0.0.955/haider-v0.0.955-aarch64-apple-darwin.tar.xz"
      sha256 "c788c51c801ee53cfad766f5e31456de1bc62300c3d09c109f56483c016975df"
    else
      url "https://github.com/Rizzist/haider-agent/releases/download/v0.0.955/haider-v0.0.955-x86_64-apple-darwin.tar.xz"
      sha256 "e09ca306c85ed78ffedef32d12e91ccc5c8c9e28d7820fdf8b5bf39a89722d7e"
    end
  end

  on_linux do
    if Hardware::CPU.arm?
      url "https://github.com/Rizzist/haider-agent/releases/download/v0.0.955/haider-v0.0.955-aarch64-unknown-linux-gnu.tar.xz"
      sha256 "6a4aa652bec1059b563191ed40f95f59217814f88afc1821dfd11c93359aa0f2"
    else
      url "https://github.com/Rizzist/haider-agent/releases/download/v0.0.955/haider-v0.0.955-x86_64-unknown-linux-gnu.tar.xz"
      sha256 "296a2dfd43cad4e426ceef36b63fc903d7100236ed15253503ce3a8e1ffa4407"
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
