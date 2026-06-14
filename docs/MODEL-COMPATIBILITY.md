# Summarizer model compatibility — maintainer directional tests

> ⚠️ **These are trimwire's OWN directional tests, NOT community data.** They were
> run by the maintainer on private API keys with a non-deterministic model. The **@128 KB**
> verdicts below are **N=10** (reliable); the **>128 KB ceilings and the free tier are
> still N=1** (directional only — see the warnings inline). Treat this as a *starting
> reference* + a tool to run yourself (`probe --runs`), not authoritative rankings. The
> **real, aggregated numbers come from opted-in users** via `trimwire share benchmark`
> and live on the community **[benchmark leaderboard](https://trimwire.dev/benchmark/)** (which fills in
> over time, once each model group crosses the k-anonymity threshold). Don't read
> this table as community results.

## What's measured

The opt-in summarizer compacts the OLD slice of a session (see [SUMMARIZER.md](SUMMARIZER.md)).
The per-engine **`slice_char_budget`** controls how much old content is summarized per
segment — bigger = the summary owns more old content, but every model has a **fidelity
ceiling** past which it starts dropping facts. This page measures that ceiling per model.

**Method:** `examples/api_harm` plants **12 distinct verbatim facts** (file paths, error
codes, decisions, identifiers) spread across the start/middle/end of a slice of a given
size, runs the real summarizer call, and checks how many survive. **Pass = ≥ 90%
retention (≥ 11/12), no false-done.** The "ceiling" is the largest slice that still
passes. Vary the slice size, *not* the fact count (a terse summary structurally holds
only ~12–18 distinct facts, so cramming more measures summary budget, not recall).
Reproduce / test your own model:

```bash
OPENROUTER_API_KEY=… TRIMWIRE_API_HARM_STYLE=openai \
TRIMWIRE_API_HARM_BASE_URL="https://openrouter.ai/api" \
TRIMWIRE_API_HARM_KEY_ENV=OPENROUTER_API_KEY \
TRIMWIRE_API_HARM_MODEL="<id>" TRIMWIRE_API_HARM_BYTES=131072 \
cargo run --release --example api_harm
```

`~$/run` is the approximate cost of one 128 KB run (~32K input + ~300 output tokens) at
the provider's listed price, for budgeting a sweep — not trimwire's per-session cost.
Prices are OpenRouter's **pass-through per-model rates** (from the `/api/v1/models`
catalog), so `$/run` = `32000 × prompt_price + ~300 × completion_price`. For *exact*
per-call cost, OpenRouter returns a generation `id` you can look up via
`/api/v1/generation?id=…`; aggregate per-model spend shows up in `/api/v1/activity`
the **next** UTC day (it only covers completed days).

## Results — verified multi-run @128 KB (2026-06-11)

> **⚠ Single-run model rankings are UNRELIABLE — these are now N=10.** Model summaries
> are non-deterministic; the 90% gate means "drop ≤ 1 of 12 facts", so a model near the
> line is a coin flip per run. A full N=10 re-test at 128 KB **overturned 11 of the prior
> single-run verdicts — almost all DOWNGRADES** (single runs were systematically
> optimistic). Two published *ceilings* (gemini-3.1-flash-lite "768 KB", minimax-m2.7
> "640 KB") were N=1 and are **INVALIDATED** — they fail even 128 KB at N=10. **Before
> trusting any model at the 128 KB default, validate it yourself:**
> `trimwire summarizer probe --model <id> --runs 10` (PASS = all 10 ≥ 90%).

### Stable-safe @128 KB — N=10, every run passed (use these)

