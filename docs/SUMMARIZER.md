# trimwire — Summarizer

trimwire is **model-free by default**. The summarizer is an optional feature that
compresses the old part of a long session with a language model. It never runs
unless you configure it.

**Never load-bearing.** Any failure (model down, slow, bad output, missing API key)
silently falls back to model-free pruning. The summarizer is strictly additive.

> **Quick start:** run `trimwire summarizer setup` — an interactive wizard that
> asks which engine you want, which model, and (for API engines) which API-key
> environment variable, then writes the config block for you.

---

### Two config concepts

- **`engine`** — *which backend runs the summary*: `"model-free"` (disabled),
  `"local"` (local ollama), or a provider `id` you define (e.g. `"anthropic"`).
- **`style`** — *the HTTP API shape of a provider*: `"anthropic"` (Anthropic
  Messages API) or `"openai"` (OpenAI-compatible `/v1/chat/completions`; covers
  OpenRouter and others). Only used in `[[summarizer.providers]]` entries;
  `local` and `model-free` have no style.

---

## Three engines

### model-free (default)

No summarizer, no model calls. This is the starting state for every install. The
eight deterministic pruning strategies still run.

### local

Sends the prunable slice to a local [ollama](https://ollama.com) server on your own
machine. No API key, no cloud call, no data leaves your machine.

Default model: **`qwen3.5:4b`** (~3.4 GB RAM with `keep_alive=0`) — the recommended
default. It and the other approved local tags (`qwen3.5:4b-q8_0`, `qwen3.5:9b`; plus
`qwen3.5:2b` as a warned RAM opt-down) have passed the cost-replay gate and the **harm
gate** (an offline benchmark that checks a model refuses to overstate completed work —
"false-done" claims — and retains load-bearing facts from the source slice).

Validated tiers:

| Model | RAM | Notes |
|---|---|---|
| `qwen3.5:4b` | ~3.4 GB | Default — the baseline |
| `qwen3.5:4b-q8_0` | ~4.8 GB | Higher-fidelity quant; fewer false-dones |
| `qwen3.5:9b` | ~6.1 GB | Cleanest on diverse real slices |
| `qwen3.5:2b` | ~2.7 GB | Lighter opt-down; drops more facts (guard warns) |

The runtime guard **refuses** known-bad models (`qwen2.5-coder:3b`, `granite4.1:3b`,
`granite4.1:8b`, `ministral-3:3b`, `gemma3:4b`, `qwen3.5:0.8b`) — they hallucinate
or overstate completed work.

### Cloud API (engine = a provider id)

Sends the prunable slice to a cloud API provider you choose: Anthropic, OpenAI, or
any OpenAI-compatible endpoint such as OpenRouter. You supply your own API key via
an environment variable. The `engine` value is your provider's `id` (e.g.
`"anthropic"`, `"openrouter"`) — not the literal string `"api"`.

See the **Privacy** section below before enabling this engine.

---

## Manual config

**Prefer the wizard:** `trimwire summarizer setup` writes this block for you
interactively. To hand-edit, add a `[summarizer]` block to
`~/.config/trimwire.toml` (or run `trimwire config` to open it). The default is
`engine = "model-free"` (no summarizer).

### Local engine

```toml
[summarizer]
engine = "local"

[summarizer.local]
endpoint = "http://localhost:11434"   # ollama default
model    = "qwen3.5:4b"
```

Pull the model first:

```
ollama pull qwen3.5:4b
```

### API engine(s)

Each cloud provider is a `[[summarizer.providers]]` entry with a short `id`. The
`engine` (and any `fallback` entry) references a provider by that `id`:

```toml
[summarizer]
engine = "anthropic"               # a provider id below (or "local" / "model-free")

[[summarizer.providers]]
id          = "anthropic"          # short name; what engine/fallback reference
style       = "anthropic"          # "anthropic" | "openai" (OpenAI-compatible)
base_url    = "https://api.anthropic.com"
model       = "claude-haiku-4-5"
api_key_env = "ANTHROPIC_API_KEY"  # NAME of the env var holding your key (never the key)
```

