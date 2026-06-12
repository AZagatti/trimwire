# Roadmap

trimwire is feature-complete for v1. This is not a release schedule. The items
below are evidence-gated ideas; each ships only if real telemetry or a benchmark
shows it earns its complexity.

## Shipped

- Eight deterministic cache-safe pruning strategies (`default` and `gentle` profiles)
- Stable-prefix re-pruning (`[reprune]`) for cache stability
- Opt-in summarizer: local ollama engine + cloud API engine (multi-provider, fallback cascade)
- Opt-in anonymous telemetry (`trimwire share stats` / `share benchmark`)
- Community collector + dashboard (collector pending deploy)
- Offline benchmark harness with quality corpus (5 slices, harm gate, FCS metric)
- `trimwire sweep` for on-disk transcript maintenance
- `trimwire preview` / `trimwire recall` / `trimwire dashboard`
- Socket-activated always-up service (systemd / launchd)

## Possible (evidence-gated, not committed)

- **Richer deterministic elision markers.** Today a stub records the size elided.
  A deterministic extractive marker (keeping first/last lines, or `error|warn|fail`
  lines) would provide a content breadcrumb with zero network or model dependency.
- **Telemetry-driven threshold tuning.** Current thresholds are conservative
  defaults. Real `trimwire share stats` data could justify per-profile tuning,
  but only once there is real-traffic evidence of the cost-vs-headroom trade-off.
- **Benchmark additions.** A post-compaction corpus (a `[summary]` turn + recent
  tail) and a profile-knob sensitivity sweep (savings vs. cache-stability across
  `keep_recent_turns` / `bloat threshold`) to show the frontier rather than three
  fixed profile points.

## Beyond Claude Code

The largest opportunity: prune for other agent harnesses too —
**aider, opencode, cline, Codex**, and more. The pruning value is universal, but
no transparent, deterministic proxy exists for them yet.

Two things make this tractable:

- **Some already work today.** Harnesses that let you point their *Claude/Anthropic*
  provider at a custom URL (opencode, cline) get full pruning right now — they
  speak the same format trimwire already understands.
- **The rest need a small adapter layer.** trimwire's pruning runs at one internal
  seam, so adding a harness means translating its request format in and out around
  that seam — the pruning logic itself stays the same. The first target would be
  OpenAI-Chat-Completions harnesses (e.g. aider, via a single `OPENAI_API_BASE`
  env var); the goal is a `trimwire install <harness>` flow.

This is discovery-complete but not committed. The full
engineering plan (adapter design, phasing, the aider spike) is tracked internally
in [`docs/MULTI-HARNESS-PLAN.md`](https://github.com/AZagatti/trimwire/blob/main/docs/MULTI-HARNESS-PLAN.md).

## Non-goals

- **Gateway-side LLM summarization on your Anthropic OAuth token.** Prohibited by
  ToS (Anthropic actively enforced this in 2026; the subscription token is licensed
  only for ordinary Claude Code use). Also: it adds latency on the request path,
  spends tokens you didn't ask to spend, and breaks bit-exact reproducibility.
  The opt-in summarizer (`[summarizer]`) threads all four objections: it uses a
  model you choose on your own key, runs in the background, and caches the result.
  See [SUMMARIZER.md](SUMMARIZER.md).
