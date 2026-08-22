class Haider < Formula
  desc "Provider-agnostic coding-agent TUI and runtime"
  homepage "https://github.com/Rizzist/haider-agent"
  license "LicenseRef-KOA-P-1.0"
  version "0.0.940"

  on_macos do
    if Hardware::CPU.arm?
      url "https://github.com/Rizzist/haider-agent/releases/download/v0.0.940/haider-v0.0.940-aarch64-apple-darwin.tar.xz"
      sha256 "acd7d5bc7333b2481bb42ea8a643aba7e1e9bd8b4826c6758c63f73834e7f648"
    else
      url "https://github.com/Rizzist/haider-agent/releases/download/v0.0.940/haider-v0.0.940-x86_64-apple-darwin.tar.xz"
      sha256 "8475ad168f9e9d053bad2832baa20b8f68a0ac1380f4e477202ea4de5f6dddf9"
    end
  end

  on_linux do
    if Hardware::CPU.arm?
      url "https://github.com/Rizzist/haider-agent/releases/download/v0.0.940/haider-v0.0.940-aarch64-unknown-linux-gnu.tar.xz"
      sha256 "bd8d9f3afe5e0c2c1b8b25cc6124077f92358f86f4c4f748c9bd9852e709453e"
    else
      url "https://github.com/Rizzist/haider-agent/releases/download/v0.0.940/haider-v0.0.940-x86_64-unknown-linux-gnu.tar.xz"
      sha256 "d86484dacc26a5973e4f624922ea01e3bd9957f3cbc0e5732686afb57f0eaf5b"
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