You can list several providers and reference each by `id`. Provider ids must be unique
and can't be `local` or `model-free` (reserved).

`style` picks the wire protocol — `anthropic` (`x-api-key`, `/v1/messages`) or `openai`
(`Bearer`, `/v1/chat/completions`). trimwire appends that path to `base_url`. For a
provider whose path **isn't** the standard one, set **`full_url`** (the exact POST URL);
`base_url` is then ignored and `style` still selects the auth header + payload shape.

### Provider recipes

```toml
# OpenAI
[[summarizer.providers]]
id = "openai"
style = "openai"
base_url = "https://api.openai.com"
model = "gpt-5.4-mini"
api_key_env = "OPENAI_API_KEY"

# OpenRouter (300+ models behind one OpenAI-compatible endpoint)
[[summarizer.providers]]
id = "openrouter"
style = "openai"
base_url = "https://openrouter.ai/api"      # NOT ".../api/v1" (trimwire adds /v1/...)
model = "minimax/minimax-m3"                # see MODEL-COMPATIBILITY.md for ceilings
api_key_env = "OPENROUTER_API_KEY"

# Anthropic
[[summarizer.providers]]
id = "anthropic"
style = "anthropic"
base_url = "https://api.anthropic.com"
model = "claude-haiku-4-5"
api_key_env = "ANTHROPIC_API_KEY"

# Z.ai — Anthropic-compatible endpoint (simplest)
[[summarizer.providers]]
id = "zai"
style = "anthropic"
base_url = "https://api.z.ai/api/anthropic"
model = "glm-4.6"
api_key_env = "ZAI_API_KEY"

# Z.ai — OpenAI-compatible endpoint (non-standard /paas/v4 path → use full_url)
[[summarizer.providers]]
id = "zai-openai"
style = "openai"
full_url = "https://api.z.ai/api/paas/v4/chat/completions"
model = "glm-4.6"
api_key_env = "ZAI_API_KEY"

# Azure OpenAI (deployment URL with api-version → use full_url)
[[summarizer.providers]]
id = "azure"
style = "openai"
full_url = "https://YOUR-RESOURCE.openai.azure.com/openai/deployments/YOUR-DEPLOYMENT/chat/completions?api-version=2024-10-21"
model = "gpt-4o-mini"
api_key_env = "AZURE_OPENAI_API_KEY"

# Self-hosted OpenAI-compatible (vLLM / LM Studio / llama.cpp server)
[[summarizer.providers]]
id = "vllm"
style = "openai"
base_url = "http://localhost:8000"
model = "Qwen/Qwen3-30B"
api_key_env = "VLLM_API_KEY"   # set to any non-empty value if the server ignores it
```

### Fallback

If the primary engine is unavailable, trimwire tries each entry in `fallback` in
order — each is a provider `id` or `"local"`. `model-free` is the implicit
terminal and never needs to be listed:

```toml
[summarizer]
engine   = "local"
fallback = ["anthropic"]    # fall back to the "anthropic" provider if ollama is down
```

---

## Commands

| Command | What it does |
|---|---|
| `trimwire summarizer setup` | Interactive wizard: configures engine, model, and API key |
| `trimwire summarizer status` | Show the current summarizer config and whether it is reachable |
| `trimwire summarizer benchmark [--model <tag\|provider-id>]` | Score a local ollama model or a configured API provider for fact retention and compression. API providers require `--yes` to make real (paid) calls; without it, prints a dry-run cost warning. See [`BENCHMARK.md`](BENCHMARK.md). |
| `trimwire summarizer probe [--model <tag\|provider-id>] [--bytes N] [--runs N] [--yes]` | Slice-ceiling fact gate: plant distinctive facts across a synthetic OLD slice at your `slice_char_budget` (or `--bytes`), summarize it with your configured model (or `--model`), and report fact retention by position. **Model summaries are non-deterministic — use `--runs 10` to see the distribution (pass-rate / p50 / min); PASS requires ALL runs ≥90%.** A single run near the gate is a coin flip. API providers need `--yes` (cost scales with `--runs`); `local`/an ollama tag runs locally. |

`trimwire doctor` also reports the summarizer configuration when `engine` is not
`model-free`.

