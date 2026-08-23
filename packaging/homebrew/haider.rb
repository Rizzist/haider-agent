class Haider < Formula
  desc "Provider-agnostic coding-agent TUI and runtime"
  homepage "https://github.com/Rizzist/haider-agent"
  license "LicenseRef-KOA-P-1.0"
  version "0.0.949"

  on_macos do
    if Hardware::CPU.arm?
      url "https://github.com/Rizzist/haider-agent/releases/download/v0.0.949/haider-v0.0.949-aarch64-apple-darwin.tar.xz"
      sha256 "6efbbf21385406877d773275013fab43ff15436b4cc8d59746ddb5ab6012cbcf"
    else
      url "https://github.com/Rizzist/haider-agent/releases/download/v0.0.949/haider-v0.0.949-x86_64-apple-darwin.tar.xz"
      sha256 "9dd413f9135b804380c50edd1df05c2b921166619cef99079e86ea3f31178aa4"
    end
  end

  on_linux do
    if Hardware::CPU.arm?
      url "https://github.com/Rizzist/haider-agent/releases/download/v0.0.949/haider-v0.0.949-aarch64-unknown-linux-gnu.tar.xz"
      sha256 "a939d695bb2e80c516da5ab7a9b90df9c715a2043bf15c674e953ebc500d0762"
    else
      url "https://github.com/Rizzist/haider-agent/releases/download/v0.0.949/haider-v0.0.949-x86_64-unknown-linux-gnu.tar.xz"
      sha256 "583f78b43bff0ed8c3c72ac1203088d3100e39bb354f3a159006e983bdd8594f"
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
