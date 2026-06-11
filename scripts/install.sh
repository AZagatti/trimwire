#!/usr/bin/env sh
# trimwire bootstrap installer.
#
#   curl -LsSf https://raw.githubusercontent.com/AZagatti/trimwire/main/scripts/install.sh | sh
#
# Downloads the trimwire binary for this OS/arch from the latest GitHub release
# into ~/.local/bin, then runs `trimwire install` (writes a starter config and
# adds the gateway env exports to your shell rc).
#
# NOTE: the release assets this fetches are produced by
# .github/workflows/release.yml (a cross-platform matrix build) on a `v*` tag
# push. Until the first tagged release exists, build from source instead:
# `cargo install --path .`.
set -eu

REPO="AZagatti/trimwire"
BIN="trimwire"
BINDIR="${TRIMWIRE_BINDIR:-$HOME/.local/bin}"

say() { printf '[trimwire install] %s\n' "$1"; }
die() { printf '[trimwire install] error: %s\n' "$1" >&2; exit 1; }

# Resolve the Rust target triple for this platform.
os="$(uname -s)"
arch="$(uname -m)"
case "$os" in
  Linux)  vendor_os="unknown-linux-gnu" ;;
  Darwin) vendor_os="apple-darwin" ;;
  *) die "unsupported OS '$os' — build from source: cargo install --path ." ;;
esac
case "$arch" in
  x86_64|amd64) cpu="x86_64" ;;
  arm64|aarch64) cpu="aarch64" ;;
  *) die "unsupported arch '$arch' — build from source: cargo install --path ." ;;
esac
triple="${cpu}-${vendor_os}"

# Pick a downloader.
if command -v curl >/dev/null 2>&1; then
  fetch() { curl -LsSf "$1" -o "$2"; }
elif command -v wget >/dev/null 2>&1; then
  fetch() { wget -qO "$2" "$1"; }
else
  die "need curl or wget on PATH"
fi

tarball="${BIN}-${triple}.tar.gz"
url="https://github.com/${REPO}/releases/latest/download/${tarball}"

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

say "downloading ${tarball}"
fetch "$url" "$tmp/$tarball" || die "download failed: $url"

# Verify the SHA-256 checksum before extracting/installing.
say "verifying checksum"
if fetch "$url.sha256" "$tmp/$tarball.sha256" 2>/dev/null; then
  if command -v sha256sum >/dev/null 2>&1; then
    ( cd "$tmp" && sha256sum -c "$tarball.sha256" ) || die "checksum verification failed"
  elif command -v shasum >/dev/null 2>&1; then
    ( cd "$tmp" && shasum -a 256 -c "$tarball.sha256" ) || die "checksum verification failed"
  else
    say "no sha256 tool found — skipping verification (install sha256sum/shasum to enable)"
  fi
else
  die "checksum file not found: $url.sha256 (refusing to install unverified binary)"
fi

say "extracting to ${BINDIR}"
mkdir -p "$BINDIR"
tar -xzf "$tmp/$tarball" -C "$tmp"
# The binary may be at the archive root or under a triple-named dir.
src="$(find "$tmp" -type f -name "$BIN" -perm -u+x 2>/dev/null | head -n1)"
[ -n "$src" ] || src="$(find "$tmp" -type f -name "$BIN" | head -n1)"
[ -n "$src" ] || die "binary '$BIN' not found in archive"
install -m 0755 "$src" "$BINDIR/$BIN"

# Warn if BINDIR is not on PATH.
case ":$PATH:" in
  *":$BINDIR:"*) ;;
  *) say "note: $BINDIR is not on your PATH — add it to your shell rc" ;;
esac

say "running '$BIN install' (writes config + shell rc)"
"$BINDIR/$BIN" install

say "done. Reload your shell (exec \$SHELL) then use 'claude' as normal."
say "  (or run one session without the always-on service: $BIN run claude)"
