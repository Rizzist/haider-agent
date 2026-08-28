class Haider < Formula
  desc "Provider-agnostic coding-agent TUI and runtime"
  homepage "https://github.com/Rizzist/haider-agent"
  license "LicenseRef-KOA-P-1.0"
  version "0.0.964"

  on_macos do
    if Hardware::CPU.arm?
      url "https://github.com/Rizzist/haider-agent/releases/download/v0.0.964/haider-v0.0.964-aarch64-apple-darwin.tar.xz"
      sha256 "124a56691d52479eae7b0a6851188105959cfbd297cb7c698a008225f3b1105f"
    else
      url "https://github.com/Rizzist/haider-agent/releases/download/v0.0.964/haider-v0.0.964-x86_64-apple-darwin.tar.xz"
      sha256 "397bc7898269a189949e936f48e543d4fe7ed0afdf704a883310fc957eb480d4"
    end
  end

  on_linux do
    if Hardware::CPU.arm?
      url "https://github.com/Rizzist/haider-agent/releases/download/v0.0.964/haider-v0.0.964-aarch64-unknown-linux-gnu.tar.xz"
      sha256 "1a87748c207e6312abf78c1c22196fefa81032482df9a9f170ad66aa5bfba94d"
    else
      url "https://github.com/Rizzist/haider-agent/releases/download/v0.0.964/haider-v0.0.964-x86_64-unknown-linux-gnu.tar.xz"
      sha256 "0507d7a5213de0f190f8b3cff188c7e422e34e80076ba37ebcffe69b984176d5"
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
