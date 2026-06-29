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

Every row names the **mode** the number belongs to — never quote a percentage
without it (model-free `default`, model-free `gentle`, or summarizer).

| Claim | Mode | Category | Strength |
|---|---|---|---|
| "Reduces request size / reclaims context-window headroom; the amount depends on session shape (≈0% on plain chat, more on tool/read-heavy work)." | any | All three agree | **Safe, headline-ready** |
| "Request size is ~60–95% lighter on tool/read/browser-heavy sessions (0% when there's nothing redundant)." | model-free `default` | Benchmark (offline replay) | **Safe — label as benchmark** |
| "`gentle` prunes less — a conservative subset; ~0–78% on the same corpora (0% on most; it only really fires on dedup/dup-heavy shapes)." | model-free `gentle` | Benchmark (offline replay) | **Safe — label as benchmark + gentle** |
| "Live `claude -p`: sent context shrinks with session size — single-digit % short/typical → ~50–65% read-heavy mid-size → ~75–94% very large (~1–1.7 MB)." | model-free `default` | Live | **Safe — label as live + mode, note fixtures** |
| "On a real 228-request dogfooding session, pruning cut sent bytes ~17%." | model-free `default` | Live (real traffic) | **Safe — the honest typical-session number** |
| "On a matched ~800 KB live pair: `default` cuts the largest request 73% on file-heavy work but only 15% on pure chat; the summarizer reaches 78% on that same chat by folding old turns." | model-free `default` vs **summarizer** | Live | **Safe — label as live + workload shape** |
| "On a measured ~1M-token session (Opus), sent context dropped ~75% and input cost ~79%, with 0 confident-wrong answers." | model-free `default` | Live | **Safe — single best-case session** |
| "Cost is a non-monotonic side effect: short sessions wash-to-loss, long ones win — about −55% cache-weighted input cost at 256 turns." | model-free `default` | Offline cost-model | **Safe — label as cost model, caveat** |
| "The optional summarizer's accumulator saved ~63–65% cache-weighted cost on long reconstructed sessions." | **summarizer** (local) | Offline replay | **Use with care — offline + summarizer only** |
| "When the summarizer runs, a good model keeps ≥90% of facts (qwen3.5:4b 92%, minimax-m3 / glm-5.2 100%)." | **summarizer**, per model | Probe (partly synthetic) | **Safe — retention, NOT reduction** |

**Do not** turn any single number into "trimwire saves X%." There is no single
number; the honest framing is always *"in `<mode>`, on `<session shape>`, the
reduction is `<range>`."* Mode + shape are part of the claim, not optional
footnotes.

## ⭐ Live online results (`claude -p` through the gateway) — the headline category

**These are the strongest evidence we have: real `claude -p` traffic on the
wire**, routed `claude → trimwire serve → api.anthropic.com`, not a synthetic
harness or a cost model. When you need one number for the site or a buyer, take
it from here — and say it's live. "DIRECT" in the sources means real Claude Code
with its own native auto-compaction and no gateway, so every comparison below is
*trimwire vs native Claude Code* — the real user-experienced comparison, not
"trimwire vs unbounded raw context."

The large-session reductions below are **reproducible from preserved raw gateway
logs + per-request audit trails** (`in=/sent=` per request, plus body-byte/offload
counts), not just a recorded summary — see the source note at the end.

> **Read this first.** The high live percentages (50–94%) come from
> **deliberately adversarial / synthetic read- and tool-heavy fixtures** — large
> files with facts buried mid-file, read once. They are engineered to maximise
> prunable dead weight (and to stress recall). They are **not** typical traffic.
> The honest *typical-session* live number is the ~17% real-dogfood figure, and a
> short cached session is correctly **0%**. Quote the range, and say which end a
> given workload sits at.

**One thing to internalise before the tables:** *sent reduction is
model-independent.* The gateway prunes the request before any model sees it, so
Haiku, Sonnet, and Opus all get the same byte cut for the same session. The
**answer model only changes no-harm** (stronger models *abstain* instead of
confabulating on an elided fact). The reduction differences between rows below
are driven by **session size/shape**, not by the model.

### A. Model-free pruning — `default` profile (the shipped default)

Eight deterministic strategies, no summarizer. This is the savings engine.

