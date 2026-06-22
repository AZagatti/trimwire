# Security & trust model

trimwire is a **local forwarding gateway**: Claude Code talks to it over plain
HTTP on `127.0.0.1`, and it forwards your requests to Anthropic over HTTPS. This
page explains what that means for your data and your credentials — what trimwire
touches, what stays on your machine, and what it deliberately won't do.

> **Found a vulnerability?** Don't open a public issue — see the reporting
> instructions in [`SECURITY.md`](https://github.com/AZagatti/trimwire/blob/main/SECURITY.md)
> on GitHub.

## What runs, and where

trimwire binds to `127.0.0.1` only — it never listens on an external interface.
It forwards to Anthropic over HTTPS with standard certificate validation
(rustls + webpki-roots). There is no CA certificate to install, no TLS
interception, and no patched client: Claude Code stays the client, unchanged.

The upstream URL routes your `Authorization` token, so it is deliberately **not**
configurable from a project-local `./.trimwire.toml` — only the global config or
`TRIMWIRE_*` environment variables can set it. A cloned repo can't silently
redirect your token. Likewise, `trimwire run` refuses to reuse a listener on the
configured port unless it answers `/healthz` (i.e. it really is trimwire), so it
won't hand your token to something else squatting on that port.

## What leaves your machine

- **Your Claude Code requests** go to Anthropic, exactly as Claude Code sent
  them, minus the conversation context trimwire pruned. Same auth header, same
  system prompt (on the default path), same model and sampling parameters.
- **Nothing else, by default.** trimwire sends no telemetry off-box unless you
  opt in.

Two opt-in features can send data, and only when you turn them on:

- **Community telemetry** (`trimwire share enable`, then `trimwire share stats`)
  sends a bucketed, content-free aggregate to the collector at
  `https://api.trimwire.dev/ingest` (and `…/ingest-benchmark` for benchmarks) —
  never prompt, file, or command content. Off until you enable it. The exact
  payload is documented in [`TELEMETRY.md`](TELEMETRY.md).
- **The optional summarizer** (`[summarizer] engine`, off by default). With
  `engine = "local"`, an old slice of `messages[]` goes to a local model
  endpoint (e.g. ollama at `http://localhost:11434`) — nothing leaves the
  machine. With a cloud provider id, that slice goes to the provider *you*
  configured, authenticated with *your own* key — never your Claude
  subscription credential. With the default `engine = "model-free"`, trimwire
  makes no model calls of its own at all. See [`SUMMARIZER.md`](SUMMARIZER.md).

## What trimwire stores on disk

- **No request or response bodies, ever.** The optional SQLite ledger records
  byte counts and cache-prefix hashes only — not message content. Set
  `[ledger] enabled = false` to record nothing.
- **The optional wire audit** (`--audit` / `TRIMWIRE_AUDIT`, off by default)
  writes one JSONL line per request with *shape metadata only* — counts, sizes,
  flags, the model, the `anthropic-beta` header, and a session id — and **never**
  message content, tool input, or result text. It's a local debugging aid.
- **Your transcripts are untouched.** trimwire shapes the request on the wire; it
  never writes to your `~/.claude` session files. (The separate, explicit
  `trimwire sweep` command is the only thing that edits on-disk transcripts, and
  only when you run it.)

## What trimwire sees in memory

To prune a request, trimwire holds it in memory for the duration of that one
request. While a request is in flight it sees:

- the full `messages[]` array, including conversation content;
- the `Authorization` Bearer token (forwarded unchanged, never logged);
- the `anthropic-*` and `x-stainless-*` headers (forwarded unchanged).

It does not modify the system prompt or any header that would change the
client's identity from Anthropic's perspective (the opt-in
`system_shape_normalize` strategy is the only exception, and only repairs a
malformed leading system message). It never reuses your credentials to originate
its own traffic.

## Trust assumptions

trimwire's security model assumes:

- you trust the binary you're running (download a release, audit the source, or
  build from the repo); and
- your localhost is not compromised.

Given those, the trust boundaries are simple: **Claude Code** is the local
client whose requests are trusted as your own; **the Anthropic API** is the
trusted HTTPS destination; and **you**, the local user, fully control the gateway
process and the on-disk session files (which are not encrypted at rest — but
contain no trimwire-added secrets).

## Out of scope

- **DoS resistance** — trimwire listens on localhost only; if a malicious local
  process can reach it, your machine already has bigger problems.
- **Encryption at rest of the ledger** — it stores no request bodies, only byte
  counts and hashes.

## Safe defaults at a glance

- Localhost-only bind; HTTPS to Anthropic with cert validation.
- No telemetry, no summarizer model calls, no system-prompt changes — unless you
  opt in.
- Ledger stores counts/hashes only; audit is off by default and content-free
  when on.
- On any error or a request it can't safely prune, trimwire forwards your
  original bytes unchanged — worst case is "no pruning this turn," never a broken
  request.

For privacy specifics (what the telemetry payload contains, retention), see the
[Privacy policy](PRIVACY.md) and [`TELEMETRY.md`](TELEMETRY.md). For trust/ToS
questions, see the [FAQ](FAQ.md).
