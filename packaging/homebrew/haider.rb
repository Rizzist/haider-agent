class Haider < Formula
  desc "Provider-agnostic coding-agent TUI and runtime"
  homepage "https://github.com/Rizzist/haider-agent"
  license "LicenseRef-KOA-P-1.0"
  version "0.0.968"

  on_macos do
    if Hardware::CPU.arm?
      url "https://github.com/Rizzist/haider-agent/releases/download/v0.0.968/haider-v0.0.968-aarch64-apple-darwin.tar.xz"
      sha256 "3e1add77ebb0795d4717b19e6d69674c3de324580f6a6f1dd1e7e731a7eaeee0"
    else
      url "https://github.com/Rizzist/haider-agent/releases/download/v0.0.968/haider-v0.0.968-x86_64-apple-darwin.tar.xz"
      sha256 "a93badbdac39274b37280399ed41a95c24a31c0a1624900aef5bca8a2526346e"
    end
  end

  on_linux do
    if Hardware::CPU.arm?
      url "https://github.com/Rizzist/haider-agent/releases/download/v0.0.968/haider-v0.0.968-aarch64-unknown-linux-gnu.tar.xz"
      sha256 "199c9061fdda2166e13443f4c485415bbbb370d35d568ad44f4f7367972d4146"
    else
      url "https://github.com/Rizzist/haider-agent/releases/download/v0.0.968/haider-v0.0.968-x86_64-unknown-linux-gnu.tar.xz"
      sha256 "d4d33e5afdb31671874a24b3946fb7b0dc1f9082ca6d93e00bebbd873fc4a890"
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
