class Haider < Formula
  desc "Provider-agnostic coding-agent TUI and runtime"
  homepage "https://github.com/Rizzist/haider-agent"
  license "LicenseRef-KOA-P-1.0"
  version "0.0.939"

  on_macos do
    if Hardware::CPU.arm?
      url "https://github.com/Rizzist/haider-agent/releases/download/v0.0.939/haider-v0.0.939-aarch64-apple-darwin.tar.xz"
      sha256 "c2029d98bcc3ace47ad4ad66b2c826405c0fcd326d057582c5d738b6d0538c98"
    else
      url "https://github.com/Rizzist/haider-agent/releases/download/v0.0.939/haider-v0.0.939-x86_64-apple-darwin.tar.xz"
      sha256 "bfefaf6142bf6027293d16532af7051303bedf47c418788fd6f8ee3a3b135f6c"
    end
  end

  on_linux do
    if Hardware::CPU.arm?
      url "https://github.com/Rizzist/haider-agent/releases/download/v0.0.939/haider-v0.0.939-aarch64-unknown-linux-gnu.tar.xz"
      sha256 "11691f62c15ff6b16cdc8cf2e7870362c9995a5241b0b9275ad33019f9cc1d39"
    else
      url "https://github.com/Rizzist/haider-agent/releases/download/v0.0.939/haider-v0.0.939-x86_64-unknown-linux-gnu.tar.xz"
      sha256 "db4f4b75f52a6212f99c4cbab8f4ae524126b8975416a8892ab6162bc7523ef1"
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
