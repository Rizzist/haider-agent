class Haider < Formula
  desc "Provider-agnostic coding-agent TUI and runtime"
  homepage "https://github.com/Rizzist/haider-agent"
  license "LicenseRef-KOA-P-1.0"
  version "0.0.945"

  on_macos do
    if Hardware::CPU.arm?
      url "https://github.com/Rizzist/haider-agent/releases/download/v0.0.945/haider-v0.0.945-aarch64-apple-darwin.tar.xz"
      sha256 "858214b392d70c8c7a9933a60e242959d2c579faddf28cb6e98f714f4206f601"
    else
      url "https://github.com/Rizzist/haider-agent/releases/download/v0.0.945/haider-v0.0.945-x86_64-apple-darwin.tar.xz"
      sha256 "66604fbdc1be59f986b85dc636c6d9e9534174521fdefbfa8adbc58f668d2232"
    end
  end

  on_linux do
    if Hardware::CPU.arm?
      url "https://github.com/Rizzist/haider-agent/releases/download/v0.0.945/haider-v0.0.945-aarch64-unknown-linux-gnu.tar.xz"
      sha256 "903ba7522acb593993c47e66e5a1220e028474deb327730bd362aece804600cb"
    else
      url "https://github.com/Rizzist/haider-agent/releases/download/v0.0.945/haider-v0.0.945-x86_64-unknown-linux-gnu.tar.xz"
      sha256 "2bcaaebad44f53d90e67b3e00adb99cd1911d4e36ee7bbb677efab3a1a2bcff2"
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
