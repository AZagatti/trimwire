# trimwire

> Long Claude Code sessions fill up with stale file reads, superseded tool output,
> and old reasoning — so every turn ships more dead weight to Anthropic. trimwire
> is a tiny local gateway that prunes that dead weight from each request,
> deterministically, keeping recent turns verbatim. No CA cert, no MITM, no restart.

[![License: MIT OR Apache-2.0](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue)](#license)
[![Crates.io](https://img.shields.io/crates/v/trimwire)](https://crates.io/crates/trimwire)
[![Rust 1.85+](https://img.shields.io/badge/rust-1.85%2B-orange)](#contributing--development)
[![Sponsor](https://img.shields.io/badge/sponsor-%E2%9D%A4-db61a2?logo=githubsponsors&logoColor=white)](https://github.com/sponsors/AZagatti)
[![Ko-fi](https://img.shields.io/badge/Ko--fi-support-ff5e5b?logo=kofi&logoColor=white)](https://ko-fi.com/azagatti)

![trimwire demo](https://raw.githubusercontent.com/AZagatti/trimwire/main/docs/demo.gif)

<!-- regenerate with: vhs demo.tape  (see demo.tape) -->

**New here?** Start with the [Overview](docs/OVERVIEW.md) (what it is, recommended
defaults, what the numbers mean, limits) and the [FAQ](docs/FAQ.md). **An LLM/agent
reading this?** See [For agents](docs/FOR-AGENTS.md) for a canonical summary.

## Quickstart

### 1. Install

**Download a prebuilt binary** — no toolchain, nothing piped to a shell. Grab the
asset for your OS/arch from the
[latest release](https://github.com/AZagatti/trimwire/releases/latest), then
(Linux/macOS example — pick the asset matching your platform):

```bash
tar xzf trimwire-x86_64-unknown-linux-gnu.tar.gz   # Windows: unzip the .zip
chmod +x trimwire
sudo mv trimwire /usr/local/bin/                   # or any dir on your PATH
trimwire --version                                 # prints version + git SHA
```

Each release asset carries a `.sha256` checksum and a build-provenance
attestation. To verify a manual download, see
[Verifying a downloaded release](docs/SECURITY-MODEL.md#verifying-a-downloaded-release).

**Have Rust?**

```bash
cargo binstall trimwire  # fetches the same prebuilt binary, no compile
cargo install trimwire   # from crates.io (builds from source; needs Rust 1.85+)
cargo install --path .   # build from a local checkout
```

**Quick install — Linux/macOS only** (convenience) — downloads the right prebuilt
binary *and* runs `trimwire install` for you. Read [the script](scripts/install.sh)
first if you pipe to a shell:

```bash
curl -LsSf https://raw.githubusercontent.com/AZagatti/trimwire/main/scripts/install.sh | sh
```

**Windows** — download `trimwire-x86_64-pc-windows-msvc.zip` from the
[latest release](https://github.com/AZagatti/trimwire/releases/latest), unzip it,
and put `trimwire.exe` on your `PATH` (or `cargo binstall trimwire`). Note: Claude
Code on Windows typically runs under WSL2 — there, use the Linux instructions
above; the always-on service targets Linux/macOS (systemd/launchd).

### 2. Wire it up

```bash
trimwire install         # config + shell-rc ANTHROPIC_BASE_URL + the always-up service (auto-started)
exec $SHELL              # pick up the new env
trimwire doctor          # optional: confirm the gateway is wired + healthy
claude                   # use claude as normal — trimwire prunes each request
```

> **Day one:** zero rows in `trimwire stats` is normal — no traffic has flowed
> through the gateway yet (or the session was short and text-only). Run a session,
> then check again.
>
> **Long, reasoning-heavy sessions?** Turn on the optional summarizer to compress
> old context with a model you choose — local (ollama) or a cloud API. One command:
> `trimwire summarizer setup`.

Optional shell completions: `trimwire completions zsh > ~/.zfunc/_trimwire`
(also `bash`, `fish`, `elvish`, `powershell`). `trimwire --version` reports the
build's git SHA + date for bug reports.

On systemd/launchd, `install` starts the service for you (socket activation). No
systemd/launchd (some containers)? `install` prints a one-time `trimwire on`.

`install` is idempotent and **never touches your statusline.** Want to try it
once without installing anything? `trimwire run` spins up the gateway in
the background and launches `claude` through it for that one session (any args
you pass are forwarded to `claude`). To see
the savings on a session you've *already* run, with zero install:

```bash
trimwire preview ~/.claude/projects/<project>/<session>.jsonl
```

It replays the real strategies over a recorded transcript and prints what each
would trim. Read-only, no network. (Reports `messages[]` only; the live request
also carries a system prompt + tool schemas not in the transcript.)

## How it works

```text
  Claude Code ──/v1/messages──▶ trimwire ──prune messages[]──▶ api.anthropic.com
   (localhost)                  (localhost)   (cache-safe)       (real upstream)
        ▲                                                              │
        └──────── response streamed back byte-for-byte, unbuffered ◀───┘
```

trimwire points Claude Code at itself via Anthropic's documented
`ANTHROPIC_BASE_URL`, intercepts every outbound `/v1/messages` request, rewrites
the `messages[]` array with eight cache-safe pruning strategies, and forwards the slimmer
payload to `api.anthropic.com`. Pure request-shaping: no CA cert, no TLS
interception, no Claude Code restart, and your on-disk transcript is left
untouched.

The always-up service uses **socket activation**: the OS owns the port, so a
crashed or restarting daemon never strands Claude Code; connections queue
instead of being refused. Set it and forget it.

## Tuning & safety

> **New here / worried about ToS or privacy?** Short answers: with an **API key**
> you're clearly fine — `ANTHROPIC_BASE_URL` is Anthropic's documented gateway
> mechanism. With a **Pro/Max OAuth** subscription the rules tightened in 2026 and
> it's a greyer area — [`docs/FAQ.md`](https://github.com/AZagatti/trimwire/blob/main/docs/FAQ.md)
> has the honest detail. On privacy: it never stores your code (the ledger is
> content-free), and you can try it with zero risk via `trimwire preview`.

- **One knob: the profile.** Run `trimwire config` and set
  `profile = "default" | "gentle"` (default `default`). These are
  *cleanliness levels*, not cost tiers. `default` prunes hardest (all eight
  cache-safe strategies, aggressive knobs). `gentle` is the lightest touch (dedup +
  failed-input-purge + a conservative bloat_cap + a conservative thinking_strip).
  Both turn on stable-prefix re-pruning (`[reprune]`, which replays the same pruning
  decisions turn-to-turn so the cache prefix stays byte-identical) to keep the
  prompt cache warm on long sessions. See
  [`CONFIGURATION.md`](https://github.com/AZagatti/trimwire/blob/main/docs/CONFIGURATION.md)
  and [`benchmark/`](https://github.com/AZagatti/trimwire/blob/main/benchmark/results/RESULTS.md)
  for what each trades.
- **Rollback any time.** `trimwire off` routes Claude Code straight to Anthropic
  again — the gateway keeps serving but forwards **unmodified** (a true bypass),
  so `ANTHROPIC_BASE_URL` stays valid and Claude keeps working with no pruning in
  every shell; `trimwire on` resumes. For a single session, `trimwire run
  --bypass`. `trimwire uninstall` removes the service, login autostart, and the statusline
  (only if trimwire added it); it **leaves the shell-rc `ANTHROPIC_BASE_URL`
  block** for you to delete by hand (it prints a reminder). Your config and
  ledger stay until you delete `~/.trimwire/`.
- **No message content stored.** The only local persistence is an optional
  savings ledger of **byte counts + hashes** (`~/.trimwire/ledger.db`); it never
  records your prompts or tool output. Disable with `[ledger] enabled = false`.
  See [`SECURITY.md`](SECURITY.md).

## Summarizer (opt-in)

trimwire is model-free by default. There's an **optional** summarizer that compresses
the old part of a long session with a language model. It's **never load-bearing**: any
failure (model down, slow, bad output) silently falls back to model-free pruning.

Configure with `trimwire summarizer setup` (interactive wizard). Three engines:

- **`local`** — ollama on your own machine. No API key, no data leaves your machine.
  Default model: `qwen3.5:4b` (~3.4 GB RAM) — the recommended local backend; see
  [SUMMARIZER.md](https://github.com/AZagatti/trimwire/blob/main/docs/SUMMARIZER.md)
  and [MODEL-COMPATIBILITY.md](https://github.com/AZagatti/trimwire/blob/main/docs/MODEL-COMPATIBILITY.md)
  for other approved local tags.
- **Cloud API** (engine = a provider id, e.g. `"anthropic"`) — sends the prunable slice to
  the provider you configure using your own API key. Your key, your provider, your choice.
- **`model-free`** — the default; no summarizer, no model calls.

The wizard writes the block for you. To edit by hand, add it to
`~/.config/trimwire.toml` (global) or `./.trimwire.toml` (per-project):

```toml
# ~/.config/trimwire.toml — local (ollama) engine
[summarizer]
engine = "local"

[summarizer.local]
model = "qwen3.5:4b"
```

```toml
# ~/.config/trimwire.toml — cloud API engine: set engine to a provider id
[summarizer]
engine = "anthropic"                 # a provider id below (or "local" / "model-free")

[[summarizer.providers]]
id           = "anthropic"
style        = "anthropic"            # or "openai" (OpenAI-compatible)
base_url     = "https://api.anthropic.com"
model        = "claude-haiku-4-5"
api_key_file = "~/.anthropic_key"     # recommended: read at runtime, works as a service.
#                                     #   printf '%s' "sk-ant-..." > ~/.anthropic_key && chmod 600 ~/.anthropic_key
# api_key_env = "ANTHROPIC_API_KEY"   # OR an env-var name — but the always-up service
#                                     # `trimwire install` sets up can't see your shell exports
```

Best for long, reasoning-dense sessions; it only fires once a request exceeds
`trigger_bytes` (200 KB default), so short sessions are untouched. Model fidelity
varies — validate your pick with `trimwire summarizer probe --model <id> --runs 10`
(add `--yes` for an API provider; local models need no flag).
See [`docs/SUMMARIZER.md`](docs/SUMMARIZER.md) and
[`docs/MODEL-COMPATIBILITY.md`](docs/MODEL-COMPATIBILITY.md) for the full guide + a
verified model ranking.

## Commands

| Command | What it does |
|---|---|
| `trimwire install [--boot]` | Config + shell-rc `ANTHROPIC_BASE_URL` export + the always-up service (systemd user / launchd, login-scoped; `--boot` starts it pre-login). Idempotent; does **not** touch your statusline. |
| `trimwire on` / `off` / `status` | Start / stop / health-check the service. |
| `trimwire doctor` | One-shot setup diagnosis: config + active profile, gateway health, `ANTHROPIC_BASE_URL` wiring, ledger. **Exits non-zero** if a hard check fails, so `trimwire doctor && claude` and CI healthchecks work. |
| `trimwire update` | Read-only check for a newer release (never downloads or changes anything). On a managed install it reports the available version and points you at `trimwire upgrade`; on cargo/manual installs it prints the right update command. |
| `trimwire upgrade [--dry-run] [--yes]` | Self-update on **managed Linux installs**: verify a signed release (SHA-256 + minisign signature against the pinned key), then atomically replace the binary and restart, rolling back on a failed health check. `--dry-run` verifies without changing anything; `--yes` applies non-interactively. Fail-closed; refuses (exit 2) on macOS/Windows and non-managed installs. |
| `trimwire completions <shell>` | Print a shell completion script to stdout (`bash`/`zsh`/`fish`/`elvish`/`powershell`). |
| `trimwire man [--out DIR]` | Generate man pages: to stdout (`trimwire man \| man -l -`), or one page per command into `DIR` (for packagers). |
| `trimwire run [claude args…]` | Start the gateway in the background and launch `claude` through it (no install needed). |
| `trimwire summarizer setup` / `status` / `benchmark` / `probe` | Configure the optional summarizer (interactive wizard), check its status, score a model for fact retention/compression, or `probe` whether your model holds your slice budget (retention by position). |
| `trimwire stats [--json] [-q] [-v] [--session [ID]] [--since YYYY-MM-DD] [--until YYYY-MM-DD]` | Savings ledger: total saved, reduction %, ~tokens removed, per-strategy gauge (bars by bytes trimmed), cache-prefix stability. `--session [ID]` drills into one session's per-model cache/token report (omit ID for the most recent); `--since`/`--until` restrict to a UTC date window; `--json` for scripting; `-q`/`--quiet` for a one-line headline; `-v`/`--verbose` for full instrumentation. |
| `trimwire recall [query] [--json] [--limit N]` | List recent sessions (content-free: date, requests, in→out + reduction %, cache-hit %, model), newest first; optional substring filter on session-id / model. Find a session id to pass to `stats --session`. |
| `trimwire dashboard [--out FILE]` | Write a self-contained local HTML stats dashboard (content-free; embeds the ledger report, no server, no network) to `FILE` (default `trimwire-report.html`). Open it via `file://`. A snapshot; re-run to refresh. |
| `trimwire share enable` / `disable` | Opt in or out of telemetry uploads. Persists `[share] enabled = true/false` in the global config. Once enabled, `trimwire share stats` uploads without `--yes` each run. |
| `trimwire share stats [--yes] [--force]` | **Opt-in, off by default.** Upload an anonymous, content-free aggregate of your ledger to the community dashboard. Requires consent (`share enable` or `--yes`). Without consent, prints the exact payload it would send (a dry run, no network). `--force` bypasses the once-per-day upload throttle. Coarse buckets only; never prompts/paths/ids. See [`docs/TELEMETRY.md`](https://github.com/AZagatti/trimwire/blob/main/docs/TELEMETRY.md). |
| `trimwire share benchmark [--model TAG] [--all-installed] [--yes]` | **Opt-in, off by default.** Score your summarizer model on a bundled synthetic corpus and upload one anonymous, content-free per-model row to the community [benchmark leaderboard](https://trimwire.dev/benchmark/). Dry-run (prints the row) without `--yes`. See [`docs/BENCHMARK.md`](https://github.com/AZagatti/trimwire/blob/main/docs/BENCHMARK.md). |
| `trimwire preview [<session.jsonl>\| --last] [--profile] [--json]` | Read-only what-if: reconstruct `messages[]` from a recorded session transcript and report what pruning *would* trim (per strategy), without touching the file or the network. `--last` auto-picks the most recent session (no path needed). |
| `trimwire statusline add` / `wrap` / `remove` | Opt-in live savings bar (see below). |
| `trimwire hook` | Optional `SessionStart` hook that warns in-session if trimwire is set but not actually serving. |
| `trimwire sweep list` / `all [--dry-run] [--yes]` / `file <path>` / `undo <path>` | Clean session transcripts on disk (atomic, backed up). `list`/`all` auto-discover; `all` confirms first (`--yes` to skip; required when piped/CI); `undo` restores a backup. |
| `trimwire config` / `config show [--json]` | Open the config in `$EDITOR`; `show` prints the *resolved* effective config + active profile. |
| `trimwire uninstall` | Remove the service, GUI/login env hooks, lingering, and the statusline (if trimwire added it). Leaves the shell-rc `ANTHROPIC_BASE_URL` block for you to delete by hand. |

> Output adapts to context: the status glyphs (`✓ ✗ ⚠ ⊡`) and the `▰▱` gauge fall
> back to ASCII (`[ok] [x] [!] ::`, `#-`) when stdout is piped/redirected or
> `NO_COLOR` is set, so logs and scripts stay clean.

## Live savings bar (opt-in)

`trimwire install` **never modifies your Claude Code statusline.** Showing live
per-session savings (`⊡ trimwire 128 KB (~32K tok) · 41% ↓ · 87 reqs`) is a
separate, explicit step:

- **No statusline yet** → `trimwire statusline add` makes trimwire your bar.
- **Already have one** (e.g. [`claude-statusline`](https://github.com/Flagrare/claude-statusline)) → `trimwire statusline wrap` adds a trimwire row *beneath* it, reversibly.
- `trimwire statusline remove` undoes either.

Embedding trimwire as a segment in your own script, and the health-alert hook,
are covered in [`CONFIGURATION.md`](https://github.com/AZagatti/trimwire/blob/main/docs/CONFIGURATION.md). `trimwire stats` always
shows totals with no statusline at all.

## Telemetry — opt-in, anonymous, off by default

trimwire collects **nothing** unless you explicitly opt in. Opt in with
`trimwire share enable` (persists consent); opt out with `trimwire share disable`.
Once enabled, `trimwire share stats` uploads a single small JSON of **coarse,
bucketed, aggregate** numbers: per-strategy reduction share, overall reduction,
version, model family, profile, cache health, conversation length bucket. Nothing
else. Never prompts, code, file paths, session ids, machine ids, IPs, or raw
counts. Everything is bucketed on your machine before it leaves.

The built-in community collector endpoint ships in the binary and points at
`https://api.trimwire.dev/ingest`. The public community dashboard at
<https://trimwire.dev> shows only k-anonymous aggregates. Full contract:
[`docs/TELEMETRY.md`](https://github.com/AZagatti/trimwire/blob/main/docs/TELEMETRY.md).

## How much does it save?

It depends on your session shape. trimwire prunes only when a strategy matches,
and **does nothing** when there's no redundancy, so there's no single "saves
X%". Offline replay through the real strategy code (`default` profile) spans the
full range (values below are observed in this benchmark suite, rounded — exact
per-corpus figures in [`RESULTS.md`](https://github.com/AZagatti/trimwire/blob/main/benchmark/results/RESULTS.md)):

| Session shape | Request size | Reduction (model-free `default`) |
|---|--:|--:|
| Plain chat, no tools | 9.5 KB | **0%** (no-op) |
| Repeated searches | 29.8 KB | **78%** |
| Coding (superseded reads + old log) | 48.0 KB | **83%** |
| Long-running diverse tools | 133.6 KB | **60%** |
| Resumed ~50-turn session | 186.4 KB | **65%** |
| Browser / screenshot-heavy | 423.2 KB | **85%** |
| Realistic composite | 363.0 KB | **95%** |

**The point is a cleaner session, not a smaller bill.** Pruning drops stale
backlog so the current task isn't buried in history ("context rot"). That lifts
the **focus ratio** (the share of the request that's your recent working window:
repeated-search 33%→66%, browser 71%→98%) and keeps it roughly 2–3× higher than no
pruning over a long session in these benchmarks. (Focus is a byte-share proxy; that it improves model
output is plausible, not proven.)

**Cost is a side effect, and non-monotonic.** Cache hits bill at ~0.1×, so
pruning *old* content can bust the cache: short sessions are a wash-to-loss, long
ones win (model-free `default`: about **−55%** at 256 turns in our cost-model benchmark; exact figure in
[RESULTS §6b](https://github.com/AZagatti/trimwire/blob/main/benchmark/results/RESULTS.md)).
The optional summarizer is a separate mode (off by default) with its own cost profile.
Overhead is roughly **sub-2 ms** per request (host-dependent), off the network path.

Numbers are reproducible (`cargo run --release --example bench`) and least
reliable for token/cost estimates (~4 bytes/token). The full 15-corpus tables,
per-strategy attribution, profiles, cache-stability, and the cost model live in
[`benchmark/`](https://github.com/AZagatti/trimwire/blob/main/benchmark/results/RESULTS.md).
For figures from *your* traffic, run `trimwire stats`.

The table above is **offline benchmark replay**. For how that compares to
**live `claude -p`** measurements and the **offline cost-model** numbers — kept
strictly separate so a benchmark/replay figure is never quoted as a live one —
see [`docs/RESULTS.md`](https://github.com/AZagatti/trimwire/blob/main/docs/RESULTS.md).

Eight cache-safe strategies ship enabled in `default`: `cross_turn_dedup`,
`failed_input_purge`, `stale_input_cap`, `stale_reads`, `bloat_cap`,
`sliding_window`, `image_strip`, `thinking_strip`. The `gentle` profile runs a
conservative subset: dedup + failed_input_purge + bloat_cap + thinking_strip. A
ninth strategy, `simhash_dedup` (near-duplicate `tool_result` collapse), is opt-in
(off in both profiles). Every threshold, denylist, profile, and the ledger are
documented in
[`CONFIGURATION.md`](https://github.com/AZagatti/trimwire/blob/main/docs/CONFIGURATION.md).


## Compatibility

- Works with both Pro/Max OAuth subscriptions and API keys (auth header
  forwarded unchanged).
- Covers the terminal `claude` CLI and IDE extensions that inherit the shell
  environment.
- Does **not** cover the standalone desktop/web app. Like any local gateway, it
  only sees clients that read `ANTHROPIC_BASE_URL`.
- Does not modify the Claude Code system prompt on the default path;
  `system_shape_normalize` is the opt-in malformed-shape recovery exception.

## Docs

- **Overview:** what trimwire is, recommended defaults, what the numbers mean, and limits:
  [`docs/OVERVIEW.md`](https://github.com/AZagatti/trimwire/blob/main/docs/OVERVIEW.md)
- **For agents:** canonical LLM/agent summary and claims to use/avoid:
  [`docs/FOR-AGENTS.md`](https://github.com/AZagatti/trimwire/blob/main/docs/FOR-AGENTS.md)
- **Results:** live `claude -p` vs benchmark vs offline cost-model numbers, and
  what's safe to claim: [`docs/RESULTS.md`](https://github.com/AZagatti/trimwire/blob/main/docs/RESULTS.md)
- **FAQ & Trust:** ToS, code privacy, latency, how to try it safely first:
  [`docs/FAQ.md`](https://github.com/AZagatti/trimwire/blob/main/docs/FAQ.md)
- **Configuration:** every strategy knob, the ledger, statusline integration:
  [`CONFIGURATION.md`](https://github.com/AZagatti/trimwire/blob/main/docs/CONFIGURATION.md)
- **Troubleshooting:** connection errors, empty stats, port conflicts, sweep aborts:
  [`docs/TROUBLESHOOTING.md`](https://github.com/AZagatti/trimwire/blob/main/docs/TROUBLESHOOTING.md)
- **Alternatives:** mitmproxy-based pruners and other options:
  [`docs/ALTERNATIVES.md`](https://github.com/AZagatti/trimwire/blob/main/docs/ALTERNATIVES.md)
- **Architecture:** module-by-module design: [`ARCHITECTURE.md`](https://github.com/AZagatti/trimwire/blob/main/ARCHITECTURE.md)
- **Design rationale + validation:** [`SPIKE.md`](https://github.com/AZagatti/trimwire/blob/main/SPIKE.md)
- **`/trimwire` Claude Code skill:** in-session savings/cache visibility by
  shelling the read-only CLI; bundled at
  [`.claude/skills/trimwire/`](https://github.com/AZagatti/trimwire/blob/main/.claude/skills/trimwire/SKILL.md)
  (copy into your project's `.claude/skills/` to use it).

## Contributing & development

```bash
cargo build
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt --check
```

See [`CONTRIBUTING.md`](https://github.com/AZagatti/trimwire/blob/main/CONTRIBUTING.md) for how to submit changes, [`AGENTS.md`](https://github.com/AZagatti/trimwire/blob/main/AGENTS.md)
for repo conventions + pre-commit gates, and [`SECURITY.md`](https://github.com/AZagatti/trimwire/blob/main/SECURITY.md) for the
threat model (a localhost proxy that buffers your requests; the only local
persistence is the savings ledger (byte counts + hashes, no message content))
and how to report a vulnerability.

## License

Dual-licensed under [MIT](https://github.com/AZagatti/trimwire/blob/main/LICENSE-MIT) or [Apache-2.0](https://github.com/AZagatti/trimwire/blob/main/LICENSE-APACHE) at your
option. Unless you state otherwise, any contribution you submit shall be
dual-licensed as above, without additional terms.