| Live session (size) | Answer model | Sent reduction | No-harm (vs native Claude Code) |
|---|---|--:|---|
| Short cached real session | Haiku | **0%** (`in==sent`, correct no-op) | nothing pruned |
| Real dogfooding session, 228 reqs | mixed real traffic | **17%** (98.7 MB → 81.5 MB, cache-hit 81%) | faithful; no load-bearing fact dropped |
| Adversarial read-heavy, ~150–250 KB | Haiku | **50–65%** (websvc 50, infra 64, datapipe 65) | 11/15 correct; **3 confident-wrong** on the `infra` shape (Haiku didn't re-read) |
| Adversarial read-heavy (websvc) | Sonnet-low | (same prune) | safe-abstain on pruned fact; **0 misleading** |
| Large synthetic, ~1.1 MB / ~275K tok | Haiku | **81–88%** | 0 misleading (correct or safe-abstain) |
| ~1M context, 1.18 MB / ~360K tok | Opus-low | **−75% sent (max −91%); −79% input cost** ($1.66→$0.35) | native 4/4 correct; trimwire 4/4 safe-abstain, **0 misleading** |
| Ceiling fixture, ~1.7 MB | Opus-low | **89–94%** | **0 misleading** (native compaction confabulated here; trimwire safer) |
| Output/generation-heavy, ~253 KB | Haiku | **~4%** | 0 misleading (little is prunable) |

> Reduction scales with read/tool context: ~0% short/typical → 50–65% mid-size
> read-heavy → 75–94% very large. No-harm held on **Sonnet and Opus in every
> cell**; the only confident-wrong answers were Haiku on one deliberately
> adversarial fixture. (No-harm result — **not** a quality-lift claim.)

### B. Model-free pruning — `gentle` profile (lighter touch)

Dedup + failed-input-purge + conservative bloat_cap + conservative thinking_strip
(no stale_reads / sliding_window / image_strip). **Prunes less**; it is *not* a
"safer" or recall-critical mode — just lower-savings.

| Live session (size) | Answer model | Sent reduction | No-harm |
|---|---|--:|---|
| Low-repetition small content | Haiku | **~1%** (DIRECT-equivalent) | 15/15 correct, 0 misleading |
| Large highly-repetitive, ~1 MB | Haiku | **0% median, up to ~75%** on big reqs (dedup/bloat_cap) | 2 abstain + 2 correct, 0 misleading |

> `gentle` is content-dependent (≈1% on low-repetition content, much higher only
> when content is highly repetitive). It did **not** buy more recall than
> `default` in testing. Recall-critical path = `default` on + agentic re-read.

### C. Summarizer mode (live behaviour) — engine = local or provider

The summarizer is **off by default** and only engages once the raw body exceeds
the 200 KB trigger *and* the old slice has ≥4 messages *and* state is initialised
(req ≥ 2). **What it adds depends entirely on workload shape** — and the
clearest way to see that is a matched pair of ~800 KB live sessions, one
file/read-heavy and one pure conversation, run through the same isolated gateway
on Haiku (`claude -p`, golden-harness posture; real ledger untouched).

| Mode | file-heavy ~810 KB (largest req / median) | chat-heavy ~790 KB (largest / median) |
|---|--:|--:|
| `gentle` | 35% / 24% | 21% / 4% |
| `default` (model-free) | **73%** / 41% | 15% / 11% |
| summarizer (`glm-5.2`, slice 384 KB, accept 1.5) | 73% / **51%** | **78%** / 39% |

> **The summarizer's standout case is conversation-heavy work.** On read-heavy
> work, model-free has already removed the bulk (re-readable file content), so
> `default` leads and the summarizer **ties on size** — its edge there is a
> higher *sustained median* (51% vs 41%) plus fidelity (the agent keeps the
> knowledge instead of re-reading). On pure chat there is **nothing for
> model-free to elide** (no tool output), so `default` caps at **15%** — but the
> summarizer **folds the discussion turns themselves** and reaches **78%**, the
> only mode that materially shrinks a long conversation. The summarizer always
> runs the model-free passes too; it **adds** a summary on top, it does not
> replace them. 0 misleading in every summarizer cell.

These are **largest-request** sent reductions read straight off preserved gateway
`in=/sent=` logs (summary replay visible as `stable_reprune` stubs on the wire).
Source: `internal/site-identity/live-mode-reduction-FINDINGS.md` (+`.jsonl`);
harness `internal/manual-test/quality-audit/harness/iso_mode_reduction.sh`
(file-heavy) and `iso_chat_reduction.sh` (chat-heavy). The earlier "summarizer
rarely installs / ≈ model-free" finding was specific to read-heavy fixtures under
the trigger; the matched chat-heavy run above is what corrects it.

### D. Summarizer-model fidelity (a different axis — retention, not reduction)

When the summarizer *does* run, the question is whether the chosen model keeps the
facts. This is measured by `trimwire summarizer probe` (N=10, **PASS = ≥90%
retention of 12 facts, no false-done**) — partly synthetic, separate from the live
runs above. A few reference points (full ranking in
[`MODEL-COMPATIBILITY.md`](MODEL-COMPATIBILITY.md)):

| Summarizer model | Engine | Retention | Note |
|---|---|--:|---|
| `qwen3.5:4b` | local (ollama) | **92%** | the recommended local default; validated at its default slice |
| `minimax/minimax-m3` | provider (OpenRouter) | **100%** | best provider value; also 100% @512 KB (N=1) |
| `glm-5.2` | provider (Z.ai) | **100%** | top of the Z.ai subscription lane |

> Retention ≠ request-size reduction — it's *summary quality*. Rankings are N=10
> and non-deterministic near the gate; verify your own pick with
> `trimwire summarizer probe --model <id> --runs 10`. Many weaker models fail the
> gate (e.g. `gpt-4o-mini` 50%, `gemini-2.5-flash-lite` 33%) — see the doc.

### Source & reproducibility

- **Canary writeup:** `internal/docs-benchmark-audit/live-canary-insights.md`
  (2026-06-21, ~$34.91 / ~145 probes across Haiku/Sonnet/Opus). Mirrored in
  project memory at `decisions/live-canary-matrix-2026-06-21.md`.
- **Per-probe metrics (authoritative for no-harm counts):**
  `internal/docs-benchmark-audit/raw/live-*-metrics-2026-06-21.jsonl`
  (model/effort/cost/correctness per probe).
- **Raw per-request reduction — preserved:**
  `internal/docs-benchmark-audit/raw/live-gateway-logs-preserved/` holds the
  surviving gateway `in=/sent=` logs **and** the per-request `--audit` trails
  (rescued from the ephemeral `/tmp/live*` on 2026-06-24). The large-session
  figures are reproducible from these, e.g.:

  | Lane | Source log | Largest request | Reduction |
  |---|---|--:|--:|
  | model-free `default`, Opus 1M | `livebig_op1__gw_mf_default.log` | 1,178,618 B → 296,776 B | **−75%** (max req −91%) |
  | model-free `default`, Opus ceiling | `livebig_oc1__gw_mf_default.log` | 1,697,491 B → 242,703 B | **−86%** (max req −94%) |
  | summarizer (local), big | `livebig_bl1__gw_local_default.log` | 1,102,045 B → 211,483 B | **−81%** (max −88%) |
  | summarizer (provider), big | `livebig_bp1__gw_prov_default.log` | 1,098,204 B → 211,060 B | **−81%** (max −88%) |
  | `gentle`, big repetitive | `livebig_bg1__gw_mf_gentle.log` | 1,026,868 B → 259,316 B | **−75%** (median 0%) |
  | model-free `default`, sequential | `liveseq_sl1__gw_local_default.log` | 349,389 B → 128,535 B | **−63%** (median 34%) |

  > **Median vs. largest request:** a growing session is small early on, so the
  > *median* request reduction (32–48%) is lower than the *largest* (full-context)
  > request (75–94%). The headline figure is the largest request — that's the one
  > that bloats and that trimwire cuts most.

- **Real-traffic 17%:** `internal/manual-test/phase0-haiku.md` (S10).
- **Matched per-mode × workload pair (file-heavy vs chat-heavy, §C):**
  `internal/site-identity/live-mode-reduction-FINDINGS.md` (+`.jsonl`); harness
  `internal/manual-test/quality-audit/harness/iso_mode_reduction.sh` and
  `iso_chat_reduction.sh` (isolated gateway, real ledger sha verified unchanged).
- **Short-session 0%:** `internal/manual-test/local-claude/20260624/`
  (`13-gateway-requests.txt`, `30-stats-iso.txt`).
- **Summarizer-model fidelity (table D):** public `docs/MODEL-COMPATIBILITY.md`.

The small-fixture lanes (websvc/infra/datapipe 50–65%) were the earliest runs and
their `/tmp` logs were already cleared before rescue; those percentages live in
the canary writeup, and the preserved JSONL metrics retain their per-probe
cost/correctness rows.

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
