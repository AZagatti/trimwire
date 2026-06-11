# Development plan

The phased build plan for trimwire from current scaffolded state to a
publishable v0.1.0 and onward. Source of truth for **what to build, in what
order, and how we know each piece is done**.

Companion docs:
- [`SPIKE.md`](SPIKE.md) — full design rationale and empirical validation
- [`ARCHITECTURE.md`](ARCHITECTURE.md) — module-level design + layer rules
- [`AGENTS.md`](AGENTS.md) — agent conventions and execution rules

---

## Where we are right now

- ✅ Scaffold: Cargo workspace, module stubs, docs, CI infrastructure,
  OSS hygiene files, dual MIT/Apache licence.
- ✅ Phase 0 (Python test harness): 21 invariant tests pass; `make phase0`
  auto-bootstraps a uv venv on first run.
- ✅ **Phase 1 Step 1 done** (commit `f1afa40`) — pass-through gateway:
  `cargo run -- on` actually starts a working HTTPS forwarding
  proxy. Acceptance verified end-to-end:
  `ANTHROPIC_BASE_URL=http://127.0.0.1:8765 claude --print "hi"` returns
  a real Anthropic response. Known follow-ups documented at the top of
  `src/gateway.rs` and in CHANGELOG.md (not blocking Step 2).
- ✅ **Phase 1 Step 2 done** — `src/pairing.rs` + `src/error.rs`:
  `PairingIndex { uses, results }` with `build` / `validate` / `pair`,
  ported from `tests/phase0/pairing.py`. 7 unit tests (empty, one pair,
  parallel 3+3, orphan result, lone-use-not-orphan, deterministic orphan
  reporting, string-content skip) + 2 `insta` snapshot tests over the
  fixture corpus. The Phase 0 synthetic fixtures are now materialized as
  committed JSON under `tests/fixtures/` via the reproducible
  `tests/phase0/dump_fixtures.py`; the Python suite's
  `test_real_fixture_files_load_if_present` cross-validates them.
- ✅ **Phase 1 Step 3 done** — `SlidingWindow` strategy end-to-end:
  - `src/config.rs`: typed `Config` (`server` + `strategies.*`) with serde
    defaults, two-tier figment loader (global `$XDG_CONFIG_HOME`/`~/.config`
    + project `./.trimwire.toml` + `TRIMWIRE_*` env), and a `*`-glob tool
    matcher. Strategies are **off by default** (transparent pass-through).
  - `src/strategies/sliding_window.rs`: faithful port of
    `apply_sliding_window` (cutoff walk, denylist/exempt glob match, atomic
    pair stub via `PairingIndex`, pre/post orphan validation).
  - `src/strategies/mod.rs`: `Stats`, the `run` orchestrator, and
    `apply_to_body` (parse → prune → re-serialize, with rollback to original
    bytes on any parse error / no-op / strategy error → cache prefix safe).
  - `src/gateway.rs` + `src/main.rs`: the daemon loads config and actually
    prunes `POST /v1/messages` bodies (Content-Length recomputed; `system`
    untouched).
  - Tests: 15 new lib unit tests (off-by-one=6, exempt, glob, parallel
    atomic, orphan pre-check, no-op byte-stability, padded-payload shrink,
    config defaults/glob); a **Python-parity integration test** asserting
    byte-identical output to the reference on all 6 fixtures (expected
    files via `tests/phase0/dump_expected.py`); an `insta` snapshot; and two
    wiremock gateway tests (prunes 96/100 turns orphan-free + shrinks;
    no-op forwards byte-for-byte).
