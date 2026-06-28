# 05 — Multi-platform App (Tauri 2)

> The desktop (and later mobile) app that controls an installed trimwire. Per doc 02 the stack
> is **Tauri 2**, wrapping the **same web frontend** as the browser cockpit (doc 04). This doc
> is the app-specific plan: process model, build/sign/CI, mobile, and what to share vs not.

## 1. Process model — app is a thin shell over the daemon

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

## 5. What to share vs not

**Share:** the entire `dist/` frontend (browser + desktop + mobile); the design tokens; the
typed control-API client; (if the app links trimwire crates) the `config`/`ledger`/`preview`
Rust code.

**Don't share / keep platform-local:** the thin shell (window, tray, native menus, secure-store
access for the remote phase's device token); signing/packaging config.

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
