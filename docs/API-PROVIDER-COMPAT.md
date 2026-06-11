# API-provider compatibility — FUTURE implementation note (not built yet)

Status: **research / design note for a FUTURE step.** Nothing here is implemented. The
current focus is the local-LLM compaction feature; this captures the verified groundwork
so the API-provider step starts solid. Grounded in a sourced research pass (2026-06-04)
that corroborated the "multiple system messages" failure mode across backends.

## TL;DR

- **trimwire is SAFE today** on the issues below — no change needed now.
- The one hard cross-provider rule is: **exactly ONE system message at index 0**, strict
  user/assistant alternation, no stray mid-history system message, no trailing-assistant
  unless the backend supports prefill. A single stateless normalization function covers
  ~95% of providers with no per-provider branching.

## Two scopes (very different cost)

1. **(LOW) Summarizer talks to a non-ollama OpenAI-compatible backend** (vLLM / LM Studio /
   OpenAI / Together / Groq for the *summary* call). This is the currently-dead
   `LocalModelConfig.api_style = "openai"` path. Add a `/v1/chat/completions` request
   builder + a `.choices[0].message.content` parser; keep the existing single-`[system,
   user]` payload (already safe). ~1 day + wiremock tests. Lets users point the summarizer
   at any OpenAI-compatible server. **Do this first.**
2. **(MED-HIGH) trimwire prunes NON-Anthropic (OpenAI-format) upstream traffic.** The whole
   pruning/replay/pairing/cache-stability engine assumes the Anthropic shape (top-level
   `system`, content blocks, `tool_use`/`tool_result` pairing, `thinking` blocks). OpenAI
   format puts `system` *in* `messages[]`, uses a different tool-call shape, and has no
   thinking blocks. Needs a normalization layer (below) + reworked strategies/pairing.

## The "multiple system messages" failure mode (verified)

Local OpenAI-compatible endpoints misbehave with >1 system message — usually **silently**
(no 400), which is worse (no error signal):

| Backend | Behavior with multiple system messages |
|---|---|
| OpenAI `/v1/chat/completions` | Tolerated (processes all) |
| Anthropic Messages API | N/A — `system` is a TOP-LEVEL field, not a role; no `system` role in `messages[]` |
| Ollama `/api/chat` (native) & `/v1` | Passed through verbatim; behavior = the model's chat template. Modelfile + request system → **duplication** (ollama #2630), not a 400 |
| vLLM `/v1` | Template-dependent; most Jinja templates assume 0/1 system at index 0; extras silently dropped/mishandled |
| FastChat | **Silently OVERWRITES** — `set_system_message()` per message, last wins (AutoGen #595) |
| LM Studio / llama.cpp `/v1` | Template-dependent; llama.cpp `--jinja` can inject its own extra system message |
| Groq | Single-system expectation; 400s on malformed content; first system likely honored |
| OpenRouter | Per-underlying-provider (e.g. Gemini: only last system wins — pipecat #3362) |

**The sharpest failure (opencode-dcp #367):** a stray mid-history system message gets
mis-routed as an **assistant-prefill** → `"Prefilling assistant messages is not supported
for this model"`. This is exactly the trap a context-pruning proxy can fall into if it
*injects* its own system message into `messages[]`.

Brief corrections from the research: FastChat overwrites *silently* (not 400); Ollama's
compat layer *duplicates* (#2630), it does not 400 on multi-system; the cited Ollama #7132
is unrelated (a connection error). The opencode-dcp #367 assistant-prefill issue is real
and accurately characterized.

## The normalization recipe (the must-haves)

Apply this stateless transform just before forwarding to ANY non-Anthropic backend:

```
normalize_for_openai_compat(messages):
  1. Collect ALL system-role content; concatenate (with "\n\n") into ONE string.
  2. Rebuild: prepend a single {role:"system", content:<combined>} at index 0 (if any).
  3. Merge consecutive same-role messages (covers FastChat / Groq / templated backends).
  4. If the last message is assistant AND the backend doesn't support prefill: drop it
     or convert to a user turn.
  5. Never emit a system-role object anywhere except index 0.
```

This covers all confirmed failure modes without per-provider branching. Per-provider
quirks (e.g. Groq requiring `content` to be a string) layer on top only as needed.

### Anthropic ↔ OpenAI translation (for scope 2)

- **Anthropic→OpenAI:** move the top-level `system` (string OR array-of-blocks with
  `cache_control`) into `messages[0]` as a system message (flatten blocks, strip
  `cache_control`); unpack `tool_result` blocks (in user messages) into `{role:"tool",…}`
  messages; strip/stringify `thinking` blocks.
- **OpenAI→Anthropic:** extract/concatenate all system messages into the top-level
  `system` field; merge consecutive same-role turns; wrap tool results back into user
  `tool_result` blocks; ensure the first message is `user`.
- Anthropic enforces strict user/assistant alternation, auto-merges consecutive same-role
  turns, requires `tool_use`→`tool_result` pairing (400 otherwise), and forbids a
  leading-assistant message (except prefill).

## Why trimwire is SAFE today (no action needed now)

- **Local summarizer call** (`src/summarizer/api.rs::call_model`) sends exactly
  `[{role:system}, {role:user}]` to ollama `/api/chat` — the universally-safe shape; works
  on every backend studied. Nothing to fix.
- **Anthropic passthrough**: trimwire prunes Anthropic-format `/v1/messages`, never touches
  the top-level `system` field, and replays summaries as `[assistant, user]` content-block
  pairs (never a system-role object in `messages[]`). The multiple-system issue is an
  OpenAI-*format* concern and **cannot manifest** in the Anthropic passthrough.

## When this is built

Bake the single-system-at-index-0 + strict-alternation invariant into the OpenAI-format
request builder from day one. Start with scope 1 (summarizer backend), defer scope 2.

## Sources

AutoGen #595 (FastChat overwrite), Ollama #2630 (compat duplication) / #682 (Modelfile),
opencode-dcp #367 (assistant-prefill misinterpretation), Anthropic Messages API docs,
vLLM OpenAI-compat docs, pipecat #3362 (OpenRouter/Gemini), smolagents #429 (Groq),
llama.cpp #20861 (qwen3.5 prefill + enable_thinking). Full list in the 2026-06-04 research
pass.
