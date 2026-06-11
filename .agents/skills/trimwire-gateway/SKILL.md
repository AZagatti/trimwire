---
name: trimwire-gateway
description: Workflow guidance for implementing the trimwire gateway. Covers request-path buffering, response-path SSE streaming, pairing-index invariants, strategy module pattern, ledger conventions, and the cache-prefix-hash requirement. Invoke when working on src/proxy/gateway.rs, src/strategies/*, src/pairing.rs, src/proxy/upstream.rs, src/proxy/proxy_stream.rs, or src/ledger.rs.
---

# trimwire Gateway Skill

This skill is the curated reading list + decision recipes for working in
the trimwire gateway internals. Read the section that matches your task
before opening source.

## Before you start any change

1. **Read [`ARCHITECTURE.md`](../../../ARCHITECTURE.md)** for the module
   layout and data flow.
2. **If you're touching `messages[]` mutation logic**, also read
   [`SPIKE.md` §5](../../../SPIKE.md) (Pairing invariants) — it has the
   pseudocode and the three invariants that every strategy must respect.
3. **Confirm you're not scope-creeping.** See the tier table in
   [`SPIKE.md` §8](../../../SPIKE.md) — only T1 (gateway) is built in
   v0.1. T2/T3/T4 are POCs in `pocs/` or documented recipes only.

## Task recipes

### Adding a new strategy

1. Create `src/strategies/<name>.rs` with a config struct in `src/config.rs`
   (e.g. `<Name>Config`, defaulting to `enabled = false`).
2. Implement `pub fn apply(messages: &mut [Value], cfg: &<Name>Config) ->
   Result<Stats>`. Dispatch is an `if cfg.strategies.<name>.enabled { ... }`
   block in `strategies::run` (a closed if-chain — there is no `Strategy`
   enum). Reuse the shared `role` / `block_mut` / `serialized_len` helpers
   from `strategies/mod.rs`.
3. **Build and validate the pairing index inside `apply`**: first line
   `let idx = PairingIndex::build(messages); idx.validate()?;` (pre-check),
   and after mutating, `PairingIndex::build(messages).validate()?;`
   (post-check). Returning `Err` makes `apply_to_body` forward the original
   body unchanged (rollback). Each strategy builds its own index — it is not
   passed one.
4. **Add a Python-parity test**: extend `tests/phase0/dump_expected.py` to
   dump the reference output for the new strategy, then add a Rust test in
   `tests/integration.rs` diffing your output against the committed
   `tests/fixtures/expected/<name>__*.json` (byte-identical, canonical
   sorted-key compact JSON). An `insta` snapshot is a fine extra regression
   anchor.
5. **Update [`ARCHITECTURE.md`](../../../ARCHITECTURE.md)** with the
   strategy and its invariants.

### Working on `proxy/gateway.rs` / the request path

- Request body is buffered fully (read once, mutate, forward). Don't try
  to stream the request body — `messages[]` mutation requires the whole
  array in memory.
- After mutation, **recompute `Content-Length`**. In hyper 1.x, use
  `http_body_util::Full<Bytes>` — `Full::new(bytes)` knows its length
  and the server auto-sets `Content-Length`.
- **Forward end-to-end headers; strip hop-by-hop; rewrite `Host`.**
  Per RFC 7230 §6.1 you must NOT forward: `connection`, `keep-alive`,
  `transfer-encoding`, `te`, `trailer`, `proxy-authorization`,
  `proxy-authenticate`, `upgrade`, `proxy-connection`. The `host` header
  must be rewritten to the upstream authority (e.g. `api.anthropic.com`),
  not passed through as `127.0.0.1:8765` — passing it through causes
  TLS SNI mismatches and HTTP 421 (Misdirected Request) responses.
- End-to-end headers to preserve verbatim include: `Authorization`,
  `anthropic-beta`, `anthropic-version`, `User-Agent`, `Accept`,
  `Accept-Encoding`, `Content-Type`, all `x-stainless-*`, `x-app`,
  `x-claude-code-session-id`.
- Log one structured line per request to stderr: in/out bytes, strategies
  fired, cache-prefix hashes.

### Working on `proxy/proxy_stream.rs` / the response path

- **Never buffer the response body.** Anthropic uses SSE
  (`text/event-stream`). In hyper 1.x, type-erase the upstream `Body` via
  `http_body_util::BodyExt::boxed()` (the Rust equivalent of Python
  httpx's `aiter_raw()`) and write it straight to the downstream response.
- Do not re-encode, do not parse, do not modify. Bytes in = bytes out.
- Preserve the response status code from upstream verbatim. Don't retry,
  don't rewrite — Claude Code's SDK handles retries.

### Working on `pairing.rs`

This is THE load-bearing correctness module. It has unit tests for every
public function plus `insta` snapshot tests over the fixture corpus
(`tests/integration.rs`). A property-style test asserting invariants over
randomly-generated message arrays is *planned* (not yet written) — add it
in `tests/integration.rs` when a strategy starts stressing the index.

The three invariants ([`SPIKE.md` §5](../../../SPIKE.md)):

1. **Pre-mutation:** every `tool_result.tool_use_id` has a matching
   `tool_use.id` in an earlier message.
2. **Atomic pair drops:** strategies always drop both halves together.
3. **Post-mutation:** re-run the orphan check.

Parallel tool calls (multiple `tool_use` blocks in one assistant message)
are treated independently — each pair stands on its own.

### Working on `ledger.rs`

- Single `Arc<Mutex<Connection>>`. SQLite serialises naturally.
- One row per request. Schema in [`ARCHITECTURE.md`](../../../ARCHITECTURE.md).
- **Always** record `prefix_hash_in` and `prefix_hash_out`. The CI test
  asserts hash stability when no strategy fired — see "Cache-prefix
  thrashing" in [`SPIKE.md` §9](../../../SPIKE.md). This catches the
  top silent-failure risk.

### Adding a CLI command

- Add the variant to `Cli` in `src/main.rs`.
- `main()` is the wiring layer — delegate to a function in the relevant
  module (e.g., `gateway::run()`, `ledger::print_stats()`).
- `main.rs` should stay under ~80 LOC. If it grows, split into
  `src/cli/*.rs`.

## Common pitfalls

- **Touching the `system` field of the request body.** Don't. It would
  trigger Anthropic's third-party-tool detection. See [`SPIKE.md` §1](../../../SPIKE.md)
  "Anthropic's stance".
- **Buffering the response.** Breaks streaming UX in Claude Code's TUI.
- **Orphaned `tool_use_id` after mutation.** Anthropic returns 400. Always
  use the pairing index for atomic drops.
- **Non-deterministic mutation.** If the same input doesn't produce the
  same output, the cache-prefix hash changes every request → prompt cache
  busts → gateway HURTS performance. CI test catches this; don't bypass.
- **Re-implementing something from `pocs/`.** Those are reference
  implementations from the spike validation. Read them for design
  inspiration, but don't shell out to Python from Rust.

## Useful skills + docs to reach for

- **`rust-engineer`** — for ownership / async / trait questions.
- **`context7`** — for fetching the current hyper / tokio / rustls /
  serde_json docs. Always prefer this over training data for library
  APIs.

## Quick-reference command list

```bash
cargo build                                      # debug build
cargo build --release                            # release build
cargo test                                       # unit + integration tests
cargo test --lib                                 # unit tests only (fast)
cargo clippy --all-targets -- -D warnings        # strict lint
cargo fmt --check                                # format check
cargo fmt --all                                  # apply formatting
cargo run -- daemon                              # run the gateway in foreground
cargo run -- stats                               # show the ledger
cargo insta review                               # batch-accept snapshot diffs
```
