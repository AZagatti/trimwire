# trimwire vs. Anthropic-native context management

**Short version: they solve different parts of the same problem, and they
compose.** Anthropic's native features decide *what the model remembers and when
the server trims*; trimwire decides *what bytes leave your machine, deterministically,
with the cost made visible* — before the request is ever sent. You can run both at
once, and the defaults are designed not to fight each other.

This page is factual and deliberately conservative: native behaviour evolves and is
partly version-/beta-gated, so where exact behaviour depends on your client or API
version it's described as "when enabled" rather than pinned to a number. Check the
current Anthropic / Claude Code docs for specifics.

## The native features (what they are)

- **Prompt caching** — Anthropic caches a byte-exact request *prefix* (`tools` →
  `system` → `messages`) so repeated prefixes are billed at a large discount. Any
  byte change in the prefix invalidates everything after it.
- **Context editing / `clear_tool_uses`** (the `context-management-*` beta) —
  *server-side*: as a conversation approaches the context limit, Anthropic can clear
  older tool-use/tool-result content automatically, replacing it with a marker.
- **Claude Code `/compact` (and auto-compact)** — *client-side*: Claude Code
  summarizes the conversation so far into a shorter form and continues from the
  summary. User-triggered, or automatic near a threshold.
- **The memory tool** — a model-driven, file-backed scratch memory the model reads
  and writes across turns. A different concern (what the model *keeps*), not wire
  pruning.

## What trimwire is

A single static binary that sits on `ANTHROPIC_BASE_URL` as a transparent HTTP
gateway and prunes `messages[]` **in flight**, per request, with deterministic
model-free strategies as the floor. It records what it changed (`trimwire stats`,
cache-hit accounting) and never makes a model call on the default path. An opt-in
summarizer can do heavier reduction but is never load-bearing — every strategy
fails open to the original body.

## Side by side

| Dimension | Anthropic-native | trimwire |
|---|---|---|
| **Where it runs** | Server-side (context editing) / inside Claude Code (`/compact`) | A local proxy on your machine, before the request leaves |
| **Determinism** | Heuristic / model-driven; output can vary | Deterministic on the default path, byte-for-byte; same input → same output (default path) |
| **Transparency** | Trims happen upstream; limited local visibility | You see exactly what changed + the spend impact (`trimwire stats`) |
| **Trigger** | On `/compact`, or context-editing on a configured threshold (can run before the hard limit) | Every request, proactively — bytes are trimmed before they accumulate |
| **Loss profile** | `/compact` is lossy summarization; context-editing drops old tool output | Default path keeps recent turns verbatim; trims stale/duplicated/oversized tool output with markers; summarizer is opt-in |
| **Control** | A beta flag / a slash command; little per-strategy tuning | Per-strategy config, protected file globs, thresholds, off-by-default levers |
| **Cache awareness** | Caching is the native mechanism | Pruning is designed to preserve the cache prefix; its stateful re-pruning is cache-stable on purpose |
| **Portability** | Anthropic API (and Claude Code) | Any Anthropic-compatible endpoint; no beta header required |
| **Dependencies** | Built in | One binary; no CA cert, no model required on the default path |

## How they compose

- **Caching:** trimwire's whole design is built around *not* breaking the cache —
  it preserves the byte-exact prefix and its stateful re-pruning exists precisely
  to keep the cache stable across turns. It complements caching rather than competing.
- **Context editing / `/compact`:** these fire at compaction or a configured
  threshold. trimwire works *early and every turn*, so there's less bloat for the native path to clear in
  the first place — and when the native path does fire, trimwire forwards it
  untouched (it already accounts for the "tool result cleared" markers on the wire).
- **Memory tool:** orthogonal. trimwire prunes wire bytes; the memory tool manages
  what the model deliberately retains. Running both is fine.

## When native alone is enough

- You don't care about *seeing* per-request cost/byte impact and the built-in
  trims keep you under the limit comfortably.
- You're happy with lossy summarization at the threshold and don't need recent
  turns kept verbatim.
- You don't want to run any local process.

## Where trimwire adds something native doesn't

- **Spend transparency** — concrete, per-session numbers for what was trimmed and
  how it moved cache-hit rate. The native path doesn't surface this locally.
- **Determinism** — reproducible pruning you can reason about and test, not a
  heuristic that varies run to run.
- **Proactive, every-turn control** — trim *before* bloat accumulates, rather than
  only when the limit is hit.
- **Tool-output control** — cap/stub stale or oversized tool results, protect
  specific files from pruning, all without a model call.
- **Portability** — the same behaviour against any Anthropic-compatible endpoint,
  no beta header.

## One-line pitch

> Anthropic-native context management decides what the model keeps and when the
> server trims. **trimwire is the transparent, deterministic, spend-visible layer
> that controls what leaves your machine — and it's built to keep the cache intact
> while it does.** Use both.
