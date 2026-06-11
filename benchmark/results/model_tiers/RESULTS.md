# Model tiering sweep (2026-06-03) — light / medium / pro

Full-roster sweep for the compaction model tiers. Each model: harm gate on the
hand-curated synthetic planted-fact slice (12 load-bearing facts, uses the real
`serialize_slice` asymmetry) + footprint (`ollama ps` resident) + a manual quality
read of the summary (hallucination / false-done / NEXT-correctness — the dimensions
plain retention misses). One model at a time on a 13 GB box.

**Validity:** the synthetic retention + footprint + quality read are TRUSTWORTHY.
The real-slice arm of this run is INVALID (the regenerated slice's facts were
mis-curated — every model scored ~0–33%, including the known-good qwen3.5:4b — a
curation artifact, not model signal). Real corroboration on the NEW high-performers
is OWED before any tier winner is finalized / added to APPROVED_MODELS.

## Synthetic retention + footprint (valid)

| Model | weights | resident | synth retention | quality read |
|-------|--------:|---------:|----------------:|--------------|
| granite4.1:8b | 5.3 GB | 5.9 GB | **100%** | faithful, NEXT ✓, no false-done, no hallucination |
| phi4-mini:3.8b | 2.5 GB | 3.1 GB | 100% | retention ✓ but **stale NEXT** (says "run tests" — already passed) |
| qwen3.5:4b | 3.4 GB | 3.0 GB | 91.7% | clean; NEXT ✓ (real-validated in P0b: 100%) |
| ministral-3:3b | 3.0 GB | 2.7 GB | 91.7% | clean, no hallucination, but **NEXT stale** (points to mid-slice work) |
| qwen2.5-coder:3b | 1.9 GB | 2.2 GB | 91.7% | clean on synthetic — but DISQUALIFIED (real-slice hallucination) |
| gemma3:4b | 3.3 GB | 2.7 GB | 83.3% | faithful, NEXT ✓ |
| llama3.2:3b | 2.0 GB | 2.6 GB | 83.3% | (P0b: 83% real; drops a fact) |
| qwen3:8b | 5.2 GB | 5.6 GB | 83.3% | — |
| qwen3.5:2b | 2.7 GB | 2.2 GB | 75% | (P0b: 75/83%, drops load-bearing facts) |
| granite4.1:3b | 2.1 GB | 2.5 GB | 75% | DISQUALIFIED (false-done at 3b) |
| qwen3:4b | 2.5 GB | 3.2 GB | 66.7% | — |
| qwen3:1.7b | 1.4 GB | 1.7 GB | 58.3% | too lossy |
| qwen2.5-coder:1.5b | 986 MB | 1.2 GB | 58.3% | too lossy |

## Preliminary tier classification (pending real corroboration + blind gut-read)

- **PRO / HEAVY (~5–6 GB): `granite4.1:8b`** — 100% retention, correct NEXT, no
  false-done, no hallucination. The 8B redeems the family disqualified at 3b. Clear
  pro winner over qwen3:8b (83%). *Now feasible since WSL → 13 GB.*
- **DEFAULT / MEDIUM (~3 GB): `qwen3.5:4b`** — stays the default: 91.7% synthetic +
  the only model real-validated (P0b 100%, no hallucination, correct NEXT). phi4-mini
  ties on retention but loses on NEXT-correctness — the dimension that matters.
- **LIGHT (≤~2.7 GB): `ministral-3:3b` is the candidate** — first sub-3GB model to
  clear ~90% (beats 2b 75%, llama3.2:3b 83%). PROMISING, but its NEXT was stale and it
  has not been real-corroborated. **If it fails real corroboration / the gut-read, the
  LIGHT tier stays EMPTY (not shipped)** — per the rule "a tier with no qualifier is
  dropped." (2b and llama3.2:3b do NOT qualify; coder:3b is disqualified.)

## ADJUDICATED (blind gut-read on a real slice + sequential Disagree-Seeking) — supersedes the "preliminary" above

Fact-curation broke 3× on real slices → pivoted to the council's proven method: a
BLIND judge read the raw slice + 4 anonymized summaries; a Disagree-Seeking pass then
verified its pivotal claims against the raw text. (Slice: a Playwright/DOM-debug
sample-project slice — acknowledged as a pathological/unusual surface.)

