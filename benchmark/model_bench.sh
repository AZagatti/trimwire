#!/usr/bin/env bash
# trimwire local-model summarization benchmark (reusable).
#
# Compares LOCAL ollama models at compacting a coding-session slice via /api/chat.
# DEFAULT HARNESS = "freeform" (no format constraint) — the council-converged
# winner for the good <=4B models (grammar/JSON-schema fragments verbatim
# identifiers on <=2.5-3B models; schema only rescued the coder models). The
# SYSTEM_FREEFORM prompt below is a COPY of SUMMARY_SYSTEM_PROMPT in
# src/summarizer/api.rs and MUST be kept in sync. A "schema" mode is still
# available (TRIMWIRE_BENCH_HARNESS=schema) for comparison only. Common to both:
# <excerpt> delimiters, stop sequences, near-greedy sampling + fixed seed,
# correct num_ctx (ollama's 4096 default truncates from the START), per-family
# think handling. Measures RAM x fact-retention x compression x speed.
#
# Reusable: override the model list / context / endpoint via env, e.g. on a
# bigger machine (Mac, or WSL with more RAM) for the PRO tier:
#   TRIMWIRE_BENCH_MODELS="qwen3:8b granite4.1:8b qwen3.5:9b gemma3:12b-it-qat" \
#   TRIMWIRE_BENCH_NUM_CTX=8192 bash benchmark/model_bench.sh
#
# RAM-safe: keep_alive short, `ollama stop` between models, free-RAM skip guard.
set -u
OLLAMA="${TRIMWIRE_BENCH_ENDPOINT:-http://localhost:11434}"
RESULTS="${TRIMWIRE_BENCH_RESULTS:-/tmp/model_bench_results.tsv}"
LOG="${TRIMWIRE_BENCH_LOG:-/tmp/model_bench.log}"
NUM_CTX="${TRIMWIRE_BENCH_NUM_CTX:-8192}"
NUM_PREDICT="${TRIMWIRE_BENCH_NUM_PREDICT:-768}"   # tight ceiling for free-form (observed max ~707); guards against budget-ignoring rambling
MIN_FREE_MB="${TRIMWIRE_BENCH_MIN_FREE_MB:-3000}"
: > "$LOG"

# Default: small models that fit ~7GB free. Override with TRIMWIRE_BENCH_MODELS.
DEFAULT_MODELS="qwen2.5-coder:1.5b qwen2.5-coder:3b qwen3:1.7b qwen3.5:2b llama3.2:3b granite4.1:3b phi4-mini:3.8b qwen3.5:4b"
read -r -a MODELS <<< "${TRIMWIRE_BENCH_MODELS:-$DEFAULT_MODELS}"

read -r -d '' SLICE <<'EOF'
### assistant
Planning the change. Read the auth module first.
[tool_use Bash] {"command":"sed -n 1,200p src/auth/session_7421.rs"}
### user
[tool_result] Opened src/auth/session_7421.rs (210 lines). It defines fn reconcile_balances() and currently uses a blocking call. (then 30 lines of routine import/log noise that should be dropped) imports resolved, no warnings, build cache warm, lints clean, nothing notable, boilerplate, boilerplate, boilerplate, boilerplate.
### assistant
Run the build to see the failure.
[tool_use Bash] {"command":"cargo build"}
### user
[tool_result] Compiling (long progress noise) error[E0277]: the trait bound `Job: Send` is not satisfied — the spawned task captures a non-Send handle. note: required by a bound in tokio::spawn. (then dozens of irrelevant Compiling lines, progress, progress, progress, blah blah).
### assistant
Decide on the storage backend for the ledger after weighing options.
[tool_use Bash] {"command":"echo decision"}
### user
[tool_result] Decision: chose SQLite over Postgres for the ledger (single-file, no server). Set MAX_RETRIES = 5 for the writer. (irrelevant trailing noise: cache stats, timing, gc, noise, noise, noise, noise, noise).
### assistant
Try connecting to the dev DB to verify.
[tool_use Bash] {"command":"psql -h 127.0.0.1"}
### user
[tool_result] Connection failed: ECONNREFUSED on 127.0.0.1. The daemon listens on port 8765; the audit sink is gated behind the TRIMWIRE_AUDIT env var. (then a wall of routine retry/backoff log lines with no decision value, retrying, retrying, retrying, retrying, retrying).
### assistant
Apply the migration and run the suite.
[tool_use Bash] {"command":"make migrate && cargo test"}
### user
[tool_result] Applied src/db/migrate_v9.sql; 37 tests passed. TODO: handle the leap-second edge case in reconcile_balances. (trailing noise: test timing breakdown, per-file coverage dump, numbers that do not matter, end).
EOF

