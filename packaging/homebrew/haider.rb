class Haider < Formula
  desc "Provider-agnostic coding-agent TUI and runtime"
  homepage "https://github.com/Rizzist/haider-agent"
  license "LicenseRef-KOA-P-1.0"
  version "0.0.948"

  on_macos do
    if Hardware::CPU.arm?
      url "https://github.com/Rizzist/haider-agent/releases/download/v0.0.948/haider-v0.0.948-aarch64-apple-darwin.tar.xz"
      sha256 "d4aea820116bcc7ac568be17fbab6b1882d24ad633bff355a023c32e711fdbed"
    else
      url "https://github.com/Rizzist/haider-agent/releases/download/v0.0.948/haider-v0.0.948-x86_64-apple-darwin.tar.xz"
      sha256 "7d54dbdb34843b942fdda6bacb2dfe9eff36ed2a2977c5606f8b45d3f92c67e8"
    end
  end

  on_linux do
    if Hardware::CPU.arm?
      url "https://github.com/Rizzist/haider-agent/releases/download/v0.0.948/haider-v0.0.948-aarch64-unknown-linux-gnu.tar.xz"
      sha256 "3b6de836a07ab9dbdcb37d61ee762673a66240e1bf2ddd09c64a239482a999e2"
    else
      url "https://github.com/Rizzist/haider-agent/releases/download/v0.0.948/haider-v0.0.948-x86_64-unknown-linux-gnu.tar.xz"
      sha256 "6c7a3f1a2804491ebf07dc48ff05d65c792e185119ef58620c1dc8739da5aa80"
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