- **qwen3.5:4b → CONFIRMED default/MEDIUM.** Ranked #1 (29/30): correct NEXT
  (investigate lore-notif trigger in index.tsx), correct upgrade costs (40E→60E), no
  hallucination, no false-done.
- **granite4.1:8b → NOT a pro winner; NEEDS-MORE (leaning disqualify).** Despite 100%
  synthetic, its NEXT was MISDIRECTED ("capture a final screenshot of the lore
  notification to confirm its appearance" — the notif never appeared; the real next
  step was source debugging). DS downgraded the judge's "false-done" label to
  "misdirected NEXT" but it's the same genus as the granite4.1:3b disqualification.
  Verdict: do NOT ship for PRO on one slice; run 2–3 non-Playwright slices — if the
  pattern recurs, disqualify the family at all sizes.
- **ministral-3:3b → NEEDS-MORE; do NOT ship LIGHT yet.** Correct NEXT, but a real
  numeric misread (wrote Lv.0→60E/Lv.1→2060E; truth 40E/60E — misparsed the
  concatenated UI string `Lv.1/2060 E`). A fidelity risk for the FACTS field. Promising
  (best new light candidate) but needs 2 more numeric-heavy slices before shipping.
- **phi4-mini:3.8b → OUT.** Stale NEXT fully confirmed (all 3 NEXT items already done
  in the excerpt; missed the actual open task).

**Decision: APPROVED_MODELS UNCHANGED** (qwen3.5:4b default + qwen3.5:2b opt-down). No
new tier shipped — one slice + one judge is insufficient (DS), and synthetic retention
(≥91.7% for granite/phi4/ministral) MISLED on every new candidate. PRO + LIGHT tiers
remain UNFILLED pending a multi-slice gut-read; if no candidate qualifies, those tiers
are simply not offered (maintainer rule). Reconfirms: gut-read > retention.

## OWED before finalizing (do NOT change APPROVED_MODELS yet)
1. Real-slice corroboration for granite4.1:8b, ministral-3:3b, phi4-mini, gemma3 —
   with CAREFULLY curated load-bearing facts on a CONCRETE-content slice (abstract
   review slices broke curation twice). qwen3.5:4b as the reference.
