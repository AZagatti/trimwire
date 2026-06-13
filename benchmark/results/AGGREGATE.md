# Cross-run aggregation

Evidence that the benchmark is reproducible. Two independent agents each ran
`cargo run --release --example bench` three times — **6 independent process
launches total** — on the same host.

> The 6-run min/max ranges below were captured on the original 11 corpora;
> `resumed_session` (added later, making 12 at capture time) is shown with a
> single representative median. The current benchmark has 14 corpora
> (`stale_input_heavy` and `thinking_heavy` added since). Its determinism and
> sub-2 ms behaviour match the other mid-size bodies.

## Deterministic sections are bit-for-bit stable

Sections 1–6 (savings, default-vs-tuned, leave-one-out attribution, cache
stability, cost model, savings-over-time + cost crossover) were **byte-identical
across all 6 runs**. They are pure functions of `(corpus, config)`; no repetition
is needed to trust them. The benchmark test suite also asserts this
(`savings_are_bit_for_bit_deterministic`).

## Timing is the only variable — and it's tight

Per-request overhead (Section 7) is wall-clock and host-dependent. Across the 6
runs, the per-corpus **median** stayed in these ranges (ms):

| Corpus | Body | median range across 6 runs |
|---|--:|--:|
| `pure_chat_floor` | 11 KB | 0.059 – 0.062 |
| `exempt_heavy` | 59 KB | 0.351 – 0.370 |
| `repeated_grep` | 31 KB | 0.183 – 0.192 |
| `coding` | 50 KB | 0.238 – 0.249 |
| `mcp_non_playwright` | 131 KB | 0.610 – 0.645 |
| `long_running` | 135 KB | 0.697 – 0.724 |
| `at_the_boundary` | 145 KB | 0.759 – 0.779 |
| `resumed_session` | 188 KB | ~1.11 (single run) |
| `mixed_realistic` | 365 KB | 1.010 – 1.418 |
| `unique_bash_spam` | 224 KB | 1.067 – 1.498 |
| `browser_heavy` | 425 KB | 1.541 – 1.689 |
| `giant_paste` | 510 KB | 1.488 – 1.731 |

Every corpus stays **sub-2 ms** per request. Small/medium bodies (≤145 KB) vary
<8% run-to-run; the large bodies (≥220 KB) are noisier — up to ~36% on
`unique_bash_spam` — because they run fewer iterations per round (200 vs 2000) so
have more allocator/cache jitter. That's timing noise on a large body, not a
correctness signal, and it's still well under the network round-trip it sits
behind.

## Verdict

The numbers that matter (savings, attribution, cache, cost) are exact and
reproducible; overhead is stable and sub-millisecond-to-low-millisecond. The
cost model honestly reports **5 of 14 corpora as cost losses** under warm caching
(stateless `default` profile: 7 wins, 5 losses, 2 zeros — see §5 of RESULTS.md);
it is not tuned to flatter. With reprune on (the shipped default), cache-stability
recovers substantially on the churny cases (§5b).
Regenerate any time with `cargo run --release --example bench`.
