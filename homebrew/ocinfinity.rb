# Homebrew formula for ocinfinity.
#
# Published in the WDRXN/homebrew-ocinfinity tap, so users install with:
#
#   brew install WDRXN/ocinfinity/ocinfinity
#
# The binaries are the release assets themselves; each architecture gets its
# own URL and checksum, matching the release's SHA256SUMS.
class Ocinfinity < Formula
  desc "OpenCode rate-limit reset via Cloudflare WARP and a Claude Code proxy"
  homepage "https://github.com/WDRXN/ocinfinity"
  version "1.0.0"
  license :cannot_represent

  on_macos do
    on_intel do
      url "https://github.com/WDRXN/ocinfinity/releases/download/v1.0.0/ocinfinity-macos"
      sha256 "24686e01af6e2e0df8549418611237f811994d2561603deffa789486dcbad212"
    end

    on_arm do
      url "https://github.com/WDRXN/ocinfinity/releases/download/v1.0.0/ocinfinity-macos-arm"
      sha256 "2d45eedc51f96328e54bb923b9343da061d561d9c61d0a30e758d71633f9ca6a"
    end
  end

  on_linux do
    url "https://github.com/WDRXN/ocinfinity/releases/download/v1.0.0/ocinfinity-linux"
    sha256 "0076878e605b7eb4900bbde117e3eaba0d6fecb9fcf1d7f3a59242d28f327415"
  end

  def install
    staged_binary = Dir["ocinfinity-linux", "ocinfinity-macos", "ocinfinity-macos-arm"].first
    odie "unexpected release asset" if staged_binary.nil?

    bin.install staged_binary => "ocinfinity"
  end

  test do
    assert_match "ocinfinity", shell_output("#{bin}/ocinfinity --version")
  end
end
