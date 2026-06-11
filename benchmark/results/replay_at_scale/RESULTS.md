# Validate-at-scale: offline replay of a real ~1M-token session (2026-06-05)

Replayed a real Claude Code session (sample-game `86544dbb`, 596 turns / 1192 msgs /
2.5 MB body) OFFLINE through the production pipeline (`examples/cost_replay.rs` →
`stable_apply_to_body` + production `call_model`), read-only from a copy. This is how the
data-dependent backlog decisions get answered WITHOUT a live session. Harness now installs a
tracing subscriber so `RUST_LOG=trimwire=debug` surfaces the in-code telemetry.

## Cost (model = qwen3.5:4b, default profile, $3/Mtok, council 1.25× cache-create)
| arm | $ | ΔC/C vs baseline |
|-----|---|------------------|
| baseline (model-free prune only) | $33.00 | — |
| + single-summary compaction | $31.41 | **−4.83%** (break-even turn 231/596) |
| + ACCUMULATOR | **$12.20** | **−63.04%** (−61.16% vs single-summary) |

48 re-summarizations; accumulator appended cleanly 47/48 times.

## Finding 1 — ACCUMULATOR is the dominant long-session lever
−63% cost on a real 1M-token session (vs −4.8% for single-summary). Clean append rate
(47/48). **Strongly supports flipping `local_model.accumulator` default-on** (backlog #10) —
a maintainer product call, now backed by real data (consistent with the prior −17/−35% on
121/221-turn sessions).

## Finding 2 — escalation tiers (#1): accumulator reached 48 / 64 segments
On a 596-turn / ~1M session the accumulator used **48 of the MAX_SUMMARY_SEGMENTS=64** cap
(75%). So it KEEPS UP well below the wall here, but a ~1.3× longer session WOULD hit 64 and
fall back to a REPLACE collapse. This confirms the design council: the eventual lever near the
cap is a **bigger budget/cap (MAX_SUMMARY_SEGMENTS)**, NOT keep_recent_turns tiers. Re-open
escalation only when a real session is observed hitting 64 before its wall. Still deferred.

## Finding 3 — minimum-savings gate (#2): CLOSED, not worth it
The decision metric is the MARGINAL savings of a re-checkpoint vs replaying the OLD decisions
on the longer array. Measured directly (`re-checkpoint marginal-vs-replay` debug log):
**0 of 104 re-checkpoints had marginal savings below 32 KB** — every bust reclaims ≥32 KB that
replaying stale decisions would miss (the newly-aged turns are tool-heavy and prune well).
The min-savings gate would NEVER fire on a real session → P0 #2 is CLOSED (upgraded from
"deferred"). Reviewer A was right.

## Instruments landed (cache-neutral, DEBUG-gated)
- `reprune re-checkpoint (cache bust)` — `grew` / `checkpoint_len` / `saved_bytes` (total vs raw)
- `reprune re-checkpoint marginal-vs-replay` — `marginal_saved` (the #2 decision metric)
- `cost_replay` now installs a tracing subscriber (RUST_LOG=trimwire=debug).

## Length sweep — 4 real sessions, offline replay (2026-06-05)

Replayed 4 real Claude Code sessions of varying length/shape through the production
pipeline (model-free + accumulator) to validate the accumulator across sizes and test the
MAX_SUMMARY_SEGMENTS=64 cap. qwen3.5:4b, default profile, $3/Mtok, 1.25× cache-create.

| session | turns | msgs | body | re-summ | segments (/64) | append rate | accumulator ΔC vs baseline | small-marginal (<32KB) |
|---------|------:|-----:|-----:|--------:|---------------:|-------------|---------------------------:|------------------------:|
| 109f2e81 | 79  | 157  | 0.5 MB | 6   | 6  | 5/6   | **−17.4%** | 0/45 |
| 86544dbb | 596 | 1192 | 2.5 MB | 48  | **48** | 47/48 | **−63.0%** | 0/104 |
| 4c97e044 | 1208| 2415 | 3.7 MB | 100 | 36 | 35/100| **−55.9%** | 0/723 |
| 17d5d249 | 925 | 1850 | 36 MB  | 76  | 13 | 12/76 | **−4.2%**  | 0/549 |

### Findings
1. **Accumulator ALWAYS saves on long sessions (−4.2% to −63%); never a loss.** Magnitude tracks
   the reasoning-vs-tool-output mix: reasoning-dense 86544dbb = −63%; the 36 MB tool-output-heavy
   17d5d249 = only −4.2% (model-free strips most of its tool mass before the summarizer ever runs,
   leaving little marginal for the summary). Even the 79-turn short session saved −17%. → flipping
   `accumulator` default-on is validated across all session sizes.
2. **#2 minimum-savings gate: CONCLUSIVELY CLOSED. 0 of 1,421 re-checkpoints (across all 4 sessions)
   had <32 KB marginal savings.** The gate would never fire on a real session. Settled.
3. **Escalation #1 (64-segment cap) is now EMPIRICALLY MOTIVATED, not speculative.** Max observed =
   **48/64 segments** on the densest 596-turn session (86544dbb). Segments track reasoning density +
   append-fit, NOT raw turns (1208-turn med→36; 925-turn 36 MB big→13; 596-turn dense→48). A
   reasoning-dense session ~1.3× longer than 86544dbb would hit 64 → REPLACE collapse (fidelity
   cliff). So a bigger `MAX_SUMMARY_SEGMENTS` / budget is an evidence-backed recommendation for the
   heaviest reasoning-dense sessions. NOT urgent (no tested session hit 64), but no longer speculative.
   Append rate also degrades on bigger/tool-heavy sessions (47/48 dense → 35/100 med → 12/76 big): the
   delta often doesn't fit the append budget and falls back to REPLACE — a second lever (bigger
   `cap_slice_end` budget) worth noting alongside the segment cap.

## REAL-SESSION accumulator validation (2026-06-05) — −63% reproduced on real traffic
Until now the −63% accumulator figure was OFFLINE-replay only (the top "peak" blind-spot).
Validated via `examples/cost_replay` on REAL reconstructed Claude Code sessions (copies of
~/.claude transcripts via reconstruct_session.py; bodies never committed):

| real session | turns | baseline | single-summary | **accumulator** | segments |
|---|---|---|---|---|---|
| 37-msg  | 19  | $0.74  | +18.4% (costs more) | +18.4% (too short to append) | 1 |
| 165-msg | 83  | $3.26  | +14.6% (costs more) | **−13.8% saves** (−24.8% vs single) | 5 (4 appends) |
| 981-msg | 491 | $27.43 | +2.0% (costs more)  | **−64.6% saves** (−65.3% vs single) | 28 (27 appends) |

CONCLUSIONS: (1) the −63% headline IS reproduced on a real long session (981-msg: −64.6%);
(2) **single-summary compaction COSTS MORE on every real session** (+2-18%) — the accumulator
is load-bearing, it's the difference between a cost loss and a 64% win; (3) short sessions still
cost more (the known caveat — the cache-bust isn't paid back). So: accumulator ON (default,
already shipped) is validated on real traffic; the bigger the session, the bigger the win.
