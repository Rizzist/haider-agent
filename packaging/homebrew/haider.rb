class Haider < Formula
  desc "Provider-agnostic coding-agent TUI and runtime"
  homepage "https://github.com/Rizzist/haider-agent"
  license "LicenseRef-KOA-P-1.0"
  version "0.0.943"

  on_macos do
    if Hardware::CPU.arm?
      url "https://github.com/Rizzist/haider-agent/releases/download/v0.0.943/haider-v0.0.943-aarch64-apple-darwin.tar.xz"
      sha256 "72d1ed56dd52fc774f71eab97c11c09c0e4bc8908639cbbf565d86f8a551cf2d"
    else
      url "https://github.com/Rizzist/haider-agent/releases/download/v0.0.943/haider-v0.0.943-x86_64-apple-darwin.tar.xz"
      sha256 "ed94eff1039b2b95073a1a15c1c4dbfaa90e30dc995e43db7da720054d48f5af"
    end
  end

  on_linux do
    if Hardware::CPU.arm?
      url "https://github.com/Rizzist/haider-agent/releases/download/v0.0.943/haider-v0.0.943-aarch64-unknown-linux-gnu.tar.xz"
      sha256 "dded66c43e633074bb0961af21af31974bb15ddf5d9a910614b14d7439121188"
    else
      url "https://github.com/Rizzist/haider-agent/releases/download/v0.0.943/haider-v0.0.943-x86_64-unknown-linux-gnu.tar.xz"
      sha256 "c36b6ce5cf2ef8feaffde9a32d7f52700348576aae2c4eae2a9dec480aed2b4b"
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
