# P0b — deterministic planted-fact harm gate (2026-06-03)

**Question P0b must answer:** independent of the (correlated, all-Claude) blind gut-read
judges, does the local-model summary PRESERVE the load-bearing facts (file paths,
error codes, decisions, constants) a resuming agent needs — and does qwen3.5:4b beat
qwen3.5:2b objectively? This is the **real "enable" precondition** (Disagree-Seeking:
P0a cost-positive is necessary-not-sufficient; a 2–5% saving that drops a load-bearing
fact is net-negative).

**Method.** `examples/compaction_harm.rs` summarizes an OLD slice with the SHIP-INTENT
free-form harness (/api/chat, conservative num_ctx, facts-first prompt — NOT the buggy
production `call_model`), then checks how many hand-picked load-bearing facts survive
(normalized match: case-insensitive, hyphen≡underscore). Gate threshold 0.90. We also
manually scan each summary for HALLUCINATED identifiers (fabricated paths/codes).

## Results

| Slice | Facts | qwen3.5:4b | qwen3.5:2b |
|-------|------:|-----------:|-----------:|
| Synthetic planted-fact (12 load-bearing) | 12 | **11/12 = 91.7% PASS** | 9/12 = 75% FAIL |
| Real slice `g_fixes` (40 KB), END-STATE facts | 6 | **6/6 = 100% PASS** | 5/6 = 83.3% FAIL |
| Hallucinated identifiers (manual scan) | — | **none** | none |

- **4b misses:** synthetic — only "37 tests" (a transient run count, the least
  load-bearing of the set); real — none.
- **2b misses:** synthetic — `reconcile_balances` (function name) + `TRIMWIRE_AUDIT`
  (env var); real — `game-engine.md` (a doc reference). All genuinely load-bearing.

## Verdict

- **qwen3.5:4b PASSES** on both a synthetic and a real slice, no hallucination → the
  harm/quality precondition is met for the default model.
- **qwen3.5:2b FAILS** both → **4b is the REQUIRED default; 2b is NOT a harm-safe
  "lighter" option** (offer only with a loud warning, if at all). The 4b>2b ordering,
  previously MODERATE (correlated judges), is now **HIGH** via this independent gate.

## Methodology lesson (honest — re-confirms why FCS was retired)

A first real-slice run scored 4b at only 33% — but the SUMMARY was faithful and
hallucination-free; the chosen facts were all **completed early work** (a doc-reorg
that finished mid-slice), which a compactor is SUPPOSED to compress (the harness rule:
"NEXT = what's left open, not work already completed"). Re-curating to **end-state /
still-relevant** load-bearing facts flipped 4b to 100%. **Lesson: planted-fact harm
gating is only valid with facts that are (a) still load-bearing at the END of the
slice and (b) on a COHERENT slice** — arbitrary/early-completed facts produce
misleading scores. This is the same trap that invalidated the FCS metric for ranking.

## Lightweight tier (is there a sub-4b model better than 2b for low-RAM?)

Maintainer question. Ran the same gate on the sub-4b candidates:

| Model | RAM | Synthetic (12 facts) | Real `g_fixes` (6 end-state) |
|-------|----:|---------------------:|-----------------------------:|
| qwen3.5:**4b** | 3.4 GB | **91.7%** | **100%** |
| qwen3.5:2b | 2.7 GB | 75% | 83% |
| **llama3.2:3b** | 2.0 GB | 83% | **67%** (dropped the Playwright-MCP decision + game-engine.md) |
| qwen3:1.7b | 1.4 GB | 58% | 67% |

**Conclusion: NO sub-4b model is dependably better than 2b.** llama3.2:3b BEAT 2b on the
synthetic slice but LOST on the real one (and dropped a load-bearing decision both 4b
and 2b kept) — it just trades *which* facts it loses. So **4b stays the only harm-safe
model; below it (2b, llama3.2:3b ~70–83%, content-dependent) is best-effort with real
risk of dropping load-bearing facts.** Do NOT swap 2b for llama3.2:3b. The real
low-RAM levers are the `trigger_bytes` gate, `keep_alive=0` (unload after batch), the
model-guard "unverified fidelity" warning, and the Phase-2.5 slice-size cap (smaller
input → less to lose). **Methodology note:** the synthetic result alone would have
wrongly recommended llama3.2:3b — the real-slice corroboration caught it. Always
corroborate a model recommendation on real content.

