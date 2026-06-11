# Architecture

Module-level design overview for trimwire. This document is the **source of
truth for structural decisions** — code follows it, and any refactor that
changes module boundaries must update this file in the same commit.

For the design rationale, validation record, and decision history, see
[`SPIKE.md`](SPIKE.md). This file is the *summary* a reader should grok in
5 minutes before opening any source file.

## Data flow (one HTTP request)

```
Claude Code  ── /v1/messages POST (full body) ──▶  gateway::handle()
                                                      │
                                                      ▼
                                              strategies::apply_to_body(bytes)
                                                │  serde_json parse → mutate messages[] → re-serialize
                                                ├─ strategies::run (each pre-validates its input; in order):
                                                │    ├─ FailedInputPurge
                                                │    ├─ StaleInputCap
                                                │    ├─ CrossTurnDedup
                                                │    ├─ StaleReads
                                                │    ├─ SimHashDedup   (opt-in, off by default)
                                                │    ├─ BloatCap
                                                │    ├─ SlidingWindow
                                                │    ├─ ImageStrip
                                                │    └─ ThinkingStrip
                                                └─ pairing::build + validate   ← single post-check for the whole pipeline
                                                            (on fail / no-op → forward original bytes)
                                                      │
                                                      ▼
                                              ledger::record(...)   (every request)
                                                      │
                                                      ▼
                                              client.request(...)   (upstream over TLS)
                                                      │
                                                      ▼
                                              proxy_stream::passthrough()
                                                      │
                                                      ▼
Claude Code  ◀── streamed SSE response (bytes-for-bytes from Anthropic)
```

## Modules

### `src/main.rs` — CLI entry (~50 LOC max)

Parses `clap` args, sets up tracing, dispatches to the subcommands
(`serve`, `run`, `stats`, `recall`, `install`/`uninstall`, `on`/`off`/`status`,
`statusline`, `hook`, `sweep`, `config`). **No business logic.** If you find
yourself adding mutation or HTTP code here, move it to the appropriate
module.

### `src/proxy/` — the HTTP transport layer

The inbound server, outbound client, and response pipe — grouped because
they form one transport tier (`gateway` → `upstream` → `proxy_stream`).
`proxy/mod.rs` only declares the three submodules.

#### `src/proxy/gateway.rs` — request/response lifecycle owner

Hyper 1.x server via `hyper_util::server::conn::auto::Builder` on a
`tokio::net::TcpListener`. Per-request:

1. Read request body fully (buffered — mutation requires it).
2. For `POST /v1/messages`, call `strategies::apply_to_body` (which
   encapsulates parse → `pairing` build/validate → `strategies::run` →
   re-serialize). The gateway holds no mutation logic and does not call
   `messages`/`pairing` directly. Any non-JSON / no-op / error forwards the
   original bytes verbatim.
3. Build the upstream request via `upstream` (`hyper_util::client::legacy::Client`).
4. Pipe the upstream response back via `proxy_stream`.
5. Log one line (ledger entry lands in Step 5).

**Must NOT contain mutation logic** — that lives in `strategies/`.

#### Header forwarding rules (RFC 7230 §6.1)

The gateway is a reverse proxy. It MUST:

- **Strip hop-by-hop headers** (these are connection-scoped, not for
  forwarding): `connection`, `keep-alive`, `transfer-encoding`, `te`,
  `trailer`, `proxy-authorization`, `proxy-authenticate`, `upgrade`,
  `proxy-connection`.
- **Rewrite `host`** to the upstream authority (`api.anthropic.com`).
  Forwarding `Host: 127.0.0.1:8765` causes TLS SNI mismatches and HTTP
  421 (Misdirected Request) responses.
- **Forward all other (end-to-end) headers unchanged**, especially:
  `Authorization`, `anthropic-beta`, `anthropic-version`, `User-Agent`,
  `Accept`, `Accept-Encoding`, `Content-Type`, `x-app`,
  `x-claude-code-session-id`, all `x-stainless-*`.
- **Recompute `Content-Length`** after any body mutation. With
  `http_body_util::Full<Bytes>` the server handles this automatically.

#### `src/proxy/upstream.rs` — HTTPS client to api.anthropic.com

Hyper-rustls client with webpki-roots, HTTP/2 enabled, connection pool
managed by hyper's default. Configurable upstream host via `[server]
upstream` in TOML (defaults to `https://api.anthropic.com`).

