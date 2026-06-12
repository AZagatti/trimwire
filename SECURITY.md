# Security policy

## Supported versions

trimwire's first public release is v0.1.0. The most recent minor release
receives security patches.

| Version | Supported |
|---------|-----------|
| 0.1.x   | ✅ |

## Reporting a vulnerability

**Do not open a public GitHub issue for security vulnerabilities.**

Instead, please email **andre.zagatti1@gmail.com** with:

- A description of the vulnerability
- Steps to reproduce (or proof-of-concept)
- The impact you've assessed (data exposure, RCE, auth bypass, etc.)
- Your contact info for follow-up

You should expect:

- **Acknowledgement within 48 hours**
- An initial assessment within 7 days
- A coordinated disclosure timeline if the report is valid

## Security model

trimwire is a **local forwarding gateway** — plain HTTP on `127.0.0.1`,
forwarding to Anthropic over HTTPS. It runs on `127.0.0.1` only; it does not
bind to external interfaces.

Trust boundaries:

- **Claude Code (the client)** sends API requests through the gateway.
  All requests originate from the local user's account; the gateway
  trusts whatever Claude Code sends.
- **The Anthropic API (the upstream)** is the trusted destination. The
  gateway forwards via HTTPS with standard certificate validation
  (rustls + webpki-roots). The upstream URL routes your `Authorization`
  token, so it is deliberately **not** configurable from a project-local
  `./.trimwire.toml` (only the global config or `TRIMWIRE_*` env) — a cloned
  repo can't redirect your token. `trimwire run` likewise refuses to reuse a
  listener on the configured port unless it answers `/healthz` (i.e. is
  actually trimwire), so it won't hand your token to a stranger squatting
  there.
- **The local user** has full control over the gateway process and the
  on-disk JSONL session files. The gateway does not encrypt these.

What trimwire **does NOT** do:

- It transmits no telemetry off-box by default. The **opt-in** community
  telemetry (`trimwire share enable`, then `trimwire share stats`) is the only
  exception: it sends a bucketed, content-free aggregate to a collector endpoint
  you have consented to — never prompt/file/command content. Off until you
  explicitly enable it (and the default endpoint is empty until the maintainer
  deploys one). See `docs/TELEMETRY.md` for the exact payload.
- It does not modify the system prompt or any header that would change
  client identity from Anthropic's perspective.
- It does not persist API request/response bodies anywhere (the SQLite
  ledger only stores byte counts + cache-prefix hashes, not content).
- The optional wire audit (`TRIMWIRE_AUDIT=<file>` / `--audit`, **off by
  default**) writes one JSONL line per request with *shape metadata only* —
  counts, sizes, flags, the model, the `anthropic-beta` header, and the session
  id — and **never message content, tool input, or result text**. It is a
  local debugging/transparency aid; nothing leaves the machine.
- It does not make any model/API calls of its own — it only forwards the
  client's own requests (it never reuses your credentials to originate traffic).
  *Exception, opt-in + off by default:* the optional summarizer (always compiled,
  inert unless configured) is switched on via `[summarizer] engine` in the config.
  With `engine = "local"`, trimwire sends a slice of OLD `messages[]` content to a
  **local** summarizer endpoint (default `http://localhost:11434`, e.g. ollama on
  your own machine) — nothing leaves the machine. With `engine = "<provider id>"`
  (a `[[summarizer.providers]]` entry), that slice goes to the cloud API provider
  *you* configured, authenticated with *your own* key from the provider's
  `api_key_env` — never with your Anthropic/Claude Code credential, which is only
  ever forwarded unchanged on your own requests. With the default
  `engine = "model-free"`, no model calls originate here at all.

What trimwire **DOES** see in memory while a request is in flight:

- The full `messages[]` array (including conversation content).
- The `Authorization` Bearer token (forwarded unchanged; never logged).
- All `anthropic-*` and `x-stainless-*` headers (forwarded unchanged).

The trust model assumes:

- The user trusts the trimwire binary they're running (download from
  releases, audit source, or build from this repo).
- The user's localhost is not compromised.

## Out of scope

- DoS resistance — trimwire listens on localhost; if a malicious local
  process can hit it, your machine has bigger problems.
- Encryption at rest of the SQLite ledger — it contains no request
  bodies, only byte counts + hashes.
