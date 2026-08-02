# Homebrew formula template for Legion.
#
# The release workflow renders this into Formula/legion.rb in the
# dawnswwwww/homebrew-tap tap, substituting @@VERSION@@, @@ARM64_SHA@@,
# and @@X64_SHA@@.
#
# Users install via:  brew install dawnswwwww/tap/legion
class Legion < Formula
  desc "Self-hosted, multi-channel AI agent gateway and CLI"
  homepage "https://github.com/dawnswwwww/uselegion"
  url "https://github.com/dawnswwwww/uselegion/releases/download/v@@VERSION@@/legion-@@VERSION@@-x86_64-apple-darwin.tar.gz"
  sha256 "@@X64_SHA@@"
  license "MIT"
  version "@@VERSION@@"

  # Both legion (CLI) and legion-gateway (sidecar) ship in the same archive.
  # On Intel macOS, the x86_64 archive is fetched; arm64 uses on_resource.
  on_macos do
    on_arm do
      url "https://github.com/dawnswwwww/uselegion/releases/download/v@@VERSION@@/legion-@@VERSION@@-aarch64-apple-darwin.tar.gz"
      sha256 "@@ARM64_SHA@@"
    end
  end

  def install
    bin.install "legion"
    bin.install "legion-gateway"
  end

  def caveats
    <<~EOS
      Run `legion setup` to configure your provider and channels for the first
      time. The gateway can be started with `legion gateway start`.
    EOS
  end

  test do
    assert_match "legion #{version}", shell_output("#{bin}/legion --version")
  end
end
