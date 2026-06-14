# FAQ & Trust

Short, honest answers to the questions people ask before pointing their Claude
Code at a local proxy. Every claim here is backed by the code and the docs linked
inline. Nothing aspirational.

## How do I install it?

```bash
# Linux/macOS quick install (downloads a prebuilt binary, runs `trimwire install`):
curl -LsSf https://raw.githubusercontent.com/AZagatti/trimwire/main/scripts/install.sh | sh
# any platform: cargo binstall trimwire   (prebuilt binary, incl. Windows)
#               cargo install trimwire     (from crates.io)
# Windows: download the .zip from the GitHub releases page, or use cargo binstall.
```

Then `trimwire install` wires it into Claude Code (config + `ANTHROPIC_BASE_URL` +
the background service). To **update**, re-run the install script or
`cargo binstall trimwire` — it replaces the binary in place. Full walkthrough: the
[README](https://github.com/AZagatti/trimwire#quickstart).

## Is trimwire safe to use with my Claude subscription? (Terms of Service)

**With an API key: unambiguously yes. With a Pro/Max subscription: a greyer,
fast-moving area — read on.** (We're not lawyers; this is our reading, verify against
the current [Claude Code legal terms](https://code.claude.com/docs/en/legal-and-compliance).)

trimwire uses the LLM-gateway pattern Anthropic documents: Claude Code points at
trimwire via `ANTHROPIC_BASE_URL`, trimwire prunes the conversation `messages[]`, and
forwards to Anthropic. LiteLLM, Vercel AI Gateway, and Cloudflare AI Gateway are
documented examples of the **same** pattern. Specifically, trimwire:

- keeps **Claude Code itself as the client, verbatim** (same binary, system prompt,
  User-Agent) — trimwire is not a client;
- **never modifies `system` or `tools[]`** — only the conversation `messages[]`;
- **forwards your auth header unchanged** and never originates its own model calls on
  your token. No CA cert, no TLS interception, no binary patching.

**API key →** gateways are explicitly permitted. You're in the clear.

**Pro/Max subscription (OAuth) → the rules tightened in 2026; treat with care.**
Anthropic's **February 2026 Consumer-Terms update** prohibits using Pro/Max **OAuth
tokens in any other product, tool, or service** (it was enforced against third-party
agent tools like OpenClaw, and from June 2026 routes third-party / Agent-SDK use
through a separate paid credit pool). trimwire's position is that it is **not** such a
tool — it doesn't authenticate as its own OAuth client or route requests on behalf of
*other* users; it's a local, transparent proxy of *your own* Claude Code, forwarding
*your* requests with the client unchanged (the documented gateway pattern, not the
banned third-party-OAuth pattern). **But the clause is broad and enforcement has shifted
repeatedly through 2026** — if you want zero ambiguity, **use an API key** for Claude
Code. See [`SPIKE.md` §1](https://github.com/AZagatti/trimwire/blob/main/SPIKE.md) and
[`SECURITY.md`](https://github.com/AZagatti/trimwire/blob/main/SECURITY.md).

> **The summarizer is separate either way.** It calls its backend with a standard
> **API key** (Anthropic / OpenRouter / Z.ai / local ollama) — it never uses your
> subscription OAuth token, so the OAuth restriction doesn't apply to it in any config.

## Does trimwire see or store my code / conversations?

- **In memory, briefly: yes.** It has to. To prune a request it parses the
  `messages[]` JSON, rewrites it, and forwards it. That's the whole job. The
  content lives in memory only for the duration of that one request.
- **On disk: no content, ever.** The optional ledger (a local SQLite file) and
  the optional `--audit` log record shape metadata only: byte counts, token
  counts, which strategies fired, the model name, timestamps, a session id, and
  prefix hashes. Never message text, tool inputs, or tool results. See
  the content-free guarantees in [`SECURITY.md`](https://github.com/AZagatti/trimwire/blob/main/SECURITY.md).
- **Your transcript is untouched.** trimwire shapes the *request on the wire*; it
  never writes to the `~/.claude` session files. (The separate, explicit
  `trimwire sweep` command is the only thing that edits on-disk transcripts, and
  only when you run it, atomically, with a backup.)
- **What leaves your machine** depends on which engine you use (see next question).
  Brief summary: `local` engine sends the prunable slice to localhost only.
  Cloud API engine sends it to the provider you configured (e.g. OpenRouter) on
  your own key. Opt-in telemetry (`trimwire share stats`) sends only content-free
  aggregate counts, never message content.

## What exactly does trimwire change in my requests?

Only the conversation `messages[]` array, via cache-safe pruning strategies
(dedup of repeated tool results, trimming oversized old outputs, paging stale
file reads, stripping old reasoning blocks, etc.; see
[README](https://github.com/AZagatti/trimwire/blob/main/README.md#how-it-works)). It **never** touches `system` (on the default path), `tools[]`,
your auth header, or the model/params. (The opt-in `system_shape_normalize`
strategy, if explicitly enabled, will lift a malformed stray
`messages[0].role:"system"` into the top-level `system` field — but it is off by
default and never fires on a well-formed body.) Removed content is replaced by a small,
content-free marker (e.g. `[trimwire: …]`) so the model knows something was
elided. It never reintroduces dropped bytes.

## Why did `trimwire install` add `ENABLE_TOOL_SEARCH=true` to my shell?

Two env vars go in your shell rc (inside a clearly-marked `# >>> trimwire >>>`
block): `ANTHROPIC_BASE_URL` (routes Claude Code through the local gateway) and
`ENABLE_TOOL_SEARCH=true`. The second simply **re-enables Claude Code's web-search
tool**, which Claude Code turns OFF automatically whenever `ANTHROPIC_BASE_URL` is
set (it assumes a non-Anthropic endpoint). Since trimwire forwards to Anthropic
unchanged, the feature is safe to keep on — that's all this does. Remove the whole
block (or run `trimwire uninstall`) to undo both.

## Does it add latency, or change Claude's responses?

- **Latency: negligible and one-directional.** trimwire buffers the request,
  prunes it (microseconds-to-low-milliseconds of JSON work), and **streams the
  response back byte-for-byte**. It does not buffer or alter the SSE response.
  It makes **no extra network round-trips** on the request path.
- **System prompt & sampling: untouched; the response stream is forwarded
  byte-for-byte.** trimwire doesn't change your prompt, the system prompt, or any
  sampling parameter. What it *does* change is the conversation context — that's
  the whole point — so **the model's output can differ because stale/redundant
  context was removed.** That's the intended effect, not a side channel. By keeping
  the pruned prefix **byte-identical** turn-to-turn (stable-prefix re-pruning), it
  also *protects* Anthropic's prompt cache rather than busting it.

## What does the opt-in summarizer send, and where?

The summarizer is **off by default** (`engine = "model-free"`). Enable it with
`trimwire summarizer setup`. When on, it sends a slice of **old** `messages[]` content
to the engine you configured:

- **`local` engine:** the slice goes to your local ollama server
  (default `http://localhost:11434`). Nothing leaves your machine. No API key used.
- **Cloud API engine** (`engine = "<provider-id>"`, e.g. `"anthropic"`): the slice is
  sent to the cloud provider you configured (e.g. `api.anthropic.com`, `api.openai.com`,
  or an OpenRouter endpoint) using **your own API key**. The privacy posture is
  determined by your provider's policy, not trimwire's.
  See [`SUMMARIZER.md`](SUMMARIZER.md) for details.

In both cases:

- It runs in the **background**: the summary is cached and replayed on the *next*
  turn, so it never blocks a request or adds latency on the path.
- If the summarizer is disabled (the default), trimwire makes **no model calls of
  its own at all**.

## Something looks wrong — where do I start?

Run `trimwire doctor` first. It checks the config, whether the gateway is
serving, whether `ANTHROPIC_BASE_URL` points at it, and whether the ledger
exists. From there, [`docs/TROUBLESHOOTING.md`](TROUBLESHOOTING.md) has the
common failure patterns.

## How do I try it safely before pointing my real Claude Code at it?

Two zero-risk, no-token, no-wire ways:

- **`trimwire preview <session.jsonl>`**: reconstructs `messages[]` from one of
  your recorded session transcripts and reports exactly what pruning *would*
  trim, per strategy, **without touching the file or the network**.
- **`trimwire sweep list` / `sweep all --dry-run`**: shows what the on-disk
  transcript cleaner would change, without changing anything.

When you do go live, `trimwire doctor` checks the wiring, and `trimwire stats`
shows what's actually being saved on your own traffic.

## What happens if trimwire crashes or is down?

The always-up service uses **socket activation**: the OS owns the listening port,
so a connection that arrives while the worker is restarting is **queued, not
refused**. Claude Code is never stranded with a connection error. And on any
internal error or a request it can't safely prune, trimwire forwards your
original bytes unchanged. The worst case is "no pruning this turn," never a
broken request. To stop using it entirely, `trimwire off` (or unset
`ANTHROPIC_BASE_URL`) sends Claude Code straight back to Anthropic.

## Do the `[trimwire: …]` markers confuse Claude?

No. Each marker is a small, content-free placeholder that tells the model
something was elided at that position (e.g. `[trimwire: superseded by a later
identical call]`). The model still sees the turn structure and knows what was
there — it just doesn't get the redundant bytes. Strategies only remove content
that is structurally redundant (e.g. superseded by a later identical call) or
older than the configured window — never anything based on a semantic judgement
of what the model "needs".

## Does it work with API key, Pro, and Max?

Yes to all three. trimwire forwards whatever auth header Claude Code sends
(Bearer for OAuth Pro/Max, `x-api-key` for API keys) unchanged. See
[Compatibility](https://github.com/AZagatti/trimwire/blob/main/README.md#compatibility).

---

*More setup/diagnosis help: [`docs/TROUBLESHOOTING.md`](TROUBLESHOOTING.md). Security
model: [`SECURITY.md`](https://github.com/AZagatti/trimwire/blob/main/SECURITY.md). Design rationale: [`SPIKE.md`](https://github.com/AZagatti/trimwire/blob/main/SPIKE.md).*
