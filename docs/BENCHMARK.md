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
  when none ran). The most dangerous failure.
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
trimwire share benchmark --model qwen3.5:4b        # prints the exact row; sends nothing
trimwire share benchmark --model qwen3.5:4b --yes  # uploads it (if an endpoint is configured)
```

`share benchmark` contributes an **anonymous, content-free** row to the community
[model-benchmark page](/benchmark/): your model's family + coarse size tier (never
the raw tag), bucketed retention/reduction, a capped false-done count, and whether
it produced usable summaries — nothing else. It is **off by default**: with no
collector endpoint configured (or without `--yes`) it only prints what it *would*
send. See [Telemetry](/guides/telemetry/) for the exact payload.
