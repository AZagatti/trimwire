# trimwire — Spike: An `ANTHROPIC_BASE_URL` gateway for automatic mid-session context pruning in Claude Code

**One-line pitch:** trimwire is a small local gateway that points Claude Code at
itself via the official `ANTHROPIC_BASE_URL` env var, mutates the `messages[]`
array of every outbound `/v1/messages` request to strip image payloads and
elide stale tool calls, and forwards the slimmer payload to `api.anthropic.com`.
Zero CA-cert install. Zero restart required. The bytes shipped to the API
actually shrink mid-session.

**Validation status:** all four candidate architectures (gateway, sweep, tmux
restart, HTTPS proxy fallback) were empirically tested against a live
Claude Code v2.1.153 with real Pro/Max OAuth. The gateway works, and sweep
shipped alongside it in v0.1.0 (`trimwire sweep`). The remaining two (HTTPS proxy
fallback, tmux restart) are documented as alternatives — see §8.

**Positioning:** trimwire is **the official, transparent, customisable
alternative to reverse-proxy + MITM workarounds** (like
`pathakmukul/claude-code-context-pruner` + mitmproxy). Both work; trimwire
uses Anthropic's documented gateway mechanism instead of TLS interception,
ships as one Rust binary, and exposes its mutations through `trimwire stats`.
If you prefer the existing mitmproxy path, that's a valid choice and is
documented in this spike too.

---

## 1. The constraint (load-bearing facts)

These facts were confirmed across three RE passes and the validation in §12.

### Claude Code's process and mutation surface

- Claude Code v2.1.153 is a **compiled Bun SEA**, not a Node.js process.
  Empirically: `file claude` → ELF 64-bit, dynamically linked. Bun
  **rejects** `NODE_OPTIONS=--require`; only honours `--preload` as a CLI
  flag (which Claude Code does not expose). Platform-specific binaries
  ship for darwin / linux / windows × arm64 / x64.
- The complete set of in-process hook mutation surfaces is exactly three:
  `PreToolUse.updatedInput`, `PostToolUse.updatedToolOutput`,
  `UserPromptSubmit.suppressOriginalPrompt`. **None can touch past
  messages.** `/clear` and `/compact` use internal `removeMessageByUuid()`
  which is not exposed.
- An undocumented HTTP-hook capability exists in v2.1.153 (binary strings
  confirm `HttpHookSchema`, `allowedHttpHookUrls`). It's a deployment
  improvement for the regular hook surfaces — doesn't expand mutation.

### Claude Code's network surface (the path we use)

- Claude Code **honours `ANTHROPIC_BASE_URL`** (documented at
  `code.claude.com/docs/en/llm-gateway`). Empirically: it accepts plain
  `http://localhost:PORT` — no TLS required for the local endpoint.
- Auth (Bearer for OAuth Pro/Max, `x-api-key` for API key) is a plain HTTP
  header. The gateway forwards it unchanged; we never touch auth.
- When `ANTHROPIC_BASE_URL` is non-firstparty, Claude Code disables an
  "optimistic tool search" optimisation by default. The workaround is
  documented: set `ENABLE_TOOL_SEARCH=true` (or `auto` / `auto:N`). The
  installer sets this automatically; see §7.

### Anthropic's stance (relevant for ban-risk)

- Anthropic explicitly sanctions `ANTHROPIC_BASE_URL` gateways and corporate
  proxies. LiteLLM, Vercel AI Gateway, Cloudflare AI Gateway are documented
  examples of the same pattern.
- The Jan 2026 third-party-tool ban targeted tools that ship a **different
  system prompt** while authenticating with Pro/Max OAuth (OpenCode, Cline,
  OpenClaw). Detection is via system-prompt content pattern-matching, not
  TLS fingerprinting or User-Agent. **trimwire keeps Claude Code as the
  client verbatim** and never modifies the `system` field — only the
  conversation `messages[]` array — so this detection vector doesn't apply.
- Patching the compiled binary is ruled out — DMCA precedent from April
  2025 against `free-code`.

---

## 2. Empirical validation

All four tiers validated end-to-end against Claude Code v2.1.153. **Full test
record in §12.** Key result: live `claude --print` through a Python aiohttp
POC gateway returned a real Anthropic response with mutation visible in the
gateway log — the architecture works. POC scripts preserved under `pocs/`.

---

## 3. Architecture