#### `src/proxy/proxy_stream.rs` — SSE response passthrough

Pipes bytes from the upstream `Body` stream to the downstream response
writer without buffering. Anthropic returns `text/event-stream` for
streamed responses; **any buffering or re-encoding here breaks the
streaming UX in Claude Code's TUI**.

#### `src/proxy/audit.rs` — opt-in metadata-only wire audit

Activated by `TRIMWIRE_AUDIT=<file>` (or `--audit`). Appends one JSONL line
per `/v1/messages` with request *shape* metadata: counts, sizes, flags, the
model, the `anthropic-beta` header, and the session id, plus the **cache-prefix
structure** (the ordered tool-definition names, which `tools`/`system` blocks
carry a `cache_control` breakpoint, the `system` shape, and the `messages[0]`
block-type sequence) — **by construction no message content, tool input, or
result text** (the `Capture` struct holds only counts/flags/hashes and structural
identifiers — names, type labels, sizes, positions; a
`capture_never_leaks_content` test enforces it).
`capture()` is a pure fn over `&[u8]`; `AuditSink` is an append-only JSONL
writer (fire-and-forget, errors swallowed). **Zero cost when unset** — the body
is never even parsed by this module unless a sink is configured. Exists to make
Claude Code's own native wire behaviour observable and to let a user audit their
own traffic.

> **Note:** message-array walk helpers (`role`, `block_mut`,
> `serialized_len`) live in `strategies/mod.rs` next to their only callers.
> A dedicated `messages.rs` types module will be (re)introduced if/when we
> strongly-type the `/v1/messages` block shapes; until then there is no such
> module.

### `src/cli/` — binary-private command implementations

Mostly one file per subcommand (`serve`, `run`, `install`, `config_edit`,
`stats`, `statusline`, `hook`, `sweep`) plus `service.rs` (the
systemd/launchd/supervisor lifecycle backing `on`/`off`/`status`/`uninstall`);
`cli/mod.rs` holds the shared starter-config writer + thin lifecycle wrappers.
Declared via `mod cli;` in `main.rs`, so it is part of the binary, not the
`trimwire` lib. `cli/sweep.rs` is a thin wrapper that calls the `trimwire::sweep`
library module (so the rewrite engine is unit-testable independent of the CLI).

### `src/pairing.rs` — the load-bearing correctness module

`PairingIndex { uses, results }` — maps each `tool_use_id` to its
`(message_idx, content_idx)` for both the use and result halves.

Used by every strategy. Enforces three invariants
([`SPIKE.md` §5](SPIKE.md)):

1. **Pre-mutation:** every `tool_result.tool_use_id` has a matching
   `tool_use.id` in an earlier message. If not, the input was already
   broken — forward unmutated.
2. **Atomic pair drops:** strategies always drop both halves together,
   never one side.
3. **Post-mutation:** re-run the orphan check. If any orphan appeared,
   strategy is buggy → roll back to original body.

**Must NOT mutate messages** (only read). Mutation lives in `strategies/`.

### `src/strategies/` — pure mutation functions

`if cfg.strategies.<name>.enabled` dispatch (not an enum, not trait
objects) over a closed set, chained in `strategies::run`. Each strategy:

