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

echo "[bench-drift] regenerating bench (offline, deterministic)…"
( cd "$ROOT" && cargo run --release --quiet --example bench ) > "$FRESH"

# Drop the host-dependent timing section (§7 → EOF) from both sides before diff.
norm() { sed '/^## 7\. Gateway overhead/,$d' "$1"; }

if diff <(norm "$COMMITTED") <(norm "$FRESH") > /tmp/bench_drift.diff 2>&1; then
  echo "[bench-drift] OK — RESULTS.md matches current bench output (timing section ignored)."
  exit 0
fi

echo "[bench-drift] FAIL: benchmark/results/RESULTS.md is STALE (drift in the deterministic sections):"
echo "------------------------------------------------------------------"
head -60 /tmp/bench_drift.diff
echo "------------------------------------------------------------------"
echo "[bench-drift] Regenerate it with:"
echo "    cargo run --release --example bench > benchmark/results/RESULTS.md"
exit 1
