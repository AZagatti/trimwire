# 03 — Local Control API

> The central missing piece. Today the daemon serves only `GET /healthz` and the
> `/v1/messages` passthrough. The cockpit (browser + app) needs a control plane. This is the
> implementation-ready design, grounded against `src/proxy/{gateway,listener}.rs`,
> `src/cli/{serve,service,stats,recall,preview,sweep}.rs`, and `src/config.rs`.

## TL;DR

1. **Transport:** a **separate admin listener on its own loopback port** (default
   `127.0.0.1:8766`) — *not* extra routes on the gateway (8765), *not* a UDS in v1. New module
   tree `src/admin/`; `gateway.rs` stays mutation-free.
2. **Endpoints:** REST-ish under `/api/v1/...`. Every mutating endpoint is a thin HTTP wrapper
   over the **same library function the CLI already calls** — no logic duplicated. Read
   endpoints reuse the existing `stats`/`recall`/`preview` JSON verbatim.
3. **Live events:** **SSE** at `/api/v1/events` — one content-free broadcast channel. Never
   touches or buffers the proxy stream.
4. **Auth:** loopback-only + a 256-bit **bearer token** at `~/.trimwire/control.token` (0600) +
   an **Origin/Host allowlist** (DNS-rebind guard). Remote is a documented seam, not built.
