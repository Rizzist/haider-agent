class Haider < Formula
  desc "Provider-agnostic coding-agent TUI and runtime"
  homepage "https://github.com/Rizzist/haider-agent"
  license "LicenseRef-KOA-P-1.0"
  version "0.0.947"

  on_macos do
    if Hardware::CPU.arm?
      url "https://github.com/Rizzist/haider-agent/releases/download/v0.0.947/haider-v0.0.947-aarch64-apple-darwin.tar.xz"
      sha256 "c04fd6fadace05a17d8793b14c86601bc48c6cfd623b99fb0bab1eda2ff259b1"
    else
      url "https://github.com/Rizzist/haider-agent/releases/download/v0.0.947/haider-v0.0.947-x86_64-apple-darwin.tar.xz"
      sha256 "222d8e75c1e6e70483787402f0712074569c395ab5dc90bdcc241fc6f152ac29"
    end
  end

  on_linux do
    if Hardware::CPU.arm?
      url "https://github.com/Rizzist/haider-agent/releases/download/v0.0.947/haider-v0.0.947-aarch64-unknown-linux-gnu.tar.xz"
      sha256 "5385a362413c5fc61713f2b372e74c2dc7d70be789f55241446997166bae5dae"
    else
      url "https://github.com/Rizzist/haider-agent/releases/download/v0.0.947/haider-v0.0.947-x86_64-unknown-linux-gnu.tar.xz"
      sha256 "0f76f89acd926ffcaa0e895a408115acd8ae93d414bb740983a28557be35310a"
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
