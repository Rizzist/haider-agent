class Haider < Formula
  desc "Provider-agnostic coding-agent TUI and runtime"
  homepage "https://github.com/Rizzist/haider-agent"
  license "LicenseRef-KOA-P-1.0"
  version "0.0.963"

  on_macos do
    if Hardware::CPU.arm?
      url "https://github.com/Rizzist/haider-agent/releases/download/v0.0.963/haider-v0.0.963-aarch64-apple-darwin.tar.xz"
      sha256 "95994fbe1046dfd126a94a3fd1e1cf028d3505f24383040c056f5575b0ca1774"
    else
      url "https://github.com/Rizzist/haider-agent/releases/download/v0.0.963/haider-v0.0.963-x86_64-apple-darwin.tar.xz"
      sha256 "3d5cd5c17edb23fe9dccbd80859221c839aa666c10aabc84a1c889bd61b66e1b"
    end
  end

  on_linux do
    if Hardware::CPU.arm?
      url "https://github.com/Rizzist/haider-agent/releases/download/v0.0.963/haider-v0.0.963-aarch64-unknown-linux-gnu.tar.xz"
      sha256 "9a089f14af1dc72b256ca022cc58033fd6af224486a67b743f3a20ce4ad53e11"
    else
      url "https://github.com/Rizzist/haider-agent/releases/download/v0.0.963/haider-v0.0.963-x86_64-unknown-linux-gnu.tar.xz"
      sha256 "2a98daa61e2485de2e7577e3ac151f67f95884a3c760f0667fb2bee64653df41"
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
