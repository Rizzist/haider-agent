class Haider < Formula
  desc "Provider-agnostic coding-agent TUI and runtime"
  homepage "https://github.com/Rizzist/haider-agent"
  license "LicenseRef-KOA-P-1.0"
  version "0.0.971"

  on_macos do
    if Hardware::CPU.arm?
      url "https://github.com/Rizzist/haider-agent/releases/download/v0.0.971/haider-v0.0.971-aarch64-apple-darwin-split.tar.xz"
      sha256 "aa2e7d4f497ff19bff7c8669a4af853fcfb2086499290416149962bcd611c598"
    else
      url "https://github.com/Rizzist/haider-agent/releases/download/v0.0.971/haider-v0.0.971-x86_64-apple-darwin-split.tar.xz"
      sha256 "6e005e938ced1f5db7ffb87f776ac715feac51a1419da07cb862e169282bd06b"
    end
  end

  on_linux do
    if Hardware::CPU.arm?
      url "https://github.com/Rizzist/haider-agent/releases/download/v0.0.971/haider-v0.0.971-aarch64-unknown-linux-gnu-split.tar.xz"
      sha256 "7ca151b99295446eb0aa2672d2bf5c079d8c78cc39edff139dfa56368dc24da6"
    else
      url "https://github.com/Rizzist/haider-agent/releases/download/v0.0.971/haider-v0.0.971-x86_64-unknown-linux-gnu-split.tar.xz"
      sha256 "51a1aa61913f92a1c3d1d575e5b0d6e03baa44a6a881d206bfef73c8f9b85ae6"
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
