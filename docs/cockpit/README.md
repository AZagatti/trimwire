# trimwire Cockpit — Research & Plan

> **Status: PLANNING / RESEARCH + a small working POC.** This folder is a design dossier
> for a proposed control UI for trimwire — a **browser web UI** plus a **multi-platform
> app**, both able to **control an installed trimwire daemon** — plus a deliberately small
> **end-to-end proof of concept** ([doc 09](09-poc.md): `src/admin/`, an embedded web
> cockpit, a `trimwire cockpit` command, and a Tauri `app/` scaffold) that compiles clean,
> passes the full test suite, and runs. It was produced by an overnight multi-agent
> research+design session (a framework council, four specialist designers, and a
> disagree-seeking reviewer), then reconciled into the plan below and a first slice of code.
>
> **Internal doc** — not published to trimwire.dev (not in `site/scripts/sync-docs.mjs`).
> Treat every recommendation as a proposal for maintainer review, not a committed roadmap.

## The ask

> "Plan a new UI for trimwire like a cockpit — a web UI opening in a browser, and a
> multi-platform app, both able to control the trimwire installed."

Maintainer decisions captured up front (these framed the whole investigation):

| Question | Decision |
|---|---|
| Framework for the app | **Agents decide** (run a real trade-off council) |
| Control scope | **Full control** (toggle, profile, config, preview, sweep, sessions, monitoring) |
| Reach (local vs remote) | **Both, phased** — local-first v1; remote control designed-for but deferred |

## The documents

| # | Doc | What it covers |
|---|---|---|
| 01 | [Research & grounding](01-research-grounding.md) | trimwire's *current* control/data/web surfaces the cockpit builds on, and the gaps it must fill |
| 02 | [Framework decision](02-framework-decision.md) | The council verdict (**Tauri 2**), scored trade-offs, runner-up, risks |
| 03 | [Control API](03-control-api.md) | The local control/management HTTP API the daemon must grow (the central missing piece) |
| 04 | [Web cockpit UI](04-web-cockpit-ui.md) | "Flightdeck" — IA, the 10 screens, component reuse, naming, shared frontend |
| 05 | [Multi-platform app](05-multiplatform-app.md) | The Tauri app plan: shells, build/sign/CI, mobile, code-sharing with the web UI |
| 06 | [Remote control](06-remote-control.md) | The deferred remote architecture + the v1 seams that keep it additive |
| 07 | [Security, ToS & red lines](07-security-tos-redlines.md) | The disagree-seeking review, the risk register, and the non-negotiable red lines |
| 08 | [Roadmap](08-roadmap.md) | The phased plan tying it all together, with a security/ToS gate at each step |
| 09 | [Proof of Concept](09-poc.md) | A small, end-to-end vertical slice (control API + auth + web UI + Tauri scaffold) that compiles, tests, and runs |
| 10 | [Security: fresh-sources addendum](10-security-fresh-sources.md) | A second adversarial pass (2026 sources): Host-pin needs 2 more gates, LNA doesn't cover localhost, token-in-HTML tradeoff, base-proxy ToS — with the POC hardening applied |
| 11 | [API stability](11-api-stability.md) | **How the cockpit is guaranteed not to break when CLI commands change** — versioned `/api/v1` contract, two-consumers-one-library, contract tests that fail CI on drift |

## Maintainer constraints (incorporated)

- **No Flutter/Dart.** It was only ever a runner-up flip-case and is now ruled out; the mobile
  fallback is the *same web frontend* (PWA/Capacitor), never a second-language UI (docs 02, 05).
- **No paid app stores for this app.** Mobile ships as an installable **PWA** ($0, both iOS +
  Android) with an optional Android APK via **GitHub Releases / F-Droid** — no Apple Developer /
  Play fee (docs 05 §4, 08 v4).
- **CLI changes must not break the cockpit.** The cockpit speaks only the versioned `/api/v1`
  contract (never the CLI); contract tests fail CI on shape drift (doc 11; tests in `src/admin/`).

## Executive summary