| Model | Provider | pass @128 KB | min | ~$/128 KB run | note |
|---|---|---|---|---|---|
| **minimax/minimax-m3** | OpenRouter | **10/10** | 100% | ~$0.003 | best value; also 100% @512 KB (N=1) |
| **mistralai/ministral-8b-2512** | OpenRouter | **10/10** | 92% | ~$0.005 | ✦ discovery — cheapest confirmed-stable model found |
| **mistralai/codestral-2508** | OpenRouter | **10/10** | 92% | ~$0.011 | ✦ discovery — the ONLY coding specialist that passes (writes structured summaries) |
| **deepseek/deepseek-v4-pro** | OpenRouter | **10/10** | 92% | ~$0.0045 | |
| **deepseek/deepseek-v4-flash** | OpenRouter | **10/10** | 92% | ~$0.003 | ⬆ upgrade — a single 83% run had wrongly condemned it |
| **glm-5 / glm-5-turbo** | Z.ai | **10/10** | 92% | (Z.ai sub) | |
| **glm-5.1** | Z.ai | **10/10** | 100% | (Z.ai sub) | ✦ new model |
| **openai/gpt-5.4-mini** | OpenRouter | **10/10** | 100% | ~$0.026 | the cheap OpenAI option that holds (NOT gpt-4o-mini) |
| **qwen/qwen3.7-plus** | OpenRouter | **10/10** | 92% | ~$0.018 | ✦ discovery — new Alibaba series (1M ctx); cheap + clean |
| **stepfun/step-3.7-flash** | OpenRouter | **10/10** | 92% | ~$0.015 | ✦ discovery — cheap + clean |
| **mistralai/mistral-large-2512** | OpenRouter | **10/10** | 92% | ~$0.028 | ✦ discovery — solid, pricier |
| **nvidia/nemotron-3-ultra-550b** | OpenRouter | **10/10** | 92% | ~$0.036 | ✦ discovery — passes but overkill/expensive |
| **openrouter/owl-alpha** | OpenRouter | **10/10** | 92% | **free** | ✦ discovery — zero-cost, BUT "alpha"/first-party: availability + rate-limits unstable, validate before relying |
| **qwen3.5:4b (local @60 KB)** | ollama | **5/5** | 100% | free | the local default |

Expensive top tier — passed but only **N=3** (directional, not high-confidence):
claude-haiku-4.5, claude-sonnet-4.6, gpt-5.4 (all 3/3); gemini-3.1-pro-preview (3/3 but
min = p50 = 92%, right at the gate).

### NOT safe @128 KB — N=10, fails some runs (pick a stable one instead)

Almost all of these were "safe" on a single lucky run. `pass` is out of 10; `min` is the
worst run's retention.

| Model | Provider | pass @128 KB | min | prior (N=1) verdict |
|---|---|---|---|---|
| google/gemini-3.1-flash-lite | OpenRouter | 8/10 | 83% | "768 KB" — **ceiling invalidated** |
| minimax/minimax-m2.7 | OpenRouter | 8/10 | 83% | "640 KB capable" — **ceiling invalidated** |
| google/gemini-2.5-flash | OpenRouter | 8/10 | 75% | "128 KB safe" |
| glm-4.6 | Z.ai | 6/10 | 83% | knife-edge |
| glm-4.5 | Z.ai | 6/10 | 75% | "128 KB safe" |
| google/gemini-3.5-flash | OpenRouter | 6/10 | 83% | "256 KB" |
| qwen/qwen3-30b-a3b-instruct-2507 | OpenRouter | 6/10 | 75% | "128 KB safe" |
| qwen/qwen3-235b-a22b-2507 | OpenRouter | 6/10 | 58% | "128 KB safe" (also 256→75%, 512→33%) |
| glm-4.5-air | Z.ai | 5/10 | 75% | "128 KB safe" |
| deepseek/deepseek-v3.2 | OpenRouter | 5/10 | 75% | "128 KB safe" |
| glm-4.7 | Z.ai | 4/10 | 67% | borderline |
| qwen/qwen3-max | OpenRouter | 2/10 | 67% | avoid (confirmed) |
| openai/gpt-4o-mini | OpenRouter | 0/10 | 50% | avoid (confirmed) |
| google/gemini-2.5-flash-lite | OpenRouter | 0/10 | 33% | avoid (confirmed) |
| moonshotai/kimi-k2.6 | OpenRouter | unreliable | — | "128 KB safe" — **wrong**: `finish_reason=length` truncation + timeouts at 128 KB |

### Ceilings above 128 KB are still N=1 — directional only

The large-slice ceilings (minimax-m3 / deepseek-v4-pro ≥ 1 MB, glm-5 family ~768 KB) come
from SINGLE runs at those sizes; only **128 KB is N=10-verified**. Treat any > 128 KB
number as a starting point and re-probe at your target size with `--runs`. The two
ceilings that the N=10 @128 KB sweep invalidated (gemini-3.1-flash-lite, minimax-m2.7) are
the warning: an N=1 ceiling can be pure luck.

Other models that showed N=1 promise but were **not re-verified at N=10** (treat as
suspect given the downgrade pattern — probe before relying): `openai/gpt-5.5` (≥512 KB?,
expensive ~$0.17/run), `moonshotai/kimi-k2.5` (896 KB?), `google/gemini-2.5-pro`,
`nvidia/nemotron-3-super-120b-a12b:free` (256 KB?).
(`openai/gpt-5.4-mini` was re-tested → 10/10, now in the stable-safe list above.)

