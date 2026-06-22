#!/usr/bin/env bash
# Guard against benchmark/results/RESULTS.md silently drifting from the actual
# deterministic bench output. Regenerates the bench and diffs it against the
# committed doc, NORMALIZING away the only host-dependent part: section "## 7.
# Gateway overhead" (per-request microsecond timings vary run-to-run). Everything
# above §7 (savings, bytes, cache stability, cost model) is deterministic and
# must match byte-for-byte.
#
# Exit 0 = in sync. Non-zero = RESULTS.md is stale; regenerate with:
#     cargo run --release --example bench > benchmark/results/RESULTS.md
#
# Usage:
#   scripts/bench-drift-check.sh        # builds + runs the bench example
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
COMMITTED="$ROOT/benchmark/results/RESULTS.md"
[ -f "$COMMITTED" ] || { echo "[bench-drift] FAIL: $COMMITTED not found"; exit 1; }

FRESH="$(mktemp)"
trap 'rm -f "$FRESH"' EXIT

echo "[bench-drift] regenerating bench (offline, deterministic; --ci-drift skips §7 timing)…"
# Run the example in --ci-drift mode: it emits the same deterministic sections
# (0–6b) but SKIPS the expensive `## 7. Gateway overhead` micro-timing loops the
# diff drops anyway (~36s saved). Prefer the PRE-BUILT binary (CI builds it in the
# release-build step) to avoid ~1s of `cargo run` freshness overhead; build it
# only if missing (standalone/local use).
BIN="$ROOT/target/release/examples/bench"
[ -x "$BIN" ] || ( cd "$ROOT" && cargo build --release --quiet --example bench )
"$BIN" --ci-drift > "$FRESH"

# Drop the host-dependent timing section (§7 → EOF) from both sides before diff.
# --ci-drift output stops at the §7 header (no table), but the committed
# RESULTS.md keeps the full §7 timing table, so normalize BOTH sides here.
norm() { sed '/^## 7\. Gateway overhead/,$d' "$1"; }

if diff <(norm "$COMMITTED") <(norm "$FRESH") > /tmp/bench_drift.diff 2>&1; then
  echo "[bench-drift] OK — RESULTS.md matches current bench output (timing section ignored)."
  # Also guard the hand-written corpus COUNT in benchmark/README.md against the
  # real number of corpora (audit P2-1: that prose used to drift — it said
  # "Twelve" when the bench had 15). Count corpus rows in §1 of the fresh output.
  README="$ROOT/benchmark/README.md"
  # shellcheck disable=SC2016  # the grep pattern is a literal regex (no shell expansion intended)
  corpus_count="$(sed -n '/^## 1\./,/^## 2\./p' "$FRESH" | grep -c '^| `[a-z_0-9]\+`')"
  if [ -f "$README" ] && [ "$corpus_count" -gt 0 ]; then
    if ! grep -q "${corpus_count} deterministic synthetic profiles" "$README"; then
      echo "[bench-drift] FAIL: benchmark/README.md corpus count is stale —"
      echo "    the bench has ${corpus_count} corpora but README does not say"
      echo "    \"${corpus_count} deterministic synthetic profiles\". Update the Corpora section."
      exit 1
    fi
    echo "[bench-drift] OK — benchmark/README.md states the correct corpus count (${corpus_count})."
  fi
  exit 0
fi

echo "[bench-drift] FAIL: benchmark/results/RESULTS.md is STALE (drift in the deterministic sections):"
echo "------------------------------------------------------------------"
head -60 /tmp/bench_drift.diff
echo "------------------------------------------------------------------"
echo "[bench-drift] Regenerate it with:"
echo "    cargo run --release --example bench > benchmark/results/RESULTS.md"
exit 1
