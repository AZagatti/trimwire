# 07 — Security, ToS & Red Lines (the disagree-seeking review)

> This doc is deliberately adversarial. While the other docs design an exciting cockpit, this
> one steelmans the case **against** building it — or against building parts of it — and where
> it drifts from trimwire's identity. It is anchored in trimwire's *own* code, not speculation.
> **The red lines here are non-negotiable constraints on everything else in this folder.**

## The dissent in five sentences

1. trimwire's whole value is "one small static binary, no heavy runtime deps, transparent and
   ToS-compliant." A full-control local API + remote stack + native multi-platform app is the
   single biggest scope expansion the project could make — driven by a *metaphor* ("cockpit"),
   not measured user demand.
2. The codebase already encodes, in `config.rs`, that **the most dangerous thing in the system is
   "where does the Bearer token get sent"** — a mutating control API is a new write path into
   exactly that surface, and a remote port is a network-reachable handle on a process holding a
   Claude OAuth token.
3. "Remote control of your Claude proxy" reads, to an enforcement team, like the redistributed-
   token pattern that **already got OpenClaw/OpenCode/Cline killed in 2026** — even if
   implemented innocently, it moves trimwire from *passive-gray* toward something that *looks*
   active.
4. The content-free guarantee holds today because the ledger *literally cannot* hold content;
   several proposed panes ("preview what's pruned," "inspect sessions") create pressure to read
   **raw transcripts** into a UI — the one move that breaks it.
5. The existing `trimwire dashboard` HTML + statusline + CLI already deliver ~90% of the
   cockpit's read value at ~0% of its risk.

## Risk register

| Risk | Likelihood | Severity | Mitigation |
|---|---|---|---|
| Control API exposes a config-write path that can redirect the OAuth token (`upstream`/summarizer URL) | Med | **Critical** | R1: forbid those keys from the API; reuse the existing global-only guard + its regression test |
| Localhost control port hit by a malicious web page (CSRF → config write / info disclosure) | **High** if unguarded | Critical | R4: per-install bearer secret + Host/Origin checks; loopback bind |
| "Remote control of your Claude proxy" pattern-matches enforcement → ToS/account action | Med | **Critical** | R6: defer + gate on ToS re-review; v1 loopback-only; keep the "compliant" brand |
| Preview/inspect pane renders transcript content → breaks content-free guarantee | **High** (UX pull) | High | R7+R8: counts/structure only; ledger-and-`--json`-only data sources; never read live transcripts |
| Native multi-platform + mobile build/signing burden sinks solo velocity | High | High | ship a PWA; no native app / mobile in the minimal v1 |
| JS SPA + node_modules reverses the lightweight ethos / adds supply-chain surface | Med | Med | reuse the existing dashboard front-end machinery; no heavy SPA framework; JS is build-time only |
| Remote relay creates recurring infra cost + uptime/abuse obligation | Med (if built) | High | don't build it in v1; BYO overlay (doc 06) needs no infra |
| Mutation logic creeps into `gateway.rs`, eroding layer discipline | Med | Med | R9: separate `admin/` module/router; gateway stays non-mutating |
| Scope absorbs bandwidth AGENTS.md reserves for CODE > DX | High | Med | decouple the cheap read view from the expensive control/remote/app; ship the cheap half first |

## RED LINES (must not be crossed — bind every other doc)

- **R1** — The control API can **never** write `server.upstream` or any summarizer
  `base_url`/`full_url`. Route any config write through the same global-only guard, or forbid
  those keys outright.
- **R2** — The control API **stays bound to `127.0.0.1`** in v1; **no** `0.0.0.0`/LAN bind option
  ships in v1.
- **R3** — The API **never** returns the OAuth token, `Authorization` header, or any derived
  value, and **never** exposes a "test/ping upstream" endpoint that originates a call on the
  subscription token.
- **R4** — The API **defends against drive-by browser requests**: a per-install bearer secret +
  Host/Origin checks on every call. A localhost port is **not** a security boundary against local
  web pages.
- **R5** — **No detection-evasion, ever** — no client-fingerprint spoofing, header-order mimicry,
  or proxy-hiding.
- **R6** — **Remote control is deferred AND gated on an explicit, written ToS re-review** before
  any relay/exposure code. "Do not design the relay until we've re-checked enforcement posture."
  (Designing the *API shape* for remote-additivity, per doc 06 R1–R10, is fine; standing up
  network infrastructure is not.)
- **R7** — **No UI pane renders message content, prompts, tool-result bodies, or file paths.**
  Preview shows counts, byte deltas, strategy names, block *types* — never the bytes.
- **R8** — The API's data sources are **the ledger + content-free `--json` outputs only.** No
  reading of `~/.claude/**` transcript bodies into any UI surface.
- **R9** — The control API lives in its **own module/router**, not inside `gateway.rs` (which the
  layer rules say must not contain mutation). Don't erode the layer discipline that keeps the
  core testable.

## The safest minimal viable cockpit (the dissent's counter-proposal)

If trimwire wanted the *smallest* responsible step:

1. A **local-only, read-mostly page served by the binary**, reusing `dashboard.rs` +
   `dashboard_template.html`. Data: ledger + content-free `--json`. ~90% of the value, ~0% new
   risk.
2. **Control limited to three verbs:** on/off; switch profile (`default`⇄`gentle`); a
   **whitelisted** subset of strategy knobs applied via restart — **never** `upstream`/summarizer
   endpoints. No general config-write; no sweep-from-UI in the minimal cut (sweep mutates on-disk
   transcripts — keep it auditable in the CLI initially).
3. **Ship as a PWA, not a native app.** Zero native build matrix, zero signing, zero app-store
   tax. No mobile in the minimal v1.
4. **No remote, at all, in the minimal v1.** Loopback only; design the data model for a future
   remote phase but don't build (or design the relay for) it until the ToS re-review (R6).

## How the rest of this folder reconciles with the dissent

The plan does **not** ship the maximal version all at once. The reconciliation (see
[README](README.md) and [roadmap](08-roadmap.md)):

- The **roadmap's v0/v1 ≈ this minimal proposal** — local, content-free, binary-served, bounded
  control. That's what gets built and proven first.
- **Full config control, the native app, LAN/overlay remote, and mobile are later phases**, each
  behind the red lines above and an explicit security/ToS gate.
- The control-API design (doc 03) and the remote design (doc 06) already adopt R1–R9 as
  load-bearing requirements — the separate loopback admin listener, the upstream-credential
  firewall with a CI leak test, Host/Origin validation, content-free events, and the
  "designed-for-but-deferred" remote seams are direct responses to this review.

So this doc isn't an objection the plan ignores — it's the **constraint system the plan is built
inside.** If a future change conflicts with a red line here, the red line wins, or the change
needs explicit maintainer + ToS sign-off.
