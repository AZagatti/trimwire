# 02 — Framework Decision: the app stack

> The maintainer asked the agents to decide. We ran a **3-member council** (independent agents,
> same question, web-research-grounded for the 2026 state of each framework) plus a
> disagree-seeking pass. **The council was unanimous.**

## Verdict: **Tauri 2** — build one web frontend, ship it as a browser PWA *and* a Tauri shell

All three council members independently scored **Tauri 2** the winner (weighted ~4.5–4.6 / 5),
named **PWA-only / shared web frontend** the runner-up, and **Flutter** the flip-case if mobile
ever becomes a v1 priority. They independently derived the *same* architecture:

> Build the cockpit UI **once** as a conservative web app. Serve it from the trimwire binary on
> loopback for the **browser web UI**. Wrap the **same bundle** in a **Tauri 2** shell — whose
> core is Rust and can link trimwire's own crates / run the daemon as a sidecar — for the
> **multi-platform app**. The daemon exposes a plain HTTP control API, so the browser, the
> desktop app, and a future phone client are *one frontend with three transports*.

## Scored trade-off (representative; the three councils agreed within rounding)

Weights reflect the brief's priorities. Scores 1–5 (5 = best fit).

| Criterion | Weight | **Tauri 2** | Electron | Flutter | Wails v3 | PWA-only |
|---|---:|:--:|:--:|:--:|:--:|:--:|
| Lightweight / single-binary ethos | 0.20 | **5** | 1 | 3 | 5 | 5 |
| Rust-native synergy (share crates / API client) | 0.20 | **5** | 1 | 3 | 1 | 1 |
| Code-sharing with the vanilla-DOM web UI | 0.16 | **5** | 5 | 1 | 5 | 5 |
| Desktop now (mac/win/linux) | 0.14 | 4 | 5 | 5 | 4 | 4 |
| Mobile later (iOS/Android) | 0.12 | 3 | 1 | **5** | 1 | 3 |
| Security / attack surface | 0.10 | **5** | 2 | 4 | 4 | 4 |
| Build/CI + solo-maintainer burden | 0.08 | 3 | 4 | 3 | 3 | **5** |
| **Weighted total** | 1.00 | **≈4.55** | ≈2.5 | ≈3.2 | ≈3.5 | ≈3.7 |

## Why Tauri 2 wins (the decisive axes)

- **It's the only Rust-native option.** trimwire is Rust; a Tauri core is Rust. The cockpit can
  depend on trimwire's **own crates** — the ledger reader, the `preview` engine, the figment
  config types, a typed control-API client — instead of reimplementing them, and can run
  trimwire as a **sidecar**. No other framework shares code at the Rust level (Flutter bridges
  via FFI, Electron/Wails/PWA share nothing in Rust).
- **It honors the single-binary ethos.** Tauri uses the OS WebView: ~3–15 MB installers,
  ~30–50 MB RAM, vs Electron's 80–150 MB / 200–300 MB. A context-*pruning* tool shipping a
  150 MB Chromium runtime would be self-parody — Electron is disqualified on principle.
- **One frontend powers both web and app.** The web UI is already Astro + vanilla DOM; that
  exact bundle drops into a Tauri WebView. Build once, ship twice. Flutter/KMP throw the web UI
  away and rebuild it in Dart/Compose — two frontends for a solo maintainer.
- **Best security posture.** Tauri's capability model is deny-by-default — the right default for
  a tool controlling a privileged, token-bearing local daemon.
- **The phasing maps cleanly.** v1: Tauri desktop hitting `127.0.0.1`. Remote: same frontend,
  remote base URL + token. Mobile: add Tauri's iOS/Android targets to the same project.

## Runner-up: PWA-only — and the recommended hybrid

A PWA scores highest on *burden* (zero native build, the web UI *is* the product) and is the
runner-up. **Pick PWA-only over Tauri if** mobile is soft/unlikely or solo bandwidth can't
absorb a native release pipeline this quarter.

But the councils' actual recommendation is **not either/or** — it's **both**: the same web
frontend is *already* a browser PWA (the "web UI" deliverable) **and** the content of a Tauri
shell (the "app" deliverable). PWA-now and Tauri-later is a coherent sequence, not a rewrite,
because both start from one clean web frontend talking to the `/api/v1` control surface.

**Flutter** is the only option that beats Tauri on *mobile today* and shares Rust logic via
`flutter_rust_bridge` — pick it **only if** mobile becomes v1-critical *and* the web and app
UIs are allowed to diverge. Under the brief (mobile deferred, share the web UI), neither holds.

## Top risks of Tauri 2 + mitigations

| # | Risk | Mitigation |
|---|---|---|
| 1 | **Linux WebKitGTK rendering drift / instability** (the strongest counter; a Tauri maintainer says they "cannot fully recommend Tauri for Linux"). **Fresh-sources update (doc 10): materially understated for a data-table-heavy cockpit** — 2025 reports include rendering glitches + DOM-heavy lag, not just cosmetic drift. | Keep the UI deliberately plain; run **Playwright against WebKit**. **The real mitigation is the HTTP-first / PWA hedge** (the same frontend survives as a PWA if the shell disappoints) — keep it load-bearing. Also lock Tauri capabilities deliberately (the Aug-2024 audit found any-origin IPC + an unauthenticated dev-server disk exposure). See doc 10 G5. |
| 2 | **Tauri 2 mobile is "a foundation, not finished"** — plugin parity gaps | Mobile is *deferred*; the mobile client is a webview control panel (Tauri's strong case), not deep-native. Re-evaluate at mobile-phase kickoff; Capacitor-wrapping the same web UI is a cheap fallback. |
| 3 | **iOS codesigning / multi-toolchain CI burden** for a solo maintainer | Don't pay it until the mobile phase. Desktop signing is incremental on the existing 3-platform CI; use Tauri's GitHub Action. |
| 4 | **A JS/Node build chain enters a pure-Rust repo** | Keep the cockpit a separate workspace member; the frontend build is the *same* one the web UI already needs — not net-new tooling. The daemon's MSRV/cargo-deny gates stay isolated. |
| 5 | **Bus factor on a younger framework** | The HTTP-first design means the web/PWA UI keeps working even if the native shell is abandoned — low regret. |

## A note on the unanimity

Three independent agents agreeing is a groupthink flag (per the repo's own working
conventions). We treated it as such — but the convergence is *forced by a hard constraint*: the
brief requires reusing the existing vanilla-DOM web UI **and** honoring the Rust single-binary
ethos, and exactly one framework maxes both. The disagree-seeking review (doc 07) was pointed at
the broader *concept* (should we build this at all, and how much), not at re-litigating the
framework — which is where the real dissent lives.

## Sources

Tauri 2.0 stable & mobile/sidecar docs (v2.tauri.app); Tauri-vs-Electron 2026 comparisons
(pkgpulse, tech-insider, buildmvpfast); WebKitGTK instability discussion (tauri-apps GitHub
#8524); Tauri iOS feedback (#10197); Playwright/WebKit pitfall notes; `flutter_rust_bridge`
(pub.dev/GitHub); Wails v3 alpha status; KMP-2026 readiness. Full URLs are in the session
transcript.
