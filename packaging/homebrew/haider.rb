class Haider < Formula
  desc "Provider-agnostic coding-agent TUI and runtime"
  homepage "https://github.com/Rizzist/haider-agent"
  license "LicenseRef-KOA-P-1.0"
  version "0.0.957"

  on_macos do
    if Hardware::CPU.arm?
      url "https://github.com/Rizzist/haider-agent/releases/download/v0.0.957/haider-v0.0.957-aarch64-apple-darwin.tar.xz"
      sha256 "dbb082e68c9ba2fce205b81c469795187f81eb0d3c1cb1a9336f69c13c9603ba"
    else
      url "https://github.com/Rizzist/haider-agent/releases/download/v0.0.957/haider-v0.0.957-x86_64-apple-darwin.tar.xz"
      sha256 "a02cf2788ea154e555cbe64fe7d18a8a6067a5d285f3580d7a32e058d36e64cc"
    end
  end

  on_linux do
    if Hardware::CPU.arm?
      url "https://github.com/Rizzist/haider-agent/releases/download/v0.0.957/haider-v0.0.957-aarch64-unknown-linux-gnu.tar.xz"
      sha256 "bf22dbe6e944c986a2450edec0d1e85a145806ad31554b2bc96b4dfcd8293240"
    else
      url "https://github.com/Rizzist/haider-agent/releases/download/v0.0.957/haider-v0.0.957-x86_64-unknown-linux-gnu.tar.xz"
      sha256 "5a7e639db23cc20558e0c0c8ab20d4d8646e40be5ea5d7f19d5e5792c00d9bd7"
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
