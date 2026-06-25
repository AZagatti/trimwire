# Results — what trimwire actually does, and what we can claim

This page is the single home for trimwire's measured results, split into the
three categories that get confused most often. **The categories are not
interchangeable.** A number from one category cannot be relabelled as another —
in particular, an *offline replay* or *cost-model* number must never be
presented as a *live* result.

Audience: anyone choosing a claim for the site, README, or a conversation with a
technical buyer. If you only read one thing, read **"What we can safely
claim."**

## Vocabulary (defined once)

- **Sent reduction / request-size reduction** — how many fewer bytes leave your
  machine on a `/v1/messages` request after trimwire prunes it (`in` → `sent`).
  This is the **reliable, always-true** number: it's just bytes on the wire.
- **Context-window headroom** — the same thing framed for the buyer: the share
  of the model's context window you get *back* by not shipping dead weight. This
  is the primary framing; prefer it over raw KB.
- **Model-free pruning** — the default. Eight deterministic, cache-safe
  strategies remove structurally-redundant or window-aged content. No model, no
  API key, nothing leaves your machine. This is the savings engine.
- **Summarizer** — an *optional, off-by-default* extra lever that compresses the
  old part of a long session with a model you choose (local ollama or a cloud
  API). It is a **separate** mechanism from model-free pruning, and it only
  engages on large/long sessions (raw body > 200 KB trigger).
- **Accumulator** — a summarizer sub-feature that appends frozen summary deltas
  instead of rewriting the whole summary, so the prompt cache survives. It is
  the lever behind the largest *cost-model* numbers — which are offline (below).
- **Cost (cache-weighted)** — modelled dollar cost under Anthropic's prompt-cache
  pricing (cache reads bill ~0.1×). This is **non-monotonic** and secondary:
  pruning old content can bust the cache, so short sessions can cost *more*.

## What we can safely claim

| Claim | Category | Strength |
|---|---|---|
| "Reduces request size / reclaims context-window headroom; the amount depends on session shape (≈0% on plain chat, more on tool/read-heavy work)." | All three agree | **Safe, headline-ready** |
| "On reproducible benchmarks, request size is ~60–95% lighter on tool/read/browser-heavy sessions (0% when there's nothing redundant)." | Benchmark (offline replay) | **Safe — label as benchmark** |
| "Measured live through the gateway with real `claude -p`: sent context shrinks with session size — single-digit % on short/typical sessions, ~50–65% on read-heavy mid-size sessions, ~75–94% on very large (~1–1.7 MB) ones." | Live | **Safe — label as live, note fixtures** |
| "On a real, typical dogfooding session, model-free pruning cut sent bytes ~17% over 228 requests." | Live (real traffic) | **Safe — the honest typical-session number** |
| "On a measured ~1M-token session (Opus), sent context dropped ~75% and input cost ~79%, with no confident-wrong answers." | Live | **Safe — single best-case session** |
| "Cost is a non-monotonic side effect: short sessions wash-to-loss, long ones win — about −55% cache-weighted input cost at 256 turns." | Offline cost-model | **Safe — label as cost model, caveat** |
| "The optional summarizer's accumulator saved ~63–65% cache-weighted cost on long reconstructed sessions." | Offline replay | **Use with care — offline only, see below** |

**Do not** turn any single number into "trimwire saves X%." There is no single
number; the honest framing is always *"depends on session shape, and here's the
range."*

## Live online results (`claude -p` through the gateway)

These are the only numbers measured with **real `claude -p` traffic** routed
`claude → trimwire serve → api.anthropic.com`. "DIRECT" in the sources means real
Claude Code with its own native auto-compaction and no gateway — so this is
*trimwire vs native Claude Code*, the real user comparison.

> **Read this first.** The high live percentages (50–94%) come from
> **deliberately adversarial / synthetic read- and tool-heavy fixtures** — large
> files with facts buried mid-file, read once. They are engineered to maximise
> prunable dead weight (and to stress recall). They are **not** typical traffic.
> The honest *typical-session* live number is the ~17% real-dogfood figure, and a
> short cached session is correctly **0%**. Quote the range, and say which end a
> given workload sits at.

