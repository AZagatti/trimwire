# Multi-harness support — engineering plan (INTERNAL)

> **Internal dev doc — not published to the site** (not in `site/scripts/sync-docs.mjs`).
> The user-facing summary lives in [ROADMAP.md](ROADMAP.md) → "Beyond Claude Code".
> This is the implementation detail behind it. Discovery: 2026-06-11 (subagent fan-out).

The opportunity after release: prune for harnesses beyond Claude Code — **aider,
opencode, cline, pi, Codex** — since the pruning value is universal but no
transparent, deterministic proxy exists for them. Not committed; this is the plan
of record once release is done.

## Discovery findings

### The seam (coupling map)

Every strategy runs through one parse choke point — `strategies::apply_to_body`
and `reprune::stable_apply_to_body`: raw body → parse → prune `messages[]`
(Anthropic content-block shape) → re-serialize. A `FormatAdapter` trait slots in
there. **Reuse the existing Anthropic block shape as the internal IR** — the nine
strategies and the load-bearing `pairing.rs` are already written and tested
against it, so the Anthropic path is a zero-overhead *identity* adapter and **the
strategies do not change.**

Two kinds of coupling to bridge per harness:

1. **Wire format** — handled by the adapter (`normalize`/`denormalize`).
2. **Tool conventions** (not wire format) — become per-harness config:
   `stale_reads` reads `tool_use.input["file_path"]`; `AUTHORING_TOOLS` are Claude
   Code tool names. `thinking_strip`, `system_shape_normalize`, and `cache_control`
   handling are Anthropic-only → no-ops elsewhere via adapter feature flags.

The gateway is only lightly coupled — three Anthropic specifics become adapter
methods: the `/v1/messages` intercept path, the `x-claude-code-session-id` session
key, and the JSON error envelope.

### Harness feasibility

| Harness | Intercept | Wire format | Verdict |
|---|---|---|---|
| **opencode / cline / pi** | per-provider `baseURL` config | their **Anthropic** provider sends `/v1/messages` | **works TODAY, unchanged** — point their Anthropic provider at trimwire |
| **aider** | global `OPENAI_API_BASE` env | OpenAI Chat Completions | **best first adapter** — monolithic format, implicit caching (no `cache_control`) |
| **Codex CLI** | `OPENAI_BASE_URL` / config | OpenAI **Responses** API (Items-based `input[]`) | **HARD / deferred** — different envelope, its own adapter |

The one structural difference an OpenAI Chat adapter must bridge: tool calls live
in `assistant.tool_calls[]` and results in separate `role:"tool"` messages keyed
by `tool_call_id` — vs Anthropic's `tool_use`/`tool_result` content-block twins.
The normalizer reconstructs the twin-block shape (collapsing parallel `role:"tool"`
results into one user message) so `pairing.rs` validates unchanged.

## Architecture

### The `FormatAdapter` trait

```rust
// src/adapter/mod.rs
pub trait FormatAdapter: Send + Sync + 'static {
    fn normalize(&self, raw: &[u8]) -> Option<NormalizedRequest>;     // None on parse fail → caller forwards verbatim
    fn denormalize(&self, req: NormalizedRequest) -> Vec<u8>;
    fn intercept_paths(&self) -> &'static [&'static str];             // ["/v1/messages"] | ["/v1/chat/completions"]
    fn session_key<'a>(&self, headers: &'a http::HeaderMap, body: &[u8]) -> Option<&'a str>;
    fn error_body(&self, status: u16, message: &str) -> Vec<u8>;
    fn supports_thinking(&self) -> bool { false }                     // gates thinking_strip / system_shape_normalize
    fn has_cache_control(&self) -> bool { false }
}

pub struct NormalizedRequest {
    pub envelope: serde_json::Value,                       // full body minus messages[] — strategies never touch it
    pub messages: Vec<serde_json::Value>,                  // Anthropic content-block shape (the IR)
    pub passthrough_fields: serde_json::Map<String, serde_json::Value>, // round-tripped verbatim
}
```

**Internal IR = the Anthropic block shape** (reason above). The Anthropic adapter
is `normalize = parse as-is`, `denormalize = serialize as-is`.

### Where it plugs in

`apply_to_body(body, cfg)` → `apply_to_body(body, cfg, adapter: &dyn FormatAdapter)`
(same for `stable_apply_to_body`). Step 1 (parse) becomes `adapter.normalize`,
steps 2–3 (prune `messages`) are unchanged, step 4 (serialize) becomes
`adapter.denormalize`. All existing callers pass `&AnthropicAdapter` → zero
behaviour change, all tests stay green.

