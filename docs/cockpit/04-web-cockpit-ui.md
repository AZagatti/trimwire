# 04 — Web Cockpit UI ("Flightdeck")

> The browser web UI that opens locally and gives **full control** of the trimwire daemon over
> the control API (doc 03). Local-first for v1; remote-ready by construction.

## 1. Delivery model — served by the binary

A new command serves the cockpit from the same process that owns the control API and the
ledger:

```
trimwire cockpit            # start control API + static UI on loopback   [IMPLEMENTED in the POC]
# Proposed (not in the POC — doc 09 ships only `trimwire cockpit`):
#   trimwire cockpit --no-open   # headless (for the app webview / remote phase)
#   trimwire dashboard --serve   # alias upgrading the static HTML report into the live cockpit
```

- Static assets are **embedded into the Rust binary** at build time (`rust-embed` /
  `include_dir!`) and served from the admin loopback handler alongside `/api/v1/*`. One origin →
  no CORS, no second port, no separate install.
- Bound to `127.0.0.1` only; same-origin → the control API's token/Host-pin auth covers the UI.
- **Fully offline, zero CDN.** Vendor every asset; system font stacks; no analytics, no remote
  calls — consistent with the content-free, transparent ethos.

**Rejected:** a separate Node dev-server (adds a runtime dep to *run* the product — Node/Vite
is a build-time tool only); folding into the public trimwire.dev site (that's a static,
public, community-aggregate site reading the collector — wrong on security, offline, and
lifecycle; the cockpit ships with the binary and must version-match it).

**Migration to remote** is a transport swap only: bind beyond loopback (opt-in, TLS + token),
and the UI's API base URL becomes configurable instead of same-origin. Because the UI talks to
`/api/v1` (never to SQLite directly), nothing in the frontend changes except the fetch base +
auth header.

## 2. Frontend stack — vanilla DOM vs Svelte (OPEN DECISION)

> **Maintainer input:** *"we'll install Svelte+Astro on another flow for page rebrand — I like
> Svelte, but if vanilla is enough for subagents, ok."*

This is a genuine fork, and the site rebrand changes the calculus. Both options are viable;
here's the honest trade-off so the maintainer can decide:

| | **Vanilla DOM + tiny store** | **Svelte** |
|---|---|---|
| Aligns with *current* site (`dashboard.ts`) | ✅ direct lift of existing components | ⚠️ would rebuild them |
| Aligns with *rebranded* site (Svelte+Astro) | ⚠️ diverges from where the site is going | ✅ **shares idioms/components with the new site** |
| Bundle size in the binary | Smallest (a few KB) | Small — Svelte compiles to vanilla JS, no VDOM runtime (~a few KB more) |
| Live updates (counters tick, rows stream) | Manual DOM patches via a ~150-line store | First-class reactivity (`$state`/stores) — *better fit for the live monitor* |
| Config forms / validation / dialogs | Hand-rolled | Componentized, less boilerplate |
| Solo-maintainer ergonomics | Lean but more glue code | More structure; one more compile step (already present once Astro+Svelte lands) |
| Tauri WebView compatibility | Perfect (plain DOM) | Perfect (compiles to plain DOM/JS) |

