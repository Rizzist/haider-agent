class Haider < Formula
  desc "Provider-agnostic coding-agent TUI and runtime"
  homepage "https://github.com/Rizzist/haider-agent"
  license "LicenseRef-KOA-P-1.0"
  version "0.0.935"

  on_macos do
    if Hardware::CPU.arm?
      url "https://github.com/Rizzist/haider-agent/releases/download/v0.0.935/haider-v0.0.935-aarch64-apple-darwin.tar.xz"
      sha256 "34a9e3b8f236ed75c2d43416982cc24180a51dc71ea902b14bfe73945d62a0d3"
    else
      url "https://github.com/Rizzist/haider-agent/releases/download/v0.0.935/haider-v0.0.935-x86_64-apple-darwin.tar.xz"
      sha256 "fb001ae86752fe606a56bc01732b4893fb6789e1ef75cbe4b3b039c9373408ed"
    end
  end

  on_linux do
    if Hardware::CPU.arm?
      url "https://github.com/Rizzist/haider-agent/releases/download/v0.0.935/haider-v0.0.935-aarch64-unknown-linux-gnu.tar.xz"
      sha256 "b73a76850ff4328e1a99464d8f6190a44f45047d91389b0f7de4f5edff219b6e"
    else
      url "https://github.com/Rizzist/haider-agent/releases/download/v0.0.935/haider-v0.0.935-x86_64-unknown-linux-gnu.tar.xz"
      sha256 "c4176aa30b1dd011868e3bb8ccc044e2f5f0dc8ede52a187085b5d95a09ba115"
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