> **"Passes a bigger slice but fails a smaller one" is impossible as a real property —
> it's the tell-tale of N=1 noise.** Fidelity falls *monotonically* as the slice grows
> (more content compressed into the same terse summary = more facts dropped; measured
> directly: qwen3-235b 128→256→512 KB = 92→75→33%). So minimax-m2.7's old "640 KB
> capable" (N=1) next to its real "fails 128 KB 2/10 of the time" (N=10) doesn't mean it's
> better at 640 KB — it means that one 640 KB run was a lucky draw, and at N=10 it would
> fail 640 KB *worse* than 128 KB. Same for any model: trust the smaller-slice multi-run
> number, and assume bigger is never better.

### Free models (OpenRouter `:free`) — N=1, unverified

> These are single-run only — given the downgrade pattern above, assume the real
> pass-rates are LOWER. Re-probe with `--runs` before relying on a free model.

| Model | Ceiling | Reliability |
|---|---|---|
| **openrouter/owl-alpha** | 128 KB | **best free option @128 KB — N=10 = 10/10** (min 92%). BUT "alpha"/first-party: availability + rate-limits are unstable; validate with `--runs` before relying. |
| nvidia/nemotron-3-super-120b-a12b:free | — | ⚠ **NOT safe** — the old "100%" was N=1 luck; the paid same-model is **0/10 @128 KB (N=10)**. Avoid. |
| qwen/qwen3-next-80b-a3b-instruct:free | — | **persistent HTTP 429** (free tier heavily rate-limited; untestable in a sweep) |
| openai/gpt-oss-20b:free / gpt-oss-120b:free | — | fail 128 KB (67–83%) |
| google/gemma-4-31b-it:free | — | fail 128 KB (83%) |

> Free-tier caveat: expect **rate-limits (429)** and the occasional transient error/empty
> response. For a one-off they're fine; for a sustained workload they're flaky.

### Local (ollama)

trimwire defaults the local slice to **~60 KB** via the `max_num_ctx = 25600` cap — a
**RAM/CPU safety guard, NOT a capability limit** (qwen3.5 has a ~256K window). (The
original default was ~38 KB; raised after the measurements below.)

| Model | Retention (early tests) |
|---|---|
| qwen3.5:4b (default) | 100% (N=5 @60 KB; the recommended local model) |
| qwen3.5:4b-q8_0 | 100% (N=1) |
| qwen3.5:9b | ~92% (N=1; a 2-word needle phrased differently — effectively fine) |

**Pushed past the old default (measured):** qwen3.5:4b at `num_ctx=40000` (a ~96 KB
serialized-slice budget = 40000×2.5−2000) held **92% (11/12, missed only the end fact)**
— i.e. **qwen3.5:4b ≈ GLM-4 class**, confirming the local model is *capable* well beyond
the old ~38 KB cap. The
RAM cost is small: the model loaded at **4.3 GB** (only ~0.9 GB of KV cache over the
3.4 GB weights) and ran in **~13 s on a GPU**.