```
┌─────────────────────────────────────────────────────────┐
│  User shell                                             │
│    export ANTHROPIC_BASE_URL=http://127.0.0.1:8765      │
│    export ENABLE_TOOL_SEARCH=true                       │
│    claude                                               │
└──────────────────────┬──────────────────────────────────┘
                       │ POST /v1/messages?beta=true
                       │ Authorization: Bearer ...
                       │ Body: { model, system, messages[…], tools, ... }
                       ▼
┌─────────────────────────────────────────────────────────┐
│  trimwire gateway   (Rust binary, ~1000-1200 LOC)         │
│  ────────────────────────────────────────               │
│  REQUEST PATH (buffered — mutation requires full body): │
│    1. Receive HTTP request on localhost.                │
│    2. Read body fully. Parse JSON (serde_json).         │
│    3. If body contains `messages[]`:                    │
│        a. Build pairing index (tool_use_id ↔ block).    │
│        b. Pre-validate (no orphans, no dups).           │
│        c. Apply enabled strategies (see §4).            │
│        d. Post-validate (invariants still hold).        │
│        e. On any failure → roll back to original body.  │
|    4. Recompute Content-Length. System prompt and       │
│       all headers (auth, anthropic-*, x-stainless-*)    │
│       forwarded unchanged.                              │
│                                                         │
│  RESPONSE PATH (streamed — no buffering):               │
│    5. Forward to https://api.anthropic.com/v1/messages  │
│       via hyper + rustls + webpki-roots.                │
│    6. Pipe response body bytes-for-bytes back to caller │
│       (SSE `text/event-stream` passthrough; never       │
│       buffer or re-encode).                             │
│    7. Log one line to stderr: in/out bytes, strategies  │
│       fired, cache-prefix hash before/after.            │
└──────────────────────┬──────────────────────────────────┘
                       │
                       ▼
              api.anthropic.com (unchanged)
```

### Request buffered, response streamed — the contract

This split is load-bearing. **Requests must be buffered fully** because the
gateway needs the entire `messages[]` array to compute mutations safely. We
recompute Content-Length and forward the mutated body in one POST. **Responses
must be streamed** because Anthropic's API uses Server-Sent Events
(`text/event-stream`); any buffering would defeat the streaming UX in
Claude Code's TUI. `hyper`'s body type supports both naturally — use
`http_body_util::BodyExt::collect()` on the request path and
`http_body_util::BodyExt::boxed()` on the response path to type-erase
the upstream body and stream it through unchanged.

### Why this delivers opencode-dcp parity

- The gateway owns the `messages[]` array at every API call — it can drop,
  dedup, or stub any past message, not just the current turn.
- Bytes shipped to the API actually shrink mid-session. The user's token
  bill drops; cache hit ratio is preserved as long as mutations are
  deterministic per content (see "cache-prefix invariant" in §9).
- Zero restart required. The user does nothing per-session.
- The on-disk JSONL is untouched — sessions remain inspectable. If the user
  kills the gateway, Claude Code falls back to the full history via the
  next request (without the env var).

### Pro/Max OAuth safety

> UPDATE (Feb 2026): Anthropic's Consumer-Terms clause broadened beyond the
> January enforcement below — it now prohibits using Pro/Max OAuth tokens in *any*
> other product/tool/service (not just system-prompt-swapping clients), and from
> June 2026 meters third-party/Agent-SDK use via a separate paid credit pool. The
> gateway-pattern argument below still stands (verbatim Claude Code client, auth
> forwarded, no own calls), but the safest footing for subscription users is an
> **API key**. See `docs/FAQ.md` for the current user-facing stance.

We are not the pattern Anthropic banned in January 2026. That ban detected
third-party tools that ship a different system prompt under Pro/Max OAuth.
trimwire uses the real Claude Code as the client; the `system` field,
tool definitions, and headers (including the User-Agent suffix
`(external, sdk-cli)`) are exactly what Anthropic's gateway documentation
expects from a corporate-proxy deployment. We modify the conversation
`messages[]` content only.

---

## 4. Strategies (5 total)

> ⚠ **STALE (historical spike).** The shipped code has **8 default strategies +
> 1 opt-in** (`simhash_dedup`), and orphan validation is a single post-`run()`
> check (not per-strategy). This section reflects the original 5-strategy spike;
> the live reference is `src/strategies/mod.rs`.

Each strategy is a Rust module under `src/strategies/`, dispatched in order by
`strategies::run`. Configurable on/off and per-tool via TOML. **All five ship
in v0.1.0** — the original plan was to ship only `SlidingWindow` + `ImageStrip`
and defer the other three, but they were promoted before release (see the
promotion note below the table).

| # | Strategy | What it does | In v0.1? |
|---|---|---|---|
| 1 | `SlidingWindow` | After N assistant turns of history, for each `tool_use` block whose tool name is on the denylist: drop the block (and its paired `tool_result`) as a unit; replace with a stub. **Pair-aware** — see §5. | **✅ v0.1** |
| 2 | `ImageStrip` | When a `tool_result` block contains a base64 image payload (Playwright et al), replace with a marker after K most-recent images are kept. Per-tool allowlist. | **✅ v0.1** |
| 3 | `CrossTurnDedup` | Same tool called with identical args more than once → keep the most recent `tool_result`, replace earlier identical ones with a superseded marker. Content-only, deterministic. | **✅ v0.1** |
| 4 | `FailedInputPurge` | When a paired `tool_result.is_error == true` and older than N turns, replace the `tool_use.input` with `{}` (preserve name + id + error text). | **✅ v0.1** |
| 5 | `BloatCap` | Single global size threshold (with a per-tool exempt list): trim a single oversized string tool_result (older than N turns) to head + tail + a `[trimwire: trimmed N bytes]` marker. Skipped if the trim wouldn't shrink the result. | **✅ v0.1** |

