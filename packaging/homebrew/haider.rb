class Haider < Formula
  desc "Provider-agnostic coding-agent TUI and runtime"
  homepage "https://github.com/Rizzist/haider-agent"
  license "LicenseRef-KOA-P-1.0"
  version "0.0.969"

  on_macos do
    if Hardware::CPU.arm?
      url "https://github.com/Rizzist/haider-agent/releases/download/v0.0.969/haider-v0.0.969-aarch64-apple-darwin.tar.xz"
      sha256 "cd3da7c826669bae30bac48aed9b5d1ff7c8c5b18b788fdb126472557f25bfbe"
    else
      url "https://github.com/Rizzist/haider-agent/releases/download/v0.0.969/haider-v0.0.969-x86_64-apple-darwin.tar.xz"
      sha256 "fbba256f1790dd618825856afe88d59d4b3944a2bb34483a6a0af9cb837b3c1e"
    end
  end

  on_linux do
    if Hardware::CPU.arm?
      url "https://github.com/Rizzist/haider-agent/releases/download/v0.0.969/haider-v0.0.969-aarch64-unknown-linux-gnu.tar.xz"
      sha256 "0f805d26de131f78f2dad4f809a284f77c0722baa6de5d54bc10f98db1db85a9"
    else
      url "https://github.com/Rizzist/haider-agent/releases/download/v0.0.969/haider-v0.0.969-x86_64-unknown-linux-gnu.tar.xz"
      sha256 "cc26d496c588e75663665981e0cd94504d900cc9d370595eb99fdc0f59848937"
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