**Local ceiling for qwen3.5:4b is ~60 KB — do NOT raise it for this model.** N=10
verification: **@60 KB = 10/10 (100%)**, but **@96 KB (`max_num_ctx=40000`) = 0/10**
— it fails *deterministically* at 83.3% (drops the same ~2 facts every run). The earlier
"~92% @96 KB" was a single lucky run. So the `summarizer.local.max_num_ctx` knob exists,
but raising it past the ~60 KB default does NOT help qwen3.5:4b — keep the default. (A
*stronger* local model might hold more; validate it with `probe --runs 10 --bytes <N>`
before raising the knob.) On CPU-only boxes keep it modest anyway — a big prompt is slow
(the summarizer runs in the background, so latency doesn't block your session).

## Choosing a backend

The summarizer runs on a local model or any cloud model via an **Anthropic-style**
(`/v1/messages` + `x-api-key`) or **OpenAI-style** (`/v1/chat/completions` + Bearer)
endpoint. Two facts shape the choice:

- **For the summarizer you need a standard API key + endpoint.** Subscription OAuth
  tokens (Claude Max, ChatGPT/Codex) are NOT a drop-in here — so for trimwire's
  summarizer, Anthropic and OpenAI are effectively **pay-per-token API**. *(Whether a
  coding **agent** can ride your subscription is a separate, fast-moving ToS question —
  Anthropic has reportedly tightened on third-party tools using Max tokens, while
  OpenAI/Codex is currently more permissive — but that's about the agent, not the
  summarizer backend.)* The subscription/plan options that DO work for the summarizer are
  **Z.ai** (GLM coding plan) and **MiniMax** (Token Plan = prepaid credits you spend via
  your normal API key — convenience/quota, not necessarily a per-token discount; verify
  on your billing page).
- **Reliability ≠ price, and newer ≠ failing.** Cheapest reliable API = DeepSeek V4-Flash
  (pennies); many pricier flash models are unreliable. On OpenAI, the *old* `gpt-4o-mini`
  fails the gate but **`gpt-5.4-mini` is 10/10** — validate the specific tag, don't assume.

Recommended by situation (all verified reliable @128 KB unless noted; pricing/plan details
drift — confirm on your own account):

| You want… | Use | Endpoint (style · base_url) | Plan |
|---|---|---|---|
| Zero cost, own hardware | **qwen3.5:4b** (local, ~60 KB ceiling) | local ollama | free |
| Cheapest reliable API | **deepseek-v4-flash** | anthropic · `api.deepseek.com/anthropic` | PAYG (pennies) |
| Top fidelity / best value | **minimax-m3** | anthropic · `api.minimax.io/anthropic` (or via OpenRouter) | PAYG / MiniMax Token Plan |
| Already on a Z.ai plan | **glm-5 / glm-5-turbo / glm-5.1** | anthropic · `api.z.ai/api/anthropic` | coding-plan subscription |
| Prefer OpenAI | **gpt-5.4-mini** (10/10; NOT gpt-4o-mini) | openai · `api.openai.com/v1` | PAYG |
| One key for everything | any of the above | openai · `openrouter.ai/api/v1` | OpenRouter PAYG credits |

Avoid: kimi-k2.6 (fails the gate / truncates), gpt-4o-mini, and the mid-tier flash models
that miss 128 KB at N=10 (see "NOT safe" above). Always confirm with
`trimwire summarizer probe --model <id> --runs 10`.

## Takeaways

- **Single-run model rankings are unreliable — N=10 overturned 11 of them, almost all
  downgrades.** Don't trust a one-run "pass"; run `probe --runs 10` (PASS = all ≥90%).
- **Keep the 128 KB default — a smaller budget does NOT rescue weak models.** We swept
  the 11 failing models at **64 KB, N=10: 0 of 11 became reliable** (they fail at nearly
  the same rates as at 128 KB). The failures are **model-capability variance** under
  long-context summarization, not "too many tokens" — so halving the budget just halves
  the useful work for the *good* models for zero safety gain. The real safeguard is
  **model choice**: pick a reliable one (below) or validate yours with `probe --runs 10`.
  128 KB is NOT a universal safe floor, but lowering the global default isn't the fix.
- **Reliable @128 KB (N=10, all-pass):** minimax/minimax-m3, deepseek/deepseek-v4-pro,
  deepseek/deepseek-v4-flash, Z.ai glm-5 / glm-5-turbo / glm-5.1, and local qwen3.5:4b
  (@60 KB). Best value: **minimax-m3** (~$0.003/run).
- **Newer ≠ better, and bigger-model ≠ safer.** qwen3-235b fails where v4-flash passes;
  glm-4.5/4.6/4.7 all fail 128 KB at N=10 while glm-5.x is rock-solid; kimi-k2.6 truncates.
  Pick a *validated* tag, not the latest or the largest.
- **A 20-model "discovery" sweep confirmed it (2026-06-11).** Most new/exotic models
  *fail* faithful summarization — Tencent Hy3 (8/10), Xiaomi MiMo family (truncate/0–6/10),
  Llama 4 Maverick/Scout (a 0% run / 0/10), StepFun-3.5, Nemotron-3 Nano/Super, qwen3-235b/30b
  (0/10). Only **step-3.7-flash, mistral-large-2512, nemotron-3-ultra, and owl-alpha (free)**
  held 10/10 (now in the stable-safe list).
- **Specialist models mostly DON'T transfer (tested it — round 4).** Coding models nearly all
  fail: qwen3-coder-480b (75%), -30b/-next/-flash, devstral, kat-coder, laguna — all fail N=10.
  Coding training teaches "ignore the noise," the opposite of verbatim fact-retention. Roleplay
  (aion-2.0) fails harder (embellishes — a catastrophic 16.7% run). The lone exception:
  **codestral-2508 passes (10/10)** precisely because it emits a STRUCTURED enumerated summary
  (GOAL/FILES/FACTS lists) that mechanically preserves identifiers. So: don't reach for a
  specialist; if one happens to write structured output it can work, but validate it.
- **Failure mode = over-terse output** (the model emits a short summary that sheds 1–2 of
  the 12 facts → 83–92% → coin-flips the gate).
- **Bigger slice is never genuinely safer** (fidelity is monotonic in size); a model that
  looks better at a larger slice is N=1 noise — see the ceilings note above.
- **Inherent limit:** any summarizer reliably surfaces ~12–18 distinct load-bearing facts;
  a very fact-dense old region loses some detail (the model re-reads files for the rest).
  Lossy summarization working as intended, not a model defect.