FACTS=( "session_7421.rs" "migrate_v9.sql" "e0277" "job: send" "econnrefused" \
        "sqlite" "max_retries" "reconcile_balances" "trimwire_audit" "8765" \
        "leap-second" )

# Real-session mode: override the synthetic slice + facts with files extracted
# from a real transcript (see /tmp/extract_slice.py). Facts are one-per-line,
# lowercase. Use with a larger TRIMWIRE_BENCH_NUM_CTX (e.g. 16384) for big slices.
if [ -n "${TRIMWIRE_BENCH_SLICE_FILE:-}" ] && [ -f "${TRIMWIRE_BENCH_SLICE_FILE}" ]; then
  SLICE="$(cat "${TRIMWIRE_BENCH_SLICE_FILE}")"
fi
if [ -n "${TRIMWIRE_BENCH_FACTS_FILE:-}" ] && [ -f "${TRIMWIRE_BENCH_FACTS_FILE}" ]; then
  mapfile -t FACTS < "${TRIMWIRE_BENCH_FACTS_FILE}"
fi

# Harness mode: "freeform" (no format constraint — best for <=2.5B models per the
# council + grammar-constrained-decoding literature) or "schema" (grammar-locked
# JSON — helps >=3B structured-output-trained models, hurts tiny ones). No 1-shot
# example in either (it crowds a small model's context). Toggle with TRIMWIRE_BENCH_HARNESS.
HARNESS="${TRIMWIRE_BENCH_HARNESS:-freeform}"

# JSON schema for schema mode. required:[] on purpose — forcing all fields makes
# the grammar mask tokens harder and hurts quality (council finding).
read -r -d '' SCHEMA <<'EOF'
{"type":"object","properties":{
"goal":{"type":"string"},
"files":{"type":"array","items":{"type":"string"}},
"decided":{"type":"array","items":{"type":"string"}},
"errors":{"type":"array","items":{"type":"string"}},
"facts":{"type":"array","items":{"type":"string"}},
"next":{"type":"array","items":{"type":"string"}}},
"required":[]}
EOF