5. **Config writes:** validate (reuse every existing `config.rs` check) → atomic write (reuse
   `sweep.rs`'s temp+fsync+rename) → apply. **Hot-reload** for strategy/profile/summarizer/share
   knobs via an `ArcSwap<Config>`; **restart-required** for `[server] listen`/`upstream`.
6. **Build:** 5 PRs, each green through the existing CI. The control plane never touches
   `strategies/`/`pairing/`, so the **Python parity oracle is unaffected**.

## 1. Why a separate loopback admin listener

| Option | Verdict |
|---|---|
| (a) Extra routes on gateway `:8765` | **Rejected.** That connection carries the Anthropic OAuth `Bearer` token; co-mingling control verbs there means a routing bug or confused-deputy could expose control to the credential path, and any client pointed at 8765 (all of Claude Code) could hit control verbs. |
| **(b) Separate admin listener `:8766`** | **Chosen.** Physically distinct socket; the proxy path can never route to it; loopback bind; its own token. The OAuth-bearing path and the control path share **zero** surface. |
| (c) Unix domain socket | Best raw security, but **a browser cannot speak AF_UNIX** — the web-UI half is dead on arrival (plus Windows/WSL2 friction). Could be added later for the native app behind the same router. |

Credential isolation is the hard constraint; browser reachability is a hard product
requirement. (b) satisfies both. Loopback-only means no remote caller in v1 — defense *before*
auth runs. Socket activation reuses `listener.rs::obtain()`, generalized to
`obtain_indexed(addr, fd_index, name)` for a second inherited fd / named `Sockets` key.

## 2. Module placement (layer rules respected)

`gateway.rs` must not contain mutation logic, so the control plane gets its own tree:

```
src/admin/
  mod.rs      run(listener, AdminState) -> Result<()>
  router.rs   method+path dispatch (hyper service_fn, like gateway.rs)
  auth.rs     token load/verify + Origin/Host allowlist; Authenticator trait (remote seam)
  state.rs    AdminState { config: ArcSwap<Config>, ledger, events, reprune_cache, paths }
  handlers/   service.rs, config.rs, ledger.rs, preview.rs, sweep.rs, summarizer.rs, share.rs
  events.rs   EventBus (tokio::sync::broadcast) + content-free Event enum
  reload.rs   validate -> atomic write -> ArcSwap publish (hot) / restart signal
```

The only gateway change: read config via `ArcSwap<Config>::load()` **per request** (a *read*
change, within its existing allowance) so a hot-reload publishes new config without a restart.
The admin listener is **opt-in-by-presence**: absent/disabled → the daemon runs exactly as
today (zero new surface for users who never open the cockpit). `ARCHITECTURE.md` gains an
`admin/*` layer-table row and a decision-log entry.

## 3. Endpoint surface (`/api/v1`)

All requests require `Authorization: Bearer <token>` (except `GET /health` and the SSE
`GET /events`, which the browser `EventSource` cannot authenticate — it stays Host/Origin/
Sec-Fetch-guarded and content-free). Errors use the
Anthropic-shaped envelope already in `gateway.rs`. Every endpoint maps to existing code
(Appendix A).

```
# Health / version
GET  /health                         {"ok":true,"version":"x.y.z"}        (unauth ok)
GET  /version                        {version, profile, control_api, upstream}

# Service lifecycle (wraps cli::service)
GET  /service                        ServiceStatus {manager, listening, serving, pid, uptime_secs, ...}
POST /service/on | /off | /restart   202 {action, ok}

# Config (wraps cli::config + config.rs)
GET  /config                         {toml, effective, source_map}
GET  /config/effective               == config show --json
PUT  /config            {toml}        {applied:"hot"|"restart_required", restart_fields?, diff}
POST /config/validate   {toml}        {valid:true} | 422 {valid:false, errors:[...]}

# Profile + per-strategy (convenience over config write; hot-reloadable)
GET/PUT /profile                      {active, available:["default","gentle"]}
GET     /strategies                   {strategies:{name:{enabled, ...knobs}}}
PUT     /strategies/{name}            partial knobs -> {applied:"hot"}

# Ledger (READ-ONLY, content-free; reuse existing --json shapes)
GET  /stats[?since=&until=]          == stats --json
GET  /stats/session/{id|last}        == stats --session --json
GET  /sessions[?query=&limit=]       == recall --json

# Preview / what-if (wraps cli::preview; pure, read-only)
POST /preview        {path, profile, with_summarizer}   == preview --json
GET  /preview/last[?profile=]

# Summarizer (wraps cli::summarizer)
GET  /summarizer                     status
POST /summarizer/probe {runs, confirm}   (paid API -> confirm:true required)

# Sweep (wraps trimwire::sweep)
GET  /sweep                          list candidates
POST /sweep/run     {path, dry_run}
POST /sweep/run-all {dry_run, confirm}
POST /sweep/undo    {path}

# Share opt-in (wraps cli::share)
GET/PUT /share        {enabled}
POST /share/stats | /share/benchmark  {confirm}

# Live events
GET  /events                         text/event-stream (SSE)
```

Paid actions map the CLI's `--yes` to a `"confirm":true` body flag. When the ledger is
disabled, read endpoints return `200 {"available":false,...}` (a valid state, like the CLI).

## 4. Live events — SSE

v1 is one-directional (daemon→UI; control *actions* go through REST), so **SSE** beats
WebSocket (no upgrade/framing/client channel) and poll. A single
`tokio::sync::broadcast::Sender<Event>` lives in `AdminState`. The gateway already computes
everything at ledger-write time; it calls `events.publish(Event::Request{..})` there with the
**same content-free fields** it writes to the ledger — a non-blocking send that never
backpressures and is fully decoupled from consumers. **The proxy SSE body is never tee'd** — we
emit a *summary event after* the response is metered, not the response bytes.

Event types (all content-free): `request` (per-request savings), `strategy_fire` (optional),
`daemon` (reloaded / upstream_error), `summarizer` (outcome), `resync` (on `broadcast` lag → UI
refetches `/stats`). A unit test mirroring `audit.rs`'s `capture_never_leaks_content` asserts
the `Event` serializer can only emit allowlisted fields.

## 5. Auth & security

> **Fresh-sources update (doc 10):** Host-pin alone is necessary-but-not-sufficient.
> Add two more independent gates, ordered **before** the token compare and any side
> effect: (a) **`Sec-Fetch-Site`** enforcement (browser-set, unforgeable — reject
> non-`same-origin`/`none`), and (b) a **custom non-simple header** on mutating
> endpoints (e.g. `X-Trimwire-Control`) to force a CORS preflight that default-deny
> CORS fails. Note Chrome 142 **Local Network Access does NOT cover localhost→localhost**,
> so do not rely on browser prompts for sibling-localhost threats. See doc 10.

- **Loopback-only bind** (`127.0.0.1`/`[::1]`); a non-loopback admin bind is **rejected** at
  bind time (analogous to `config.rs`'s `is_unsafe_listen`).
- **Bearer token:** 256-bit random, `~/.trimwire/control.token` mode `0600` (reuse
  `fsperm.rs`). Required on every request incl. SSE. Constant-time compare. **Not a cookie** →
  not auto-attached cross-site → CSRF is structurally prevented.
- **DNS-rebinding guard:** reject any request whose `Host` isn't the literal loopback authority
  (`127.0.0.1:8766` / `localhost:8766` / `[::1]:8766`) and whose `Origin` (when present) isn't
  allowlisted.
- **Same-origin UI:** the daemon serves the cockpit static bundle under `GET /` on the admin
  port, so the UI never needs cross-origin; CORS stays default-deny. The browser UI is handed
  the token via a same-origin bootstrap injected at page load (never in a URL).
- **`X-Content-Type-Options: nosniff`, `Cache-Control: no-store`** on API responses.

**Remote seam (designed, not built):** `auth.rs` exposes `trait Authenticator { fn
authenticate(&Request) -> Result<Principal> }` with one v1 impl `LoopbackToken`. Non-loopback
bind is hard-rejected unless a strong Authenticator is active — so token-only auth can never be
accidentally exposed to a network. See doc 06 for the full remote requirement list (R1–R10).

## 6. Config write safety

- **Validate before write:** run the *same* figment merge `Config::load` uses, with the
  submitted TOML as the global layer, then every existing `config.rs` check (summarizer
  provider/style/`accept_ratio`, `is_unsafe_listen`, profile/mode recognition, the upstream
  credential-routing guard). Surface the existing user-facing messages verbatim in a `422`.
  **Refactor:** extract `Config::validate()` / `load_from_str()` shared by `load()` and admin.
- **Atomic write:** temp + fsync + rename + dir-fsync — **reuse `sweep.rs`'s** hardened
  primitive. Keep one `.bak` so the UI can offer "revert". Whole-file replace (matches
  `config edit`) avoids partial-merge/comment-loss footguns.
- **Apply (hot-reload vs restart):** the gateway reads an `ArcSwap<Config>`.

| Hot-reloadable (gateway reads per request) | Restart-required |
|---|---|
| `profile`, all `[strategies.*]`, `[reprune]`, `[summarizer]`, `[share]`, `[ledger] retain_days` | `[server] listen` (bind), `[server] upstream` (credential routing — deliberately restart-only), `[ledger] db_path` (open handle) |

- **In-flight requests** snapshot `config.load()` at entry → a mid-flight swap never affects a
  request already past that point; the proxy stream is never interrupted.
- **Reprune cache:** clear (mark-dirty) the reprune `DashMap` when a *pruning-affecting* section
  changes (profile/strategies/reprune/summarizer), so the next turn re-checkpoints under the new
  config. Reprune is self-correcting, so a clear is safe. Non-pruning toggles (`[share]`,
  `retain_days`) don't clear it.
- `[server] upstream` is restart-only even though the gateway reads it per request — changing
  where the OAuth token goes should be a conscious restart, never a config-editor keystroke.

## 7. Phased build plan (mapped to PR/CI)

Each PR is green through `fmt + clippy + test` (MSRV 1.85), the **Python parity oracle**
(untouched — control plane doesn't touch the prune path), cargo-deny/audit, and 3
cross-platform builds. Only new dep: `arc-swap` (tiny); `tokio` broadcast already in tree;
`getrandom` for the token.

1. **PR 1 — Plumbing + read-only API.** `ArcSwap<Config>` refactor (behavior-identical); admin
   listener (loopback + socket-activation); `src/admin/` skeleton + auth (token + Host pin);
   `GET /health`/`/version`/`/service`(status)/`/stats`/`/sessions`/`/config`. Extract the
   `serde_json::Value` builders out of the `stats`/`recall` `println!` wrappers so CLI + HTTP
   share them. Tests: 401/200 auth, Host-pin rejection, JSON shape == CLI `--json`.
2. **PR 2 — Live events (SSE).** `EventBus`; publish content-free `Event::Request` at
   ledger-write time; `GET /events` (keepalive, lagged→resync). Tests: `event_never_leaks_content`,
   SSE framing, proxy stream path unchanged.
3. **PR 3 — Config write + hot-reload.** Extract `Config::validate`; `reload.rs` (validate →
   atomic write → diff → ArcSwap publish / restart-required; clear reprune cache); `PUT /config`,
   `/config/validate`, `/profile`, `/strategies`, `/share`. Tests: bad TOML→422, hot-reload
   visible to next request, restart classification, crash-safe atomic write.
4. **PR 4 — Service + preview/sweep/summarizer/share actions.** `POST /service/{on,off,restart}`;
   `/preview`; `/sweep/*`; `/summarizer/probe`; `/share/*`. Reuse `cli::*` bodies + `trimwire::sweep`.
   Tests: self-`off` 202-then-teardown, `confirm`-gated paid calls, sweep run-all aborts on active file.
5. **PR 5 — Static UI hosting + docs + remote seam.** Serve the cockpit bundle under `GET /`;
   finalize the `Authenticator` seam; update `ARCHITECTURE.md`/`CONFIGURATION.md`/`SECURITY.md`/README;
   add the global-only `[admin]` config section.

```toml
[admin]
enabled = true                 # spawn the control listener (off -> daemon == today)
listen  = "127.0.0.1:8766"     # loopback-only; non-loopback rejected in v1
# token lives in ~/.trimwire/control.token (0600), not in config
```
`[admin]` is **global-only** (never read from a project `./.trimwire.toml`) — a cloned repo
must not be able to open or relocate a control port.

## Appendix A — endpoint → reuse map (no duplicated logic)

| Endpoint | Backed by |
|---|---|
| `/health` `/version` | `gateway::health_response` + `Config` |
| `/service*` | `cli::service::{detect,tcp_open,healthz_ok,on,off}` (+ new `restart`) |
| `/config*` `/profile` `/strategies` | `config::{load_from_str,validate,PROFILES}`, `sweep.rs` atomic-write |
| `/stats*` | `cli::stats` JSON builder (extracted) |
| `/sessions` | `cli::recall` JSON builder (extracted) |
| `/preview*` | `cli::preview` JSON builder (extracted) |
| `/sweep*` | `trimwire::sweep` library module |
| `/summarizer*` | `cli::summarizer` status/probe |
| `/share*` | `cli::share` enable/stats/benchmark |
| `/events` | `EventBus` fed by gateway/summarizer/ledger (content-free) |

## Appendix B — deliberately NOT in v1

No WebSocket (SSE suffices); no UDS (browser can't use it); no remote exposure (loopback hard-
enforced; seam present); no interactive `summarizer setup` raw endpoint (UI drives config
primitives); no partial-merge config PUT; no content in events/queries (structural — the ledger
is content-free).
