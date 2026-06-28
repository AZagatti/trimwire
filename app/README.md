# trimwire Flightdeck — desktop app shell (Tauri 2, POC scaffold)

A minimal **Tauri 2** shell that wraps the **same web cockpit** the trimwire binary
serves on its loopback control API. This is the "multi-platform app" layer of the
cockpit POC (see [`../docs/cockpit/05-multiplatform-app.md`](../docs/cockpit/05-multiplatform-app.md)).

> **Scaffold only — not wired into the Rust workspace or CI.** The trimwire repo
> root is a single Rust package (no `[workspace]`), so this nested `src-tauri/`
> crate is ignored by `cargo build`/`fmt`/`clippy` at the root. Building the app
> needs the Tauri toolchain (`npm`, `tauri-cli`) and is intentionally out of scope
> for the daemon's CI. It exists to show the shape.

## What it demonstrates

- **Shape A** from the plan (`05-multiplatform-app.md` §1): the app is a *thin
  webview shell* pointed at the already-running daemon's cockpit
  (`http://127.0.0.1:8766`). No second UI — it loads the exact frontend the
  browser does.
- The Rust core is tiny (Tauri's value): `src-tauri/src/main.rs` is the standard
  builder. The daemon is a *separate process* the app talks to over the loopback
  control API — so there's no sidecar to notarize (sidestepping the worst signing
  pitfalls, per `05-multiplatform-app.md` §3/§6).
- One frontend, many shells: browser PWA + this desktop app + a future mobile/
  remote client are the same bundle with different transports.

## Run (requires the Tauri toolchain)

```bash
# 1. Start the daemon's cockpit (from the repo root):
trimwire cockpit            # serves the control API + web UI on 127.0.0.1:8766

# 2. In another terminal, run the desktop shell:
cd app
npm install
npm run tauri dev           # opens a native window onto the cockpit
```

## Layout

```
app/
  package.json            # tauri-cli dev/build scripts
  dist/index.html         # offline fallback shown if the daemon isn't up yet
  src-tauri/
    Cargo.toml            # independent crate (NOT a root workspace member)
    build.rs              # tauri-build
    tauri.conf.json       # window points at the loopback cockpit (shape A)
    src/main.rs           # standard Tauri 2 builder
```

## Production notes (from the plan, not done here)

- Swap the hardcoded `127.0.0.1:8766` window URL for a discovered/configured base
  URL once the control API's `[admin] listen` is read by the app.
- Offer **shape B** (bundle + manage the daemon as a sidecar) as an opt-in mode.
- Add desktop signing via `tauri-action`; defer mobile/iOS signing to the mobile
  phase. Remote control (phone -> laptop) is the deferred, ToS-gated v3 phase
  (`06-remote-control.md`) — the app just points the same window at a remote URL.
- **Lock Tauri's CSP + capabilities.** `src-tauri/tauri.conf.json` sets `security.csp: null`
  for the scaffold; the Aug-2024 Tauri audit found any-origin IPC + an unauthenticated
  dev-server disk exposure, so deny-by-default capabilities and a real CSP are required
  before this ships. See `../docs/cockpit/10-security-fresh-sources.md` G5.
