# Troubleshooting

> For trust/ToS/privacy questions ("is this safe?", "does it see my code?", "how
> do I try it without risk?"), see [**`FAQ.md`**](FAQ.md).

**Start with `trimwire doctor`** — it checks the config + active profile, whether
the gateway is serving, whether `ANTHROPIC_BASE_URL` points at it, and whether the
ledger exists, in one shot. `trimwire config show` prints the resolved effective
config (after the profile + global/project/env merge). For deeper digging, run a
foreground session with `TRIMWIRE_LOG=info trimwire run` (or `=debug`) and
watch the gateway's stderr.

### Claude Code returns connection / TCP errors
The gateway isn't running, or `ANTHROPIC_BASE_URL` points somewhere nothing is
listening. trimwire is a live proxy: if you `export ANTHROPIC_BASE_URL=...` and
then kill the daemon, every `claude` call fails until you restart it
(`trimwire on`) or `unset ANTHROPIC_BASE_URL`. The `trimwire run`
wrapper avoids this by scoping the env var to that one invocation.

### `trimwire stats` shows nothing / zero rows
The ledger records **every** `/v1/messages` request the gateway sees (including
no-ops — that's how the cache-prefix stability ratio is measured), so zero rows
means **no traffic has gone through the gateway yet**, not that nothing pruned.
Check: is the daemon running (`trimwire status`) and is `ANTHROPIC_BASE_URL`
actually set in the shell/app you launched `claude` from? Run a `claude` turn,
then re-check. (Savings can still be 0 on a short text-only session — the
workhorse strategies need repetition: re-reads, a failed command, old
screenshots — but rows should appear once requests flow.) Watch the gateway's
`pruned[...]` stderr line (`TRIMWIRE_LOG=info trimwire run`) to confirm it's
on the wire. Once rows exist, `trimwire recall` lists recorded sessions (newest
first, content-free) so you can find a session id to pass to `stats --session`.

### "Address already in use" on startup
Something is already on `127.0.0.1:8765` — likely a trimwire daemon you already
started. Either reuse it, or change `[server] listen` in
`~/.config/trimwire.toml` (and the matching `ANTHROPIC_BASE_URL`).

### A request was rejected as too large (HTTP 413)
The gateway caps the buffered request body at 32 MB to bound memory. Real
Claude Code requests are far smaller; a 413 usually means something upstream is
wrong, not normal traffic.

### `trimwire sweep` says "aborted, file untouched"
The transcript changed while sweep was running (an active session, or a
compaction). This is the safety design — sweep only rewrites **inactive**
sessions. Close the session (or wait for it to settle) and re-run. Use
`trimwire sweep file <path> --dry-run` to preview without writing.

### I want to undo a sweep
Just run `trimwire sweep undo <path>` — it restores the most recent backup
(`<name>.bak.<timestamp>`; the 3 most recent are kept next to the file).

### `trimwire sweep` saved 0 bytes on a session I expected to shrink
`sweep` is on-disk *maintenance*, not the main pruner: it only drops empty
thinking blocks and purges failed-call inputs. The big savings (dedup of
repeated tool calls, trimming oversized results, stripping old screenshots)
happen **live in the gateway** on every request — they're never written to your
on-disk transcript, so `sweep` won't reproduce them. Use `trimwire stats` to see
the gateway's actual savings.

### Summarizer not working?

1. `trimwire summarizer status` — shows the configured engine and whether the
   endpoint is reachable.
2. `trimwire doctor` — shows overall config health including the summarizer section.
3. Check the engine isn't `model-free` (the default): `trimwire config show | grep engine`,
   or re-run `trimwire summarizer setup` to (re)configure it.
4. The summarizer **only fires on sessions over `trigger_bytes`** (default 200 KB) —
   short or text-only sessions won't trigger it. Check `trimwire stats` for trigger
   counts; lower it by setting `trigger_bytes` under `[summarizer]` in
   `~/.config/trimwire.toml` (`trimwire config` opens it).
5. Any failure silently falls back to model-free pruning by design — the
   summarizer is never load-bearing. If it fails silently, `TRIMWIRE_LOG=info trimwire run`
   will show the reason in the gateway's stderr.

## FAQ

**Does it work with my Pro/Max subscription, or only API keys?**
Both. trimwire forwards your existing auth header unchanged.

**Does it modify the Claude Code system prompt?**
No — only the `messages[]` array. The Jan 2026 third-party-tool detection
pattern keys off the system prompt, so it doesn't apply.

**Does pruning ever break a session?**
It's designed not to: every mutation is pair-aware (never orphans a
`tool_use`/`tool_result`), only ever shrinks the body, and leaves the cache
prefix intact. `trimwire stats` surfaces a cache-prefix **stability ratio** as
a silent-failure tripwire (see [`SPIKE.md` §9](https://github.com/AZagatti/trimwire/blob/main/SPIKE.md)).

**Will it let me blow past the context-window / token limit?**
No. It slims requests so you hit Anthropic's per-request limit *later*, but it
can't exceed it.

**Does `trimwire sweep` work on a session I'm actively using?**
No — it deliberately aborts if the file changes mid-sweep. Sweep is for
transcripts of finished/idle sessions.

**How much will it trim?**
Depends entirely on session shape — request-size reduction spans **0–99%**
(nothing when there's no redundancy; browser/screenshot-heavy sessions trim the
most). The point is context-window **headroom** / a cleaner session, not the
bill: net cost is non-monotonic under prompt caching (short sessions can cost
slightly more, long ones win — see
[`benchmark/`](https://github.com/AZagatti/trimwire/tree/main/benchmark)). For your own
numbers: `trimwire preview <session.jsonl>` (no install) or `trimwire stats`
(live traffic).

**Does it send my data anywhere new?**
No. It forwards to `api.anthropic.com` exactly like Claude Code already does;
the only local persistence is the savings ledger (byte counts + hashes, no
message content) at `~/.trimwire/ledger.db`. See the [Security & trust model](SECURITY-MODEL.md).