- ✅ **Phase 1 Step 4 done** — `ImageStrip` strategy:
  - `src/strategies/image_strip.rs`: faithful port of `apply_image_strip`
    — collect image-tool `tool_use` ids chronologically, keep the K most
    recent, replace older base64-image `tool_result.content` with the
    marker. Base64 detection is a dependency-free char scan (the `regex`
    dep was dropped in Step 3); also handles structured
    `[{"type":"image",...}]` content. Pre/post orphan validation.
  - Wired into `strategies::run` (after `SlidingWindow`); shared
    `role`/`block_mut`/`serialized_len` helpers hoisted to
    `strategies/mod.rs` (`pub(crate)`).
  - Tests: 8 lib unit tests (keep-K-recent, ≥90% byte reduction on a
    20×50KB inline session, keep=0, glob `applies_to`, small-text not
    stripped, structured-image-block detection, non-matching-tool no-op,
    base64 threshold); a **Python-parity integration test** (byte-identical
    on all 6 fixtures) + a screenshot-fixture shrink test.
- ✅ **Phase 1 Step 5 done** — SQLite ledger + `trimwire stats`:
  - `src/ledger.rs`: `Ledger` over `Arc<Mutex<Connection>>`; `record()` is
    fire-and-forget via `spawn_blocking` (never blocks/fails the request);
    open-failure → degraded no-op ledger. WAL + `synchronous=NORMAL`,
    single `STRICT` schema (no migrations — unreleased), startup age-prune (`retain_days`,
    default 365), `prefix_hash` (body minus `messages`, sorted-compact
    SHA-256 matching the Python contract), and `report()` over a READ-ONLY
    connection with the §9 cache-prefix stability ratio.
  - `src/config.rs`: `[ledger]` section (`enabled`/`db_path`/`retain_days`).
  - `src/gateway.rs`: computes in/out prefix hashes + reads
    `x-claude-code-session-id` and records each `POST /v1/messages`.
  - `src/main.rs`: `trimwire stats` prints the report (human-readable bytes,
    per-strategy counts, stability ratio with a §9 warning when <100%).
  - Design vetted by a subagent council (proposer/alternatives/decider):
    chose `spawn_blocking`+`Mutex` over a writer-thread+channel; rejected
    `tokio-rusqlite`/`sqlx`/**libSQL** as needless deps; age-prune over
    rollup. See ARCHITECTURE.md decision log.
  - Tests: 6 ledger unit tests (prefix-hash §9 guard, insert+report
    roundtrip incl. an unstable offender, retention prune, degraded/bad
    path) + 2 integration tests (ledger records through the gateway with a
    1.0 stability ratio; ImageStrip strips through the gateway — the
    deferred Step 4 coverage).
- ✅ **Phase 1 Step 6 done** — CLI + installer + an architecture pass:
  - `trimwire run [claude args...]`: start the gateway on a background thread
    (reusing one already listening), launch `claude` with
    `ANTHROPIC_BASE_URL`+`ENABLE_TOOL_SEARCH`, wait, propagate exit code.
    Deliberately NOT `exec` (avoids orphaning the background daemon).
  - `trimwire install`: write a commented starter config if absent (never
    clobbers) + add an idempotent guarded shell-rc block.
  - `trimwire config`: ensure config exists, open in `$EDITOR`.
  - `scripts/install.sh`: OS/arch-detecting bootstrap that downloads the
    binary (Step-7 release assets) and runs `trimwire install`.
  - **Architecture restructure** (maintainer-mandated; LOC figures are
    estimates not caps): HTTP layer grouped into `src/proxy/`
    (gateway/upstream/proxy_stream); CLI split into `src/cli/` (one file per
    subcommand); empty `messages.rs` stub deleted. Decided via a subagent
    council; ceremony (ledger split, lib facade, 1-file folders) skipped.
  - Folded the deferred Step-5 items: CSV-token per-strategy matching
    (boundary-safe `instr`), reopen+prune ledger test, `human_bytes` test,
    "ledger disabled" stats message.
- ✅ **Phase 1 Step 7 done — v0.1.0 release-ready:**
  - `.github/workflows/release.yml`: hand-rolled matrix (linux-x64,
    darwin-x64, darwin-arm64, windows-x64) that builds on a `v*` tag and
    uploads `trimwire-<triple>.tar.gz`/`.zip` assets — the exact names
    `scripts/install.sh` fetches. (Chose a transparent matrix over cargo-dist
    to avoid pinning/regenerating its workflow; revisit if richer installers
    are wanted.)
  - Per-platform **daemon smoke test** added to the cross-platform CI job
    (start daemon → assert it listens → kill), unix + windows.
  - CHANGELOG `[0.1.0]` entry; README install finalized (from-source +
    released paths).
  - Folded the deferred testability items: `tests/cli.rs` drives the built
    binary end-to-end (`run` propagates claude's exit + sets the gateway env;
    `install` writes config+rc idempotently; `stats` disabled message) +
    a `KNOWN_STRATEGIES` guard test.
- ✅ `clippy --all-targets -- -D warnings` / `fmt --check` / full `cargo test`
  (50 lib + 5 bin + 3 cli + 13 integration) all clean; `make phase0`
  (21 Python tests) clean.
- ⏳ **Pre-tag manual checks (need creds / a real tag):** `cargo run -- on`
  against a real Claude Code session (confirm `pruned[...]` + `trimwire stats`),
  and a real `v*` tag to validate `release.yml` + `scripts/install.sh`
  end-to-end. Everything automatable is green in CI.
- ✅ **Post-capstone "make it actually useful" (2026-05-29):** the shipped
  defaults pruned nothing on a normal coding session. Grounded in the
  inspiration tools (opencode-dcp's two default zero-LLM strategies), added
  `CrossTurnDedup` (keep most-recent of identical repeated calls) +
  `FailedInputPurge` (clear old errored-call inputs), both **on by default** —
  these are the real byte-savers and are safe (no unique-result loss). Kept
  `SlidingWindow` conservative (browser tools only). Plus prod hardening: 32 MB
  request-body cap (→413) and SHA-256 verification in the installer. SPIKE §4
  updated with the empirical promotion rationale.
- ✅ **Pulled all of Phase 2 into v0.1.0 (2026-05-29):** `BloatCap` (old-only
  string-result trim — the old-only rule resolves the "no safe threshold" risk,
  on by default) and the `trimwire sweep` on-disk JSONL cleanup (drop empty
  thinking blocks + purge failed-call inputs; atomic, backed up, validated).
  Both ported/grounded in opencode-dcp + cozempic prior art.
- 🎉 **Phase 1 + 2 complete — v0.1.0 ships everything.** All five pruning
  strategies are on by default and the on-disk sweep command is available. v0.2
  is now purely telemetry-driven tuning (thresholds, denylists) — no new
  capabilities pending.

### Status-update discipline

After every Phase 1 step lands, the agent that closed it should update
this section in the same commit (or the next one). A fresh agent
landing in the repo reads this first; staleness here is a silent
onboarding bug.

---

## Phase 0 — Python test harness (~1 evening)

### What this is

A throwaway Python validator that loads real Claude Code session JSONLs
(captured fixtures), applies a Python reference implementation of each
mutation strategy against the `messages[]` array, and asserts the
invariants we'll need to hold in the Rust port.

The harness produces three artefacts:

1. **A fixture suite** (`tests/fixtures/*.json`) — captured (redacted)
   request bodies from real Claude Code sessions. Each fixture is a
   complete `/v1/messages` body the gateway would actually see.
2. **A Python reference implementation** of `SlidingWindow` + `ImageStrip`
   strategies — terse, untested-for-prod, just rigorous enough to lock
   down expected behaviour.
3. **A pytest suite** that runs the strategies against the fixtures and
   asserts the invariants from [`SPIKE.md` §5](SPIKE.md).

### Why this exists (Phase 1 cannot start without it)

The Rust port is mechanical IF the semantics are nailed down first. The
expensive bugs all live in the same place: tool_use ↔ tool_result
pairing across edge cases (parallel tool calls, compact boundaries,
sub-agent results, very long single results, etc.). Catching those in
Python takes minutes; catching them in Rust takes hours of borrow-checker
debugging on top of the actual fix.

Two prior critique agents (Validation 1 + 3 in the spike conversation)
independently flagged this: "fixture-driven snapshot testing is the
discipline that prevents the load-bearing pairing bugs."

### Subtasks

1. **Capture 3 fixture sessions** (~45 min) — three representative
   `/v1/messages` request bodies. The capture rig is committed inline
   so this step doesn't depend on any throwaway POC:

   1.1. Drop the minimal logging proxy into `tests/phase0/capture.py`
   (~50 LOC stdlib Python `http.server`). It listens on
   `127.0.0.1:8765`, reads the request body, redacts headers
   (`Authorization` → `[REDACTED]`), writes the body JSON to
   `tests/fixtures/<name>.json`, and returns 200 OK with an empty
   error body so claude exits cleanly.

   1.2. For each of the three target sessions:
   ```bash
   python3 tests/phase0/capture.py --output tests/fixtures/<name>.json &
   ANTHROPIC_BASE_URL=http://127.0.0.1:8765 claude --print "<prompt>"
   kill %1
   ```
   Run it three times with different setups:
   - `technical.json` — fresh `claude --print "summarise SPIKE.md"` (mix
     of Read, Grep). Expected ~10-30 KB.
   - `screenshot.json` — resume an existing Playwright-heavy session
     with a screenshot prompt. Expected ~500 KB - 2 MB.
   - `failure.json` — fresh session, deliberately trigger an erroring
     Bash command (`claude --print "run: foo-does-not-exist"`). Expected
     ~30-100 KB.

   1.3. **Redact personal content** in each fixture: scan `messages[]`
   for absolute paths matching `/home/<username>/`, real names, real
   email addresses, real project names. Replace with `[REDACTED]`.
   Authorization header is already redacted by the capture proxy.

   1.4. Commit fixtures to `tests/fixtures/`.

2. **Write the Python reference impl** (~250 LOC) — single file
   `tests/phase0/strategies.py`. Two functions:
   ```python
   def apply_sliding_window(messages: list, denylist: set[str],
                            keep_recent_turns: int = 4) -> dict
   def apply_image_strip(messages: list, image_tools: set[str],
                         keep_recent_count: int = 3) -> dict
   ```
   Both return `{"stubbed": int, "elided_bytes": int}` and mutate
   `messages` in place. Logic mirrors the Rust spec from
   [`ARCHITECTURE.md`](ARCHITECTURE.md).

3. **Write the pairing index helper** (~80 LOC) —
   `tests/phase0/pairing.py`. The same `PairingIndex` shape as the Rust
   module ([`SPIKE.md` §5](SPIKE.md)): `build()`, `validate()`. Used by
   both strategy functions and the test invariants.

4. **Write pytest suite** (~400 LOC) —
   `tests/phase0/test_strategies.py`. Five blocker tests:
   - `test_parallel_tool_use_blocks` — single assistant message with
     multiple `tool_use` blocks. Mutating one should not orphan others.
   - `test_long_session_cumulative_correctness` — 100+ turn synthetic
     fixture. Apply strategies, verify no off-by-one in the sliding
     window cutoff.
   - `test_real_world_fixtures` — load each of the 3 captured fixtures,
     run both strategies, assert (a) no orphans, (b) bytes shrink,
     (c) all required envelope fields preserved.
   - `test_huge_single_tool_result` — synthetic fixture with one 1MB
     `tool_result.content`. Verify memory bounded, no escaping issues.
   - `test_compact_boundary_message` — fixture with a `compact_boundary`
     system message. Strategies must not touch it.

5. **Add cache-prefix-hash invariant test** (~60 LOC) —
   `tests/phase0/test_cache_prefix.py`. Hash the serialised request
   prefix (everything *except* the `messages` key in the top-level
   object) with sha256 before + after mutation.

   **Serialisation contract (so Rust matches exactly):** the prefix is
   a top-level dict with `messages` removed; serialised as
   `json.dumps(prefix, sort_keys=True, separators=(',', ':'))`; hashed
   over the UTF-8 bytes via `hashlib.sha256(...).hexdigest()`. Document
   this in `tests/phase0/README.md` so the Rust hash function uses
   identical normalisation.

   Assert:
   - If no strategy fires (denylist empty, no images), hash is
     byte-for-byte identical (cache stays warm).
   - If a strategy fires, hash changes deterministically (same input
     → same hash across two runs).

6. **Wire up `make phase0`** (~10 LOC `Makefile`) — single command to
   run the whole suite: `python -m pytest tests/phase0/ -v`.

### Acceptance criteria

- All 5 blocker tests pass.
- Cache-prefix hash test passes (stability + determinism).
- Fixtures live under `tests/fixtures/*.json`; Python harness code lives
  under `tests/phase0/` (`capture.py`, `strategies.py`, `pairing.py`,
  `test_strategies.py`, `test_cache_prefix.py`, `README.md`).
- Redaction audit on every fixture passes:
  - `grep -iE 'authorization|bearer|sk-ant-|api[_-]key' tests/fixtures/*.json`
    → zero matches
  - `grep -E '/home/[a-z]+/' tests/fixtures/*.json` → zero matches
- `tests/phase0/README.md` documents (a) what + how to run, (b) the
  cache-prefix-hash serialisation contract verbatim (the Rust port must
  match it exactly).
- If any fixture happens to trigger Anthropic's content-filtering when
  replayed in Rust tests, swap that fixture for a hand-crafted synthetic
  one with the same structural pattern (rare but possible).

### Effort

~1 evening (4-5 hours) including fixture capture and redaction.

### Why this exists (and why we kept it)

The Python harness is **not** part of the shipped product. It exists
purely to lock down the mutation semantics before the Rust port. The Rust
port (Phase 1):
- Reuses the same fixture JSONs (in `tests/fixtures/`).
- Reimplements the same invariants in `tests/integration.rs` using `insta`.

**Update:** the original plan was to *delete* `tests/phase0/` once the Rust
tests passed. We instead **retained it as a parity oracle** — `make phase0`
still runs the 21 Python tests, and a Python-parity integration test asserts the
Rust output is byte-identical to the reference on every fixture. It's cheap
insurance against a silent semantic drift in the strategies, so it earns its
keep rather than being thrown away.

---

## Phase 1 — Rust MVP (~7-10 evenings)

The actual product. Builds in strict dependency order; each step
unlocks the next.

### Step 1 — Pass-through gateway (~1.5 evenings)

**Goal:** prove the full HTTP forwarding loop works end-to-end against
real Claude Code with real Pro/Max OAuth, before any mutation logic.

**Implementation notes (these reflect a simulated implementation pass
that landed cleanly; deviate at your own risk):**

- `src/upstream.rs` (~40 LOC realised): build the shared client as
  `hyper_util::client::legacy::Client<HttpsConnector<HttpConnector>, Full<Bytes>>`.
  Hyper 1.x split the pooled client out of the main crate into
  `hyper-util`. The webpki-roots feature on `hyper-rustls` ships ALPN
  for h1/h2. **You must call
  `rustls::crypto::aws_lc_rs::default_provider().install_default()` once
  at startup** or the first handshake panics (see SPIKE §6).
- `src/proxy_stream.rs` (~30 LOC realised): type-erase the upstream
  `Body` via `http_body_util::BodyExt::boxed()`.
- `src/gateway.rs` (~150-200 LOC realised): hyper server via
  `hyper_util::server::conn::auto::Builder` on a `tokio::net::TcpListener`.
  Per request:
  1. Buffer the inbound body fully.
  2. Build the upstream URI by combining `<upstream>` with the inbound
     path + query.
  3. Copy headers EXCEPT the RFC 7230 §6.1 hop-by-hop set
     (`connection`, `keep-alive`, `transfer-encoding`, `te`, `trailer`,
     `proxy-authorization`, `proxy-authenticate`, `upgrade`,
     `proxy-connection`) AND rewrite `host` to the upstream authority.
     Passing through `Host: 127.0.0.1:8765` would cause TLS SNI
     mismatches / HTTP 421.
  4. Send to the shared client; stream the response back unchanged.
  5. Log one line to stderr.
- `src/main.rs`: implement `trimwire on --listen <addr> --upstream <url>`
  with sensible defaults (`127.0.0.1:8765` and `https://api.anthropic.com`).
  (Historical note: config loading was originally deferred to Step 6 but
  landed early in Step 3 — `Config::load()` is wired and the CLI
  `--listen`/`--upstream` flags override it.)

**Acceptance:**
- `cargo run -- on` starts the gateway, listens on 127.0.0.1:8765
  (or whatever `--listen` was passed). If 8765 is already taken on your
  machine, pick a different port for testing.
- In a separate shell:
  ```bash
  ANTHROPIC_BASE_URL=http://127.0.0.1:8765 claude --print "hi"
  ```
  prints a real Anthropic response with no errors.
- Gateway stderr shows one line per request: `[gateway] METHOD path
  in=NB out={NB|stream} status=SSS Nms`. `out=` is the byte count when
  upstream provided `Content-Length`; `out=stream` when it didn't (SSE
  responses don't have one, and adding a counting body adapter is Step
  5 territory — not required for Step 1).
- For SSE smoke: `--print` returns a buffered 200, NOT an SSE stream.
  To actually exercise streaming, run `claude` interactively or with
  `--output-format stream-json` and observe that tokens appear
  incrementally rather than all at once.

**Tests:**
- 1 integration test in `tests/integration.rs` using `wiremock` to stub
  a fake upstream. Verifies request body byte-equality + response stream
  passthrough.

### Step 2 — Pairing module (~1.5 evenings)

**Goal:** build the load-bearing correctness layer before any strategy
can touch `messages[]`. See [`SPIKE.md` §5](SPIKE.md).

**Implementation:**
- `src/pairing.rs` (~150 LOC):
  - `pub struct PairingIndex { uses, results: HashMap<String, (usize, usize)> }`
  - `pub fn build(messages: &[Value]) -> PairingIndex` — single pass.
  - `pub fn validate(&self) -> Result<(), OrphanError>` — every
    `tool_result.tool_use_id` has a matching `tool_use.id` in
    `self.uses`.
- `src/error.rs`: `Error` enum. Step 2 lands only `OrphanResult(String)`
  (the one variant `validate` constructs); `OrphanUse`, `MalformedJson`,
  and upstream variants arrive with the strategy/gateway steps — clippy
  `-D warnings` rejects shipping unused variants ahead of their use.

**Acceptance:**
- Unit tests in `src/pairing.rs` cover:
  - Empty messages → empty index, validate passes.
  - One pair → index size 1+1, validate passes.
  - Parallel tool_use (3 blocks one assistant msg, 3 results) → index
    size 3+3, validate passes.
  - Orphan `tool_result` with no matching `tool_use` → validate returns
    `OrphanResult`.
- All 6 Phase 0 fixtures load + `PairingIndex::build` + `validate`
  succeed without error.

**Tests:** snapshot tests via `insta` over the Phase 0 fixtures.

### Step 3 — `SlidingWindow` strategy (~1.5 evenings)

**Goal:** implement the headline strategy. Use `PairingIndex` for safe
pair drops.

**Implementation:**
- `src/strategies/sliding_window.rs` (~150 LOC):
  - `pub fn apply(messages: &mut Vec<Value>, cfg: &SlidingWindowConfig) -> Result<Stats>`
  - Build pairing index, pre-validate, walk backwards counting
    assistant turns, collect tool_use ids in older turns matching
    denylist, mutate pairs as a unit, post-validate.
- `src/strategies/mod.rs` (~30 LOC): `Strategy` enum + dispatch.
- `src/config.rs` (~80 LOC): figment-based TOML loader.

**Acceptance:**
- Snapshot test passes against `tests/fixtures/`: real fixture →
  apply SlidingWindow → assert the mutated body matches a recorded
  snapshot (use `insta` `assert_json_snapshot!`).
- Phase 0 Python test for SlidingWindow and Rust implementation produce
  identical output on the same fixture (byte-for-byte after sort).
- `cargo run -- on` with `[strategies.sliding_window] enabled = true`
  in `~/.config/trimwire.toml` actually stubs old tool calls on a real
  multi-turn session (verify via gateway log).

### Step 4 — `ImageStrip` strategy (~1 evening)

**Goal:** the highest-byte-savings strategy. Detect base64 image
payloads in `tool_result` blocks; replace with marker.

**Implementation:**
- `src/strategies/image_strip.rs` (~100 LOC):
  - `pub fn apply(messages: &mut Vec<Value>, cfg: &ImageStripConfig) -> Result<Stats>`
  - Walk `tool_result` blocks; for each whose `tool_use_id` resolves
    (via index) to a `tool_use` with a name matching `applies_to_tools`,
    check if content is/contains base64 image data. Replace with
    `"[trimwire: image stripped]"` marker if it's older than
    `keep_recent_count` matching results.

**Acceptance:** *(met — landed in Step 4.)*
- ≥90% byte reduction on a screenshot-heavy session: asserted by the
  `screenshot_heavy_reduction_over_90pct` unit test (20×50 KB inline
  images) — the committed `tests/fixtures/screenshot_heavy.json` is kept
  small (~35 KB, 3×8 KB) on purpose, so the large-scale ≥90% check lives
  in the synthetic unit test while the fixture drives the byte-for-byte
  Python-parity test (`image_strip_matches_python_reference`).
- Pairing invariant still holds after mutation (asserted in both the unit
  and integration tests).

### Step 5 — Ledger + `trimwire stats` (~1 evening)

**Goal:** SQLite-backed savings ledger. Cache-prefix hash recording is
critical (see `ARCHITECTURE.md` and [`SPIKE.md` §9](SPIKE.md)).

**Implementation:**
- `src/ledger.rs` (~150 LOC):
  - `Arc<Mutex<rusqlite::Connection>>` initialised at startup.
  - `pub fn record(session_id, in_bytes, out_bytes, strategies, prefix_hash_in, prefix_hash_out)`.
  - `pub fn report() -> Stats` — aggregate per-day savings, per-strategy
    counts, cache-prefix-hash stability ratio.
- `src/gateway.rs` updated: compute sha256 of the request body prefix
  (everything before `messages[]`) pre- and post-mutation; pass to
  `ledger::record`.
- `src/main.rs`: implement `trimwire stats` to print `report()`.

**Acceptance:**
- After 5 gateway requests, `trimwire stats` shows accurate totals.
- A CI test asserts: identical input → identical `prefix_hash_in` and
  `prefix_hash_out` across runs (cache stability invariant).
- Ledger DB at `~/.trimwire/ledger.db` persists across gateway restarts.

### Step 6 — CLI + installer (~1 evening)

**Goal:** make it actually shippable.

**Implementation:**
- `src/main.rs` finalised: `trimwire run [args...]` spawns the gateway in the
  background and launches `claude` as a child (spawn + wait + propagate exit;
  deliberately NOT `exec`, to avoid orphaning the daemon). `trimwire install`
  writes config + shell rc. `trimwire config` opens the config in `$EDITOR`.
- `scripts/install.sh` (~80 LOC): downloads the binary from
  GitHub Releases, drops into `~/.local/bin`, writes default config,
  edits shell rc with the `ANTHROPIC_BASE_URL` + `ENABLE_TOOL_SEARCH`
  exports.

**Acceptance:**
- Fresh `trimwire install` on a clean machine works end-to-end.
- `trimwire run claude --print "hi"` works in one command.

### Step 7 — Polish + release readiness (~half evening)

- README updated with actual install command (replacing "(planned)").
- CHANGELOG updated with v0.1.0 entry.
- `.github/workflows/release.yml` — a hand-rolled cross-platform matrix
  (linux x64/arm64, macOS x64/arm64, windows x64) on `v*` tag push, chosen
  over cargo-dist to avoid pinning/regenerating its workflow (see the
  rationale in the "Where we are" entry above).
- **Per-platform smoke test** for each cross-compiled binary:
  ```bash
  cargo build --release --target <triple>
  target/<triple>/release/trimwire on &
  sleep 1
  # Gateway is listening
  nc -z 127.0.0.1 8765 || (echo FAIL; exit 1)
  # Clean shutdown
  kill %1
  wait %1 2>/dev/null
  ```
  Required to pass on `x86_64-unknown-linux-gnu`,
  `aarch64-apple-darwin`, `x86_64-pc-windows-msvc` before tagging.

### Phase 1 acceptance criteria (= v0.1.0 release-ready)

- `cargo build --release` → single binary, <12 MB.
- `trimwire run claude --print "hi"` works in a fresh checkout.
- All Phase 0 fixtures pass the Rust integration tests.
- CI green on Linux, macOS, Windows.
- `cargo-deny check` clean.
- README's install command actually works after `release.yml` runs.
- Cache-prefix hash stability CI check passes.

---

## Phase 2 — shipped in v0.1.0

The original Phase 2 was a holding pen for telemetry-driven additions. All of
it was pulled forward into v0.1.0 (see "Where we are right now"), so there are
no deferred capabilities left. Recorded here for history:

### `CrossTurnDedup` strategy — ✅ shipped, on by default

Same tool + identical args → keep only the most-recent result, supersede the
earlier identical ones. `src/strategies/cross_turn_dedup.rs`.

### `FailedInputPurge` strategy — ✅ shipped, on by default

Paired `tool_result.is_error == true` → clear the `tool_use.input` after
`keep_recent_turns` (keep name + id + error text).
`src/strategies/failed_input_purge.rs`.

### `BloatCap` strategy — ✅ shipped, on by default

Old-only smart-trim of a single oversized string `tool_result` (head + tail +
marker). The old-only rule resolves the original "no safe threshold" concern.
`src/strategies/bloat_cap.rs`.

### `trimwire sweep` CLI — ✅ shipped

On-disk JSONL cleanup, ported from `pocs/tier2-sweep.py`: drop empty thinking
blocks + purge failed-call inputs; minimal-diff rewrite, atomic + backed up +
validated. `src/sweep.rs`, `src/cli/sweep.rs`. `--validate-only` checks a file
without modifying it.

### Genuinely deferred to v0.2 (telemetry-driven tuning only)

No new capabilities — only parameter tuning once `trimwire stats` shows real
usage: default thresholds (`bloat_cap` size, `keep_recent_turns`), and which
tools belong on `sliding_window`'s denylist by default.

---

## Cross-cutting practices

These apply at every step in Phase 1.

### Test discipline

- Every strategy needs at least one snapshot test against a fixture.
- Every public function in `pairing.rs` needs unit coverage.
- New CLI behaviour needs an integration test via `wiremock`.

### Commit discipline

- Conventional commits (`feat:`, `fix:`, `refactor:`, `docs:`, `test:`,
  `chore:`, `perf:`, `ci:`, `build:`).
- One commit = one logical change. Rebase WIP commits before merging.
- Lefthook enforces fmt / clippy / test on pre-commit + conventional
  commit format on commit-msg.

### Doc discipline

- Module boundary changes → update `ARCHITECTURE.md`.
- Spike-level decision changes → update `SPIKE.md` with new empirical
  evidence.
- User-facing changes → update `README.md` and `CHANGELOG.md`.

### Subagent usage

Use subagents at these inflection points:

- **Before each Phase 1 step,** validate the planned approach against
  the spike + this doc.
- **After each strategy implementation,** have a subagent review the
  diff for correctness + adherence to the layer rules.
- **Before tagging v0.1.0,** run the full validation panel (5+ agents)
  from the validation session that audited the scaffold.

### When NOT to add a feature

Re-read [`SPIKE.md` §8](SPIKE.md) every time you're tempted to add
something. The build/defer/document split exists for a reason. If you
find yourself implementing T2/T3/T4 functionality in the Rust binary,
**stop** — that's scope creep we deliberately rejected.

---

## How to use this document

- **You're picking up after a break:** read "Where we are right now"
  and find the next unchecked step in Phase 0 or Phase 1.
- **You're reviewing a PR:** check it against the acceptance criteria
  for the step it claims to be implementing.
- **You're considering a v0.2 feature:** find it under "Phase 2" or
  add it there if missing. Do not implement without telemetry support.