---

## How it works

At a reprune checkpoint (every ~8 messages on a large session), trimwire sends the
old prunable slice to the summarizer in the background and caches the result. On
later turns, the cached summary is replayed verbatim in place of the old slice — so
the prompt cache stays warm after the initial one-time bust.

The **accumulator** (default on; appends new frozen delta segments rather than
replacing the whole summary on each re-summarization, so older segments stay
byte-frozen and the cache only busts on the new delta) delivers measured savings
of **-64.6% cost vs baseline** on a real 981-turn session.

> **What "cost" means here.** trimwire's cost figure is **cache-weighted tokens** —
> `input + 0.1·cache_read + 1.25·cache_creation` — because every turn re-sends the
> whole conversation and Anthropic bills cache reads at ~10% and cache writes at
> ~125% of the input rate. On a **pay-per-token API** that maps straight to dollars;
> on a **subscription/Max plan** it maps to your 5-hour quota and rate limits (fewer
> weighted tokens = more turns before you hit the wall). "Savings" is the reduction
> in that figure vs sending the un-pruned body. It is **non-monotonic**: pruning that
> churns the cached prefix can cost *more* on short sessions — which is why reprune
> (cache-stable replay) is on by default. The −64.6% above is a long-session figure,
> not a universal one; short sessions are a wash. See [`BENCHMARK.md`](BENCHMARK.md) §5.

The summarizer is built from the **original, un-pruned** messages (trimwire snapshots
the request body *before* the deterministic strategies run) — so the summary always
reflects the real conversation, never elision markers.

By default the summarizer only keeps a summary when it **beats** model-free pruning of
that slice. On tool-output-heavy sessions the deterministic strategies usually win; the
model earns its keep on old, reasoning-dense content they cannot compress.

### Letting the summary own more of the old content (API engines)

