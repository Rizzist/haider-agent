class Haider < Formula
  desc "Provider-agnostic coding-agent TUI and runtime"
  homepage "https://github.com/Rizzist/haider-agent"
  license "LicenseRef-KOA-P-1.0"
  version "0.0.967"

  on_macos do
    if Hardware::CPU.arm?
      url "https://github.com/Rizzist/haider-agent/releases/download/v0.0.967/haider-v0.0.967-aarch64-apple-darwin.tar.xz"
      sha256 "3b57e822bf0291f320497da7349650e42468f89868771c7512ee4cad53a1d14d"
    else
      url "https://github.com/Rizzist/haider-agent/releases/download/v0.0.967/haider-v0.0.967-x86_64-apple-darwin.tar.xz"
      sha256 "a2a32d01a6824e076facd93eb6e0e1e2108576c4d49c42ca38c0f89632881070"
    end
  end

  on_linux do
    if Hardware::CPU.arm?
      url "https://github.com/Rizzist/haider-agent/releases/download/v0.0.967/haider-v0.0.967-aarch64-unknown-linux-gnu.tar.xz"
      sha256 "01599529b6633b1ab4ef171c26d8969673effd88269ce0ceb1be2bd6b273437e"
    else
      url "https://github.com/Rizzist/haider-agent/releases/download/v0.0.967/haider-v0.0.967-x86_64-unknown-linux-gnu.tar.xz"
      sha256 "6f107f936c555e08184c11346955fa58c2c3c13313488a9da782c4ff86164857"
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
