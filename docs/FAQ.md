# FAQ & Trust

Short, honest answers to the questions people ask before pointing their Claude
Code at a local proxy. Every claim here is backed by the code and the docs linked
inline. Nothing aspirational.

## How do I install it?

```bash
# 1. Prebuilt binary, no toolchain — grab the asset for your OS/arch from
#    github.com/AZagatti/trimwire/releases/latest, then: chmod +x trimwire &&
#    sudo mv trimwire /usr/local/bin/   (Windows: unzip the .zip onto your PATH)
# 2. Have Rust?  cargo binstall trimwire   (fetches the prebuilt binary, no compile)
#                cargo install trimwire     (builds from source; needs Rust 1.85+)
# 3. Convenience — Linux/macOS only (downloads the binary AND runs `trimwire install`):
curl -LsSf https://raw.githubusercontent.com/AZagatti/trimwire/main/scripts/install.sh | sh
```

If you used option 1 or 2, run `trimwire install` next — it wires Claude Code
(config + `ANTHROPIC_BASE_URL` + the always-up service). The `curl | sh` script in
option 3 already runs that for you. Full walkthrough: the
[README](https://github.com/AZagatti/trimwire#quickstart).

To verify a downloaded binary's checksum and build provenance, see
[Verifying a downloaded release](SECURITY-MODEL.md#verifying-a-downloaded-release).

**To update**, re-run the same method you installed with — each overwrites the
binary in place (your config and shell rc are untouched):

- **Installed via `curl | sh`** (option 3): re-run that one-liner. It downloads
  the latest release binary and re-runs `trimwire install` (idempotent).
- **`cargo binstall`/`cargo install`** users: `cargo binstall trimwire` (fetches
  the latest prebuilt) or `cargo install trimwire --locked` (rebuilds).
- **Manual binary** (option 1): download the new asset from
  [releases/latest](https://github.com/AZagatti/trimwire/releases/latest) and
  replace the binary on your `PATH`.

`trimwire update` **checks** whether a newer release is available (read-only) —
for a `curl | sh` install it reports the available version; for cargo/manual
installs it prints the per-method paths above. It never downloads or changes
anything. `trimwire upgrade --dry-run` downloads the latest release and verifies
its checksum + signature without changing anything. On a managed (`curl | sh`)
**Linux** install, `trimwire upgrade` self-updates: it verifies the download
(SHA-256 + a minisign signature against a key pinned in the binary), atomically
replaces the binary, restarts the service, and rolls back if the new build isn't
healthy (it asks before applying on a terminal; `--yes` skips the prompt).
Self-update is fail-closed and **requires the maintainer to have published a
signing key** — if none is pinned yet it refuses with "no pinned update-signing
key", so use the per-method update above. macOS/Windows and cargo/manual installs always use the
per-method update. See
[`docs/UPDATE-COMMAND-SPIKE.md`](https://github.com/AZagatti/trimwire/blob/main/docs/UPDATE-COMMAND-SPIKE.md).
After a manual update, restart with `trimwire off && trimwire on` (or open a new
shell) so the new binary serves.

## Is trimwire safe to use with my Claude subscription? (Terms of Service)

**With an API key: unambiguously yes. With a Pro/Max subscription: a greyer,
fast-moving area — read on.** (We're not lawyers; this is our reading, verify against
the current [Claude Code legal terms](https://code.claude.com/docs/en/legal-and-compliance).)

> **Last reviewed: 2026-06.** The Claude ToS landscape shifted repeatedly through
> 2026 — re-check the current terms before relying on the Pro/Max reading below.

trimwire uses the LLM-gateway pattern Anthropic documents: Claude Code points at
trimwire via `ANTHROPIC_BASE_URL`, trimwire prunes the conversation `messages[]`, and
forwards to Anthropic. LiteLLM, Vercel AI Gateway, and Cloudflare AI Gateway are
documented examples of the **same** pattern. Specifically, trimwire:

- keeps **Claude Code itself as the client, verbatim** (same binary, system prompt,
  User-Agent) — trimwire is not a client;
- **never modifies `system` (on the default path) or `tools[]`** — only the conversation `messages[]`;
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
the [Security & trust model](SECURITY-MODEL.md).

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
  the content-free guarantees in the [Security & trust model](SECURITY-MODEL.md).
  (trimwire also writes a small local **install receipt** — install method,
  version, target, binary path — stored locally only and never transmitted.)
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
broken request. To send Claude Code straight back to Anthropic, `unset
ANTHROPIC_BASE_URL` (or open a fresh shell after `trimwire uninstall`). Note
`trimwire off` only stops the gateway — your shell still exports
`ANTHROPIC_BASE_URL`, so Claude calls fail until you `trimwire on` again or unset
it.

## Do the `[trimwire: …]` markers confuse Claude?

No. Each marker is a small, content-free placeholder that tells the model
something was elided at that position (e.g. `[trimwire: superseded by a later
identical call]`). The model still sees the turn structure and knows what was
there — it just doesn't get the redundant bytes. Strategies only remove content
that is structurally redundant (e.g. superseded by a later identical call) or
older than the configured window — never anything based on a semantic judgement
of what the model "needs".

## What if trimwire pruned an old detail I still need?

Keep `default` on and **ask the agent to re-read the file or re-run the tool** —
the underlying data is still on disk; pruning only changes what's sent on *this*
request, not your history. The `[trimwire: …]` markers are **retrieval cues**:
they tell the model exactly what was elided and where, so it can fetch the source
again instead of guessing.

That's the recommended recall-critical path: **`default` ON + agentic re-read**,
not switching to `gentle`. `gentle` is a *lighter-touch, lower-savings* profile —
it prunes less, but it is not a "recall mode" and not safer; relying on re-read
keeps the default pruning behavior while giving the agent a cue to recover specific
details when the original source is still available to re-read or re-run. (When a
session overflows the context window, any lossy step — a
summarizer or a plain window cutoff — can discard older detail; trimwire's
per-request pruning leaves a `[trimwire: …]` cue so the agent can re-read it.)

## Does it work with API key, Pro, and Max?

Yes to all three. trimwire forwards whatever auth header Claude Code sends
(Bearer for OAuth Pro/Max, `x-api-key` for API keys) unchanged. See
[Compatibility](https://github.com/AZagatti/trimwire/blob/main/README.md#compatibility).

---

*More setup/diagnosis help: [`docs/TROUBLESHOOTING.md`](TROUBLESHOOTING.md). Security
model: [Security & trust](SECURITY-MODEL.md). Design rationale: [`SPIKE.md`](https://github.com/AZagatti/trimwire/blob/main/SPIKE.md).*
