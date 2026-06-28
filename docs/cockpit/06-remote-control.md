# 06 — Remote Control (designed-for, deferred)

> **v1 is local-loopback only.** Remote control (phone → laptop's trimwire) is a later phase.
> This doc designs that phase **and** specifies the cheap seams v1 must include now so remote is
> purely **additive**, never a rewrite.

**Hard invariant (the whole ballgame, repeated everywhere):**
> The Anthropic subscription **OAuth token never leaves the host** and is never exposed by the
> control API. The control surface is **daemon control + content-free stats only**. `[server]
> upstream` is **never remotely settable**. Enforced structurally (layer boundary) *and* by a CI
> leak test.

## 1. Transport options

| Option | What it is | Lightweight | ToS-safe | Solo-maintainer | Secure-by-default | Reach | Verdict |
|---|---|---|---|---|---|---|---|
| **(a) Direct LAN** | daemon binds LAN iface, mDNS `_trimwire._tcp`, self-signed TLS | excellent | neutral | excellent | good *if* opt-in + TLS + pairing | same subnet only | **opt-in primary, same-WiFi** |
| **(b) BYO overlay** (Tailscale/WireGuard) | **daemon stays loopback**; user's overlay forwards to `127.0.0.1` | excellent (ships nothing) | neutral | excellent (no infra) | **best — bind never widens** | full NAT traversal | **primary, cross-network** |
| (c) trimwire-hosted relay | rendezvous/TURN brokers both ends | poor (24/7 infra, cost) | caution | poor | only if zero-knowledge | full | **v3 last resort** |
| (d) Reverse tunnel | daemon dials out to a user-owned endpoint | good | neutral | medium | good | full (outbound) | v3 alternative |

### Recommendation

- **Cross-network primary: (b) bring-your-own overlay.** The daemon stays bound to loopback
  exactly as in v1; the user runs Tailscale/WireGuard and the overlay forwards a private address
  to `127.0.0.1:8766`. trimwire ships **nothing extra** and runs **no infrastructure**, and the
  token-bearing process never widens its bind. Auth is doubled (overlay keys + trimwire pairing
  token). **Do NOT embed `tsnet`/`tailscale-rs`** — too heavy for the single-binary ethos;
  recommend BYO in docs.
- **Same-LAN convenience: (a) Direct LAN, opt-in.** For "phone on the same WiFi," offer explicit
  opt-in LAN exposure with mDNS + TLS + pairing. This is the only mode where trimwire changes its
  bind, so it is **gated hard** (§4, §5).
- **Relay/reverse-tunnel: v3 only**, off by default, **zero-knowledge** (relay brokers an
  encrypted channel; never terminates TLS, never sees the pairing token or any stats). Build only
  if BYO-overlay proves too high-friction.

## 2. Pairing & auth — trust-on-first-use

1. **Host generates a pairing offer** (`trimwire pair` or "Add device" in the local cockpit):
   a short-lived (~120s), single-use, high-entropy pairing code + the daemon's TLS cert
   fingerprint + reachable address, rendered as a **QR** (typeable fallback):
   `trimwire-pair://<addr>?fp=<sha256>&pc=<code>&v=1`.
2. **App scans / types**, connects, **pins the fingerprint** (TOFU — rejects on mismatch
   thereafter), and presents the pairing code over the cert-pinned channel.
3. **Token exchange:** daemon validates the code (single-use, unexpired), issues a **per-device
   bearer token** (256-bit, stored hashed server-side), app stores it in the platform secure
   store (Keychain/Keystore/DPAPI/libsecret). Pairing code is burned.
4. **Subsequent calls** present `Authorization: Bearer <device-token>` over the pinned channel.

This is the **OAuth 2.0 Device Authorization Grant shape** but fully self-contained — the local
daemon *is* the authorization server; no external IdP, no trimwire-hosted auth. TOFU cert
pinning + a pairing code is the documented best practice for local daemons that can't run PKI;
the code defeats a same-LAN attacker who reaches the port but can't see the host's screen.

**Per-device tokens** live in a `devices` table (separate from the content-free ledger):
`device_id, label, token_hash, created_at, last_seen, expires_at, capabilities, revoked_at`.
Revocation is immediate (`trimwire devices revoke <id>`) — the server-side token store *is* the
revocation list (no CRL). Tokens carry `expires_at` (30–90 days, configurable); optionally
issue short-lived (5–60 min) access tokens derived from the device token to bound replay.

## 3. Threat model

| # | Threat | Mitigation |
|---|---|---|
| T1 | OAuth token theft | token never readable via control API (structural); `upstream` not remotely settable; control responses schema-tested to exclude it |
| T2 | Daemon hijack to redirect upstream | `upstream` immutable from any remote/control path; `config:write` hard deny-lists `server.upstream`; re-pointing stays a local-file + restart op |
| T3 | MITM on control channel | TLS required for any non-loopback bind; cert-fingerprint pinning (TOFU); mismatch = hard reject |
| T4 | Replay | short-lived derived access tokens; TLS; optional per-request nonce/timestamp on writes |
| T5 | Malicious LAN peer | bind opt-in (off by default); pairing code required to mint a token; rate-limit + lockout |
| T6 | Exposed-port scanning | default loopback (unreachable); when exposed, TLS + valid token (scanner gets 401); `doctor` warns on non-loopback bind |
| T7 | DNS rebinding / cross-origin (browser cockpit) | strict `Host`/`Origin` allowlist; same-origin token, no ambient auth |
| T8 | Relay compromise (v3) | zero-knowledge relay — never sees plaintext/token/stats; can DoS but not read |
| T9 | Pairing-code brute force | ≥40-bit code, ~120s TTL, single-use, attempt lockout, valid only on the pinned channel |
| T10 | Stolen/lost device | per-device revocation; expiry; secure-store at rest; remote revoke from host |
| T11 | Scope confusion (read token used for control) | capability scope checked per-endpoint, deny-by-default |
| T12 | Detection-evasion creep | **forbidden** — remote control is faithful proxy control only; no obfuscation, no upstream spoofing |

## 4. What v1 MUST NOT preclude — requirements for the v1 control API (doc 03)

Each is cheap now, expensive to retrofit. The control-API design (doc 03) already commits to
these:

- **R1 — Auth-token abstraction.** Every request flows through an `Authenticator` trait;
  handlers receive an authenticated `Principal`, never raw trust. v1 impl = `LoopbackToken`.
- **R2 — Bind-address config + explicit opt-in.** Bind is a config field; v1 ships loopback-only
  and **refuses** any non-loopback bind (error points at the future opt-in).
- **R3 — TLS-readiness.** The listener is constructed through an abstraction that can wrap a
  rustls acceptor (config-driven branch). Cert/key paths reserved in config (unused in v1).
- **R4 — Per-request device identity.** Handlers operate on `Principal { device_id, scopes,
  channel }`, even when v1 fills a synthetic "local" principal.
- **R5 — Capability scoping.** Define caps now (`stats:read`, `ledger:read`, `service:toggle`,
  `profile:switch`, `config:read`, `config:write` [upstream-excluded], `sweep:run`,
  `summarizer:manage`); each endpoint declares its required cap; deny-by-default.
- **R6 — Upstream-credential firewall.** Control handlers live in a module that *cannot* import
  the proxy credential; `config:write` hard deny-lists `server.upstream`; **a CI test asserts no
  control response field derives from the upstream credential.**
- **R7 — Host/Origin validation hook** (defeats DNS-rebinding) — built now, even loopback.
- **R8 — Versioned, content-free, token-free API contract** (`/api/v1`); pairing/`devices`
  endpoints stubbed/absent in v1 but the version prefix + content-free discipline hold.
- **R9 — Rate-limit/lockout seam** on the auth layer (no-op/generous in v1).
- **R10 — Secure local-credential storage** (`0600` token in `~/.trimwire/`) — the convention the
  `devices` table extends.

## 5. Phasing & the gate at each step

| Phase | Transport / bind | Auth | Gate to ship |
|---|---|---|---|
| **v1 — local loopback** | `127.0.0.1` only; refuses non-loopback | local principal (R1) + `0600` token (R10) | seams R1–R10 present & tested; **token-leak CI test is the merge gate** (R6) |
| **v2a — Direct LAN (opt-in)** | LAN bind + mDNS + self-signed TLS | per-device tokens via QR/pairing TOFU; scopes (R5); revocation/expiry | off by default; refuses to expose without TLS+pairing; Host/Origin active; pairing rate-limit; `doctor` warns |
| **v2b — BYO overlay (recommended)** | **daemon stays loopback**; user's overlay forwards | same per-device-token layer (defense in depth atop overlay keys) | bind unchanged from v1 (best posture); docs only; pairing still required |
| **v3 — relay / reverse tunnel** | zero-knowledge rendezvous OR user-owned tunnel; off by default | E2E; per-device tokens unchanged; relay token/content-blind | build only if BYO friction demands it; provably content/token-blind; abuse/DoS plan |

**Gate principle:** every phase is off by default, requires explicit opt-in, and cannot regress
the token invariant. Each step up the ladder widens *reachability*, never *trust* — the
auth/scope/credential model is identical from v2 onward; only the transport changes.

## Sources

Tailscale `tsnet` docs + `tailscale-rs` preview; mDNS/DNS-SD references; OAuth 2.0 Device
Authorization Flow (Descope) + IETF cross-device security BCP; self-signed TLS client-auth /
fingerprint-pinning patterns; bearer-token best-practice guides. Full URLs in the session
transcript.