Gateway: store `Arc<dyn FormatAdapter>` on shared state; replace `MESSAGES_PATH`,
the `x-claude-code-session-id` lookup, and `anthropic_error()` with adapter calls.

### Cache safety (critical)

`denormalize(normalize(x))` must be byte-identical to `x` for any body trimwire
would not otherwise mutate, or a no-op prune would change bytes and bust the cache.
This is **guaranteed for free**: `denormalize` runs only on *mutated* bodies; the
`BodyOutcome::Unchanged` path forwards the original bytes verbatim and never calls
`denormalize`. A golden round-trip test per adapter is still added as a guard.

### Per-harness tool conventions

```rust
pub struct ToolConventions {
    pub authoring_tools: Vec<String>,   // never elide their input (file bodies)
    pub path_tools: Vec<String>,        // carry structured file-path inputs
    pub path_field: Option<String>,     // e.g. "file_path" for Claude Code
}
```

For aider these are empty/user-provided → `stale_reads`/`stale_input_cap` simply
fire on no tools (safe no-op) until configured.

### OpenAI Chat ↔ Anthropic IR mapping

- `assistant` + `tool_calls[]` → assistant message with one `{type:"tool_use",
  id, name, input}` block per call (parallel calls → multiple blocks in one array).
- `role:"tool"` messages → `role:"user"` with `{type:"tool_result", tool_use_id,
  content}`; **collapse consecutive `role:"tool"` messages into one user message**
  with multiple `tool_result` blocks (the trickiest part).
- `denormalize` inverts both.

## Phasing

| Phase | Scope | Effort | Risk |
|---|---|---|---|
| **0 — free win** | Document that opencode/cline/pi pointing their **Anthropic** provider at trimwire already get full pruning (a `docs/HARNESSES.md`); verify manually. A launch talking point. | S | none |
| **1 — `FormatAdapter` refactor** | trait + `AnthropicAdapter` identity + `harness` config field; thread `&dyn FormatAdapter` through `apply_to_body`/`stable_apply_to_body`/gateway. **No behaviour change — all tests green;** add normalize→denormalize byte-identity test on real fixtures. | M | low |
| **2 — aider / OpenAI Chat adapter** | `src/adapter/openai_chat.rs` (tool-call/result mapping incl. parallel collapse), `trimwire install aider` (writes `OPENAI_API_BASE`), golden round-trip + parallel-call unit tests. | M–L | medium |
| **3 — Responses API / Codex** | `openai_responses.rs` for the Items-based `input[]` format. Separate from Chat Completions — its own adapter, no shared normalizer. Do only after aider proves the architecture. | L | higher |

### Phase-2 spike (the minimal first proof)

Build in order: (1) `normalize` text-only case, confirm `bloat_cap` fires on a
long aider conversation; (2) single tool call + unit test; (3) parallel tool calls
+ `PairingIndex::build` validates clean; (4) wire `trimwire install aider`, run a
real session. **Success** = round-trip byte-faithful on captured aider bodies,
`bloat_cap`/`cross_turn_dedup` reduce a long session >10%, and aider completes a
real multi-file task uncorrupted. **What could kill it**: non-canonical JSON field
ordering breaking the round-trip — mitigated because only the `Unchanged` (no-op)
path requires raw-byte identity, and it never calls `denormalize`.

## CLI / UX

`trimwire install <harness>` (`claude-code` default, zero-regression) selects the
adapter and writes the right env export + upstream; one adapter per running
instance (a second harness runs its own instance/port). Config gains a top-level
`harness` field.

## Risks & non-goals

- **Pairing reconstruction** (collapsing parallel OpenAI tool results) is the
  highest-risk piece — but pairing validation fails *open*, so the worst case is a
  missed optimization, never a corrupted session.
- **The Responses API is a separate beast** — no shared normalizer with Chat.
- **Multimodel sessions** (aider switching models mid-run) fall back to stateless
  reprune until a stable pseudo-session-id is designed.
- **Third-party-provider ToS** is the user's responsibility — document per harness.

## Critical files for implementation

- `src/strategies/mod.rs` — `apply_to_body`, the primary adapter insertion point.
- `src/reprune.rs` — `stable_apply_to_body`; session-key + `PruneState` cache key
  must be adapter-aware.
- `src/proxy/gateway.rs` — the three Anthropic seams; `Arc<dyn FormatAdapter>`.
- `src/config.rs` — `harness` field + `ToolConventions`.
- `src/cli/install.rs` — `trimwire install <harness>`.
