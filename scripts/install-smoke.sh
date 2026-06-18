#!/usr/bin/env bash
# End-to-end smoke for scripts/install.sh against a LOCALLY packaged tarball
# (file:// URL), so it does NOT depend on GitHub release availability and makes
# no network/model calls. Mirrors release.yml packaging (binary at archive root,
# `<hash>  <file>` sha256). Sandboxes HOME so `trimwire install` writes nothing real.
#
# Usage:
#   scripts/install-smoke.sh                 # builds target/release/trimwire if needed
#   TRIMWIRE_SMOKE_BIN=path/to/trimwire scripts/install-smoke.sh   # reuse a built binary (CI)
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BIN="${TRIMWIRE_SMOKE_BIN:-}"
if [ -z "$BIN" ] || [ ! -x "$BIN" ]; then
  echo "[smoke] building release binary…"
  ( cd "$ROOT" && cargo build --release )
  BIN="$ROOT/target/release/trimwire"
fi
[ -x "$BIN" ] || { echo "[smoke] FAIL: no executable binary at '$BIN'"; exit 1; }

# Resolve the target triple exactly like install.sh does.
os="$(uname -s)"; arch="$(uname -m)"
case "$os" in
  Linux)  vos="unknown-linux-gnu" ;;
  Darwin) vos="apple-darwin" ;;
  *) echo "[smoke] SKIP: unsupported OS '$os' (installer only supports Linux/Darwin)"; exit 0 ;;
esac
case "$arch" in
  x86_64|amd64) cpu="x86_64" ;;
  arm64|aarch64) cpu="aarch64" ;;
  *) echo "[smoke] SKIP: unsupported arch '$arch'"; exit 0 ;;
esac
triple="${cpu}-${vos}"
tarball="trimwire-${triple}.tar.gz"

WORK="$(mktemp -d)"   # the "release" artifact dir (served via file://)
SB="$(mktemp -d)"     # sandbox HOME so `trimwire install` touches nothing real
trap 'rm -rf "$WORK" "$SB"' EXIT

# Package like release.yml: binary at archive root + `<hash>  <file>` checksum.
cp "$BIN" "$WORK/trimwire"
tar -C "$WORK" -czf "$WORK/$tarball" trimwire
( cd "$WORK" && { if command -v sha256sum >/dev/null 2>&1; then sha256sum "$tarball"; else shasum -a 256 "$tarball"; fi; } > "$tarball.sha256" )
rm -f "$WORK/trimwire"   # leave only the tarball + .sha256, like a real release dir

run_install() { # $1 = base url, $2 = bindir ; sandboxed HOME/XDG
  HOME="$SB" XDG_CONFIG_HOME="$SB/.config" \
    TRIMWIRE_RELEASE_BASE_URL="$1" TRIMWIRE_BINDIR="$2" \
    sh "$ROOT/scripts/install.sh"
}

echo "[smoke] (1) happy path: install from file://$WORK"
run_install "file://$WORK" "$SB/bin"
installed="$SB/bin/trimwire"
[ -x "$installed" ] || { echo "[smoke] FAIL: binary not installed/executable at $installed"; exit 1; }
echo "[smoke]   installed binary present + executable ✓"
ver="$("$installed" --version 2>&1)" || { echo "[smoke] FAIL: --version did not run: $ver"; exit 1; }
cargo_ver="$(grep -m1 '^version' "$ROOT/Cargo.toml" | sed -E 's/.*"([^"]+)".*/\1/')"
case "$ver" in
  "trimwire $cargo_ver"*) echo "[smoke]   --version='$ver' matches Cargo.toml ($cargo_ver) ✓" ;;
  *) echo "[smoke] FAIL: --version='$ver' does not start with 'trimwire $cargo_ver'"; exit 1 ;;
esac

echo "[smoke] (2) bad checksum must be rejected"
bad="$(mktemp -d)"
cp "$WORK/$tarball" "$bad/"
printf '%s  %s\n' "0000000000000000000000000000000000000000000000000000000000000000" "$tarball" > "$bad/$tarball.sha256"
if run_install "file://$bad" "$SB/bin2" >/dev/null 2>&1; then
  echo "[smoke] FAIL: install succeeded with a bad checksum"; rm -rf "$bad"; exit 1
fi
rm -rf "$bad"; echo "[smoke]   bad checksum rejected (non-zero exit) ✓"

echo "[smoke] (3) missing artifact must be rejected"
empty="$(mktemp -d)"
if run_install "file://$empty" "$SB/bin3" >/dev/null 2>&1; then
  echo "[smoke] FAIL: install succeeded with a missing artifact"; rm -rf "$empty"; exit 1
fi
rm -rf "$empty"; echo "[smoke]   missing artifact rejected (non-zero exit) ✓"

echo "[smoke] PASS"