2. Blind gut-read (false-done / NEXT / hallucination) on the new winners — the same
   bar qwen3.5:4b passed. Especially confirm granite4.1:8b doesn't false-done on real
   long sessions (the 3b's failure mode) and ministral-3:3b's NEXT on real slices.
3. Subagent results-review + sequential Disagree-Seeking on this classification.

## RESOLVED — multi-slice blind gut-read (2026-06-04): NO tier qualifies; both candidates DISQUALIFIED

The OWED multi-slice gut-read ran on the HARDENED prompt (rule 5), against fresh diverse
NON-Playwright concrete-edit real slices (fh1 PR-review fixes + b_s1 drizzle-snapshot +
b_s3 anti-cheat-plan + b_s2 lore/era wiring), qwen3.5:4b as the reference. Method: blind
judge (model identity hidden) + a sequential Disagree-Seeking pass re-verifying every
disqualifying claim against the raw with line-cites.

**PRO candidate — `granite4.1:8b` → DISQUALIFIED (granite-family rule fired).**
- fh1: PASS (faithful, no hallucination, correct NEXT).
- b_s3: **FALSE-DONE** — NEXT claimed "all tasks marked completed … no further open items
  remain. Proceed to Phase 11" while the raw ends mid-edit of the plan doc; PLUS a
  **fabricated** `src/lib/server/drizzle/` path prefix (raw uses repo-root `drizzle/`).
- b_s1: **FALSE-DONE** — ERRORS said the README crash "is now fixed" while the `git mv`
  was still in-flight at the cut; omitted the corrective-`0004`-must-be-discarded caveat
  the raw flagged (NEXT would mislead).
- The granite-FAMILY false-done pattern (the exact reason granite4.1:3b was disqualified)
  RECURRED on the 8b → **added `granite4.1:8b` to `DISQUALIFIED_MODELS`.** PRO tier EMPTY.

**LIGHT candidate — `ministral-3:3b` → DISQUALIFIED.**
- b_s2: **FABRICATED IDENTIFIER** `era1_garage` (not in raw; garage is stage 0) +
  misattributed `getEra()` as the lore-unlock site + a NEXT instruction ("uncomment the
  `return () => true` block") describing code that doesn't exist (gate uses `return () =>
  false`).
- b_s3: **CONTENT COLLAPSE** — emitted only a lone GOAL line, zero FILES/DECIDED/ERRORS/
  NEXT; a resuming agent gets nothing actionable.
- b_s1: wrong path (dropped `meta/` from `drizzle/meta/_journal.json`) — DS reduced this
  one to a recoverable nitpick, but the fabrication + collapse stand.
- A fabrication + a content collapse on real slices → **added `ministral-3:3b` to
  `DISQUALIFIED_MODELS`.** LIGHT tier EMPTY.

**Verdict: APPROVED_MODELS UNCHANGED — qwen3.5:4b only (with qwen3.5:2b as the
warned opt-down). PRO and LIGHT tiers stay UNFILLED = not offered.** Synthetic retention
had rated both candidates ≥91.7%; the real-slice gut-read disqualified both — the Nth
confirmation that gut-read > retention, and that one model family's failure mode (granite
false-done) carries across sizes. Pinned by the `tier_search_disqualifications_are_pinned`
unit test. Slices kept OUT of the repo (other projects' transcripts; aggregate only).

## PRO TIER FILLED — qwen3.5:9b APPROVED (2026-06-04, second pass)

The first tier pass (above) prematurely concluded "no tier" — it disqualified
granite4.1:8b + ministral-3:3b but NEVER real-gut-read the larger qwen-family model or
gemma. This pass closes that gap. RAM measured on THIS 13GB box first (benchmark/ram_probe.sh,
/api/ps the reliable number — the `free` AVAILABLE delta is page-cache-confounded):

| Model | /api/ps resident @num_ctx 8192 | fits one-at-a-time (≤11GB) |
|-------|-------------------------------:|:--------------------------:|
| qwen3.5:4b (default) | 3035 MB | yes |
| qwen3:8b | 5899 MB | yes |
| qwen3.5:9b | 6102 MB | yes |

**Blind real-slice gut-read + sequential Disagree-Seeking** on 3 diverse non-Playwright
concrete-edit slices (b_s1 drizzle-snapshot, b_s2 lore/era wiring, b_s3 anti-cheat plan
edits), model identity hidden, qwen3.5:4b harness:

| Model | b_s1 | b_s2 | b_s3 | Verdict |
|-------|------|------|------|---------|
| **qwen3.5:9b** | PASS | PASS | **PASS** (strongest) | **APPROVED → PRO tier** |
| qwen3:8b | PASS | PASS (thin FACTS) | **FAIL** (misdirected-NEXT) | NOT approved (not refused) |

- **qwen3.5:9b → APPROVED** as the PRO/high-RAM option: 3/3 clean — faithful, ZERO
  hallucinated identifiers, correct NEXT, no false-done. Same family as the validated
  default, so it inherits the passing behavior. Added to `APPROVED_MODELS`; default stays
  qwen3.5:4b (9b is an opt-in upgrade for users with the RAM).
- **qwen3:8b → NOT approved.** Decisive A/B: on the SAME b_s3 slice, 9b's NEXT correctly
  said "continue applying review fixes / finalize plan promotion" while 8b's NEXT directed
  a resuming agent to CREATE migration SQL files (`0004_…`–`0007_…`) — real filenames
  lifted from the plan-document content, but the wrong ACTION (the session was editing the
  plan, not executing migrations) → a resuming agent would do wrong work. It fabricated
  nothing and had no false-done, so it is NOT added to `DISQUALIFIED_MODELS` (the runtime
  guard warns "unverified" if a user configures it); it simply doesn't clear the
  clean-on-all-slices PRO bar. (qwen3:4b, synthetic 66.7%, not gut-read — below the bar.)

**Net: PRO tier = qwen3.5:9b. APPROVED_MODELS = {qwen3.5:4b default, qwen3.5:9b PRO,
qwen3.5:2b warned opt-down}.** Reconfirms the family lesson: the larger member of the
ALREADY-VALIDATED family transferred cleanly, whereas every cross-family candidate
(granite, ministral, qwen3-non-3.5) hit a real-slice failure mode. Pinned by
`tier_search_disqualifications_are_pinned`.

## DEFAULT/MEDIUM reconsider (2026-06-04) — gemma3:4b DISQUALIFIED; qwen3.5:4b-q8_0 APPROVED (medium upgrade)

Gut-read of two default-alternatives on 3 NEW harder real slices (b_s1 Phase-9c CI/snapshots,
b_s2 Phase-9b lore gate, b_s3 Phase-10 plan edits), blind judge + sequential Disagree-Seeking,
qwen3.5:4b(Q4) as the head-to-head baseline. RAM (ram_probe.sh, /api/ps): q8_0 4764 MB, fits.

| Model | b_s1 | b_s2 | b_s3 | tally |
|-------|------|------|------|-------|
| qwen3.5:4b-q8_0 | FAIL (recoverable path slip) | **PASS** | **PASS** | 2/3 |
| qwen3.5:4b (Q4, current default) | FAIL (Bun-vs-vitest version + path slip) | **FAIL (false-done)** | PASS | 1/3 |
| gemma3:4b | FAIL (fabricated action + false-done) | FAIL (garbled DECIDED) | FAIL (misdirected NEXT) | 0/3 |

- **gemma3:4b → DISQUALIFIED.** On b_s1 it fabricated an action that never happened
  ("modified db:generate to use a fake backend" — that was a different task) and false-done'd
  ("reconstructed 0002/0003 snapshots" — not done at the cut); on b_s3 a misdirected NEXT
  (Phase-10 execution tasks vs. the plan-editing the session was doing). Fabrication +
  false-done = the disqualifying class. Added to `DISQUALIFIED_MODELS`.
- **qwen3.5:4b-q8_0 → APPROVED (higher-fidelity MEDIUM upgrade).** It measurably beat Q4
  on these slices: its ONLY defect across 3 slices was a *recoverable* path-format slip
  (`drizzle/meta/`→`drizzle.meta/`, shared with Q4), whereas Q4 committed a genuine
  **FALSE-DONE on b_s2** (claimed the lore gate "reactivated" when the session only located
  it — a continuation agent would skip the work) plus a `Bun v4.1.7` version fabrication on
  b_s1. Added to `APPROVED_MODELS` as an opt-in upgrade (~4.8 GB resident, +~1.7 GB vs Q4).
- **DEFAULT unchanged (qwen3.5:4b).** Promoting q8_0 (or 9b) to default is a maintainer
  call (RAM cost). **RECORDED for the maintainer:** Q4's false-done on b_s2 is a genuine
  weakness on TRUNCATED-high-intent slices (the session's last words announce a task that
  isn't done yet; rule 5 didn't fully prevent the model from claiming it done). q8_0 and
  qwen3.5:9b did NOT false-done there — so for truncation-prone workflows, q8_0/9b are more
  robust, and a future "rule 5 v2" (an announced/started task is NOT done) is a candidate.
  These b_s slices are harder than the fh1/fh2/fh3 the original 4b harm-gate used; this
  refines (doesn't invalidate) that validation.

**Net APPROVED_MODELS = {qwen3.5:4b default, qwen3.5:4b-q8_0 medium upgrade, qwen3.5:9b PRO,
qwen3.5:2b warned opt-down}. DISQUALIFIED += gemma3:4b.** Pinned by the tier test.

## LIGHT tier (2026-06-04) — NOT VIABLE; qwen3.5:0.8b DISQUALIFIED

The only same-family sub-2b candidate is qwen3.5:0.8b (995 MB /api/ps). Blind gut-read on
the 3 real slices: **3/3 FAIL.** It COMMITS (not just omits): b_s1 garbled the path
(`drizzle.meta README.md`), fabricated a task number, and dropped 3 committed tasks; b_s2
fabricated a `getLoreForEra` change that never happened and pointed NEXT at already-done
work; b_s3 **false-done'd "0 BLOCKER issues"** when the raw raised (and fixed) blockers
(10 "BLOCKER" mentions), and garbled the field name ("ERRORES"). Both the false-done and
the fabrication were verified directly against the raw.

**Verdict: no viable LIGHT tier.** qwen3.5:0.8b → DISQUALIFIED (it fabricates/false-dones,
the commission class — refused, unlike qwen3.5:2b which only OMITS and is warned).
`qwen3.5:2b` remains the only lighter-than-default option, allowed with a loud warning.
This closes the tier hunt:

**FINAL tier lineup (real-slice gut-read + Disagree-Seeking):**
- DEFAULT: `qwen3.5:4b` (~3.0 GB)
- MEDIUM upgrade: `qwen3.5:4b-q8_0` (~4.8 GB) — fewer false-dones on truncated sessions
- PRO: `qwen3.5:9b` (~6.1 GB) — cleanest on diverse real slices
- lighter opt-down (warned): `qwen3.5:2b` (~2.7 GB) — drops facts
- DISQUALIFIED (refused): granite4.1:3b/8b, qwen2.5-coder:3b, ministral-3:3b, gemma3:4b,
  qwen3.5:0.8b
- not-approved-not-refused (warn-on-use): qwen3:8b (misdirected-NEXT)

## qwen3.6 — no-go FOR NOW (re-check when a smaller variant ships; NOT excluded forever)

Checked (2026-06-04): ollama's qwen3.6 family is *currently* **27B/35B only** — smallest
pullable tag is `qwen3.6:27b-q4_K_M` at ~17 GB disk / ~17–20 GB resident, which does NOT
fit the 13 GB box (would swap, <1 tok/s); at this size it's agentic-coding-focused, not a
summarization/instruction upgrade. qwen3.5 stays the basis for all tiers FOR NOW.
**RE-OPEN TRIGGER (do not exclude forever):** when ollama publishes a sub-10B qwen3.6
(a 3.6 tag that fits ~≤7 GB resident), gut-read it as a same-arch upgrade candidate the way
qwen3.5:9b was. This is a deferral, not a permanent exclusion — periodically re-probe
`ollama.com/library/qwen3.6/tags`.

## LIGHT-tier fresh search under rule 6 (2026-06-04) — qwen3.5:2b confirmed the best (warned); no clean sub-default tier

Research (verified ollama tags) found the candidate space is essentially exhausted: the
credible lightweight options are all already-handled — granite4.1:3b (DISQUALIFIED family),
phi4-mini:3.8b (rejected; also 3.5 GB resident, HEAVIER than the 3.0 GB default), qwen3.5:2b
(the warned same-family opt-down). Since the granite/phi4 disqualifications predated rule 6
(for false-done / stale-NEXT, which rule 6 targets), they were re-tested UNDER rule 6.

Sweep: rule-6 summaries from qwen3.5:2b (2.7 GB) + granite4.1:3b (2.78 GB) + phi4-mini
(3.5 GB) on b_s1/b_s2/b_s3, blind judge + the b_s2 false-done test.

| Model | resident | tally | finding |
|-------|---------:|------:|---------|
| **qwen3.5:2b** | 2.7 GB | **2/3** | best of all three; its b_s2 was the top summary (correct NEXT, NO false-done under rule 6); one FAIL = a GOAL mischaracterization on the plan slice, not fabrication |
| granite4.1:3b | 2.78 GB | 0/3 | rule 6 FIXED its false-done, but it fabricated paths, raw-dumped code into DECIDED, and on b_s2 summarized the WRONG slice → stays DISQUALIFIED |
| phi4-mini:3.8b | 3.5 GB | 0/3 | fabricated path + wrong NEXT + FILES inflation; also heavier than the default, so not a LIGHT option at all |

**Verdict: qwen3.5:2b is the best (and only viable) lightweight — it stays the WARNED opt-down,
not a clean LIGHT tier; no sub-default model is clean.** Notable meta-finding: this judge rated
the IDENTICAL qwen3.5:2b summaries differently than the step-2 re-gate judge (2/3 vs 1/3) — q2b
sits at the quality boundary where even blind judges disagree, which is exactly why "warned /
inconsistent" is the right label. **APPROVED/DISQUALIFIED unchanged.** rule 6 generalizes the
false-done fix beyond qwen (it fixed granite's false-done too) — but granite has other
disqualifying failures (fabrication / raw-dump / slice-mismatch). LIGHT search CLOSED.

## MEDIUM/PRO other-family fail-fast sweep under rule 6 (2026-06-04, in progress)

Maintainer-authorized: test other-family MEDIUM/PRO candidates under rule 6, fail-fast
(run a discriminator slice first, exclude on first-slice fabrication/false-done/raw-dump).

- **granite4.1:8b → EXCLUDED (stays disqualified).** Fail-fast gate b_s2: NEXT = "No further
  tasks are open; the session is complete" — a clear FALSE-DONE (Task 4.3 was only announced
  + located). Rule 6 fixed the granite *3b* false-done but NOT the 8b's. Family stays refused.
- **qwen3:8b → NOT approved (0/3 on the full battery).** Passed my quick eyeball on b_s1/b_s2
  but a strict blind judge failed all three: FILES-inflation on every slice (lists read-only
  files, not the edited set), a false-done on b_s1 (claims the README `git mv` done while it
  was mid-command), wrong-task GOAL on b_s2, and the plan-vs-implementation misclassification
  on b_s3. **Confirms the maintainer's intuition: the older qwen3 line does LESS than qwen3.5
  at the same size** (q8_0/9b of qwen3.5 are 3/3 and 2/3; qwen3:8b is 0/3). Stays unlisted
  (warn-on-use), not refused. Don't pursue the qwen3 (non-3.5) family — incl. qwen3:14b
  (same family, would just cost more RAM for likely-worse output).
- **Pending:** mistral-nemo:12b (new family, ~8 GB) + gemma3:12b (gemma fabricates → expect
  fail-fast). qwen3:14b DEPRIORITIZED per the qwen3<qwen3.5 finding.

So far: no other-family candidate beats the qwen3.5 incumbents (MEDIUM qwen3.5:4b-q8_0,
PRO qwen3.5:9b). Sweep continues.

## MEDIUM/PRO sweep — CONCLUDED (2026-06-04): no other-family candidate beats the qwen3.5 incumbents

Final results of the maintainer-authorized other-family fail-fast sweep under rule 6:

| Candidate | resident | result |
|-----------|---------:|--------|
| granite4.1:8b | 5.9 GB | EXCLUDED — false-done on b_s2 ("session complete" when Task 4.3 only announced); family stays refused |
| qwen3:8b | 5.9 GB | NOT approved — strict judge 0/3 (FILES-inflation ×3, b_s1 false-done, b_s2 wrong-task GOAL, b_s3 plan-vs-impl). Confirms qwen3 < qwen3.5 at the same size |
| mistral-nemo:12b | 9.0 GB | EXCLUDED — fail-fast: fabricated a filename (`cheat_signalls.sql`, double-l) on b_s1 + thin DECIDED; also the heaviest |
| gemma3:12b | — | NOT EVALUATED — pull/load failure on this box (HTTP 404; partial blobs after a transient network drop). Gemma is an already-DISQUALIFIED family (gemma3:4b fabricates) → low-value, deferred to a network-stable session |
| qwen3:14b | — | DEPRIORITIZED — qwen3 (non-3.5) family confirmed inferior to qwen3.5; more RAM for likely-worse output |

**CONCLUSION: the qwen3.5 family wins every tier.** No other-family model beat the
incumbents. FINAL LINEUP (all gut-read-validated, rule 6): DEFAULT `qwen3.5:4b` (3.0 GB) ·
MEDIUM upgrade `qwen3.5:4b-q8_0` (4.8 GB) · PRO `qwen3.5:9b` (6.1 GB) · warned light opt-down
`qwen3.5:2b` (2.7 GB). DISQUALIFIED (refused): granite4.1:3b/8b, qwen2.5-coder:3b,
ministral-3:3b, gemma3:4b, qwen3.5:0.8b. APPROVED/DISQUALIFIED UNCHANGED — the sweep
confirmed the lineup rather than changing it. The recurring lesson holds: same-family
scaling (qwen3.5) transfers cleanly; cross-family candidates each hit a real-slice failure
(false-done / fabrication / FILES-inflation). **Tier hunt fully CLOSED.**

---

## FILES-field hardening landed (2026-06-04)

The peripheral-identifier residual noted across this sweep (garbled paths, FILES-inflation)
is now FIXED by a `FILES:` descriptor tightening, blind A/B + DS re-gated on the exact
b_s1/2/3 across all 4 approved models: inflation 21→7, garbles 2→1, no false-done / no
coverage regression on the 3 approved tiers; mild inflation regression on warned 2b only.
Tier lineup UNCHANGED (DEFAULT qwen3.5:4b · MEDIUM qwen3.5:4b-q8_0 · PRO qwen3.5:9b ·
warned-light qwen3.5:2b). Full table + method in `harm_gate/RESULTS.md` (FILES-field A/B).
