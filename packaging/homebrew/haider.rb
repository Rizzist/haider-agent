class Haider < Formula
  desc "Provider-agnostic coding-agent TUI and runtime"
  homepage "https://github.com/Rizzist/haider-agent"
  license "LicenseRef-KOA-P-1.0"
  version "0.0.972"

  on_macos do
    if Hardware::CPU.arm?
      url "https://github.com/Rizzist/haider-agent/releases/download/v0.0.972/haider-v0.0.972-aarch64-apple-darwin-split.tar.xz"
      sha256 "e8314e78a453269037ab543f0d82d81e4e417d5c2ce1339065a3dbddaf4054c8"
    else
      url "https://github.com/Rizzist/haider-agent/releases/download/v0.0.972/haider-v0.0.972-x86_64-apple-darwin-split.tar.xz"
      sha256 "1b835a3c66519a30017045dca154b3fed3b2829f60faf97f9552dd926a2042d0"
    end
  end

  on_linux do
    if Hardware::CPU.arm?
      url "https://github.com/Rizzist/haider-agent/releases/download/v0.0.972/haider-v0.0.972-aarch64-unknown-linux-gnu-split.tar.xz"
      sha256 "28ffe822c50d88bd09b5271023ff062a3fb13d103bc2cbec4183bd9932525c7c"
    else
      url "https://github.com/Rizzist/haider-agent/releases/download/v0.0.972/haider-v0.0.972-x86_64-unknown-linux-gnu-split.tar.xz"
      sha256 "da7508b8ed16ab66cc800a83dcb12c070358b0a445cd395609ac6a4a1b7c8e88"
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
