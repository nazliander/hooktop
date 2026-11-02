#!/usr/bin/env bash
# Write the Homebrew formula for a release to stdout.
#   packaging/brew-formula.sh v0.1.0 dist/
# `dist/` holds hooktop-<tag>-<target>.tar.gz.sha256 for each target.
set -euo pipefail

tag="$1"
dir="${2:-dist}"
version="${tag#v}"
repo="${HOOKTOP_REPO:-nazliander/hooktop}"
base="https://github.com/$repo/releases/download/$tag"

sha() {
  local f="$dir/hooktop-$tag-$1.tar.gz.sha256"
  [ -f "$f" ] || { echo "missing $f" >&2; exit 1; }
  cut -d' ' -f1 "$f"
}

asset() {
  printf '      url "%s/hooktop-%s-%s.tar.gz"\n      sha256 "%s"\n' "$base" "$tag" "$1" "$(sha "$1")"
}

cat <<RUBY
class Hooktop < Formula
  desc "Live view of Claude Code tool calls, subagents and CPU/RAM use"
  homepage "https://github.com/$repo"
  version "$version"
  license "Apache-2.0"

  on_macos do
    on_arm do
$(asset aarch64-apple-darwin)
    end
    on_intel do
$(asset x86_64-apple-darwin)
    end
  end

  on_linux do
    on_arm do
$(asset aarch64-unknown-linux-gnu)
    end
    on_intel do
$(asset x86_64-unknown-linux-gnu)
    end
  end

  def install
    bin.install "hooktop"
  end

  # \`brew services start hooktop\` keeps the server up, so every Claude Code
  # session is recorded even when no hooktop window is open. It's meant to
  # stay up, so the idle timeout is off.
  service do
    run [opt_bin/"hooktop", "serve", "--idle-timeout", "off"]
    keep_alive true
    log_path var/"log/hooktop.log"
    error_log_path var/"log/hooktop.log"
  end

  def caveats
    <<~EOS
      Point Claude Code at hooktop once:
        hooktop hooks install

      Then run \`hooktop\`. To keep recording sessions when no hooktop window
      is open:
        brew services start hooktop
    EOS
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/hooktop --version")
  end
end
RUBY