- Exposes `apply(messages: &mut [Value], cfg: &<Name>Config) -> Result<Stats>`.
- Builds its own `pairing::PairingIndex` and validates pre/post (so a
  preceding strategy's bug is caught), finding safe drop sites.
- Returns `Stats { stubbed, original_bytes, final_bytes }`.
- Has unit tests. `sliding_window` and `image_strip` additionally have a
  byte-identical Python-parity test against `tests/fixtures/expected/` (the
  Phase-0 reference oracle covers those two; the others are Rust-tested only).

**Must NOT do any I/O.** Pure functions, fully testable in isolation.

**`run()` applies enabled strategies in this fixed order** (8 cache-safe on in
`default`, plus opt-in `simhash_dedup`):
- `failed_input_purge.rs` — clear `tool_use.input` of old errored calls (keep the error result).
- `stale_input_cap.rs` — cap the bulky input of an old *successful* tool call.
- `cross_turn_dedup.rs` — keep only the latest of identical repeated tool calls; stub earlier identical `tool_result`s.
- `stale_reads.rs` — elide a `Read` later superseded (re-read / Write / Edit on the same path); demand-page the last large read.
- `simhash_dedup.rs` — **opt-in (off in both profiles)**: stub *near*-duplicate `tool_result`s that exact-match dedup misses.
- `bloat_cap.rs` — trim a single oversized old `tool_result` to head+tail+marker; for array-content results, salvage the bulky text blocks in place (total-erase only pure non-text/image arrays).
- `sliding_window.rs` — stub old tool_use/tool_result pairs whose tool is on the denylist (browser tools by default).
- `image_strip.rs` — replace base64 image payloads with a marker (keep K most-recent).
- `thinking_strip.rs` — drop old `thinking` blocks (reprune replays the removals by signature).

**Also shipped:** `trimwire sweep` (on-disk JSONL cleanup) lives in
`src/sweep.rs` — a CLI command distinct from the gateway pipeline. The gateway
only mutates the *live* request; sweep rewrites the on-disk transcript so
resumed sessions start leaner. Two safe mutations (drop empty thinking blocks,
purge failed-call inputs); minimal-diff rewrite (only mutated lines
re-serialized, blank lines + trailing-newline state preserved), write-to-temp +
fsync, then abort-if-the-file-changed (any concurrent append/compaction leaves
the file untouched — sweep is for inactive sessions), backup + atomic rename +
dir fsync, retains 3 backups, temp-file drop-guard, post-write validation, and
`--dry-run`. Ported from `pocs/tier2-sweep.py`, hardened beyond it.

### `src/config.rs` — TOML loader

Two-tier merge via `figment`:

1. Global: `~/.config/trimwire.toml`
2. Per-project: `./.trimwire.toml` (overrides global; lists are replaced
   not appended)
3. Env-var overrides (`TRIMWIRE_*`) take final precedence.

### `src/error.rs` — error types

`thiserror`-derived enum for internal failures.

HTTP-code mapping (gateway behaviour, [`SPIKE.md` §6](SPIKE.md)):

| Internal failure | Gateway response |
|---|---|
| Mutation crash (caught) | Log + roll back; forward 200 with original body |
| Malformed JSON request | Forwarded verbatim to upstream (cache-prefix safe; never mutated) |
| Request body > 32 MB | 413 Payload Too Large |
| Upstream connection failure | 502 Bad Gateway |
| Upstream timeout (headers) | 504 Gateway Timeout |
| Gateway startup failure (port bind, TLS init) | Non-zero exit at startup |
| Anything else | Pass through upstream's status code unchanged |

### `src/ledger.rs` — SQLite savings store

Single `STRICT` table, created once with `CREATE TABLE IF NOT EXISTS` — no
versioning or migrations (trimwire is unreleased, so there's no older on-disk
schema to migrate from):

```sql
CREATE TABLE IF NOT EXISTS requests (
    id              INTEGER PRIMARY KEY,   -- rowid alias (no AUTOINCREMENT)
    ts              INTEGER NOT NULL,      -- request time, caller-supplied unix secs
    session_id      TEXT,                  -- x-claude-code-session-id, if present
    in_bytes        INTEGER NOT NULL,
    out_bytes       INTEGER NOT NULL,
    strategies      TEXT NOT NULL DEFAULT '',  -- comma-sep names that fired; '' = none
    prefix_hash_in  TEXT NOT NULL,         -- SHA-256 of pre-mutation prefix
    prefix_hash_out TEXT NOT NULL,         -- SHA-256 of post-mutation prefix
    strategy_bytes  TEXT NOT NULL DEFAULT '',  -- "name:bytes" CSV, per-strategy elided bytes
    -- response-side metrics (all INTEGER NOT NULL DEFAULT 0):
    ttft_us, input_tokens, cache_read_input_tokens, cache_creation_input_tokens,
    output_tokens, applied_edits_cleared_thinking_turns,
    applied_edits_cleared_tool_uses, applied_edits_cleared_input_tokens,
    model           TEXT                   -- request model family, if known
) STRICT;
CREATE INDEX IF NOT EXISTS idx_requests_ts ON requests(ts);
```

The **prefix** is the top-level request body with `messages` removed,
serialized sorted-key + compact, then SHA-256'd (`ledger::prefix_hash`). The
hash only needs to be **self-consistent within Rust** (same body → same hash,
so a no-op leaves it unchanged); it is recorded only in the local ledger,
never compared cross-implementation at runtime. (It is close to but not
byte-identical to the Phase 0 Python `cache_prefix_hash` — serde emits raw
UTF-8 vs Python's `ensure_ascii` escaping; parity isn't required.)
Cache-prefix hashes are critical: if
they change when no strategy fired, we're busting the prompt cache for no
reason — see [`SPIKE.md` §9](SPIKE.md) "Cache-prefix thrashing".
`trimwire stats` reports the **stability ratio** over the no-strategy-fired
cohort (fraction with `prefix_hash_in == prefix_hash_out`; should be 1.0).

**Write path:** a single `Arc<Mutex<Connection>>`, but `record()` runs the
blocking insert on `tokio::task::spawn_blocking` and is **fire-and-forget** —
the gateway never `.await`s it, and a failed insert is logged + swallowed. A
DB that can't open yields a *degraded* (no-op) `Ledger` so the gateway keeps
proxying; telemetry never gates traffic. PRAGMAs: `WAL` + `synchronous =
NORMAL`. Growth is bounded by a startup age-prune (`retain_days`, default
365); no rollup table, no `auto_vacuum` (unneeded at this data scale). The
`trimwire stats` reader opens a separate `READ_ONLY` connection (WAL = one
writer + concurrent readers).

## Layer rules (enforced by structure)

| Module | May import from | Must NOT import from |
|---|---|---|
| `main.rs` | All (wiring layer) | – |
| `cli/*` *(binary-private)* | `trimwire::*` (esp. `proxy::gateway`, `config`, `ledger`) | – |
| `proxy/gateway.rs` | `pairing`, `strategies`, `proxy::{upstream,proxy_stream,audit}`, `ledger`, `config`, `error` | – |
| `proxy/upstream.rs` | `config`, `error` | `strategies`, `pairing` |
| `proxy/proxy_stream.rs` | `error` | Everything else |
| `proxy/audit.rs` | (external only: `serde_json`, `serde`) — pure metadata derivation + an append-only file sink | `strategies`, `pairing`, `ledger`, `config` |
| `strategies/*` | `pairing`, `config`, `error` | `proxy::*`, `ledger` |
| `pairing.rs` | `error` | Everything else |
| `ledger.rs` | (external only: `rusqlite`, `serde_json`, `sha2`) — callers pass `db_path: &str` + `retain_days`, so ledger stays decoupled from `config` | `strategies`, `proxy::*` |
| `config.rs` | `error` | Everything else |
| `error.rs` | – | – |
| `summarizer/mod.rs` | `config`, `proxy::upstream`, `strategies`, `reprune`, `ledger`, `summarizer::{api,harm_check,slice}` | `proxy::gateway` |
| `summarizer/api.rs` | `config`, `proxy::upstream`, `summarizer` (sibling constants) | `strategies`, `pairing`, `ledger` |
| `summarizer/harm_check.rs` | `summarizer` (sibling constants) | Everything else |
| `summarizer/slice.rs` | (external only: `serde_json`) | Everything else |

These are not currently enforced by a tool; clippy doesn't have a
restricted-imports check equivalent to biome's. Discipline + code review.

## Decision log

- **Why Rust over Python?** Single static binary, no interpreter
  dependency, predictable performance under sustained request load,
  memory safety.
- **Why `hyper` directly, not `axum`?** We need fine-grained control of
  the response body stream for SSE passthrough. `axum` is convenient but
  adds a layer; `hyper` is the minimum surface.
- **Why `rustls` over `native-tls`?** Pure-Rust TLS, no OpenSSL build
  pain, consistent cross-platform behaviour.
- **Why are `CrossTurnDedup` + `FailedInputPurge` v0.1 (not v0.2 as the
  spike planned)?** Empirically the shipped defaults pruned nothing on a
  normal coding session, and these two are exactly the default zero-LLM
  strategies the primary inspiration (opencode-dcp) ships — the engine behind
  real savings. They're safe to default-on (dedup removes only superseded
  duplicates; purge touches only failed inputs), unlike denylisting Bash in
  `SlidingWindow`. `BloatCap` (size trimming) ships too but **old-only** —
  it trims string results (and salvages text blocks inside array results) older
  than the recent window, so the model is never deprived of a result it's actively
  using (resolving the "no safe threshold" concern). See SPIKE §4 promotion note.