> **Promotion note (2026-05-29).** All three "v0.2" strategies were pulled into
> v0.1. `CrossTurnDedup` + `FailedInputPurge` because the shipped defaults
> (`SlidingWindow` denylist = `mcp__playwright__*` only) pruned **nothing** on
> a normal coding session — and these two are exactly opencode-dcp's default
> zero-LLM strategies (the engine behind its 50–70%), so trimwire's "parity" (§3)
> was incomplete without them. They're safe to default-on (dedup removes only
> *superseded* duplicates; purge touches only *failed* inputs).
> `BloatCap` was the originally-risky one ("no safe size threshold"); the
> shipped design resolves that by being **old-only** — it trims only string
> results *older* than `keep_recent_turns`, so a result the model is actively
> using is never touched, and file-editing tools are exempt. It catches the
> large *unique* old output that dedup/sliding_window miss.

### Per-tool defaults (semantics clarified)

- **Tool matching** is exact name match; wildcards (`mcp__playwright__*`)
  supported via glob in TOML.
- **Exempt** in a strategy's config means exempt from *that strategy only*,
  not from all mutations.
- Per-strategy config can override globals; project-level `.trimwire.toml`
  overrides global `~/.config/trimwire.toml` (replace-not-merge for lists).

| Tool | Default policy | Exempt-from |
|---|---|---|
| `Read`, `Edit`, `Write`, `MultiEdit`, `Task` | Never *stubbed* by SlidingWindow; but `CrossTurnDedup` supersedes older identical repeats (keeps the latest) | SlidingWindow, ImageStrip |
| `Bash` (repeated identical) | Dedup (keep latest); errored inputs purged | `CrossTurnDedup`/`FailedInputPurge` configurable |
| `Grep` / `Glob` (repeated identical) | Dedup (keep latest) | `CrossTurnDedup` configurable |
| MCP image-returning tools (`*screenshot*`) | Strip after K recent | All except `ImageStrip` |
| All other MCP tools | `SlidingWindow` denylist candidates (off by default) | Configurable |

Explicit non-goals: tokenisation-aware trimming, LLM-driven summarisation,
gzip, system-prompt mutation, anything requiring restart.

---

## 5. Pairing invariants (the load-bearing detail)

The hardest correctness property in the gateway. **Get this wrong and Anthropic
rejects with 400 — every mutation must be pair-aware.**

### Definitions

- A **tool_use block** lives in `messages[i].content[]` where
  `messages[i].role == "assistant"`. It has an `id` field.
- A **tool_result block** lives in `messages[j].content[]` where
  `messages[j].role == "user"`, typically in a message immediately
  following the tool_use's assistant message. It has a `tool_use_id` field
  that must match the `tool_use.id`.
- A **pair** is a (tool_use, tool_result) tuple matched by id.
- An **orphan** is a tool_result whose tool_use_id has no matching tool_use
  in the request, or vice versa. Anthropic API rejects orphans.

### Invariants (Rust enforces these around every strategy)

1. **Pre-mutation validation:** every tool_result.tool_use_id has a matching
   tool_use.id in an earlier message. Fail loudly if not (the input was
   already broken; we forward unmutated).
2. **Atomic pair drops:** strategies always drop both halves together. If a
   `SlidingWindow` decides to stub tool_use `abc`, the pairing index gives
   it the matching tool_result block; both are mutated as a unit.
3. **Parallel-call pairs:** a single assistant message can contain N
   `tool_use` blocks; the next user message can contain N matching
   `tool_result` blocks. Treat each pair independently — don't drop all-or-
   none across the parallel set.
4. **Post-mutation validation:** re-run the orphan check. If any orphan
   appeared, the mutation is buggy; roll back to the original body and log
   loudly.

### Pseudocode