## FINAL harm gate (current shipped harness: 1b asymmetry + 2.5a cap + facts-first + /api/chat + num_ctx)

Gut-read on 3 NEW real slices (concrete coding sessions; qwen3.5:4b summaries blind-judged
+ a sequential Disagree-Seeking pass that re-verified hallucination/NEXT against the raw):

| Slice | Hallucinated identifiers | NEXT correct | Verdict |
|-------|--------------------------|--------------|---------|
| fh1 (PR-review fix queue) | none | yes (lists open tasks) | PASS |
| fh2 (QA + docs trim) | none | yes | PASS |
| fh3 (PR-poll loop) | none | yes | PASS |

**3/3 PASS — zero hallucinated identifiers, NEXT correct on all 3.** Joined with P0b
(synthetic 91.7%, real g_fixes 100%) → qwen3.5:4b is harm-validated on the shipped
harness. **The harm gate PASSES; the feature is safe to enable (opt-in, off-by-default).**

**Mandatory caveat — ✅ RESOLVED 2026-06-04 (see "PROMPT-HARDENING re-validation" below):**
the summarizer can OVERSTATE completion in the FACTS section (fh1 said "5 HIGH/12 MEDIUM
findings fixed" when only ~2 were done at cut). It's rescued because NEXT correctly lists
the still-open tasks, so a resuming agent following NEXT won't skip them. **Recommended
near-term hardening (tracked, not blocking):** add a prompt rule — FACTS must not say
"fixed" for any finding still listed in NEXT; use "N of M complete" / "partially
addressed". fh2's defect was the SAFE direction (understated a done edit → re-applying
fails cleanly). Raw slices kept OUT of the repo (other projects' transcripts).

## PROMPT-HARDENING re-validation (2026-06-04) — the FACTS-overstatement fix shipped + re-gated

The "mandatory caveat" above (summarizer can overstate completion) is now **fixed**.
Added **rule 5** to `SUMMARY_SYSTEM_PROMPT` (mirrored atomically into the 3 copies —
`model_bench.sh` SYSTEM_FREEFORM + `examples/{compaction_harm,cost_replay}.rs`; guarded
by a unit-test assertion in `system_prompt_is_facts_first`):

> *5. Do NOT mark work finished unless the excerpt explicitly shows it completed. Never
> write 'fixed'/'done'/'resolved'/'implemented'/'complete' for an item still open or in
> progress. For a multi-item task (a review queue, a finding list, a checklist), state
> progress as 'N of M complete' and list EVERY still-open item in NEXT. When unsure
> whether something finished, treat it as still open.*

**Re-gated at FULL rigor on the hardened prompt** — regenerated qwen3.5:4b summaries for
the same 3 real slices (fh1 = PR-review/multi-finding fix queue — the original offender;
fh2 = QA + docs trim; fh3 = PR-poll loop), then a **blind judge** + a **sequential
Disagree-Seeking** pass, both reading each summary against the full raw slice:

| Slice | Overstatement of completion | Hallucinated identifiers | NEXT correct | Verdict |
|-------|-----------------------------|--------------------------|--------------|---------|
| fh1 (PR-review fix queue) | **GONE** — findings now listed by severity in FACTS (DS-E1/SEC-H2/SEC-H1 HIGH); the in-flight SEC-H1 cap sits in NEXT (Tasks 14–21), not claimed done | none | yes | PASS |
| fh2 (QA + docs trim) | none | none | yes (forward work: fix fake-ad.tsx then record it) | PASS¹ |
| fh3 (PR-poll loop) | none | none | yes | PASS |

**Result: the overstatement is eliminated and NO regression was introduced** — zero
hallucinated identifiers and correct NEXT on all three, same as the pre-hardening gate.
fh1's old "5 HIGH/12 MEDIUM fixed" overstatement does not recur; it now lists findings by
severity and keeps the open tasks in NEXT.

¹ **fh2 honest note (recoverable, NOT a harm, NOT caused by rule 5):** the blind judge
first flagged fh2 FAIL on two points; the Disagree-Seeking pass overturned both against
raw line-cites — (a) NEXT's "update QA-REPORT.md … v5 entry" reads as recording the *new*
fake-ad fix (legitimate forward work), and even on the literal reading the model's own
ERRORS line documents its Read-before-Edit pattern, so a resuming agent re-reads and won't
blind-duplicate the already-written v5 row (safe-direction understatement, same class the
pre-hardening gate already accepted for fh2); (b) `dev-menu.tsx` is omitted from FILES
though edited for B4/B5 — but DECIDED names it with the fix mechanism, same minor
FILES-coverage class as fh1's `consistency.ts` omission. **Known minor limitation (tracked,
not blocking):** the 4B model occasionally omits an edited file from FILES when it is
already named in DECIDED. Not a state-corruption risk; would need a further prompt iteration
+ re-gate to chase.

