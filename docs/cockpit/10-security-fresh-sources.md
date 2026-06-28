# 10 — Security: Fresh-Sources Addendum (2026 review)

> A second, adversarial research pass against the plan using sources the other docs
> do **not** cite (independent / 2025–2026-dated). It found four things the plan got
> right, and several real gaps. The corrections below are folded back into docs 03,
> 04, 07, 02 by reference, and the cheap ones are already implemented in the POC
> (doc 09). Citations are inline.

## What the plan got right (confirmed)

1. **Treating localhost as hostile is correct.** The Ollama DNS-rebinding CVE
   (CVE-2024-28224) is a localhost daemon with *no* Host pinning getting full remote
   API access from a malicious web page — exactly the attack the Host-pin guards.
   ([NCC Group](https://www.nccgroup.com/research/technical-advisory-ollama-dns-rebinding-attack-cve-2024-28224/))
2. **"Bearer token, not a cookie → CSRF structurally prevented" is the strongest part
   of the design** — a token in an `Authorization` header is not ambient/auto-attached.
   ([OWASP CSRF](https://cheatsheetseries.owasp.org/cheatsheets/Cross-Site_Request_Forgery_Prevention_Cheat_Sheet.html))
3. **Serving the UI same-origin on the admin port** (CORS stays default-deny) is right,
   and `http://localhost` / `127.0.0.0/8` are "potentially trustworthy" secure contexts,
   so the page can use SubtleCrypto and isn't mixed-content-blocked.
   ([W3C Secure Contexts](https://www.w3.org/TR/secure-contexts/))
4. **"Defer remote, stay loopback, no detection-evasion" (R5/R6) is vindicated by 2026
   events** — Anthropic's Jan–Apr 2026 crackdown killed exactly the named tools.
   ([The Register, 2026-02-20](https://www.theregister.com/2026/02/20/anthropic_clarifies_ban_third_party_claude_access/))

## Gaps and corrections

### G1 — Host-pin alone is too thin; add two more independent gates *(highest priority)*

DNS rebinding **defeats Origin checks** (the attacker's page origin is unchanged; only
the resolved IP flips), so Host-pin is the *only* barrier in the original design — and a
single allowlist bug collapses it. A cross-site **simple request** (`GET`/form `POST`)
reaches localhost with **no preflight**, so a *blind write* needs no response-read.

**Mitigations (defense-in-depth, independent of Host-pin correctness):**
- **`Sec-Fetch-Site` enforcement** — browser-set, page-unforgeable; reject anything not
  `same-origin`/`none`. **Implemented in the POC** (`src/admin/mod.rs` `sec_fetch_site_ok`).
- **A custom non-simple header on mutating endpoints** (e.g. `X-Trimwire-Control: 1`) to
  force a CORS preflight that default-deny CORS fails. The POC is read-only + token-gated,
  so this is a **production requirement** for when `POST /service/*` / `PUT /config` land.
- **Order all gates before the token compare and before any side effect** — a rebinding
  caller never reaches auth even if the token check had a bug. **Done in the POC.**

([OWASP CSRF](https://cheatsheetseries.owasp.org/cheatsheets/Cross-Site_Request_Forgery_Prevention_Cheat_Sheet.html),
[Mixmax: CORS-as-CSRF](https://www.mixmax.com/engineering/modern-csrf))

### G2 — Browser Local Network Access does NOT cover localhost→localhost

Chrome shipped **Local Network Access** (LNA, ex-PNA) in **Chrome 142 (2025-10-28)**,
gating *public-site → loopback* behind a permission prompt — good for the public drive-by
case. **But the same announcement says localhost→localhost is NOT yet gated**, and the old
PNA CORS preflight was **withdrawn**. So: a malicious *other localhost app* is unchanged by
LNA, and the plan must **not** lean on browser network-access prompts — the G1 gates carry
the sibling-localhost threat. ([Chrome for Developers](https://developer.chrome.com/blog/local-network-access),
[chromestatus](https://chromestatus.com/feature/5152728072060928))

### G3 — "Token injected into served HTML" is a tradeoff, not "solved"

Any **XSS in the cockpit UI exfiltrates the control token** (then full control). Precedent
is mixed (VS Code's connection-token is widely seen as the weak link). Corrections:
- **Mandatory strict CSP** (ideally nonces, not `'unsafe-inline'`) on the token-bearing
  page. The POC adds a CSP + `X-Frame-Options: DENY` (`src/admin/mod.rs` `build`); production
  should move inline script/style to nonces.
- **Prefer a one-time same-origin handshake → `HttpOnly; SameSite=Strict; Secure` cookie +
  the custom anti-CSRF header** over an in-DOM bearer (cookie unreadable by XSS; header
  unsettable cross-site). `localhost` is a secure context, so `Secure` cookies work.
- **Native/Tauri client reads the token from the OS keychain**, not the webview DOM.

([OWASP CSRF](https://cheatsheetseries.owasp.org/cheatsheets/Cross-Site_Request_Forgery_Prevention_Cheat_Sheet.html),
[OWASP XSS Prevention](https://cheatsheetseries.owasp.org/cheatsheets/Cross_Site_Scripting_Prevention_Cheat_Sheet.html))

### G4 — No public-hosted "controls-your-localhost" cockpit

A hosted SPA (e.g. `cockpit.trimwire.dev`) pointing at `127.0.0.1:8766` is *public→local*
and hits the LNA prompt on every grant (and silently fails if denied). **Commit to
binary-served, same-origin UI as the only browser transport** (docs 02/04 already lean this
way — now with the LNA reason). ([Chrome for Developers](https://developer.chrome.com/blog/local-network-access))

### G5 — Tauri 2 call stands, but the WebKitGTK Linux risk was understated

A **Tauri maintainer** says they "cannot fully recommend Tauri for Linux"; 2025 reports
include rendering glitches and DOM-heavy lag — material for a **data-table-heavy cockpit on
Linux** (trimwire's core audience), not merely cosmetic. The **HTTP-first / PWA hedge is what
makes the bet safe** — keep it load-bearing. Also cite the **Aug 2024 Radically Open Security
audit** (11 High, incl. any-origin IPC + an unauthenticated dev-server disk exposure) as
evidence Tauri's localhost/IPC surface needs deliberate capability locking.
([tauri #13157](https://github.com/tauri-apps/tauri/issues/13157),
[discussion #8524](https://github.com/orgs/tauri-apps/discussions/8524),
[Tauri Security / audit](https://v2.tauri.app/security/))

### G6 — The base passthrough proxy itself is grayer post-Feb-2026 *(highest-impact ToS)*

The 2026-02-20 clarification is broad: OAuth tokens from Claude Free/Pro/Max "in any other
product, tool, or service … is not permitted," with **no carve-out for personal/local
proxies or context-management middleware**, and enforcement is partly behavioral ("unusual
traffic patterns / missing telemetry"). trimwire is a transparent passthrough that *modifies
request bodies* (prunes context) — which changes traffic shape. This risk exists
**independent of the cockpit**; the cockpit's "remote control" framing only amplifies the
*appearance*. Action: **pre-write the affirmative compliance argument** in SECURITY/docs —
"trimwire is invoked by and forwards on behalf of Claude Code; it never extracts or reuses
the token in any other tool" — and keep R6's hard defer-and-re-review gate on remote.
([The Register](https://www.theregister.com/2026/02/20/anthropic_clarifies_ban_third_party_claude_access/),
[VentureBeat](https://venturebeat.com/technology/anthropic-cracks-down-on-unauthorized-claude-usage-by-third-party-harnesses))

## What was implemented in the POC from this pass

- `Sec-Fetch-Site` gate, ordered before the token compare (G1).
- CSP + `X-Frame-Options: DENY` on every response, incl. the token-bearing HTML (G3, partial).
- Authority check uses the request authority (h2 `:authority` ∪ `Host`), default-deny (G1).
- (Already present:) loopback-only bind, content-free responses, token never returned.

## Production follow-ups (tracked for v1, not done in the POC)

- Custom preflight-forcing header on all mutating endpoints (G1).
- CSP nonces instead of `'unsafe-inline'`; consider the HttpOnly-cookie handshake (G3).
- `[admin]` global-only — **done** (`src/config.rs`, with a regression test).
- `db_path` stripped from the control-API `/stats` response — **done** (G-equivalent to the
  content-free red line).
- SECURITY.md: the affirmative ToS compliance statement (G6); note custom upstreams must not
  embed secrets in the URL.

## Net

The framework decision (Tauri 2) and the "defer remote / stay compliant" posture **hold and
are reinforced** by 2026 sources. The security model needed **more than Host-pin** (now three
gates), an honest **token-in-HTML / CSP** treatment, an explicit **LNA-doesn't-help-localhost**
note, and a **base-proxy ToS** acknowledgement. None of these change the destination; they
make the path defensible.