```rust
// src/pairing.rs (dedicated module)
pub struct PairingIndex {
    // tool_use_id → (message_idx, content_idx)
    pub uses: HashMap<String, (usize, usize)>,
    pub results: HashMap<String, (usize, usize)>,
}

impl PairingIndex {
    pub fn build(messages: &[Value]) -> Self { /* one pass */ }
    pub fn validate(&self) -> Result<(), OrphanError> {
        for (id, _) in &self.results {
            if !self.uses.contains_key(id) {
                return Err(OrphanError::Result(id.clone()));
            }
        }
        Ok(())
    }
}

// src/strategies/sliding_window.rs
pub fn apply(messages: &mut Vec<Value>, cfg: &SlidingWindowConfig) -> Result<Stats> {
    let idx = PairingIndex::build(messages);
    idx.validate()?;  // pre-check

    let to_stub: HashSet<String> = collect_old_denylist_ids(messages, &idx, cfg);
    for id in &to_stub {
        let (mi, ci) = idx.uses[id]; stub_tool_use(&mut messages[mi].content[ci]);
        let (mj, cj) = idx.results[id]; stub_tool_result(&mut messages[mj].content[cj]);
    }

    PairingIndex::build(messages).validate()?;  // post-check
    Ok(Stats { stubbed: to_stub.len() })
}
```

### Stub format (consistent across strategies)

All stubs use the prefix `[trimwire: ...]` (lowercase, square brackets, single
prefix). Examples:

- `tool_use.input` replaced with `{}` and a sibling field tracking the stub
  reason (or just `{}` — discussed in §10).
- `tool_result.content` replaced with `"[trimwire: elided, older than sliding window]"`
- Image payload replaced with `"[trimwire: image stripped, original in session JSONL line N]"`

---

## 6. Rust scaffold

> **Superseded — see [`ARCHITECTURE.md`](ARCHITECTURE.md) for the as-built
> layout.** This was the pre-implementation sketch. The shipped code groups the
> HTTP layer under `src/proxy/` (gateway/upstream/proxy_stream), splits the CLI
> into `src/cli/` (one file per subcommand), dropped the planned `messages.rs`
> (no typed request structs were needed) and the `regex` dependency (base64
> detection is a dependency-free char scan), and added `src/sweep.rs`. All five
> strategies ship, not just the two marked "MVP" below.

```
trimwire/
├── Cargo.toml
├── src/
│   ├── main.rs                # CLI: `run`, `daemon`, `stats`, `install`
│   ├── lib.rs                 # module re-exports
│   ├── gateway.rs             # hyper server + dispatch
│   ├── upstream.rs            # rustls client to api.anthropic.com
│   ├── proxy_stream.rs        # SSE response passthrough
│   ├── messages.rs            # request/response types
│   ├── pairing.rs             # PairingIndex (see §5)
│   ├── config.rs              # figment + TOML (global + per-project)
│   ├── error.rs               # thiserror types + HTTP-code mapping
│   ├── ledger.rs              # savings accounting (SQLite-backed; see below)
│   └── strategies/
│       ├── mod.rs             # Strategy enum + dispatch
│       ├── sliding_window.rs  # MVP
│       └── image_strip.rs     # MVP
└── tests/
    ├── integration.rs         # insta snapshot tests over fixtures
    └── fixtures/              # captured /v1/messages bodies (redacted)
```

```toml
[dependencies]
hyper = { version = "1", features = ["server", "client", "http1", "http2"] }
hyper-util = { version = "0.1", features = ["tokio", "server", "client-legacy"] }
http-body-util = "0.1"
hyper-rustls = { version = "0.27", features = ["webpki-roots", "http2"] }
rustls = "0.23"
webpki-roots = "0.26"
tokio = { version = "1", features = ["macros", "rt-multi-thread", "signal", "net", "sync"] }
serde = { version = "1.0", features = ["derive"] }
serde_json = "1.0"
thiserror = "2"
anyhow = "1.0"
figment = { version = "0.10", features = ["toml", "env"] }
toml = "0.8"
tracing = "0.1"
tracing-subscriber = { version = "0.3", features = ["env-filter", "fmt"] }
regex = { version = "1", features = ["unicode"] }
ahash = "0.8"
sha2 = "0.10"                            # cache-prefix hashing
hex = "0.4"                              # hash → hex string for ledger
rusqlite = { version = "0.32", features = ["bundled"] }   # ledger store
clap = { version = "4", features = ["derive"] }

[dev-dependencies]
insta = { version = "1.41", features = ["json"] }
rstest = "0.23"
wiremock = "0.6"
reqwest = { version = "0.12", default-features = false, features = ["json", "stream", "rustls-tls", "http2"] }

[profile.release]
opt-level = 3
lto = "thin"
strip = true
codegen-units = 1
```

### rustls 0.23 — required runtime setup

rustls 0.23 requires an explicit `CryptoProvider::install_default()` at
process startup. **Without this, the first TLS handshake panics with an
opaque `no process-level CryptoProvider available` error.** Install once
at the top of `main()` (or in `upstream::init()`):

```rust
rustls::crypto::aws_lc_rs::default_provider()
    .install_default()
    .expect("install rustls crypto provider");
```

(`aws-lc-rs` is the default for `hyper-rustls`'s `webpki-roots` feature
and what `Cargo.toml` pulls in transitively. If you swap to `ring`,
update the call.)