**Integration smoke (real production path vs live ollama):** `examples/compaction_bench`
on a reconstructed 241-turn / 666 KB real-session body — the actual `call_model`
(hardened `SUMMARY_SYSTEM_PROMPT`) → install gate (summary < model-free → KEEP) →
reprune replay = **PASS, pairing valid, no orphaned pairs**, 10.6 s latency. Confirms the
hardened prompt flows through the shipped code path and preserves the cache-stable replay
invariant. (Quality was gated on the properly-`cap_slice_start`-sized 40 KB slices above;
this smoke exercises the code path + replay.)

## Caveats

- Synthetic slice is fabricated (committed); the real slice is another project's
  transcript (kept OUT of the repo for privacy — only aggregate numbers reported here).
- 1 synthetic + 1 real slice, normalized substring match (paraphrase OK, fact-drop
  not). Hallucination check is a manual scan, not automated.
- Pairs with the blind 5-slice gut-read (qualitative, also favored 4b) and P0a (cost).
- ✅ DONE (2026-06-04): a deterministic MOCK-model harm test in the CI suite (no live
  ollama) guards the gate LOGIC — `harm_gate_retention_logic_is_deterministic` in
  `src/local_compactor/mod.rs` drives the real `call_model` with a wiremock CANNED
  summary, then asserts `fact_retention`/`normalize_fact` (separator- + case-insensitive
  matching; a dropped load-bearing fact pulls retention below the 0.90 gate). The gate's
  pure logic was factored into `local_compactor::{normalize_fact, fact_retention}` (the
  example now calls them, so the CI test pins the SAME code the gate runs).

## RULE 6 cross-qwen RE-GATE (2026-06-04) — false-done FIXED across the family

Rule 6 (active anti-false-done enumeration; single call, think:false) was re-gated on the
whole approved qwen family. New-rule summaries generated for qwen3.5:4b / 4b-q8_0 / 9b / 2b
on 3 diverse real slices (b_s1 Phase-9c, b_s2 = the truncated-high-intent FALSE-DONE slice,
b_s3 phase-10 plan), then a BLIND judge + sequential Disagree-Seeking (model-blind).

**KEY RESULT — false-done ELIMINATED across all 4 models.** On b_s2 (Task 4.3 only
ANNOUNCED + the gate only LOCATED, no edit), every model now correctly leaves Task 4.3
OPEN ("locate/modify the gate, re-invert test") — the pre-rule-6 qwen3.5:4b baseline
false-done'd here ("Task 4.3 … complete"). No false-done anywhere in the re-gate. No
empty/truncated outputs (think:false). Rule 6 also validated to NOT over-enumerate
finished bookkeeping on b_s3.

| Model | b_s1 | b_s2 (false-done test) | b_s3 | clean |
|-------|------|------------------------|------|------:|
| qwen3.5:4b-q8_0 (medium) | PASS | PASS | PASS | **3/3** |
| qwen3.5:4b (default) | PASS | **PASS (was false-done)** | FAIL¹ | 2/3 |
| qwen3.5:9b (PRO) | FAIL¹ | PASS | PASS | 2/3 |
| qwen3.5:2b (warned) | FAIL¹ | FAIL¹ | PASS | 1/3 |

¹ The non-b_s2 FAILs are **peripheral-identifier slips, NOT false-done**: q4 wrote
`src/routes/+layout.svelte.ts` (wrong `.ts` ext, file only referenced); q9 garbled
`drizzle/meta/_journal.json` → `drizzle.meta._journal.json`; q2b invented "Task 3.4" /
"Phase 10b" / `era1_Lore`. DS confirmed these are PRE-EXISTING (the drizzle slip appeared
in the T84 round, before rule 6) and content-dependent — a separate failure mode from the
false-done that rule 6 targets, NOT a rule-6 regression.

