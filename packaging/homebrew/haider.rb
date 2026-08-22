class Haider < Formula
  desc "Provider-agnostic coding-agent TUI and runtime"
  homepage "https://github.com/Rizzist/haider-agent"
  license "LicenseRef-KOA-P-1.0"
  version "0.0.938"

  on_macos do
    if Hardware::CPU.arm?
      url "https://github.com/Rizzist/haider-agent/releases/download/v0.0.938/haider-v0.0.938-aarch64-apple-darwin.tar.xz"
      sha256 "0cfb5e45176e9a0ff76e763c04b6908f267c4c00bd32fdd2531c6f8c0fb47dc8"
    else
      url "https://github.com/Rizzist/haider-agent/releases/download/v0.0.938/haider-v0.0.938-x86_64-apple-darwin.tar.xz"
      sha256 "951e0952f1e50a853153eeee218b2ab3f8f6445273a05c31e96469429ade480e"
    end
  end

  on_linux do
    if Hardware::CPU.arm?
      url "https://github.com/Rizzist/haider-agent/releases/download/v0.0.938/haider-v0.0.938-aarch64-unknown-linux-gnu.tar.xz"
      sha256 "26eb243b9c9822748dc6954b52f2208c0f03d094a90ea82f8d2660808336ec8e"
    else
      url "https://github.com/Rizzist/haider-agent/releases/download/v0.0.938/haider-v0.0.938-x86_64-unknown-linux-gnu.tar.xz"
      sha256 "544cebae7b675c0c3e3dcc06932dff26d22d2726f1826de4b39d3d671b117a3a"
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
