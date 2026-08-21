class Haider < Formula
  desc "Provider-agnostic coding-agent TUI and runtime"
  homepage "https://github.com/Rizzist/haider-agent"
  license "LicenseRef-KOA-P-1.0"
  version "0.0.936"

  on_macos do
    if Hardware::CPU.arm?
      url "https://github.com/Rizzist/haider-agent/releases/download/v0.0.936/haider-v0.0.936-aarch64-apple-darwin.tar.xz"
      sha256 "27172b7d51d70401daac981277f99dbbf755d06c4cce2f72a72c80270eb8b277"
    else
      url "https://github.com/Rizzist/haider-agent/releases/download/v0.0.936/haider-v0.0.936-x86_64-apple-darwin.tar.xz"
      sha256 "7b563ec1cfccad39955d861238efff07820520a764f2cf01249d8eafb028707f"
    end
  end

  on_linux do
    if Hardware::CPU.arm?
      url "https://github.com/Rizzist/haider-agent/releases/download/v0.0.936/haider-v0.0.936-aarch64-unknown-linux-gnu.tar.xz"
      sha256 "cedbeef9e5b786a20551a4a0b476b00489b672ef7c2fb33d5c433f7324624114"
    else
      url "https://github.com/Rizzist/haider-agent/releases/download/v0.0.936/haider-v0.0.936-x86_64-unknown-linux-gnu.tar.xz"
      sha256 "2409a49213967a6a839d8f8c456bc09508b0f1926bc1a7eadf1ae951c57b29d4"
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
