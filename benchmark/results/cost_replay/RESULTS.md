# P0a — cost-replay results (2026-06-03)

**Question P0a must answer honestly:** does opt-in local-model compaction actually
SAVE Anthropic quota, or does re-summarizing bust the prompt cache by more than the
smaller prefix saves? (We must not ship/recommend a net-negative feature.)

**Reviewed:** subagent script-review + results-review + a sequential Disagree-Seeking
pass. DS confirmed the cost model and baseline-vs-compaction bust symmetry are sound
(no confound) → the SIGN is trustworthy. Magnitude is optimistic ~10–30% (see caveats).

## Method

`examples/cost_replay.rs` replays a REAL session turn-by-turn under Anthropic
prompt-cache pricing and compares two arms on the SAME transcript:
- **baseline** = production model-free pruning (reprune + strategies), no LLM.
- **compaction** = the same + a live `qwen3.5:4b` summary of the OLD slice, installed
  at re-summarization cadence `R` (messages of slice growth between summaries),
  behind the production `trigger_bytes` (200 KB) gate.

Both arms drive the real `stable_apply_to_body` path; the only difference is the
summary. Per turn we bill, at WHOLE-MESSAGE cache granularity (one changed message
invalidates the rest — Anthropic's real behavior): the leading byte-identical run is
`cache_read` (0.1×); the changed/extended region (incl. a re-summarization bust) is
`cache_creation`. We report a **billing BRACKET**: `cache_creation` at **1.25×** (the
council formula — CC caches the conversation prefix incrementally, so a bust really
re-writes at the cache-write rate) and at **1.0×** (the simplification that treats new
tokens as plain input). **The true cost lies between these bounds; the SIGN is
identical across the whole bracket, so the verdict does not depend on the exact
breakpoint physics.** `P = $3/Mtok`, ~4 bytes/token. Sessions reconstructed from real
CC `.jsonl` via `benchmark/reconstruct_session.py`.

## Results (production-faithful: trigger_bytes gate modeled)

| Session | Turns | R | #re-summ | ΔC/C @1.25× | ΔC/C @1.0× | break-even |
|---------|------:|---:|---------:|------------:|-----------:|-----------:|
| s3 | 121 | 24  | 7  | −0.73% | −2.03% | 115 |
| s2 | 121 | 24  | 9  | −4.84% | −6.10% | 73 |
| s3 | 250 | 48  | 9  | −2.70% | −3.51% | 45 |
| s2 | 250 | 48  | 11 | −1.74% | −2.99% | 221 |
| s3 | 250 | 96  | 6  | −2.30% | −3.08% | 60 |
| s2 | 250 | 96  | 6  | −0.21%¹| −1.44% | 221 |
| s3 | 400 | 96  | 9  | −5.06% | −5.32% | 60 |
| s3 | 400 | 200 | 4  | −3.77% | −3.86% | 255 |

¹ s2 @ R=96 (−0.21% @1.25×, break-even turn 221/251) is **within modeling noise**
(the ~4-bytes/token estimate alone is worth more than 0.2%) — treat as break-even,
not a real save.

## Findings

1. **Net saving across every tested session, under both billing bounds.** Once the
   production `trigger_bytes` gate is modeled, the only previously-losing row (s3 @ 121
   turns, +2.28% in the un-gated run) flips to a saving (−0.73%/−2.03%) — the gate
   delays compaction until the body is large enough that busts amortize. So the
   earlier short-session LOSS was a benchmark artifact, not feature behavior.
2. **The win comes from a smaller cached prefix (cache_read at 0.1× every turn);** the
   bust (cache_creation, occasional) is paid back because it is a one-turn event vs an
   every-turn saving. This is why even naive (no-accumulator) re-summarization wins
   once a session is past the trigger gate.
3. **Optimal cadence is non-monotonic and length-dependent** (R=48 > R=96 at 250
   turns; R=96 > R=200 at 400 turns) → a FIXED `resummarize_after` leaves money on the
   table; motivates the adaptive/cost-aware trigger (§ADJUDICATED FINAL PLAN Phase 2.5).
4. **Magnitudes are modest** (−0.7% to −6.1% input cost). The cost saving is a
   secondary benefit; the PRIMARY value is window-pressure relief on long/1M sessions
   (pre-empting CC's lossy auto-compaction) — which is only worth it if quality holds
   (see "necessary-not-sufficient" below).

## IMPORTANT caveats (do not over-read)

- **COST-POSITIVE IS NECESSARY-NOT-SUFFICIENT (Disagree-Seeking).** This measures only
  token cost, not summary FIDELITY. A 2–5% saving that drops a load-bearing fact
  (wrong path, lost constraint) is net-negative in user value. The **P0b deterministic
  harm/quality gate is the real "enable" precondition** — do NOT recommend enabling on
  cost alone.
- **Magnitude optimistic ~10–30%:** `cap_slice_start` selects the LARGEST ≤40 KB old
  sub-window (the most compressible recent-old chunk); production (no slice cap yet)
  would summarize the whole old region. Partly offset by the `summary_is_smaller` gate
  (rejects summaries that don't beat model-free pruning).
- **NAIVE re-summarization, NO accumulator.** Each re-summarization re-writes the whole
  `[start..end]` (busts the whole old prefix). The Phase-2.5 accumulator (append-only
  delta segments) should improve margins — but could ALSO erode them if the carried
  summary grows unbounded; **must be modeled (both directions) before a final ship
  number.**
- **Ship-intent harness, not current shipped code:** `summarize_live` uses the fixed
  free-form harness (/api/chat, conservative num_ctx, facts-first prompt). This is now
  the SHIPPED `call_model` (Phase 1a) — config.rs defaults to `qwen3.5:4b` and a
  runtime model-guard refuses disqualified models (Phase 1c).
- **2 real sessions, replayed at 2 cadences each (correlated rows), 1 model.**
  Directionally strong, NOT a population study. A 3rd session of different content
  density would harden it.
- **~4 bytes/token** estimate and **flat per-block-cap 8000** (ship intent is
  asymmetric: tool ~400 / reasoning ~8000). Sign is robust to these; magnitude less so.

## ACCUMULATOR arm — IMPLEMENTED + MEASURED (2026-06-04)

The accumulator is no longer modeled-only: it is implemented (opt-in config
`local_model.accumulator`, default off) and `cost_replay` now runs a THIRD arm that
exercises the real append path (`cap_slice_end` → `append_summary`). Re-summarization
APPENDS a budget-sized frozen delta segment instead of replacing the whole summary, so
older segments stay byte-frozen and the prompt cache busts only on the delta.

| Session | turns | re-summ | appends | baseline | single-summary | **accumulator** | marginal vs single |
|---------|------:|--------:|--------:|---------:|---------------:|----------------:|-------------------:|
| smoke (fd3e82b2) | 121 | 8 | 7/8 | $2.720 | +10.50% (costs more) | **−17.11% SAVES** | −24.99% |
| s2 (86544dbb) | 221 | 17 | 16/17 | $6.046 | +0.41% (≈break-even) | **−35.21% SAVES** | −35.47% |

**The accumulator is a clear, growing win — and it flips sessions where single-summary
is net-negative.** On the 121-turn session single-summary COSTS +10.5% (model-free's
cache is already efficient there) yet the accumulator SAVES −17%; on the 221-turn
session the saving grows to −35% (more frozen segments → more cache preserved as the
session lengthens). Append fired on 7/8 and 16/17 re-summarizations (the lone fallbacks
are the seed/first-summary and any delta with no ≥2-pair fit).

**Why the earlier "DEFER — modest cost" verdict was wrong:** the original (pre-fix)
accumulator gated append on the WHOLE delta fitting `SLICE_CHAR_BUDGET`; since the
adaptive trigger fires at 64 KB > the ~39 KB budget, a reasoning-dense delta almost
always exceeded it and fell back to REPLACE (measured: appends 1/8, accumulator +3.52%
— a near-no-op). A 2-reviewer council flagged this (MAJOR); the fix (`cap_slice_end` —
append a budget-sized chunk and march forward, never discard the chain) made append the
common path and turned the feature into the saver above.

Caveats unchanged (ship-intent harness = shipped call_model; 2 sessions / 1 model;
~4 bytes/token; sign robust, magnitude optimistic). The accumulator's *fidelity* benefit
(oldest facts never drift out) is proven separately by the unit test
`accumulator_appends_and_preserves_the_oldest_segments_fact`.
