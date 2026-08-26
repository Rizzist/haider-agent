class Haider < Formula
  desc "Provider-agnostic coding-agent TUI and runtime"
  homepage "https://github.com/Rizzist/haider-agent"
  license "LicenseRef-KOA-P-1.0"
  version "0.0.961"

  on_macos do
    if Hardware::CPU.arm?
      url "https://github.com/Rizzist/haider-agent/releases/download/v0.0.961/haider-v0.0.961-aarch64-apple-darwin.tar.xz"
      sha256 "a4150ba26be56b8ac44b6f2aca541c77ef08190cc1f11c7089afe21dd168f5e8"
    else
      url "https://github.com/Rizzist/haider-agent/releases/download/v0.0.961/haider-v0.0.961-x86_64-apple-darwin.tar.xz"
      sha256 "b2cbc23727aa14e5476f5fbfee01c533ba2eb8e8605576b007a52172ef1b7c0b"
    end
  end

  on_linux do
    if Hardware::CPU.arm?
      url "https://github.com/Rizzist/haider-agent/releases/download/v0.0.961/haider-v0.0.961-aarch64-unknown-linux-gnu.tar.xz"
      sha256 "1401c83e967f36c93936583f0a5b00f50531ed5d89f3c1ef46b26470d54a1cea"
    else
      url "https://github.com/Rizzist/haider-agent/releases/download/v0.0.961/haider-v0.0.961-x86_64-unknown-linux-gnu.tar.xz"
      sha256 "080aa9aaf857a7b58480e32ccac2bd0e04be0f29c0188d7a2ce7fbcd8b61cf6a"
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