### Error handling

- Process exit is reserved for startup-fatal errors only (port bind,
  TLS init). Mid-request errors NEVER crash the process.
- Wrap mid-request handler failures in an Anthropic-compatible error
  envelope and return a `4xx`/`5xx` status:
  ```json
  {"type":"error","error":{"type":"gateway_error","message":"<detail>"}}
  ```
  Claude Code's SDK already knows how to surface this shape to the user.
- Specific status mapping:
  - Malformed inbound JSON → 400
  - Upstream connection failure → 502
  - Strategy crash (caught) → forward the original unmutated body, 200
    (or whatever upstream returns); log loudly
- Upstream non-200 responses pass through unchanged. Don't retry,
  don't rewrite — Claude Code's SDK handles retries.

### Concurrency model

- Each request is independent. Tokio task per request via `hyper::Service`.
- Shared state: config (read-only after startup) + ledger (write-heavy).
- Ledger: write to SQLite via a single `Arc<Mutex<Connection>>` (sqlite is
  serialised by default; this is fine for the rate of inserts we expect).
  One row per request: `(timestamp, session_id, in_bytes, out_bytes, strategies_fired, cache_prefix_hash)`.
- Rationale for SQLite over in-memory `dashmap`: post-mortems. Users will
  ask "why did session X save nothing yesterday?" — we need persistent
  per-request data to answer.

### Performance

