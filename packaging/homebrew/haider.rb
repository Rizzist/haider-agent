class Haider < Formula
  desc "Provider-agnostic coding-agent TUI and runtime"
  homepage "https://github.com/Rizzist/haider-agent"
  license "LicenseRef-KOA-P-1.0"
  version "0.0.942"

  on_macos do
    if Hardware::CPU.arm?
      url "https://github.com/Rizzist/haider-agent/releases/download/v0.0.942/haider-v0.0.942-aarch64-apple-darwin.tar.xz"
      sha256 "95f5cc3b36e43b6c0da9e787a79ddfbe3f31ec2dcf11cd915577007db6db49c1"
    else
      url "https://github.com/Rizzist/haider-agent/releases/download/v0.0.942/haider-v0.0.942-x86_64-apple-darwin.tar.xz"
      sha256 "871433d6f8bd46c5659d3b5105fefc2995f45a081ad3a9d6f61c163ec6b726e0"
    end
  end

  on_linux do
    if Hardware::CPU.arm?
      url "https://github.com/Rizzist/haider-agent/releases/download/v0.0.942/haider-v0.0.942-aarch64-unknown-linux-gnu.tar.xz"
      sha256 "4bf4d9c22aa1abdeb7432a78e50e28e374dce9c27ed5b61119e84a1a93581a2f"
    else
      url "https://github.com/Rizzist/haider-agent/releases/download/v0.0.942/haider-v0.0.942-x86_64-unknown-linux-gnu.tar.xz"
      sha256 "77c7db1854d8a9447b3676b2a0a0f7059cd7f3036daa4e8b9e42c4cda1edc7d5"
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