**Recommendation:** **If the site is being rebranded to Svelte+Astro anyway, build the cockpit
in Svelte too.** The original "reuse the vanilla-DOM components" argument was the main case for
vanilla — and it weakens precisely because the site is moving off vanilla. Svelte keeps the
cockpit and the rebranded site on **one idiom and one shared component library**, gives
first-class reactivity for the live session monitor (the cockpit's most dynamic screen), and
still compiles to a tiny, VDOM-free bundle that satisfies the lightweight ethos and embeds
cleanly in both the browser and the Tauri shell. The **design tokens** (`custom.css`) port
verbatim regardless of framework.

Use **vanilla DOM only if** the cockpit ships *before* the Svelte rebrand and you want zero new
build steps in the interim — in which case the components below are a direct lift from
`dashboard.ts`, and a later port to Svelte is mechanical (the data contracts and tokens don't
change). Either way, **screens consume the control API through a typed `api.*` client + a small
reactive store**, so the rendering layer is swappable without touching data flow.

*(The screen specs below are framework-agnostic — they describe data, actions, and states, not
DOM construction. They hold whether rendered by Svelte components or vanilla render functions.)*

## 3. Information architecture

Single-page app, left-rail nav, persistent global status header. Ten destinations in four
groups:

```
┌──────────────────────────────────────────────────────────────────────┐
│  trimwire · Flightdeck      ● running  default  127.0.0.1:8765  ◑ ⏻   │  ← global header (always live)
├────────────┬─────────────────────────────────────────────────────────┤
│ MONITOR    │  Live · Savings · Strategies · Sessions                  │
│ TUNE       │  Preview/What-if · Config                                │
│ OPERATE    │  Daemon · Sweep · Summarizer                             │
│ SHARE      │  Telemetry                                               │
└────────────┴─────────────────────────────────────────────────────────┘
```

The **global header** carries the four highest-frequency controls — power toggle (on/off),
profile pill, listen address, theme toggle — always live via the control-API health poll / SSE.

## 4. Reuse map (design system + components)

Lift the token layer from `site/src/styles/custom.css` verbatim (teal `#2aa39c`, semantic
good/warn/bad, `0.55rem` radius, 3px cap, dark/light parity, `tabular-nums`, reduced-motion) so
the cockpit and site read as one brand. Component patterns to reuse (as components if Svelte, as
render fns if vanilla): KPI strip, sortable sticky table (`twd` + `Col` model), in-cell mini-bar,
labelled detail bar, expandable detail row, summary tiles, segmented token bar, per-day rows,
honest empty-state, banner, semantic helpers (`fmtBytes` **must stay 1024-based** to match
`stats::human_bytes`), `STRATEGY_LABELS`.

New components: left-rail nav, global live status header, reactive store + SSE/poll-fallback
client + platform adapter, live ticker, validated config form controls, confirm dialog, profile
switcher, preview two-column diff, telemetry payload-preview.

## 5. The ten screens (purpose · data · actions · states)

1. **Live Session Monitor** — watch per-request savings as you work. Data: SSE `request` stream
   + rolling KPIs. Actions: pause, clear, jump to session. Empty: "Idle — run a `claude` turn
   and rows appear live." API: `GET /events` (live) + `GET /stats` (backfill).
2. **Savings** — the `trimwire dashboard` report, live. Data: `stats --json` (totals, per_day,
   cache_stability, response_metrics). Actions: window selector, export the self-contained HTML
   snapshot. Keeps the **"headroom, not dollars"** framing verbatim.
3. **Strategies** — per-strategy bytes/fire-rate for the active profile. Data: `stats`
   per_strategy + `config` profile. Disabled strategies dimmed with "enabled in default only".
   Deep-links each strategy to its Config knob.
4. **Sessions** — `recall` list → per-session per-model drilldown (`stats --session`). Actions:
   sort/filter by model, "Preview this session", copy id (truncated, full on hover).
5. **Preview / What-if** — deterministic default-vs-gentle compare on a captured request.
   **Content-free: shows byte deltas + per-strategy elided bytes + block *types*, never block
   *content*.** Pure/read-only (`POST /preview`). The safe sandbox before any Config change.
6. **Config editor** — validated per-strategy knobs + profile switch, with **per-field source
   attribution** (global/project/env). Mandatory honesty banner: *"Saved. Restart to apply
   (off → on)"* with a one-click restart, since the daemon reads config only at startup.
   Project-scoped `upstream` shown **read-only/ignored** with the token-guard tooltip. Validate
   live (`POST /config/validate`); write (`PUT /config`).
7. **Daemon** — lifecycle + health + doctor. Status card (running/stopped, version, addr,
   service manager), Doctor panel (pass/warn/fail), on/off/restart (confirm-dialog'd; restart
   warns "in-flight requests complete"). Driven by `service` SSE events.
8. **Sweep** — list → **dry-run by default** → review → Apply (confirm) → Undo always offered.
   `trimwire::sweep` library; partial-apply state made explicit with Undo available.
9. **Summarizer** — backend status, benchmark, probe. Copy states the boundary: **never
   originates model calls on the user's Claude OAuth token**. Switch backend routes to Config.
10. **Telemetry / Share** — opt-in with a **payload preview** (show the literal content-free
    k-anon JSON before sending — making the privacy claim *inspectable*, not just asserted),
    k-anon explainer (K=10 stats / K=5 bench), link to the public community dashboard.

Every screen has loading (skeleton + "Connecting to daemon…"), empty (honest CTA, no fake
data), and error (banner naming the failed endpoint + Retry; daemon-down → "Start daemon")
states. If the SSE feed drops, fall back to a 2s `?since=` poll with exponential backoff.

## 6. Brand / naming

Honor the cockpit *metaphor*, avoid marketing-speak. Proposals:

1. **trimwire Flightdeck** *(recommended)* — a flight deck = full instrumentation + control;
   reads as a place, not a pitch; pairs with the "wire" word. Visible name "trimwire ·
   Flightdeck"; command verb `trimwire cockpit` (metaphor in the verb, restrained noun in the
   UI). Wordmark reuses the split-color treatment: **trim**wire (teal) · Flightdeck.
2. **trimwire Console** — maximally plain fallback if any metaphor feels too cute.
3. **trimwire Panel** — instrument-panel nod without the word.

## 7. Shared frontend (build once, ship many)

The cockpit `dist/` *is* the app's UI. The multi-platform app (doc 05) is a thin webview shell
that either embeds the trimwire binary serving loopback and points the webview at it, or loads
the embedded `dist/` and talks to the local control API. The only shell-specific layer is a
tiny **platform adapter** behind the `api.*` client: same-origin fetch (browser/served),
`tauri::invoke`/native bridge (app), or remote base-URL + token (deferred remote). Screens never
know which transport they're on. Because all state flows through `/api/v1` + the event stream,
**the remote-control phase is a transport swap, not a rewrite** — browser cockpit, desktop app,
and future phone client are one frontend with three adapters.
