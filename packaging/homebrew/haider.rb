class Haider < Formula
  desc "Provider-agnostic coding-agent TUI and runtime"
  homepage "https://github.com/Rizzist/haider-agent"
  license "LicenseRef-KOA-P-1.0"
  version "0.0.970"

  on_macos do
    if Hardware::CPU.arm?
      url "https://github.com/Rizzist/haider-agent/releases/download/v0.0.970/haider-v0.0.970-aarch64-apple-darwin-split.tar.xz"
      sha256 "af491b0ff35a3308cef7740e964bc5477dbe302ad1b9fe777d81180e8c0ba1b4"
    else
      url "https://github.com/Rizzist/haider-agent/releases/download/v0.0.970/haider-v0.0.970-x86_64-apple-darwin-split.tar.xz"
      sha256 "054e21356fadff577a2e2d99622dd41ea3ca62ee3ff8ede23151ea3d7f28a5ba"
    end
  end

  on_linux do
    if Hardware::CPU.arm?
      url "https://github.com/Rizzist/haider-agent/releases/download/v0.0.970/haider-v0.0.970-aarch64-unknown-linux-gnu-split.tar.xz"
      sha256 "cc32f8d1dbe9be0ee13b765187b90db1c6cc81f65620b8a6c5e95992b8e703fc"
    else
      url "https://github.com/Rizzist/haider-agent/releases/download/v0.0.970/haider-v0.0.970-x86_64-unknown-linux-gnu-split.tar.xz"
      sha256 "c2130890836f85a09266579521be8339e673ce47eb7678c78033df751e3bce44"
    end
  end

  def install
    bundle = Dir["haider-v#{version}-*"].first
    source = bundle || "."
    bin.install "#{source}/haider", "#{source}/haiderd"
    # Keep the currently published legacy pin installable until release CI
    # re-pins this formula to the first split archive.
    if version >= Version.new("0.0.970")
      bin.install "#{source}/haider-tui"
    end
    portal = "#{source}/haider-wayland-portal"
    bin.install portal if OS.linux? && File.exist?(portal)
  end

  test do
    assert_equal "haider #{version}\n", shell_output("#{bin}/haider --version")
    assert_equal "haiderd #{version}\n", shell_output("#{bin}/haiderd --version")
    if version >= Version.new("0.0.970")
      assert_equal "haider-tui #{version}\n", shell_output("#{bin}/haider-tui --version")
    end
  end
end