- Per-request overhead target: 5-20ms (TLS handshake to upstream on first
  request; keep-alive reused thereafter via hyper's pool).
- Request body buffer: bounded by Anthropic's per-request payload limit
  (~10MB typical); fits in memory comfortably.
- Response: zero buffering. Stream chunks straight from upstream to
  downstream socket.

### LOC + timeline (realistic, per critique)

- **Production code:** ~1000-1200 LOC including pairing logic, error paths,
  validation, ledger.
- **Tests:** ~400-500 LOC (insta snapshots over fixtures + wiremock
  integration).
- **Timeline to publishable v0.1.0:** ~7-10 evenings of focused work,
  including the Phase 0 Python test harness (see §10).

---

## 7. UX and install

```bash
curl -LsSf https://github.com/<user>/trimwire/releases/latest/download/trimwire-installer.sh | sh
```

The installer:

1. Downloads `trimwire` binary into `~/.local/bin/`.
2. Writes default config to `~/.config/trimwire.toml` (only T1 enabled; all
   strategies start conservative; user opts into more).
3. Adds to shell rc:
   ```bash
   export ANTHROPIC_BASE_URL=http://127.0.0.1:8765
   export ENABLE_TOOL_SEARCH=true
   ```
4. Optionally registers as systemd user unit (Linux) / launchd plist (macOS).
5. Or — alternative pattern — drops a `trimwire run` wrapper that spawns the
   gateway in the background and `exec`s `claude`, so the env var is scoped
   to that invocation only. *(Impl note, Phase 1 Step 6: the shipped
   `trimwire run` uses spawn + wait rather than `exec` — keeping trimwire as the
   parent tears the background gateway down on exit, avoiding an orphaned
   daemon. Same UX, cleaner lifecycle.)*

### About `ENABLE_TOOL_SEARCH`

This Claude Code env var enables an "optimistic tool search" that's normally
disabled when `ANTHROPIC_BASE_URL` points at a non-first-party host. The
behaviour the installer enables is: `claude` forwards `tool_reference` blocks
through the proxy as if the proxy supports them — which we do, transparently.
Without setting it, Claude Code may silently disable an internal optimisation
that improves tool-suggestion latency. **The installer sets it; the user
shouldn't have to think about it.** A future version of the gateway may
auto-inject equivalent behaviour at the request level to remove this
requirement.

### Commands (MVP)

> **Superseded — see [`README.md`](README.md#commands) for the shipped command
> set.** This was the planned MVP list; v0.1.0 also ships `trimwire sweep`
> (on-disk transcript cleanup, with `--dry-run` / `--validate-only`).

| Command | What it does |
|---|---|
| `trimwire on` | Start the gateway in the foreground |
| `trimwire run [args...]` | Start gateway in background, launch `claude` (spawn + wait, not `exec` — see impl note above) |
| `trimwire stats` | Show savings ledger (per-day bytes elided, requests, strategies fired) |
| `trimwire install` | One-shot installer (binary, config, shell rc) |
| `trimwire config` | Open `~/.config/trimwire.toml` in `$EDITOR` |
| `trimwire sweep <file>` | Clean a session JSONL on disk (atomic, backed up) |

### Config (TOML, two-tier)

Global at `~/.config/trimwire.toml`, per-project at `.trimwire.toml` (project
overrides merged onto global). Pattern lifted from `code-context-engine`.

```toml
[server]
listen = "127.0.0.1:8765"
upstream = "https://api.anthropic.com"

[strategies.sliding_window]
enabled = true
keep_recent_turns = 4
denylist_tools = [
    "mcp__playwright__browser_navigate",
    "mcp__playwright__*",
    # user adds known-elidable tool patterns here
]
exempt_tools = ["Read", "Edit", "Write", "MultiEdit", "Task"]
stub = "[trimwire: elided, older than sliding window]"

[strategies.image_strip]
enabled = true
applies_to_tools = ["mcp__playwright__browser_take_screenshot", "*screenshot*"]
keep_recent_count = 3
stub = "[trimwire: image stripped]"
```

---

## 8. Build / defer / document split

After empirical validation, the four candidate architectures split cleanly:

| | What it is | v0.1 status |
|---|---|---|
| **T1 — Gateway** | The headline `ANTHROPIC_BASE_URL` Rust gateway | **✅ Built (this MVP)** |
| **T2 — HTTPS proxy alternative** | `pathakmukul/claude-code-context-pruner` + mitmproxy via `NODE_EXTRA_CA_CERTS` | **Documented only** — README has the 3-command setup recipe |
| **T3 — Sweep** | On-disk JSONL cleanup between sessions | **✅ Shipped in v0.1.0** (`trimwire sweep`, `src/sweep.rs`) — pulled forward from v0.2 as a deliberate product call (see the T3 note below), not because telemetry demanded it |
| **T4 — tmux restart** | Kill+resume via `tmux send-keys` if gateway fails | **Documented only** — `pocs/tier3-restart.sh` shipped as reference script |

Why each is what it is (verdicts from the build-vs-document critique):

- **T1** is the only path that delivers the headline feature (true past-message
  cleanup, no restart, no cert).
- **T2** (HTTPS proxy alternative) is a working, battle-tested existing tool.
  Wrapping or re-implementing it would split trimwire into two architectures
  with no extra user benefit. Better: link to it honestly and let users
  who prefer that path use it directly.
- **T3** (sweep) saves ~0.76% on a real 72MB session (validated). Real bloat
  is `tool_result` content (images, reads), handled in-flight by T1, so the
  original recommendation was to defer until usage data justified it.
  **Update (v0.1.0):** sweep was nonetheless shipped in v0.1.0 — a deliberate
  product decision to land the whole feature set at once, made *ahead* of the
  telemetry this analysis asked for, not because that telemetry arrived. The
  honest framing: its measured value is modest and it carries the higher blast
  radius of an in-place on-disk rewrite, mitigated (not erased) by abort-on-
  concurrent-change, backup-before-rename, `--dry-run`, and a post-write
  validation. It is a separate opt-in command, never run automatically.
- **T4** (tmux restart) only works inside tmux/screen (~20% of users). For
  the other 80%, building it into the binary would be a silent no-op. The
  failure mode it addresses (gateway crash mid-session) is rare; users can
  manually `claude --resume <id>` in 5 seconds. Build complexity isn't
  warranted.

### What documented-only actually means

For T2 and T4 we ship the recipe in the README as a complete copy-paste-able
section, not just a link. Example for T4:

```bash
# If your gateway is misbehaving and you're inside tmux, drop this snippet
# into ~/.local/bin/trimwire-restart.sh and `chmod +x` it:
#   [contents of pocs/tier3-restart.sh]
# Run: trimwire-restart.sh --session-id <UUID>
```

Discoverability via the README is enough for the audiences these features
serve.

---

## 9. Risks and open questions

| Risk | Likelihood | Mitigation |
|---|---|---|
| **Cache-prefix thrashing** (mutations fire inconsistently across requests, busting prompt cache, making gateway HURT performance) | **Medium — and silent if it happens** | Log cache-prefix hash before/after every mutation; CI test that asserts hash is stable when no strategy fires and deterministic when one does; `trimwire stats` surfaces cache-hit-rate trend |
| Anthropic adds endpoint-origin checks in OAuth | Low | They explicitly document corporate-proxy use; class-of-tools risk, not trimwire-specific |
| Anthropic adds TLS cert pinning | Very low | Would break corporate-proxy support |
| `ENABLE_TOOL_SEARCH=true` workaround missed by user | Low | Installer sets it automatically; documented |
| Gateway crashes mid-session | Medium-low | Supervisor / systemd auto-restart; fallback is user unsets env var and continues |
| Strategies mutate `messages[]` in a way Anthropic rejects (orphaned `tool_use_id`) | Medium → **Low** with pairing module | The pairing invariants in §5 + pre/post validation + rollback on failure |
| Tool-use/tool-result pairing is subtle | High at impl time | Dedicated `pairing.rs` module + comprehensive integration tests with real fixtures |
| Parallel tool_use blocks (multiple tools in one assistant message) | Medium correctness risk | Pairing index treats each pair independently; test fixtures cover this case |
| `compact_boundary` system messages | Medium correctness risk | Strategies skip system messages by default; explicit test fixture |
| Long-running gateway under sustained load | Low | Tokio + hyper is battle-tested; SQLite ledger serialises |

---

## 10. Recommended next step — two phases

### Phase 0: Pre-Rust test harness (~1 evening)

Before writing any Rust, build a Python test harness (~1200 LOC) that locks
down the mutation semantics against real fixtures. Reuses the same fixtures
the Rust integration tests will use, so this work isn't wasted.

Five blocker tests:

1. **Parallel `tool_use` blocks** — single assistant message with 3 parallel
   tool calls; verify pairing index treats each independently.
2. **100+ turn cumulative correctness** — synthetic long session; verify
   no off-by-one in sliding-window cutoff after many turns.
3. **Real-world fixture suite** — 3 captured (redacted) sessions covering
   technical / screenshot-heavy / failure-heavy patterns.
4. **1MB+ single tool_result** — memory + escaping correctness; dedup
   performance.
5. **API endpoint enumeration** — capture a full 10-turn session via the
   POC gateway, enumerate every URL Claude Code hits; ensure gateway
   handles each (most: pass-through unmutated).

Plus: **cache-prefix hash CI check** — log the SHA-256 of the serialised
prefix before/after mutation; assert deterministic behaviour.

### Phase 1: Rust MVP (~7 evenings)

In strict order:

1. **Pass-through gateway** (no mutation) — `hyper` server + upstream client
   + response stream. Validate full forwarding loop against a real session.
   ~200 LOC.
2. **`pairing.rs`** + validation suite + unit tests for the index. ~150 LOC.
3. **`SlidingWindow` strategy** — uses pairing index; runs against
   Phase-0 fixtures. ~150 LOC + tests.
4. **`ImageStrip` strategy** — base64 detection in tool_result blocks.
   ~100 LOC + tests.
5. **Ledger (SQLite)** + `trimwire stats` command. ~120 LOC.
6. **CLI** + config + installer + shell rc setup. ~200 LOC.
7. **README** with honest pitch + T2/T4 alternative recipes. Half a day.

Total Rust: ~900-1200 LOC. Total tests: ~400-500 LOC. Total: ~7-10 evenings.

### Phase 2 (v0.2 candidates, post-launch)

- All five strategies (`SlidingWindow`, `ImageStrip`, `CrossTurnDedup`,
  `FailedInputPurge`, `BloatCap`) shipped in v0.1 — see the §4 promotion note.
- `trimwire sweep` — on-disk JSONL cleanup, ported from `pocs/tier2-sweep.py`,
  also pulled into v0.1.

---

## 11. Honest pitch (draft README)

> trimwire is a tiny local gateway for Claude Code. It points Claude at itself
> via Anthropic's official `ANTHROPIC_BASE_URL` mechanism, intercepts every
> outbound API call, and automatically strips bloat from your conversation
> context before forwarding to `api.anthropic.com`. Old screenshots get
> stripped. Stale tool calls get elided. The bytes shipped to Anthropic
> actually shrink — no restart, no manual intervention.
>
> Install with one command. Claude Code works exactly as before, except your
> context stays lean. Works with both Pro/Max subscriptions and API keys.
>
> Built in Rust, ships as a single self-contained binary (~9MB — bundles
> SQLite + a pure-Rust TLS stack), sub-20ms overhead (measured ~1ms locally).
> No CA cert install. No MITM. No patched binary. No risk of Anthropic's
> third-party-tool detection (we keep Claude Code as the client verbatim
> and only modify conversation history).
>
> **Compared to alternatives:** `pathakmukul/claude-code-context-pruner` is
> a battle-tested HTTPS proxy that does similar mutation via mitmproxy
> + `NODE_EXTRA_CA_CERTS`. Both work; trimwire uses Anthropic's documented
> gateway path instead of TLS interception, ships as one binary, and
> exposes its mutations via `trimwire stats`. If you prefer the mitmproxy
> approach, here's the setup recipe: [link to README section].
>
> Honest limitations: trimwire won't make Claude Code work past Anthropic's
> per-request token limit; it slims requests so you hit that limit later.
> For long-lived sessions resumed across days, cleanup of the on-disk
> session JSONL isn't built in — coming in v0.2 if usage shows it matters.

---

## 12. Validation appendix (empirical, 2026-05-28)

All four tiers tested end-to-end against Claude Code v2.1.153 (re-verified
2026-05-28 against v2.1.154 — protocol unchanged). POCs preserved under `pocs/`.

### Tier 1 — Gateway (`pocs/` retained design only; throwaway POC was Python aiohttp ~140 LOC)

| Test | Result |
|---|---|
| `ANTHROPIC_BASE_URL=http://127.0.0.1:8765` accepted by Claude Code | ✅ Empirically confirmed |
| OAuth Bearer token forwarded unchanged | ✅ `Authorization: <115 bytes>` arrived at gateway |
| 139KB request body parseable + mutable | ✅ JSON body parsed cleanly |
| Forward to real `api.anthropic.com` succeeds with mutated body | ✅ 401 (fake key) on fixture, 200 on live test |
| Streaming response (SSE) passthrough works without buffering | ✅ Claude rendered response normally |
| Mutation actually shrinks bytes | ✅ Fixture: 1479B → 1323B (10.5% on 5-turn fixture) |
| Tool-use/tool-result pairing preserved (no orphans) | ✅ Pre/post validation passes |
| Live `claude --print` → gateway → response: "Help you with coding tasks." (exit 0) | ✅ Full round-trip works with real Pro/Max OAuth |

### Tier 2 alternative — `pathakmukul/claude-code-context-pruner` (existing tool)

| Test | Result |
|---|---|
| mitmproxy installed via `uv tool install mitmproxy` (no sudo) | ✅ |
| Per-process cert trust via **`NODE_EXTRA_CA_CERTS`** (no system trust store changes) | ✅ Much less invasive than full CA install |
| `HTTPS_PROXY=http://localhost:58473 NODE_EXTRA_CA_CERTS=~/.mitmproxy/mitmproxy-ca-cert.pem claude --print "…"` | ✅ "Yes, message received clearly." (exit 0) |
| mitmproxy log shows TLS termination + forward to `api.anthropic.com:443` | ✅ Live trace captured |
| Full cleanup leaves system CA bundle byte-identical to baseline | ✅ md5 matches pre-test snapshot |

### Tier 3 — Sweep (`pocs/tier2-sweep.py`, ~320 LOC stdlib Python)

| Test | Result |
|---|---|
| Real 72MB / 4273-line session swept cleanly | ✅ All 6 validation checks pass |
| Atomic write (`mkstemp` + `fsync` + `os.replace`) + rolling `.bak` | ✅ Backup created, atomic rename succeeds |
| 305 empty thinking blocks dropped + 20 failed-tool inputs purged | ✅ Counts match validator's pre-investigation |
| Pairing intact (862 tool_use = 862 tool_result, 0 orphans) | ✅ |
| `claude --resume <swept-id>` accepts swept file | ✅ "Yes, still here." (exit 0) |
| Savings on real 72MB session | **~575KB (0.76%)** — modest; shipped in v0.1.0 anyway as a product call (see §8 T3 note), Rust port hardened beyond this POC |

**Bug found + fixed:** Initial sweep stripped only the `signature` field from
thinking blocks. Anthropic API rejected with `messages.2.content.0.thinking:
each thinking block must contain thinking`. Fix: drop the entire thinking
block when `thinking == ""`. Validator agent was wrong about API tolerating
empty thinking blocks — empirical test caught it.

**Known POC bugs (acceptable for reference status):** append-detect race
needs `flock` + open-fd inode check for production use; the fix is included
in the §10 Phase-0 spec.

### Tier 4 — tmux restart (`pocs/tier3-restart.sh`, ~80 LOC bash)

| Test | Result |
|---|---|
| Synthetic test (`cat` stand-in in tmux) | ✅ All 7 mechanism checks pass |
| Real `claude` in detached tmux | ✅ C-c interrupts, new `claude --resume` starts in same pane, `pane_current_command == claude` again |
| Session JSONL created during test | ✅ |

**Correction from validation:** original design called for `/exit` slash
command. That command does not exist in Claude Code v2.1.153. The actual
mechanism is just `C-c` (resets the prompt) followed by the resume command.

**Known POC bugs (acceptable for reference status):** hardcoded sleep
timing, `pane_current_command` glob too loose for crashes, no session-UUID
format validation. Production version would address these.

### Things NOT empirically validated (deferred to Phase 0 test harness)

| Open item | Plan |
|---|---|
| Cache-prefix thrashing under real workload | Phase 0 test #1 — assert hash stability; CI check |
| Parallel `tool_use` blocks | Phase 0 fixture-driven test |
| `compact_boundary` system messages | Phase 0 fixture from a real `/compact` |
| Sub-agent (Task) result boundaries | Phase 0 fixture from a real Task invocation |
| MCP tools with complex result shapes | Phase 0 — sample real MCP results |
| Very long single `tool_result` (1MB+) | Phase 0 — synthetic stress test |
| Interactive streaming (not just `--print`) | Phase 1 step 1 (pass-through gateway) test |
| Gateway crash mid-request | Phase 1 — wiremock-driven failure injection |
| Cross-platform (macOS, Windows native) | `cargo-dist` cross-compile; smoke-test on each target before release |
| API endpoint enumeration beyond `/v1/messages` | Phase 0 — capture full session, list all URLs |
