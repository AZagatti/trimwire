#!/usr/bin/env bash
# Measure the ACTUAL resident RAM of an ollama model on THIS box — do NOT trust the
# published/disk numbers (weights + KV cache + runtime overhead vary by num_ctx, quant,
# and flash-attention). Used to confirm a PRO-tier candidate genuinely fits one-at-a-time
# before spending a gut-read on it.
#
# Usage:   bash benchmark/ram_probe.sh MODEL [NUM_CTX]
# Example: bash benchmark/ram_probe.sh qwen3.5:9b 8192
#          for m in qwen3.5:4b qwen3:8b; do bash benchmark/ram_probe.sh "$m"; done
#
# Method (mirrors the council recipe): stop everything → sample `free` AVAILABLE →
# load the model with keep_alive=-1 + one tiny inference at NUM_CTX → re-sample AVAILABLE
# and /api/ps → unload. ONE model at a time; OLLAMA_FLASH_ATTENTION cuts KV-cache RAM.
#
# PRIMARY number = `/api/ps` size (ollama's own weights+graph accounting). The `free`
# AVAILABLE delta is reported too but is SECONDARY and UNRELIABLE on a warm box: once a
# model has been pulled, its weights are mmap'd from the page cache (buff/cache), so
# loading it barely moves AVAILABLE — the delta then badly UNDERSTATES footprint. Trust
# /api/ps for the fit verdict; read the AVAILABLE delta only as "fresh headroom impact".
set -u
OLLAMA="${TRIMWIRE_BENCH_ENDPOINT:-http://localhost:11434}"
MODEL="${1:?usage: ram_probe.sh MODEL [NUM_CTX]}"
NUM_CTX="${2:-8192}"
# 13 GB box: leave ~2 GB for the OS + the user's Claude Code; treat >11 GB resident as
# "does not fit one-at-a-time".
FIT_CEIL_MB="${TRIMWIRE_RAM_FIT_CEIL_MB:-11000}"
export OLLAMA_FLASH_ATTENTION="${OLLAMA_FLASH_ATTENTION:-1}"

avail_mb() { free -m | awk 'NR==2{print $7}'; }

ollama stop "$MODEL" >/dev/null 2>&1
sleep 2
before=$(avail_mb)

# Load forever (so it's still resident when we sample) + one tiny inference to force the
# weights + KV cache to actually allocate.
curl -s "$OLLAMA/api/generate" \
  -d "{\"model\":\"$MODEL\",\"prompt\":\"ping\",\"keep_alive\":-1,\"stream\":false,\"options\":{\"num_ctx\":$NUM_CTX}}" \
  >/dev/null
during=$(avail_mb)
ps_json=$(curl -s "$OLLAMA/api/ps")

delta=$(( before - during ))
ps_mb=$(printf '%s' "$ps_json" \
  | jq -r --arg m "$MODEL" '.models[]? | select(.name==$m or .model==$m) | (.size/1048576) | floor' 2>/dev/null \
  | head -1)

printf 'model=%s  num_ctx=%s  flash_attn=%s\n' "$MODEL" "$NUM_CTX" "$OLLAMA_FLASH_ATTENTION"
printf '  /api/ps size:   %sMB   (PRIMARY — weights+graph)\n' "${ps_mb:-?}"
printf '  free AVAILABLE: %sMB -> %sMB   (delta ~%sMB; SECONDARY, page-cache-confounded)\n' \
  "$before" "$during" "$delta"
# Fit verdict from /api/ps (the reliable number); fall back to the delta only if ps is
# unavailable. Leaves ~2 GB headroom on a 13 GB box.
fit_mb="${ps_mb:-$delta}"
if [ "${fit_mb:-999999}" -le "$FIT_CEIL_MB" ]; then
  printf '  fits one-at-a-time (<=%sMB): YES\n' "$FIT_CEIL_MB"
else
  printf '  fits one-at-a-time (<=%sMB): NO / borderline — measure before committing\n' "$FIT_CEIL_MB"
fi

ollama stop "$MODEL" >/dev/null 2>&1