A clean summary is far clearer to the model than lossy elision markers, so on a strong
**API engine** you can let the summary cover a much larger fraction of old content (and
prefer it even when it isn't strictly smaller than model-free):

| Knob | Default | What it does |
|---|---|---|
| `slice_char_budget` | unset → per-engine | Max serialized bytes of the old slice summarized per segment. Unset = **local** uses a num_ctx-safe cap (~60 KB from the default `max_num_ctx=25600`; ollama's KV cache is sized for `num_ctx`, so a bigger slice risks OOM), an **API-only** chain uses ~128 KB (cloud models have 100K+ windows). Set an explicit value to override. Capped to the local size whenever the local engine is anywhere in the chain. |
| `accept_ratio` | `1.0` | `1.0` = strict (keep the summary only if it's smaller than model-free). `>1.0` (e.g. `1.5`, recommended for API) keeps a higher-fidelity summary up to `accept_ratio ×` the model-free size, bounded by a 16 KB absolute growth cap. Keep `1.0` for weak local models. |

```toml
[summarizer]
engine = "myprovider"      # an API-only chain (no "local")
accept_ratio = 1.5         # prefer the clearer summary over lossy trims
# slice_char_budget = 196608  # optional: override the 128 KB API default
```

These are opt-in: the local-only defaults and the model-free path are unchanged. The
deterministic strategies still run (and remain the full fallback when the summarizer is
off or fails) — they simply own less of the old region once the summary covers it.

> **`slice_char_budget` has a per-MODEL fidelity ceiling — validate before raising it.**
> The 128 KB API default is a safe floor for low-tier models. Measured retention ceilings
> (12 verbatim facts, `examples/api_harm`):
>
> | Model class | Safe `slice_char_budget` | Notes |
> |---|---|---|
> | Unknown / low-tier models | **128 KB** (the default) | The conservative floor — don't raise it without gating the model below. Note the **GLM-4.x family (incl. 4.5-Air) FAILS 128 KB at N=10** ([MODEL-COMPATIBILITY.md](MODEL-COMPATIBILITY.md)); prefer GLM-5.x. |
> | **GLM-5 / GLM-5-Turbo / GLM-5.1** | **~700 KB** (much more) | Reliable through ~512 KB, ~92% at 768 KB. Big coverage win — point the summarizer at a GLM-5-class model and set e.g. `slice_char_budget = 720896`. |
>
> There's a clear capability cliff between the GLM-4.x and GLM-5 generations. **Default
> stays 128 KB (protects weak models); raise it only on a model you've gated.** To find a
> configured provider's ceiling: `trimwire summarizer probe --model <provider-id> --bytes 720896
> --runs 10 --yes` — keep the pass rate high (retention ≥ 90%, no false-done) before raising it.
>
> **Summarizing more often does NOT add coverage.** Lowering `resummarize_after_bytes`
> just splits the same old-content delta into more (smaller) segments — total bytes
> summarized is unchanged, and it can hit `max_summary_segments` early on long sessions and
> collapse the frozen chain. To cover *more* of a very long session, raise
> `max_summary_segments` (not lower the threshold).

**Which model can hold how big a slice?** See [MODEL-COMPATIBILITY.md](MODEL-COMPATIBILITY.md)
for our directional pre-tests (e.g. MiniMax-M3 / DeepSeek-V4-pro ≈ 1 MB; GLM-5 ≈ 768 KB;
GLM-4.x ≈ 128 KB) — note those are *our own* small-N measurements, **not** the live
community benchmark.

---

## Limits and long sessions

The summarizer extends how far a session runs before the context fills and slows the
drift of facts out of the window — but it is **not infinite**, and it's not a substitute
for Claude's own memory of the conversation.

**What it preserves well:** verbatim tokens it's prompted to copy (file paths, error
codes, identifiers, decisions) and the GOAL/DECIDED/NEXT thread.

**What it loses:** dense reasoning (the summary is terse by design), facts buried in the
middle of large tool outputs (head/tail trimmed), and — on a *very* long session — the
oldest detail as **summary coverage shrinks**. The accumulator builds a chain of frozen
summary segments; at `max_summary_segments` (default 128 ≈ ~4 MB of old content on an API
engine) the chain **collapses** — it is REPLACED by a single fresh summary of the most
recent old turns that fit the budget. The collapse re-summarizes the **original** message
bytes (it is *not* a telephone-game summary-of-summaries — the old detail is read fresh,
not compressed twice), but the older turns that no longer fit the summary window revert to
**model-free** pruning: deterministic stubs you can recover by re-reading the files, never
fabrication. Frozen segments themselves replay **verbatim** — the summarizer never
re-compresses its own output, so already-summarized facts don't drift turn-to-turn.

So on a truly long, multi-day project the model *can* get less faithful about the
earliest work. Honest guidance:

- **Checkpoint at task boundaries** with Claude Code's `/compact` (consolidates from the
  model's own view), or **start a fresh session with a short handoff** ("continuing from
  X; key decisions are Y and Z") — a fresh session also resets the accumulator chain.
- **Lean on files, not chat memory:** files are always re-readable — ask Claude to
  re-read the relevant file rather than trust a summary of it. The most recent
  `keep_recent_turns` turns are always kept verbatim.
- trimwire logs a one-time warning to its terminal when a chain collapse happens — that's
  the cue a checkpoint is worth it. The summarizer is a *second chance at runway*, not a
  replacement for task decomposition.

---

## Privacy

### Local engine

The prunable slice goes to `http://localhost:11434` (or your configured endpoint) and
stays on your machine. Nothing leaves your machine. No API key is involved.

### Cloud API engine

**The prunable slice is sent to the provider you configure** (e.g. `api.anthropic.com`,
`api.openai.com`, or your chosen OpenRouter endpoint). This is the old part of your
session: it may include older tool outputs, reasoning, and file content from earlier
in the conversation.

Key points:

- **Your key, your provider, your choice.** trimwire has no default cloud endpoint.
  You configure the provider and supply your own API key. The privacy posture is
  determined by your provider's data handling policy, not trimwire's.
- The slice is the *old* part of context (outside the recent `keep_recent_turns`
  window), not your current prompt or system prompt.
- The summary is returned and cached locally; trimwire does not forward the summary
  upstream to Anthropic.

If sending older session content to a cloud provider is not acceptable for your use
case, use the `local` engine instead — it is functionally equivalent with no data
leaving your machine.