**Verdict: rule 6 SHIPS — false-done fixed family-wide, no regression. Tier lineup HOLDS**
(q8_0 the only 3/3 → reinforces it as the medium upgrade; q4 default, q9 PRO, q2b warned
unchanged). **Separate non-blocking follow-up (recorded for maintainer):** the residual
peripheral-identifier hallucinations (garbled paths / wrong extensions / invented
task-numbers) — a candidate fix is a FILES-field path-pattern constraint ("entries must be
real file paths, not prose"); its own workstream, does NOT block rule 6.

---

## FILES-field hardening — A/B re-gate (2026-06-04)

Closes the non-blocking follow-up above (the residual peripheral-identifier slips:
garbled paths / wrong extensions / FILES-inflation = listing referenced-only files).

**Change:** tightened the `FILES:` section descriptor in `SUMMARY_SYSTEM_PROMPT` (and the
byte-identical `model_bench.sh` `SYSTEM_FREEFORM`) from `<verbatim paths touched, one per
line>` to: *only paths an edit/write/create/move actually targeted in the excerpt, copied
verbatim; never a file merely read/grepped/referenced; if you cannot copy a path exactly,
omit it rather than guess a directory or extension.* No rule added (kept the prompt short
for small models); inherits rule 1's verbatim+FAILURE framing. Asserted by
`system_prompt_is_facts_first`; sync-guarded by `model_bench_freeform_prompt_matches_*`.

**Method:** the slices were the exact historical b_s1/b_s2/b_s3 (recovered byte-identically
from the sample-game sessions via an extract_slice fraction sweep — facts Jaccard 1.000;
b_s1=7064701a@0.45, b_s2=86544dbb@0.55, b_s3=109f2e81@0.49). Clean A/B: OLD vs NEW FILES
descriptor on the SAME slice, SAME model, mirroring production `call_model` exactly (num_ctx
16028, num_predict 4007, temp 0.1/top_k20/seed42, top_p0.8 + think:false for qwen3). 12
pairs (4 approved models × 3 slices) were BLINDED (random A/B order, held-out key) and given
to an independent blind judge, then a sequential Disagree-Seeking red-team challenged every
verdict against the raw slices. DS UPHELD all 12 winner calls (no flips).

Decoded by variant (blind judge, DS-confirmed):

| Model | b_s1 | b_s2 | b_s3 | net |
|-------|------|------|------|-----|
| qwen3.5:4b (default) | **NEW** (garble README fixed, 7→3, de-inflated) | TIE | **NEW** (glob `supabase/migrations/*` + 5 inflated dropped) | **better** |
| qwen3.5:4b-q8_0 (medium) | **NEW** (7→4) | TIE | **NEW** (5→3) | **better** |
| qwen3.5:9b (PRO) | **NEW** (canonical garble `drizzle.meta._journal.json` ELIMINATED) | TIE | TIE | **better** |
| qwen3.5:2b (warned) | OLD (NEW added `_journal.json`) | TIE | OLD (NEW 1→3 + a false-done) | **mild regression** |

Aggregate (approved + warned, DS-verified counts): **total FILES-inflation 21 (OLD) → 7
(NEW)**; **garbles 2 (OLD) → 1 (NEW)** (the remaining NEW garble `drizzle/config.ts` on 4b
is flagged `(inferred)`); **false-done: 0 on the three approved tiers** (the one NEW
false-done is on warned 2b/b_s3). Coverage check (the `omit-if-unsure` risk): the only
omission DS found is a SYMMETRIC two-file drop on b_s2 (`prestige.ts`,
`phase-09b-catalog-wiring.md`) present in BOTH arms — pre-existing truncation, NOT introduced
by the rule. So the rule does not drop real edited files on the approved tiers.

New artifact: capable models sometimes emit honest provenance annotations next to a path
(`(referenced)`, `(inferred)`, `(edited via plan text)`). These are NOT garble — they reduce
misleadingness and are harmless to replay (the summary is replayed as opaque text, never
parsed). Acceptable.

**Verdict: FILES-hardening SHIPS.** Net win on the three approved tiers (default/medium/PRO):
inflation cut ~67%, garbles halved (incl. the canonical `drizzle.meta._journal.json` on 9b),
no false-done, no coverage regression. The only regression is on the already-warned 2b
(more inflation + one false-done) — consistent with its degraded-fidelity opt-down status;
documented in `WARN_MODELS`. Tier lineup UNCHANGED. NEVER pushed/tagged.

### FILES-hardening — size/compression tradeoff (verification, 2026-06-04)

Post-landing check (does the FILES change affect compression %, cache, other parts — subagent-reviewed):
- **Cache: unaffected.** The prompt change only alters newly-GENERATED summary text; an already-cached
  summary is replayed verbatim (anchor-hash gated). The new compression-ratio logging is observability
  only (background task, no mutation of the decision/snapshot). Replay path untouched. 292 tests + clippy
  green.
- **Compression %: a real tradeoff.** Summary byte-size OLD→NEW on the A/B slices: mostly neutral or
  smaller (4b b_s2 −204, 9b b_s3 −358), but on the b_s3 PLAN-EDITING slice the approved tiers got LARGER
  (4b 1165→2015 = +73%, q8_0 +212) and the warned 2b ballooned (b_s1 +1329) — the stricter FILES rule
  makes models add provenance prose / `(verified)`-style annotations. Still ~95% smaller than the 40 KB
  raw, and `summary_is_smaller` guarantees a summary can never exceed model-free pruning (worst case =
  rejected → model-free fallback), so NO correctness/cache risk. Net: FILES fidelity bought at a small
  summary-size cost on some session shapes.
- **Mitigation:** the `num_predict` hard cap (IMPROVEMENTS-RESEARCH.md P0 #4) would mechanically bound
  this worst-case verbosity — the FILES rule + a generation cap together give fidelity AND tight size.

### num_predict cap — tested, NOT viable (2026-06-04)

P0 #4 from IMPROVEMENTS-RESEARCH.md, tested before implementing. The generic research advice ("cap
num_predict ≈15% of input tokens") assumes the model generates near the ceiling. trimwire's summaries
DON'T: measured at 5–16% of the current `num_predict = num_ctx/4 ≈ 4019`-token ceiling (4b b_s1 ~225 tok,
4b b_s3 ~503, warned 2b b_s1 ~675). So a 15% cap (~2400 tok) is inert. A cap tight enough to bind the
blowup cases truncates instead of compressing: 4b/b_s3 at num_predict=384 → `done_reason=length`, NEXT
section DROPPED; at 256 → cut mid-DECIDED. NEXT is the most load-bearing section for a resuming agent.
Uncapped already ends `done_reason=stop` (no ramble), so the stop-sequence half is inert too. BACKED OFF
— no code change. Summaries are already ~3–7% of the 40 KB raw; no slack to reclaim without dropping
facts. Real compression levers are model-free (P1) + the 1M escalation tiers (P0 #1).

### NEXT-reorder A/B — TESTED → REJECTED (regression), 2026-06-05
Round-8 (handoff-formats) proposed reordering SUMMARY_SYSTEM_PROMPT sections to GOAL→NEXT→FILES→
DECIDED→ERRORS→FACTS (BLUF + a claimed NEXT-truncation fix). A/B on qwen3.5:4b over the exact b_s1/2/3
(orig vs reorder, /tmp/ab_reorder.py mirroring call_model). RESULT: **REGRESSION — not adopted.**
- The NEXT-truncation motivation does NOT manifest: num_predict (~4007) is generous; NEITHER variant
  truncated NEXT on any slice. (Truncation only appears under a tight num_predict cap, which we don't use.)
- b_s3: reorder NEXT overstates completion ("Plan edits ... are complete") — a false-done-ish regression
  (orig correctly left it OPEN: "verify... confirm..."); and the later sections collapsed (DECIDED 6→2
  bullets, ERRORS dropped).
- b_s1: reorder DROPPED load-bearing version facts (Bun v4.1.7, Drizzle Kit ^0.31.10) and REINTRODUCED the
  `drizzle.meta/README.md` dots-for-slashes garble the FILES-hardening had eliminated.
- Root cause = exactly the DS-flagged risk: a forward-reasoning 4B that emits NEXT before spelling out
  FILES/DECIDED/FACTS produces a vaguer/overstated NEXT AND terser, lossier anchoring sections.
**Conclusion: the current facts-first / NEXT-last order is VALIDATED as superior. NEXT-reorder REJECTED.**
The repeat_penalty 1.1→1.05 micro-tweak is untested + marginal (low priority). The model-summary side
has NO adopted change — the format is already well-tuned; the real gains are model-free (density-aware
select_slice).
