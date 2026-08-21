class Haider < Formula
  desc "Provider-agnostic coding-agent TUI and runtime"
  homepage "https://github.com/Rizzist/haider-agent"
  license "LicenseRef-KOA-P-1.0"
  version "0.0.937"

  on_macos do
    if Hardware::CPU.arm?
      url "https://github.com/Rizzist/haider-agent/releases/download/v0.0.937/haider-v0.0.937-aarch64-apple-darwin.tar.xz"
      sha256 "80d33980b4a218937bcd5bd30880b5fa5cef4fdcbec741b574e88e694a8ab628"
    else
      url "https://github.com/Rizzist/haider-agent/releases/download/v0.0.937/haider-v0.0.937-x86_64-apple-darwin.tar.xz"
      sha256 "1ebd933734cda67d95c54896cad9c5ed8217f7b307f547c025ac97657505a82b"
    end
  end

  on_linux do
    if Hardware::CPU.arm?
      url "https://github.com/Rizzist/haider-agent/releases/download/v0.0.937/haider-v0.0.937-aarch64-unknown-linux-gnu.tar.xz"
      sha256 "747601daeef2c4e03b9adda22cb43e1662ddd952e224b14dd6f596b850bf57cd"
    else
      url "https://github.com/Rizzist/haider-agent/releases/download/v0.0.937/haider-v0.0.937-x86_64-unknown-linux-gnu.tar.xz"
      sha256 "75eac355915fac7153d7170b7a52fb259cfed37e405b4f4e3097a5365db3d01d"
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
