# 05 — Multi-platform App: PWA-primary, Tauri-for-desktop

> The "multi-platform app" that controls an installed trimwire. **Strategy (updated):
> PWA-first.** The cockpit is one installable web app — that single artifact *is* the app on
> every platform: desktop browsers, and **iOS + Android via Add-to-Home-Screen**, with **no app
> store and no developer fee**. **Tauri 2** (doc 02) is the optional *desktop convenience wrapper*
> around that same PWA (native window, tray, autostart, manage-the-daemon). Native mobile builds
> are **deliberately de-prioritized** — see §4.

## 0. Why PWA-first (and why native mobile is the wrong bet)

- **Maximal code-sharing:** the PWA *is* the shared artifact — one build, every surface (§6).
  A native mobile app would, at best, wrap the same web view; at worst, fork the UI. PWA gets the
  reach with zero extra UI.
- **No paid stores, on both OSes** (maintainer constraint): add-to-home-screen is free on iOS and
  Android; the binary already serves the manifest + icon (the POC, doc 09).
- **Android is converging toward iOS.** Google's **developer-verification** requirement begins
  **Sept 2026** (Brazil/Indonesia/Singapore/Thailand first, then global): all apps — *including
  sideloaded ones* — must be registered by a verified developer on certified devices
  ([Android Developers Blog](https://android-developers.googleblog.com/2026/03/android-developer-verification-rolling-out-to-all-developers.html),
  [Gadget Hacks](https://android.gadgethacks.com/news/googles-new-android-sideloading-rules-start-august-2026/)).
  So a "free APK sideload" path is becoming as gated as iOS — **the durable store-free path on
  both is the PWA.**
- **The cockpit doesn't need native capabilities.** It's a foreground control panel over a live
  local daemon. The known PWA gaps are all *background* features — no background sync / data-only
  push, ~70–85% iOS push delivery
  ([MagicBell](https://www.magicbell.com/blog/pwa-ios-limitations-safari-support-complete-guide)) —
  none of which a foreground panel uses. (Caveat: EU DMA can downgrade standalone PWAs to a Safari
  tab; acceptable for self-hosted dev tooling.)

## 1. Process model — every shell is a thin client over the daemon

The app does **not** reimplement trimwire. Two viable shapes (pick per-platform; they're not
exclusive):

- **A — Talk to the running daemon.** The app webview loads the shared `dist/` and calls the
  local control API (`127.0.0.1:8766`) exactly as the browser cockpit does. The daemon is the
  one the user already installed/`on`'d. Simplest; the app is "a nicer window onto the daemon."
- **B — Sidecar.** The Tauri core bundles the trimwire binary as a **sidecar**, starting/managing
  it. Good for users who want the app to *be* the install. Tauri's Rust core can also link
  trimwire's crates directly (`config`, `ledger` read, `preview`) for actions that don't need
  the daemon running.

**Recommendation:** ship **A** first (lowest risk, mirrors the browser path), offer **B** as an
opt-in "manage the daemon for me" mode once stable. Either way the **frontend bundle is
identical** — no second UI.

The only app-specific code is the **platform adapter** behind the `api.*` client (doc 04 §7):
same-origin fetch in the browser; a Tauri command/native bridge in the app; remote base-URL +
token in the deferred remote phase. Screens are transport-agnostic.

## 2. Why Tauri here (recap of doc 02, app lens)

- Rust core → can depend on trimwire's own crates and run it as a sidecar; no logic re-impl.
- OS WebView → ~3–15 MB installer, ~30–50 MB RAM; honors the single-binary ethos.
- Same web frontend in browser and app — build once.
- Deny-by-default capability model — right for an app controlling a token-bearing daemon.
- Phasing: desktop now → remote (point the same frontend at a remote URL) → mobile (add Tauri
  iOS/Android targets to the same project).

## 3. Build, signing & CI

Keep the app as a **separate workspace member / sub-crate** so it doesn't entangle the daemon's
MSRV-1.85 / cargo-deny / parity-oracle gates. The frontend build is the *same* one the web UI
needs (doc 04), so it's not net-new tooling.

| Platform | Packaging | Signing | CI note |
|---|---|---|---|
| Linux | AppImage / DEB / RPM | optional | incremental on the existing 3-platform matrix; WebKitGTK is the rendering risk (doc 02 R1) — test with Playwright/WebKit |
| macOS | DMG / `.app` | notarization (Apple Developer acct) | add when shipping publicly; avoid `externalBin` notarization pitfalls by talking to a *separate* daemon process (shape A) rather than bundling, where possible |
| Windows | NSIS / MSI | Authenticode | Tauri GitHub Action handles it |
| iOS / Android | **see §4 — no paid store required** | self-sign / sideload / PWA | **deferred to the mobile phase**; the no-store distribution paths below avoid the App Store / Play fees entirely |

Use Tauri's official `tauri-action` GitHub workflow. **Defer all mobile pipeline cost** to the
mobile phase. Early internal builds can be unsigned/ad-hoc.

## 4. Mobile — without paying Google Play or the App Store

**Maintainer constraint:** *won't pay for Google Play / Apple Developer just for this app.* That
is fine — none of trimwire's other distribution (the binary ships via GitHub releases / crates.io
/ binstall) goes through an app store, and mobile here is a **remote-control panel** (phone →
laptop's trimwire), not a consumer app. Distribution options, by platform, that need **no paid
store account**:

| Platform | No-store option(s) | Cost | Notes |
|---|---|---|---|
| **Android** | (a) **PWA / "Add to Home Screen"** of the cockpit web UI; (b) **direct APK** from GitHub Releases (sideload); (c) **F-Droid** (free, open-source store) | **$0** | Android allows sideloading by default. A Tauri-Android or Capacitor APK built in CI and attached to a release is installable with no Play account. F-Droid is the free "real store" path. |
| **iOS** | (a) **PWA / "Add to Home Screen"** (Safari) — *recommended*; (b) free **sideload** (AltStore/SideStore, 7-day re-sign on a free Apple ID) | **$0** | iOS has no free public store, but an installable PWA covers the remote-control use case with zero fee and zero signing. Native sideload exists but is higher-friction (7-day expiry). |
| **Desktop** | GitHub Releases (DMG/MSI/AppImage), Homebrew, etc. | $0–low | Optional code-signing only when shipping publicly. |

**Recommendation: PWA-first for mobile.** Because the cockpit is one web frontend (doc 04), the
phone "app" can simply be the **installable PWA** — add-to-home-screen on both iOS and Android,
zero store, zero fee, zero second codebase. Layer a **Tauri-Android / Capacitor APK** (distributed
via GitHub Releases + F-Droid) on top *only if* a native Android shell is wanted later. This keeps
the maintainer's "no paid stores" constraint as a first-class design choice, not a compromise.

> Note: a **remote** PWA controlling a laptop over the network interacts with the deferred remote
> architecture (doc 06) and Chrome's Local Network Access (doc 10 G2/G4) — the phone reaches the
> daemon over the user's **overlay/LAN** (a loopback-equivalent origin via the tunnel), not a
> public page hitting `127.0.0.1`. Same-machine use stays trivial; cross-device is the v3 gate.

Tauri 2 mobile itself is *stable-API but maturing* — fine for this webview-driven panel, weak for
deep-native; re-evaluate at the mobile-phase kickoff. **Flutter is not the fallback** (maintainer
preference, and it would mean a second UI); the fallback is always **the same web frontend** as a
PWA or Capacitor wrapper.

## 5. Code & component sharing (app ↔ web cockpit) — the core of the strategy

The cockpit is **one codebase, one build artifact**. The browser PWA, the Tauri desktop shell,
and any future remote/mobile client are the **same `dist/`** plus a thin per-platform adapter.
That is the whole reason this is cheap for a solo maintainer.

**Shared — ~100% of the frontend (single source of truth):**

- **UI components** — KPI cards, sortable/sticky tables, in-cell bars, config forms, dialogs,
  the live-monitor list (doc 04 inventory).
- **Design tokens** — `tokens.css` (teal palette, `0.55rem` radius, dark/light, `tabular-nums`),
  lifted verbatim from the site so cockpit + site read as one brand.
- **The typed API client** for `/api/v1` — the *only* way any shell talks to the daemon (doc 11).
- **The reactive store** + screen/route definitions + formatting/validation helpers
  (`fmtBytes` stays 1024-based to match `stats::human_bytes`).

**Not shared — a thin platform adapter (tens of lines), behind one interface:**

| Concern | Browser **PWA** | **Tauri** desktop | Remote/mobile (deferred) |
|---|---|---|---|
| transport (`api.*`) | same-origin `fetch` / `EventSource` | same-origin to loopback/sidecar, or `invoke` | base URL + device token over TLS/overlay |
| token / secure store | server-injected same-origin bootstrap | **OS keychain** | OS keychain + pairing (doc 06) |
| shell chrome | browser tab / installed PWA window | native window, tray, autostart, manage-daemon | mobile home-screen PWA |

**Adding a platform = writing one small adapter, never a new UI.** Screens are
transport-agnostic; they call typed `api.*` methods and never know which shell they're in.

**Build once, serve/embed everywhere:** a single Vite (or SvelteKit) build emits `dist/` + the
PWA `manifest`/icon (and later a service worker). The trimwire binary `include_str!`/`rust-embed`s
and serves it — **the POC already serves the HTML + `manifest.webmanifest` + `icon.svg` this way**
(doc 09) — and the Tauri shell bundles the *same* `dist/`. One build feeds the browser, the PWA,
and the desktop app.

**An extra sharing layer on desktop only:** because Tauri's core is Rust, the desktop shell can
*additionally* reuse trimwire's own crates (ledger read, config types, the typed control-API
client) — a second layer of sharing no other framework offers (doc 02). The PWA can't link Rust
and doesn't need to: it speaks the same `/api/v1` contract.

**Net:** sharing isn't just "some components" — it's the **entire frontend artifact + the API
contract**, with platform differences quarantined to a tiny adapter, plus a bonus Rust-crate layer
on desktop. That is what lets "one app" mean *every* platform without a second team.

### Offline / service-worker note (additional finding)

A control panel needs the *live* daemon for data, so "offline" is limited by nature: a service
worker can cache the **app shell** (HTML/CSS/JS/manifest/icon) so the window opens instantly and
shows a clean "daemon unreachable" state, but the data panes still require the daemon. The POC
ships the manifest + icon (installable today); the **service worker for app-shell caching is a
tracked follow-up** — it must be served with a permissive-enough CSP (`worker-src 'self'`) and
must never cache `/api/*` responses (always network for live, content-free data).

### Secure-context / remote tie-in (additional finding)

PWA install + service workers require a **secure context** (HTTPS *or* `localhost`). Same-machine
use is `localhost` → secure → works with no TLS (the common case). A **remote** PWA (phone →
laptop) therefore inherits the **v3 TLS/overlay requirement** (doc 06): the daemon must be reached
over an HTTPS overlay/LAN origin, not a public page hitting `127.0.0.1` (which also trips Chrome
Local Network Access, doc 10 G2/G4). So "PWA-first" and "remote is gated on TLS" are consistent,
not in tension.

## 6. Risks specific to the app

- **WebKitGTK drift on Linux** — keep the UI plain; CI against WebKit. (doc 02 R1)
- **Notarization/signing burden** — defer mobile; reuse `tauri-action`; prefer shape A to dodge
  sidecar-notarization bugs.
- **Daemon coupling** — the app is a *client* of the control API; if the daemon is old/missing,
  the app shows a clear "daemon not found / version mismatch" state and offers to install/start
  (shape B) rather than failing opaquely.
- **Scope/regret** — the HTTP-first design means if Tauri ever disappoints, the same frontend
  survives as a PWA. The app is the lowest-regret native option.

## 7. Open questions for the maintainer

- Shape **A vs B** as the default (talk-to-daemon vs sidecar)? (Recommended: A first.)
- Does the desktop app ship in the **same repo** (workspace member) or a sibling repo? (Workspace
  member keeps the shared frontend + Rust client in one place; sibling keeps the daemon repo
  pure. Recommended: workspace member, gated so it doesn't touch the daemon's CI matrix.)
- Is mobile genuinely wanted, or is "multi-platform" satisfied by cross-OS **desktop** + a
  browser PWA on phones? (Decides whether the Apple/Play tax is ever paid — see doc 06.)
