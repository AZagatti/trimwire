# Benchmark a summarizer model

`trimwire summarizer benchmark` scores a local or API summarizer against a small,
bundled, reasoning-dense corpus, so you can sanity-check a model before trusting
it to compact your sessions.

It is a **directional sanity-check, not an authoritative quality ranking.** The
curated approved-model list (a blind human read) stays the authority; see the
validated-tiers table in [`SUMMARIZER.md`](SUMMARIZER.md). Use this benchmark to
spot a clearly-bad model, not to split hairs between two good ones.

## Local models (ollama)

**Requires a running [ollama](https://ollama.com).** Pass a local ollama tag with
`--model`, or omit it to score your configured model.

```sh
trimwire summarizer benchmark                       # scores your configured local model (qwen3.5:4b default)
trimwire summarizer benchmark --model qwen3.5:4b    # a specific tag (repeatable)
trimwire summarizer benchmark --all-installed       # every installed model (disqualified ones skipped)
trimwire summarizer benchmark --out ./summaries     # also save each summary to skim yourself
trimwire summarizer benchmark --json                # machine-readable
```

**Runtime:** a few minutes per model, model-RAM-bound. See the model table in
[`SUMMARIZER.md`](SUMMARIZER.md) for per-model RAM figures.

## API providers

Configure the provider first — `trimwire summarizer setup`, or add a
`[[summarizer.providers]]` block to `~/.config/trimwire.toml` (see
[Summarizer](SUMMARIZER.md)). Then pass that provider's `id` to `--model`. Without
`--yes`, this is a
**dry run** (prints the cost/scope warning and exits; no API calls, no charges).
With `--yes`, each corpus slice is a **real, paid call** on your provider's key
(not your Anthropic subscription token).

```sh
trimwire summarizer benchmark --model anthropic          # dry run: prints warning, no calls
trimwire summarizer benchmark --model anthropic --yes    # real calls on your API key
trimwire share benchmark --model anthropic --yes         # run AND upload the score
```

**API scores are directional only.** The corpus is tuned for local summarizers
(dense reasoning excerpts, tight length budget, free-form FACTS-FIRST prompt).
Cloud models with larger context windows and different temperature defaults may
score differently for structural reasons unrelated to summarisation quality.
Treat API scores as a sanity-check within the same model family, not as a
cross-backend ranking.

## How to read it

Each model gets four components and one composite:

- **retention** — how many of the corpus's load-bearing facts survived the summary.
- **compression** — how much smaller the summary is than the input (`1 − out/in`).
  (This is the *summary's* shrink — distinct from the *reduction* trimwire does on
  your request bytes, shown in `trimwire stats`.)
- **false-done** — completion claims the source never supported ("tests passed"
  when none ran; "committed" when nothing was). The most dangerous failure. The
  detector is deliberately **high-precision**: it flags a claim only when the source
  slice contains *no* matching evidence, and it does **not** flag honest hedged
  phrasing — conditional or future statements like "ship if tests green", "awaiting
  results to confirm green", or "tests should pass once the build finishes" are
  not-yet-done notes, not completions, so they pass. (Careful models that phrase this
  way are no longer penalised — that earlier false-positive over-gated good API
  models; it's fixed.)
- **usable** — did it produce a non-empty, non-verbatim summary at all.
- **FCS** (faithful-compression score) = `(retention/100) × (compression/100) × 100`
  — both inputs are percentages (0–100), so FCS is also 0–100 (not a raw product,
  which would reach 10000). A verbatim copy *or* a fact-dropper both score low.

**The safety gate dominates:** any false-done, or any slice with no usable
summary, drops the model to the bottom (shown as `FAIL`/`gated`) regardless of
FCS — because a confident false completion is worse than a merely terse one. The
scores can't judge prose, so pass `--out` and skim a few summaries yourself.

## Share your results (optional)

```sh
trimwire share benchmark --model qwen3.5:4b        # local: prints the exact row; sends nothing
trimwire share benchmark --model qwen3.5:4b --yes  # local: uploads it to the community leaderboard
trimwire share benchmark --model anthropic --yes   # API provider (configured): uploads an api row
```

`share benchmark` contributes an **anonymous, content-free** row to the community
[model-benchmark page](https://trimwire.dev/benchmark/): coarse model family + bucket
(never the raw tag), bucketed retention/compression, a capped false-done count, and
whether it produced usable summaries — nothing else. It is **off by default**:
without `--yes` it only prints what it *would* send (a dry run).

**Both local and API/provider models are supported** (including OpenRouter-style
testing across many models), but they are kept **distinct**: every row carries a
`backend` (`local`/`api`), and the leaderboard ranks + filters them separately —
API scores are a *directional cross-check*, never compared head-to-head with local
ones. API rows derive their family/bucket from the **real model** (`claude-haiku-4-5`,
`gpt-4.1-mini`), never the provider id; the provider shows only as a coarse
`provider_route` (anthropic/openai/openrouter/azure/other). Partial runs (e.g.
`--max-calls`) are labeled **partial** and ranked apart from full-corpus runs. If a
provider/model **call** fails, that row is **not** uploaded — trimwire prints a
report-an-issue hint instead (so a broken key/network never looks like a weak
model). See [Telemetry](TELEMETRY.md) for the exact payload.
