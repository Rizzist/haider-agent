class Haider < Formula
  desc "Provider-agnostic coding-agent TUI and runtime"
  homepage "https://github.com/Rizzist/haider-agent"
  license "LicenseRef-KOA-P-1.0"
  version "0.0.952"

  on_macos do
    if Hardware::CPU.arm?
      url "https://github.com/Rizzist/haider-agent/releases/download/v0.0.952/haider-v0.0.952-aarch64-apple-darwin.tar.xz"
      sha256 "260cde3f311fc481fe5f1bd68950708f4d999aae147ee8f0028de8b1bcb2d80b"
    else
      url "https://github.com/Rizzist/haider-agent/releases/download/v0.0.952/haider-v0.0.952-x86_64-apple-darwin.tar.xz"
      sha256 "6c54f1026ac8c83ef54223827fccd85ae8f857883664bd55d5aa017f1c711348"
    end
  end

  on_linux do
    if Hardware::CPU.arm?
      url "https://github.com/Rizzist/haider-agent/releases/download/v0.0.952/haider-v0.0.952-aarch64-unknown-linux-gnu.tar.xz"
      sha256 "b97164835e744f4dc056687d580c33749180b9a06031588e528271a00c814b90"
    else
      url "https://github.com/Rizzist/haider-agent/releases/download/v0.0.952/haider-v0.0.952-x86_64-unknown-linux-gnu.tar.xz"
      sha256 "27128d7d891a6abaf8fb944f5d7342e58fa9d0fac02f7dafa7987e4f3d380dbf"
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
