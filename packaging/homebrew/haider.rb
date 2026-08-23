class Haider < Formula
  desc "Provider-agnostic coding-agent TUI and runtime"
  homepage "https://github.com/Rizzist/haider-agent"
  license "LicenseRef-KOA-P-1.0"
  version "0.0.950"

  on_macos do
    if Hardware::CPU.arm?
      url "https://github.com/Rizzist/haider-agent/releases/download/v0.0.950/haider-v0.0.950-aarch64-apple-darwin.tar.xz"
      sha256 "3f4b1f9fe85e9487b564eac7dcde8b3a96b739ca70c4a73f5c45c6245c1572b9"
    else
      url "https://github.com/Rizzist/haider-agent/releases/download/v0.0.950/haider-v0.0.950-x86_64-apple-darwin.tar.xz"
      sha256 "af999b01899da9365c502e447601d90bc8e1be75f71ad13627dd457aa30876bf"
    end
  end

  on_linux do
    if Hardware::CPU.arm?
      url "https://github.com/Rizzist/haider-agent/releases/download/v0.0.950/haider-v0.0.950-aarch64-unknown-linux-gnu.tar.xz"
      sha256 "414eeb59217a8aba3b1d9f58540ae1afae6e57fda39130efc13e849c521b1e0e"
    else
      url "https://github.com/Rizzist/haider-agent/releases/download/v0.0.950/haider-v0.0.950-x86_64-unknown-linux-gnu.tar.xz"
      sha256 "ac27e8590b47669fc058e3ff986e6d29876e9f0c2b51e6fb349246f171e02af4"
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
