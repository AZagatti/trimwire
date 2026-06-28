# 08 — Roadmap

> The phased plan that ties docs 03–07 together. Each phase is **off by default until the prior
> one is proven**, ships through the existing PR/CI flow, and is bounded by the red lines in
> [doc 07](07-security-tos-redlines.md). The destination is the cockpit the maintainer asked for
> (full control, multi-platform app, phased remote); the sequencing is what makes getting there
> safe.

## Phase map at a glance

| Phase | Deliverable | Reach | Risk | Gate to start |
|---|---|---|---|---|
| **v0** | Read-only cockpit (live, served by the binary) | local loopback | very low | none — it's a reskin of `dashboard` |
| **v1** | Full **local** control (the control API + Flightdeck UI) | local loopback | low–med | v0 shipped; red lines R1–R9 enforced; token-leak CI test green |
| **v2** | **Desktop app** (Tauri) wrapping the same UI | local | med | v1 shipped; app is a thin client of the v1 API |
| **v3** | **Remote** control (BYO overlay + opt-in LAN) | cross-network, opt-in | high | **written ToS re-review (R6)**; remote seams from v1 in place; pairing/devices built |
| **v4** | **Mobile** app (remote controller) | remote | high | v3 shipped; Tauri-mobile (or Capacitor) maturity check; Apple/Play pipeline |

## v0 — Read-only cockpit (the cheap, near-free win)

**What:** upgrade `trimwire dashboard` into a live, locally-served page. Reuse `dashboard.rs` +
`dashboard_template.html` + the site's dashboard components + design tokens. Data: ledger +
`stats`/`recall`/`preview --json`. **No control actions**, **no remote**, content-free.

**Why first:** it's ~90% of the perceived value at ~0% new risk, validates the UI/design, and
ships before any of the contentious surface exists. Maps to the disagree-seeking review's
"safest minimal" read view (doc 07).

**Backend need:** minimal — could even be the static HTML report plus a tiny read-only loopback
serve. If building toward v1, start the `admin/` read endpoints here (control-API PR 1, read-only
subset).

## v1 — Full local control

**What:** the control API (doc 03, PRs 1–5) + the Flightdeck UI (doc 04) with full control:
on/off, profile switch, validated config + per-strategy knobs (restart-or-hot-reload), preview,
sweep (dry-run-first), summarizer, share opt-in, live SSE monitor. **Loopback only.**

**Non-negotiables (doc 07):** separate `127.0.0.1:8766` admin listener (R2, R9); bearer token +
Host/Origin guard (R4); `config:write` deny-lists `server.upstream`/summarizer URLs (R1, R3);
content-free panes & data sources only (R7, R8); no detection-evasion (R5). **The token-leak CI
test (doc 06 R6) is the merge gate.**

**Frontend stack decision:** vanilla DOM vs **Svelte** (doc 04 §2). Given the site is being
rebranded to Svelte+Astro, the recommendation is to build the cockpit in **Svelte** so it shares
one idiom + component library with the new site — *unless* the cockpit ships before the rebrand,
in which case start vanilla (a direct lift) and port later (mechanical; data contracts + tokens
don't change). **Open for maintainer confirmation.**

**Exit:** people actually use it locally. That usage is the evidence (AGENTS.md: "measure, don't
guess") that justifies climbing to v2+.

## v2 — Desktop app (Tauri)

**What:** a Tauri 2 desktop shell wrapping the **same** web bundle (doc 05). Shape **A**
(talk-to-running-daemon) first; optional sidecar mode (B) later. Adds desktop packaging/signing
to the existing 3-platform CI via `tauri-action`. **Still local** — the app talks to
`127.0.0.1:8766`.

**Why after v1:** the app is a thin client of the v1 API; building it earlier would mean building
the API and a native shell at once. The HTTP-first design means v2 is low-regret (the PWA still
works if Tauri disappoints).

## v3 — Remote control (the gated leap)

**What:** reach a daemon on another machine. **Primary: BYO overlay (Tailscale/WireGuard)** — the
daemon stays loopback, the user's overlay forwards (trimwire ships no infra). **Plus opt-in
Direct LAN** (mDNS + self-signed TLS + QR/pairing-code TOFU). Per-device tokens, capability
scopes, revocation/expiry (doc 06).

**Hard gate (doc 07 R6):** a **written ToS re-review before any exposure/relay code.** "Remote
control of the process holding your Claude OAuth token" is the feature most likely to spend
trimwire's compliance moat — it must be deliberate, off by default, and never expose the token or
make `upstream` remotely settable. The v1 control API already ships the additive seams (doc 06
R1–R10) so this is new endpoints (`/pair`, `/devices`) + a transport, not a v2 API.

**Relay (c)/reverse-tunnel (d) are v3.x last resorts** — zero-knowledge, off by default, built
only if BYO-overlay friction demands it.

## v4 — Mobile

**What:** a phone client (remote controller) — the same web frontend in Tauri-mobile or, if it
still lags, Capacitor. Depends on v3 (remote) being in place. The heaviest CI/signing tax
(App Store / Play) — pay it only if mobile is genuinely wanted vs "cross-OS desktop + a browser
PWA on phones" already satisfying "multi-platform" (doc 05 §7 open question).

## Cross-cutting workstreams

- **Docs:** update `ARCHITECTURE.md` (new `admin/` module + layer rows + decision log),
  `CONFIGURATION.md` (`[admin]`), `SECURITY.md` (loopback+token+Host-pin model, content-free
  events) as the relevant phase lands.
- **Tests:** content-free event test (mirrors `audit.rs`); token-leak CI test; JSON-shape
  snapshots equal to the CLI `--json`; atomic-write crash-safety; hot-reload visibility.
- **Parity oracle:** untouched throughout — the control plane never touches `strategies/` or
  `pairing/`.

## Decision log (open questions for the maintainer)

1. **Frontend stack:** Svelte (recommended given the site rebrand) vs vanilla-now-port-later?
2. **How far is v1's config control?** Full validated editor (doc 03) vs the disagree-seeking
   review's bounded "on/off + profile + whitelisted knobs" (doc 07)? Recommendation: ship the
   bounded version in v1, expand to the full editor once the write path + restart UX are proven.
3. **App home:** workspace member vs sibling repo (doc 05 §7)?
4. **Is mobile (v4) actually wanted,** or is multi-platform satisfied by desktop + PWA?
5. **Visible name:** Flightdeck / Console / Panel (doc 04 §6)?

These don't block v0/v1 — they shape v2+. None requires answering tonight.