read -r -d '' SYSTEM_FREEFORM <<'EOF'
RULES (violations are FAILURES):
1. Copy VERBATIM, character-for-character, never paraphrase: every file path, error code (error[E0277]-style), identifier, port, env var, version, command, number.
2. Output MUST be shorter than the excerpt — aim for ≤25% of its length. Prefer shorter: the main model can re-read files for detail. Drop tool-call boilerplate, progress/log spam, repeated scaffolding, and exploration that led nowhere.
3. Capture the state at the END of the excerpt: NEXT must be what was actually left open/in-progress, not work already completed. If the excerpt ends mid-assistant-turn (the last turn is incomplete), NEXT is what that turn was in the middle of doing.
4. No preamble or closing remarks — start directly with GOAL. Do not reproduce raw tool output. Do not invent anything.
5. Do NOT mark work finished unless the excerpt explicitly shows it completed. Never write 'fixed', 'done', 'resolved', 'implemented', or 'complete' for an item still open or in progress. For a multi-item task (a review queue, a finding list, a checklist), state progress as 'N of M complete' and list EVERY still-open item in NEXT. When unsure whether something finished, treat it as still open.
6. Before writing NEXT, scan the last ~5 assistant turns for any task ANNOUNCED or STARTED with no completing edit/output/confirmation in the excerpt (e.g. 'now I'll…', 'next step…', a file only located/grepped but not yet edited, a 'Task N' just begun): list each in NEXT as still-open — an announced-or-only-located task is OPEN, never done. Do NOT put already-finished work (completed edits, commits, done task-status updates) in NEXT.

You are a coding-session compactor. Summarize the excerpt into ONLY these sections (omit any that are empty), terse:
GOAL: <one line — what the coding SESSION is trying to accomplish, NOT a description of this summarization task>
FILES: <ONLY paths an edit/write/create/move actually targeted in this excerpt, each copied verbatim from the text, one per line; never a file merely read, grepped, or referenced; if you cannot copy a path exactly as written, omit it rather than guess a directory or extension>
DECIDED: <decisions made, verbatim identifiers inline>
ERRORS: <error codes/messages, verbatim, or omit>
FACTS: <other exact identifiers, numbers, ports, env vars, versions, commands>
NEXT: <what was about to happen / left open at the end>
EOF

read -r -d '' SYSTEM_SCHEMA <<'EOF'
You are a coding-session summarizer. Extract ONLY load-bearing facts from the excerpt into the JSON structure.
CRITICAL: every file path, error code (error[E0277]-style), identifier, env var, port, number, version, command MUST appear VERBATIM in the appropriate field. Copy file paths character-for-character including directories and extension. A missing fact is a FAILURE; extra tokens are acceptable. Do not invent anything.
Drop: tool-call boilerplate, repeated scaffolding, intermediate steps, generic descriptions with no decision attached.
Fields: goal (one line); files (verbatim paths); decided (decisions); errors (verbatim error codes/messages); facts (numbers/ports/env vars/versions/commands); next (next actions).
EOF

if [ "$HARNESS" = "schema" ]; then SYSTEM="$SYSTEM_SCHEMA"; else SYSTEM="$SYSTEM_FREEFORM"; fi
USERMSG="<excerpt>
$SLICE
</excerpt>

Compact the excerpt above into the sections below."

printf 'model\tweights_mb\tload_s\tprompt_tok\tgen_tok\tprefill_tps\tgen_tps\ttotal_s\tout_chars\tcompress_pct\tretention\tfcs\n' > "$RESULTS"
SLICE_CHARS=${#SLICE}
echo "[bench] harness=$HARNESS; slice=$SLICE_CHARS chars; num_ctx=$NUM_CTX; loadavg=$(awk '{print $1}' /proc/loadavg); models=${MODELS[*]}; $(date)" | tee -a "$LOG"

strip_think() { sed -z 's#<think>.*</think>##g' 2>/dev/null; }

for M in "${MODELS[@]}"; do
  AVAIL=$(free -m | awk '/^Mem:/{print $7}')
  echo "=== $M (free ${AVAIL}MB) ===" | tee -a "$LOG"
  if [ "$AVAIL" -lt "$MIN_FREE_MB" ]; then
    for x in "${MODELS[@]}"; do ollama stop "$x" >/dev/null 2>&1; done; sleep 5
    AVAIL=$(free -m | awk '/^Mem:/{print $7}')
    if [ "$AVAIL" -lt "$MIN_FREE_MB" ]; then
      echo "[bench] STILL LOW RAM (${AVAIL}MB); skipping $M" | tee -a "$LOG"
      printf '%s\tLOW_RAM\n' "$M" >> "$RESULTS"; continue
    fi
  fi
  if ! ollama pull "$M" >>"$LOG" 2>&1; then
    echo "[bench] pull FAILED $M" | tee -a "$LOG"; printf '%s\tPULL_FAIL\n' "$M" >> "$RESULTS"; continue
  fi

  TOPP=0.9; case "$M" in qwen3*) TOPP=0.8 ;; esac
  RP=1.1; [ "$HARNESS" = "schema" ] && RP=1.15   # grammar can loop → slightly higher penalty
  AVAIL_BEFORE=$(free -m | awk '/^Mem:/{print $7}')
  PAYLOAD=$(jq -n --arg m "$M" --arg s "$SYSTEM" --arg u "$USERMSG" \
    --argjson nc "$NUM_CTX" --argjson np "$NUM_PREDICT" --argjson topp "$TOPP" --argjson rp "$RP" \
    '{model:$m, stream:false, keep_alive:"20s",
      messages:[{role:"system",content:$s},{role:"user",content:$u}],
      options:{temperature:0.1, top_p:$topp, top_k:20, min_p:0, repeat_penalty:$rp, repeat_last_n:64,
               seed:42, num_predict:$np, num_ctx:$nc, stop:["<|im_end|>","\n\n---","\nNote:","\nSummary:","\nConclusion:"]}}')
  if [ "$HARNESS" = "schema" ]; then PAYLOAD=$(printf '%s' "$PAYLOAD" | jq --argjson schema "$SCHEMA" '. + {format:$schema}'); fi
  case "$M" in qwen3*) PAYLOAD=$(printf '%s' "$PAYLOAD" | jq '. + {think:false}') ;; esac

  RESP=$(curl -s --max-time 1200 "$OLLAMA/api/chat" -d "$PAYLOAD")
  AVAIL_DURING=$(free -m | awk '/^Mem:/{print $7}')
  WEIGHTS_MB=$(curl -s "$OLLAMA/api/ps" | jq -r --arg m "$M" '.models[]? | select(.name==$m or (.model==$m)) | .size' 2>/dev/null | head -1)
  WEIGHTS_MB=$(awk -v b="${WEIGHTS_MB:-0}" 'BEGIN{printf "%.0f", b/1048576}')
  FOOTPRINT_MB=$(( AVAIL_BEFORE - AVAIL_DURING )); [ "$FOOTPRINT_MB" -lt 0 ] && FOOTPRINT_MB=0

  RAW=$(printf '%s' "$RESP" | jq -r '.message.content // ""' 2>/dev/null)
  RESPONSE=$(printf '%s' "$RAW" | strip_think)
  if [ -z "$RESPONSE" ]; then
    echo "[bench] $M NO_RESPONSE: $(printf '%s' "$RESP" | head -c 200)" | tee -a "$LOG"
    printf '%s\t%s\t%s\tNO_RESPONSE\n' "$M" "$WEIGHTS_MB" "$FOOTPRINT_MB" >> "$RESULTS"; ollama stop "$M" >/dev/null 2>&1; continue
  fi
  PTOK=$(printf '%s' "$RESP" | jq -r '.prompt_eval_count // 0')
  GTOK=$(printf '%s' "$RESP" | jq -r '.eval_count // 0')
  LOAD_S=$(awk -v n="$(printf '%s' "$RESP" | jq -r '.load_duration // 0')" 'BEGIN{printf "%.1f", n/1e9}')
  TOTAL_S=$(awk -v n="$(printf '%s' "$RESP" | jq -r '.total_duration // 0')" 'BEGIN{printf "%.1f", n/1e9}')
  PREFILL_TPS=$(awk -v t="$PTOK" -v n="$(printf '%s' "$RESP" | jq -r '.prompt_eval_duration // 0')" 'BEGIN{if(n>0)printf "%.0f", t/(n/1e9); else print "0"}')
  GEN_TPS=$(awk -v t="$GTOK" -v n="$(printf '%s' "$RESP" | jq -r '.eval_duration // 0')" 'BEGIN{if(n>0)printf "%.0f", t/(n/1e9); else print "0"}')
  OUT_CHARS=${#RESPONSE}
  COMPRESS=$(awk -v o="$OUT_CHARS" -v i="$SLICE_CHARS" 'BEGIN{printf "%.0f", 100*o/i}')
  LC=$(printf '%s' "$RESPONSE" | tr 'A-Z' 'a-z')
  KEPT=0; for f in "${FACTS[@]}"; do case "$LC" in *"$f"*) KEPT=$((KEPT+1));; esac; done
  RET="$KEPT/${#FACTS[@]}"
  # FCS = faithful-compression score: retention x (1 - compression), 0..100.
  # Rewards keeping facts AND actually shrinking; a verbatim copy or a fact-dropper both score low.
  FCS=$(awk -v k="$KEPT" -v n="${#FACTS[@]}" -v o="$OUT_CHARS" -v i="$SLICE_CHARS" \
    'BEGIN{r=(n>0?k/n:0); c=(i>0?o/i:1); v=r*(1-c); if(v<0)v=0; printf "%.0f", v*100}')
  printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n' \
    "$M" "$WEIGHTS_MB" "$LOAD_S" "$PTOK" "$GTOK" "$PREFILL_TPS" "$GEN_TPS" "$TOTAL_S" "$OUT_CHARS" "$COMPRESS" "$RET" "$FCS" >> "$RESULTS"
  echo "[bench] $M -> weights ${WEIGHTS_MB}MB, ${TOTAL_S}s, out ${OUT_CHARS}c/${GTOK}tok (${COMPRESS}%), retention ${RET}, FCS ${FCS}" | tee -a "$LOG"
  # Full summary saved for re-scoring / human + subagent review (not truncated).
  SUMDIR="${TRIMWIRE_BENCH_SUMDIR:-/tmp/real_summaries}"; mkdir -p "$SUMDIR"
  printf '%s' "$RESPONSE" > "$SUMDIR/${HARNESS}__${M//[:\/]/_}.txt"
  { echo "----- $M summary -----"; printf '%s' "$RESPONSE" | head -c 1800; echo; } >> "$LOG"
  ollama stop "$M" >/dev/null 2>&1; sleep 2
done
echo "[bench] DONE $(date)" | tee -a "$LOG"
{ echo "===== RESULTS (harness=$HARNESS) ====="; column -t -s$'\t' "$RESULTS"; } | tee -a "$LOG"
