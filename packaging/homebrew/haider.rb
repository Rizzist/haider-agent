class Haider < Formula
  desc "Provider-agnostic coding-agent TUI and runtime"
  homepage "https://github.com/Rizzist/haider-agent"
  license "LicenseRef-KOA-P-1.0"
  version "0.0.965"

  on_macos do
    if Hardware::CPU.arm?
      url "https://github.com/Rizzist/haider-agent/releases/download/v0.0.965/haider-v0.0.965-aarch64-apple-darwin.tar.xz"
      sha256 "2d816cfd1a973b53b409607147f29d73d8540c59ecabc019d1db941c50c2c593"
    else
      url "https://github.com/Rizzist/haider-agent/releases/download/v0.0.965/haider-v0.0.965-x86_64-apple-darwin.tar.xz"
      sha256 "b44e604058ccfd0bce09dc363f54aff90a4dd438da7c67f1ee7c773d7a2305ca"
    end
  end

  on_linux do
    if Hardware::CPU.arm?
      url "https://github.com/Rizzist/haider-agent/releases/download/v0.0.965/haider-v0.0.965-aarch64-unknown-linux-gnu.tar.xz"
      sha256 "8ca43a0c56899f1e3f8f68f5032c9609995e74f30b72f55fd9cd7e66110f4627"
    else
      url "https://github.com/Rizzist/haider-agent/releases/download/v0.0.965/haider-v0.0.965-x86_64-unknown-linux-gnu.tar.xz"
      sha256 "25b7f5416acff2b063ad4f824b36a73075bf45321d9f9b4ba23a47e18b002af2"
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