| Session (live `claude -p`) | Profile / mode | Sent reduction | No-harm |
|---|---|--:|---|
| Short, cached real session (acceptance canary, 2026-06-24) | model-free default | **0%** (`in==sent`, correct no-op) | n/a (nothing pruned) |
| Real dogfooding session, 228 requests | model-free default | **17%** (98.7 MB → 81.5 MB, cache-hit 81%) | faithful summary; no load-bearing fact dropped |
| Adversarial read-heavy fixtures, ~150–250 KB (websvc/infra/datapipe) | model-free default, Haiku | **50–65%** | 11/15 correct; 3 confident-wrong on one adversarial shape (Haiku didn't re-read) |
| Large synthetic session, ~1.1 MB / ~275K tok | model-free / summarizer, Haiku | **81–88%** | 0 misleading (correct or safe-abstain) |
| Measured ~1M-context, max_in 1.18 MB / ~360K tok | model-free default, Opus-low | **−75% sent (max −91%); −79% input cost** ($1.66 → $0.35) | DIRECT 4/4 correct; trimwire 4/4 safe-abstain, **0 misleading** |
| Opus ceiling fixture, max_in ~1.7 MB | model-free default | **89–94%** | trimwire 0 misleading; native compaction confabulated here |
| `gentle` profile, low-repetition small content | gentle | **~1%** (DIRECT-equivalent) | 0 misleading |
| `gentle` profile, large highly-repetitive content | gentle | up to **~75%** (dedup/bloat_cap; fixture-dependent) | 0 misleading |
| Output/generation-heavy (~253 KB of assistant text) | model-free default | **~4%** | 0 misleading (little is pruned) |

**Live takeaways that are safe to repeat:**

- **Model-free pruning is the savings engine, and it scales with read/tool
  context** — single-digit % on short/typical sessions, rising to 75–94% on very
  large read-heavy ones. Reduction is **model-independent** (the gateway prunes
  the wire before the model sees it).
- **No-harm vs native Claude Code held in every measured cell on Sonnet and
  Opus** (0 confident-wrong answers). The only confident-wrong cases were Haiku on
  one adversarial fixture where it didn't re-read. Stronger models *abstain*
  rather than confabulate. (This is a no-harm result, **not** a quality-lift
  claim.)
- **The summarizer rarely engages on automated/medium sessions** — on read-heavy
  work model-free has already removed the bulk, so the summarizer's measured
  incremental savings were small. Its value is on conversation/message-heavy old
  content and very long human-paced sessions.
- **`gentle` is content-dependent, not "always ~0 savings"** and is **not** a
  recall-critical / safer mode — it simply prunes less. The recall-critical path
  is *default on + agentic re-read* (the elision stub tells the agent what to
  re-read).

Source: `internal/docs-benchmark-audit/live-canary-insights.md` (2026-06-21,
~$34.91 / ~145 probes across Haiku/Sonnet/Opus); raw rows in
`internal/docs-benchmark-audit/raw/live-*-metrics-2026-06-21.jsonl`;
real-dogfood 17% in `internal/manual-test/phase0-haiku.md` (S10); short-session
0% in `internal/manual-test/local-claude/20260624/` (`13-gateway-requests.txt`,
`30-stats-iso.txt`). The per-request gateway reduction logs were written to
`/tmp/live*` and are ephemeral; the percentages above are the recorded summaries,
and the JSONL metrics (model/effort/cost/correctness per probe) are preserved and
authoritative for the no-harm counts.

## Benchmark results (reproducible request-size / headroom)

Deterministic synthetic `/v1/messages` bodies fed through the **real strategy
code** under the shipped `default` config. Reproducible by anyone:
`cargo run --release --example bench`. Byte columns are exact; token/cost columns
are estimates. **This is offline, not live** — but it's the most *reproducible*
evidence, and the request-size reductions match the live trend.

| Session shape | Request size | Reduction |
|---|--:|--:|
| Plain chat, no tools | 9.5 KB | **0%** (no-op) |
| Repeated searches | 29.8 KB | **78%** |
| Coding (superseded reads + old log) | 48.0 KB | **83%** |
| Long-running diverse tools | 133.6 KB | **60%** |
| Resumed ~50-turn session | 186.4 KB | **65%** |
| Browser / screenshot-heavy | 423.2 KB | **85%** |
| Realistic composite | 363.0 KB | **95%** |

- **Headline range: ~60–95% on tool/read/browser-heavy sessions, 0% on plain
  chat.** Keep this range in *benchmark* context — it is not a live claim.
- **Focus ratio** (the share of the request that is your recent working window)
  roughly 2–3× higher than no-pruning over a long session; it's a byte-share
  proxy, not a proven quality improvement.
- **Overhead** is roughly **sub-2 ms** per request (host-dependent), off the
  network path.

Source: `benchmark/results/RESULTS.md` (full 15-corpus tables, per-strategy
attribution, profiles, cache-stability, overhead).

## Replay / cost-model results (offline, cache-weighted)

These answer "does the **bill** go down?" They are **modelled** under Anthropic's
prompt-cache pricing and **replayed offline** over recorded or reconstructed
transcripts. They are **not** live, and the dollar figures are directional
(≈4 bytes/token estimate, sub-cent synthetic sessions — read sign and trend, not
magnitude).

| Result | Value | What it is |
|---|--:|---|
| Cache-weighted input cost at 256 turns | **−54.6%** | Cost-model crossover on a synthetic Bash/Read session (`RESULTS.md §6`). Short sessions cost *more* (cache busting); long ones win. |
| Accumulator vs baseline, real 596-turn / ~1M-token session | **−63%** | Offline replay of a real transcript via `examples/cost_replay` (`replay_at_scale/RESULTS.md`). |
| Accumulator vs baseline, real 981-turn session | **−64.6%** | Same harness, reconstructed real session. Single-summary *costs more* on every session; the accumulator is the lever. |
| Naive single-summary cost saving | **−0.7% to −6.1%** | Production-faithful cost replay, 2 real sessions × cadences (`cost_replay/RESULTS.md`). |

> ⚠️ **Do not present the ~63–65% accumulator number as a live result, and do
> not put it on a landing page as a savings claim.** It is an *offline
> cost-model replay* of reconstructed JSONL, it is summarizer-specific
> (off by default), and it measures **cost** (secondary, non-monotonic) — not
> sent reduction. The live measured cost win we can stand behind is the ~1M-token
> Opus session (−79% input cost on that one session).

Sources: `benchmark/results/RESULTS.md` (§5/§5b/§6),
`benchmark/results/cost_replay/RESULTS.md`,
`benchmark/results/replay_at_scale/RESULTS.md`.

### Summarizer no-degradation validation (separate result)

A scoped cognitive validation found **no measurable trimwire-attributable
degradation** of the answer model's judgment across model-free, local summarizer
(qwen3.5:4b), and provider (GLM-5.2) modes — on one fixture, with Sonnet
`--effort low`. This is a **no-harm** result, **not** a quality-lift claim.
Source: `docs/SUMMARIZER-REPLAY-VALIDATION.md`.

## What NOT to claim

- ❌ "trimwire saves X%" as a flat number. Savings are session-shape-dependent;
  plain chat is a no-op.
- ❌ Calling the **~63–65% accumulator** or **−55% cost-model** numbers "live."
  They are offline replay / cost-model.
- ❌ Putting any **cost** percentage on a landing page as the headline. Cost is
  secondary, non-monotonic, and can go **up** on short sessions (cache busting).
  Lead with request-size / context-window headroom.
- ❌ Quoting the high live percentages (50–94%) without the caveat that they come
  from adversarial/synthetic read-heavy fixtures. The typical real-session live
  number is ~17%.
- ❌ Treating the 200M-token projection as measured. No model accepts that much
  context; `claude -p` natively caps raw sent context around ~1.7 MB, so 200M is
  **projection only**.
- ❌ "gentle is the safe / recall-critical profile." It just prunes less. Recall
  path = default on + re-read.
- ❌ Any model-quality-lift claim. The focus ratio is a byte-share proxy;
  trimwire reports headroom, not better output.
- ❌ Claiming the summarizer is on by default, or that a local model runs on every
  install. The default is model-free; no model runs unless you enable it.

## Source files / reproducibility notes

**Public (in-repo, reproducible):**

- `benchmark/results/RESULTS.md` — request-size + cost-model benchmark
  (`cargo run --release --example bench`).
- `benchmark/results/cost_replay/RESULTS.md` — offline cost replay of real
  sessions.
- `benchmark/results/replay_at_scale/RESULTS.md` — ~1M-token offline replay +
  accumulator.
- `docs/SUMMARIZER-REPLAY-VALIDATION.md` — no-degradation validation summary.
- For *your own* live numbers: `trimwire stats` (real per-session ledger).

**Internal (gitignored — evidence, not shipped):**

- `internal/docs-benchmark-audit/live-canary-insights.md` — the canonical live
  `claude -p` canary (2026-06-21). Mirrored in project memory at
  `decisions/live-canary-matrix-2026-06-21.md`.
- `internal/docs-benchmark-audit/raw/live-*-metrics-2026-06-21.jsonl` — per-probe
  raw rows (authoritative for no-harm counts).
- `internal/manual-test/phase0-haiku.md` — real dogfooding session (17% live
  reduction, S10).
- `internal/manual-test/local-claude/20260624/` — 2026-06-24 acceptance canary
  (short-session live `in==sent` 0%; 53% `preview` on a real transcript).

> A `preview`/`stats` number is from *your* recorded transcript replayed locally;
> a benchmark number is from synthetic corpora; a live number is from real
> `claude -p` traffic on the wire. When in doubt, name the source.
