class Haider < Formula
  desc "Provider-agnostic coding-agent TUI and runtime"
  homepage "https://github.com/Rizzist/haider-agent"
  license "LicenseRef-KOA-P-1.0"
  version "0.0.960"

  on_macos do
    if Hardware::CPU.arm?
      url "https://github.com/Rizzist/haider-agent/releases/download/v0.0.960/haider-v0.0.960-aarch64-apple-darwin.tar.xz"
      sha256 "251aa70bf561d18d8c9fb06ed375cf26da5a07a55da7372ad3ca8b4a57b69fc2"
    else
      url "https://github.com/Rizzist/haider-agent/releases/download/v0.0.960/haider-v0.0.960-x86_64-apple-darwin.tar.xz"
      sha256 "eafd1fc575e39fbda1d4a83eb691fe78fb6fddb0e8fa5e84318c30e7d6fbd0eb"
    end
  end

  on_linux do
    if Hardware::CPU.arm?
      url "https://github.com/Rizzist/haider-agent/releases/download/v0.0.960/haider-v0.0.960-aarch64-unknown-linux-gnu.tar.xz"
      sha256 "f85e1fb78f62d7ca2f719bdac0a52bc01ba99ddd54fcb23e3d237b045799a626"
    else
      url "https://github.com/Rizzist/haider-agent/releases/download/v0.0.960/haider-v0.0.960-x86_64-unknown-linux-gnu.tar.xz"
      sha256 "847af45af6c0d7e0e5eabb2b9e7e4b130fd1759052beafdf258b018e56219b31"
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