- **Why if-enabled dispatch (not a `Strategy` enum or trait objects)?**
  Closed, small set of strategies chained in `strategies::run`; a plain
  `if cfg.<name>.enabled` chain is static, allocation-free, and each
  `apply` is independently unit-testable. An enum/trait adds indirection
  for no current gain. If strategies ever become plugin-loadable, revisit.
- **Why SQLite for the ledger (not in-memory)?** Survives gateway
  restart; enables post-mortems ("why did session X save nothing
  yesterday?").
- **Why `spawn_blocking` over a `Mutex<Connection>` (not a dedicated
  writer thread + channel, and not `tokio-rusqlite`/`sqlx`/libSQL)?**
  Single-user traffic with WAL + `synchronous = NORMAL` makes an insert a
  sub-millisecond page-cache write, dwarfed by the upstream round-trip;
  `spawn_blocking` keeps that blocking call (and rare checkpoint stalls)
  off the async workers without the lifecycle cost of a channel+thread.
  `tokio-rusqlite`/`sqlx` are async wrappers that still run blocking SQLite
  on a pool — needless deps. **libSQL** rejected: its value (replicas,
  server mode, Turso sync) is irrelevant to a local single-writer file and
  would bloat the single static binary + work against "data stays local".
- **Why bound growth by a startup age-prune (not a rollup table or
  `auto_vacuum`)?** At single-user rates the DB is tens of MB/year; a
  365-day prune is plenty and keeping raw rows preserves the per-request
  prefix-hash needed for §9 post-mortems. A rollup would destroy exactly
  that signal.
- **Why group `src/proxy/` and split `src/cli/` (Step 6)?** `gateway`,
  `upstream`, and `proxy_stream` form one transport tier that changes
  together → grouped behind `proxy/`. The 5 CLI subcommands are independent
  bodies sharing a config-writer → split into `cli/` (one file each), which
  also keeps `main.rs` to pure dispatch. The empty `messages.rs` stub was
  deleted (no types to hold; the walk helpers live next to their callers in
  `strategies/mod.rs`) and will be re-introduced only when we strongly-type
  the block shapes. Deliberately skipped as premature ceremony: a `lib.rs`
  re-export facade (a binary has no external consumers — only 2 in-repo call
  sites, updated directly), splitting `ledger.rs`, and 1-file folders.
- **Why `ANTHROPIC_BASE_URL` (not `HTTPS_PROXY`)?** No CA cert install
  needed (we're the endpoint, not intercepting TLS). Anthropic-documented
  mechanism (LiteLLM, Cloudflare AI Gateway, Vercel AI Gateway all use
  this pattern). See [`SPIKE.md` §1](SPIKE.md).
- **Why does `sweep` abort on concurrent change instead of splicing the
  append?** The original port spliced the new tail (records appended while we
  worked) onto the rewritten temp before renaming. That path could silently
  drop records written in the window *between* the re-read and the `rename` —
  reproduced as a ~7,600-record gap under a racing appender. There's no way to
  make read-modify-rename airtight against an active writer without locking the
  file (which Claude Code won't honor). Since `sweep` targets *inactive*
  sessions, the safe choice is to abort and leave the file untouched the moment
  it detects any change since the snapshot. Losing the optimization (sweeping a
  live session) is worth never losing a record.
- **Why does `BloatCap::trim` return `None` when it wouldn't shrink?** On small
  or pathological results, head + tail + the `[trimwire: trimmed N]` marker can
  exceed the original. Emitting that would *grow* the request and bust
  Anthropic's prompt-cache prefix for negative benefit. Returning `None` (skip,
  don't count it) guarantees every fired strategy only ever shrinks the body —
  the invariant the whole gateway leans on for cache stability (§9).
- **Why cap the request body at 32 MB → HTTP 413?** The gateway buffers the
  whole `/v1/messages` body to mutate it, so an unbounded body is an
  unbounded-memory DoS on a localhost service. 32 MB sits comfortably above any
  real Claude Code request (which Anthropic's own limits keep far smaller)
  while bounding worst-case allocation; over it we fail fast with a 413 rather
  than OOM.

## Where to look first

- Working on a strategy → `src/pairing.rs` + read [`SPIKE.md` §5](SPIKE.md)
  before touching anything.
- Working on the HTTP loop → `src/proxy/` (`gateway.rs`, then `upstream.rs`
  and `proxy_stream.rs`).
- Working on a CLI command → `src/cli/<subcommand>.rs`.
- Working on config → `src/config.rs` + [`SPIKE.md` §7](SPIKE.md).
- Working on ledger / `trimwire stats` → `src/ledger.rs` +
  [`SPIKE.md` §9](SPIKE.md) on cache-prefix invariants.
