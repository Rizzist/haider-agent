class Haider < Formula
  desc "Provider-agnostic coding-agent TUI and runtime"
  homepage "https://github.com/Rizzist/haider-agent"
  license "LicenseRef-KOA-P-1.0"
  version "0.0.956"

  on_macos do
    if Hardware::CPU.arm?
      url "https://github.com/Rizzist/haider-agent/releases/download/v0.0.956/haider-v0.0.956-aarch64-apple-darwin.tar.xz"
      sha256 "f9cfecb74881b37291f3475ec4aadd12253c3069edcb8dc6e04ff196a3072402"
    else
      url "https://github.com/Rizzist/haider-agent/releases/download/v0.0.956/haider-v0.0.956-x86_64-apple-darwin.tar.xz"
      sha256 "7276f9456bef543d1d29ae9446a6fd9e9900ff22d85aedb1d30bd2341e07de58"
    end
  end

  on_linux do
    if Hardware::CPU.arm?
      url "https://github.com/Rizzist/haider-agent/releases/download/v0.0.956/haider-v0.0.956-aarch64-unknown-linux-gnu.tar.xz"
      sha256 "4ca1734030f6d94ac525bf7ce770240db32628e23b3ae64e1ee1e92952a758f3"
    else
      url "https://github.com/Rizzist/haider-agent/releases/download/v0.0.956/haider-v0.0.956-x86_64-unknown-linux-gnu.tar.xz"
      sha256 "d04cc3f91fab38b5961c0b60e04b1b8135b9d5614a351407b6cdc705384f8658"
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