trimwire today is a single static Rust binary that proxies Claude Code traffic and prunes
context. Its only HTTP surface is `GET /healthz` and the `/v1/messages` passthrough;
everything else is CLI (`on`/`off`/`stats`/`config`/`preview`/`sweep`/`dashboard`) over a
**content-free** SQLite ledger. There is already a `trimwire dashboard` command that emits a
self-contained HTML report — proof that a UI over this data is natural and low-risk.

The cockpit is therefore **mostly a presentation + control layer over data and verbs that
already exist** — plus one genuinely new piece of backend: a **local control API**.

**The shape the whole council and specialist set converged on:**

1. **One web frontend, shipped twice.** Build the cockpit UI once (the POC uses vanilla DOM
   reusing the site's existing dashboard components + teal design system; **doc 04 recommends
   Svelte** once the site rebrands to Svelte+Astro — an open decision, not settled). Serve it
   from the trimwire binary on a loopback port for the **browser web UI**, and wrap the *same
   bundle* in a **Tauri 2** shell for the **multi-platform app**. No second UI.
2. **A separate loopback admin listener** (`127.0.0.1:8766`) carries the control API, kept
   physically off the gateway port (`8765`) that transits the Anthropic OAuth token. REST
   verbs wrap existing CLI/lib functions; SSE pushes content-free live events.
3. **Tauri 2** wins the framework council *unanimously* — it's the only option that honors
   trimwire's lightweight single-binary ethos, shares Rust crates with the daemon, and
   reuses the existing web UI. Desktop now; mobile and remote are later phases.
4. **Remote control stays local-only in v1**, but the control API ships ten cheap "seams"
   (auth abstraction, capability scoping, an upstream-credential firewall enforced by a CI
   leak test, Host/Origin validation, …) so the remote phase is *additive*. The recommended
   remote path is **bring-your-own overlay (Tailscale/WireGuard)** — the daemon never widens
   its bind past loopback.

## The reconciliation (read this before the rest)

The plan above is ambitious — and a dedicated **disagree-seeking reviewer** (doc 07) argued,
rigorously and correctly, that the *maximal* version (full remote control + a native
multi-platform app + a general config-write API, all at once) is the single largest
simultaneous expansion of every guardrail trimwire wrote to protect itself, for a solo
maintainer, driven by a metaphor rather than measured user demand.

We did not discard either side. The synthesis:

- **Honor what was asked for** — full control, a multi-platform app, and phased remote — as
  the *destination*.
- **Sequence the risky parts behind the reviewer's red lines** (doc 07, R1–R9) as
  non-negotiable constraints, and behind explicit security/ToS gates (doc 08).
- **Ship the safe, high-value core first** (a local, content-free, binary-served cockpit with
  on/off + profile + bounded config control), prove demand, then climb the ladder
  (native app → LAN/overlay remote → mobile) one gated phase at a time.

So the cockpit the user asked for is the roadmap's endpoint; the disagree-seeking review is
what makes getting there safe. Every doc here is written with that tension explicit.

## Hard invariants that survived every agent (the spine)

- **The Anthropic OAuth token never leaves the host** and is never exposed by the control
  API. `[server] upstream` is never remotely settable. Enforced structurally *and* by a CI
  leak test.
- **Content-free, always.** The ledger holds counts/hashes/timestamps, never message content.
  No cockpit pane ever renders prompts, tool results, or file paths. Preview shows *byte
  deltas and strategy names*, never the bytes.
- **Lightweight stays lightweight.** No heavy runtime deps; JS is a *build-time* tool, the
  shipped artifact is still one static binary. No embedded overlay/relay runtime.
- **ToS-compliant, never evasive.** No detection-evasion, ever. trimwire never originates a
  model call on the subscription token. Being the compliant one is the moat.
- **Local by default.** Loopback-only bind in v1; every step up the reachability ladder is
  off by default and opt-in.

## How this was produced

10 subagents over one session: 3 grounding explorers (control surface, data model, web/brand),
a 3-member framework council + a disagree-seeking pass, and 4 specialist designers
(control-API, web UX, remote architecture, security/ToS). Web-research citations are inline in
docs 02 and 06. The full per-agent reasoning lives in the session transcript; these docs are
the reconciled distillation.
