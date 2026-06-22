# Overview

**trimwire is a tiny local gateway that prunes Claude Code's API context on every request.**
Claude Code re-sends the whole conversation on every turn, so long sessions fill up with
stale tool output, superseded file reads, and old logs. trimwire sits between Claude Code
and Anthropic and removes that dead weight before it's sent — keeping the current task from
being buried in history.

```
Claude Code  ──►  trimwire (prune on every request)  ──►  Anthropic API
                  • deterministic, cache-safe
                  • optional summarizer
                  • leaves re-read cues for what it elided
```

It's a transparent proxy: same auth, same sampling, and — on the default path — the same system
prompt (the opt-in `system_shape_normalize` strategy is the only exception); only the
**conversation context** changes (that's the point). It works with API key, Pro, and Max
(API key is clearly fine; Pro/Max OAuth has a grey-area ToS note — see [FAQ.md](FAQ.md)).

## Why it matters

When a session outgrows the model's window, some context has to be dropped — and any lossy
step (a model summarizer, including trimwire's own, or a plain window cutoff) can discard
older detail. trimwire's mechanical difference: it prunes only what is **structurally
redundant or older than a configured window**, and leaves a small
`[trimwire: …]` marker where it elided content — a **retrieval cue** the agent can act on by
re-reading the source. You get a cleaner, more focused request, and the markers can help the
agent re-read an elided source when that source is still available as a file or re-runnable tool.

## Recommended defaults

- **Pruning profile: `default` (ON).** Eight cache-safe strategies, aggressive knobs — the
  shipped default. Recommended for almost everyone.
- **`gentle` is a lighter-touch, lower-savings profile** — it prunes less; use it only if you
  specifically want minimal pruning. It is *not* a "recall mode" — see the recall note below.
- **Summarizer: off by default (`engine = "model-free"`).** Pruning is purely deterministic
  out of the box. The optional summarizer compresses old content with a model; when enabled,
  the **recommended backend is local `qwen3.5:4b`** (via ollama — no API key, nothing
  leaves your machine). It is never load-bearing: any failure falls back to deterministic
  pruning. See [SUMMARIZER.md](SUMMARIZER.md).
- **Need an old detail back?** Keep `default` on and ask the agent to re-read the file or
  re-run the tool — see the recall question in [FAQ.md](FAQ.md).

## What the numbers look like

Request-size reduction depends entirely on session shape; trimwire does nothing when there's
no redundancy. Offline replay through the real strategy code (reproducible via
`cargo run --release --example bench`) spans the full range:

- **~0%** on plain chat with no tools (no-op).
- **~60–95%** on tool-, read-, and browser-heavy sessions (the common agentic case).
- Savings **grow with session length** — the longer and more repetitive the session, the more
  there is to prune.

Cost is a *side effect* and **non-monotonic** (prompt-cache hits bill at ~0.1×, so pruning old
content can bust the cache): short sessions are a wash-to-slight-loss, long ones win
(about **−55%** cache-weighted cost at 256 turns in our benchmark cost model). The optional
summarizer helps most on long sessions (up to roughly **−65%** cache-weighted cost observed on one
long 981-turn session — a best case, not a guarantee). Overhead is roughly **sub-2 ms** per request
(host-dependent), off the network path. Full tables, profiles,
and the cost model live in [`benchmark/`](https://github.com/AZagatti/trimwire/blob/main/benchmark/results/RESULTS.md);
for figures from *your* traffic, run `trimwire stats`.

## Limits & honest caveats

- **Cost can go up on short sessions** (cache busting). trimwire reports *headroom*, not a
  guaranteed dollar figure.
- **The "focus ratio" is a byte-share proxy.** That a cleaner context improves the model's
  output is plausible but not proven; trimwire makes no quality-lift claim.
- **Very large / multi-hundred-million-token figures are projections only** — no model accepts
  that much context in one request, so they can't be measured directly.
- **Model-compatibility ceilings above ~128 KB are directional (small-N)** — verify with a
  probe before relying on them. See [MODEL-COMPATIBILITY.md](MODEL-COMPATIBILITY.md).

## Where to go next

- New here? [FAQ.md](FAQ.md) (trust, safety, what changes) and the
  [README](https://github.com/AZagatti/trimwire/blob/main/README.md) (install + quickstart).
- Tuning knobs/profiles: [`CONFIGURATION.md`](CONFIGURATION.md).
- Optional summarizer: [SUMMARIZER.md](SUMMARIZER.md).
- An LLM/agent reading this? See [For agents (canonical summary)](FOR-AGENTS.md) for a machine-readable summary.
