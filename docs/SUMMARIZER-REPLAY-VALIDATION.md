# Summarizer replay — no-degradation validation

A high-level record of the F10 fix and the Phase-4 cognitive validation behind it. The detailed
run artifacts (transcripts, per-probe answers, judge packs) are kept locally and are not part of
the repo; this note preserves the conclusion and the headline numbers.

## The bug (F10) and the fix

reprune keeps a per-session checkpoint and replays its pruning decisions while the conversation
stays an append-only extension of that checkpoint (the cache-stable fast path). The `append_only`
guard compared the checkpoint prefix **byte-exact**, which includes Anthropic's `cache_control`
markers. Those markers move forward every turn, so a message *inside* the checkpoint prefix changed
its `cache_control` even when its content was unchanged. That made `append_only` false on every
turn → a full re-checkpoint each turn → the stable-replay branch (the only path that splices a
cached summary) never ran, and the cached summary was cleared. Net: in live Claude Code sessions,
accepted local/provider **summaries were computed but never applied on the wire**. Deterministic
pruning was unaffected.

**Fix:** `append_only` now ignores `cache_control` when comparing the checkpoint prefix (stability
decision only); the outgoing request still carries `cache_control` unchanged. Any real content
change still forces a re-checkpoint.

- Fix commit: `a9cdccf`
- Changelog entry: `510871c`

## Conclusion (scoped)

Across three trimwire execution modes — local summarizer (qwen3.5:4b), model-free deterministic
pruning, and provider GLM-5.2 — with the **Sonnet `--effort low`** answer model on the
**`trap-frozen` test fixture**, there is **no measurable trimwire-attributable degradation of
judgment**. This is **not** a claim that trimwire improves model quality or is "better" than running
without it.

## Validation summary

Closed-book late-session probes (P1–P7) scored 0/1/2 against a fixed judge rubric.
Δ = direct − trimwire; a value near 0 means effectively tied (either sign is within noise).

| Run | direct | trimwire | Δ | fabrications / notes |
|---|---|---|---|---|
| Mode 1 — frozen-history fork | 11/14 | 10/14 | +1 | 0 |
| Mode 2 — N=3 median | 12/14 | 13/14 | −1 | 1 non-recurring watch item |
| Priority 1 — isolated probe-format | 12/14 | 14/14 | −2 | 0 |
| Priority 2 — model-free (RAW) | 13/14 | 10/14 | +3 | 0 — see diagnostic note |
| Priority 3 — provider GLM-5.2 | 13/14 | 14/14 | −1 | 0 |

**Priority 2 diagnostic note (diagnostic-only — the RAW table above is preserved, not replaced):**
the raw model-free Δ=+3 is model noise, not compression. The trimwire P5 drop was a low-effort
"no-context" Sonnet confabulation with **diagnostically-proven intact context** (a diagnostic-only
rerun, same setup, scored P5=2); the P3 drop is the documented low-effort model-reliability
instability. So model-free shows no trimwire-attributable degradation.

## Compression evidence

Compression was verified on every trimwire probe, by mode:
- **Local / provider summarizer modes:** summary-on-wire (the cached summary was spliced into the
  upstream request).
- **Model-free mode:** there is no summary — compression evidence is deterministic `stable_reprune`
  (replayed thinking-strip / cross-turn-dedup decisions). Model-free is **not** described as
  summary-on-wire.

## Caveats / scope

- This `trap-frozen` fixture only.
- Sonnet `--effort low` answer model.
- N=1 per probe for Priority 2 and Priority 3.
- GLM-5.2 ran at the 128 KB default with a ~64 KB actual slice; **512 KB+ behaviour untested**.
- Deterministic `stale_reads` / `bloat_cap` / `failed_input_purge` were not exercised by this fixture.
- An intermittent low-effort Sonnet "no-context" P5 confabulation appeared on different arms in
  different runs — it is a model/effort quirk, **not** a trimwire/compression effect.

## Future work

- A large-slice (512 KB+) stress fixture to exercise GLM-5-class budgets and the unfired
  deterministic strategies.
- A context-rot benchmark.
- A deliberate drawback / regression search.
