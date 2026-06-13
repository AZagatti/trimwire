# Configuration reference

`trimwire install` writes a commented starter config and turns the workhorse
strategies on. This file documents **every** knob. You rarely need most of them.

## Where config lives (two-tier + env)

Resolved in this order, later wins:

1. **Global:** `$XDG_CONFIG_HOME/trimwire.toml`, falling back to
   `~/.config/trimwire.toml`.
2. **Per-project:** `./.trimwire.toml` in the working directory (overrides
   global; **lists are replaced, not merged**).
3. **Environment:** `TRIMWIRE_*` variables, nested with `__`. E.g.
   `TRIMWIRE_SERVER__LISTEN=127.0.0.1:9000`,
   `TRIMWIRE_STRATEGIES__BLOAT_CAP__THRESHOLD_BYTES=32768`.

Edit the global config quickly with `trimwire config`.

## Per-session control (one Claude Code on, another off)

trimwire routes Claude Code through the gateway via the `ANTHROPIC_BASE_URL` env
var that `trimwire install` adds to your shell rc. Activation is therefore
**per-shell**, so running it for one Claude Code session but not another already
works — no global toggle needed:

- **Bypass trimwire for one shell** (gateway stays up for everyone else):
  `unset ANTHROPIC_BASE_URL` then run `claude` — it goes straight to Anthropic.
- **Bypass for a single command:** `ANTHROPIC_BASE_URL= claude …`.
- **Use trimwire for ONE session without the always-on service:**
  `trimwire run claude …` — it scopes `ANTHROPIC_BASE_URL` to just that child
  process (reusing the running gateway if one is up, else starting a private one)
  and tears it down afterward.

So "active for this session, off for that one" is just which shells export
`ANTHROPIC_BASE_URL`. (Note: `trimwire off` stops the gateway globally and does
**not** edit your rc — see its output for how to send a shell straight to Anthropic.)

## `profile` — the one knob most people need


```toml
profile = "default"   # "default" | "gentle"
```

A profile seeds every strategy knob below it, so you usually don't touch the
`[strategies.*]` tables at all. It sits *below* your explicit keys in the merge
order; anything you set by hand still wins.

| profile | strategies | who it's for |
|---|---|---|
| `default` *(shipped)* | All eight cache-safe strategies with aggressive knobs: mostly `keep_recent_turns=2` (a few wider — `stale_reads`/`thinking_strip` keep 4), bloat cap 4 KB, image keep 1, verb-class denylist for browser tools, plus `stale_input_cap`, `stale_reads`, `thinking_strip`, and reprune on. (Exact per-strategy values are in the sections below.) | Most people, especially Max/quota-rich or long sessions. Cleanest context, kept cache-stable by reprune. |
| `gentle` | `cross_turn_dedup` + `failed_input_purge` + conservative `bloat_cap` (32 KB / keep 6) + `thinking_strip` (keep 8) + reprune. `stale_input_cap`, `stale_reads`, `sliding_window`, and `image_strip` are off. | Lightest-touch option; least pruning, least rot protection. |

Both profiles turn on stable-prefix re-pruning (`[reprune]`), which keeps the
pruned prefix byte-identical between checkpoints so the prompt cache survives.
That's what makes the aggressive default cache-safe. Cost behaviour across
profiles is **non-monotonic** (more pruning is not always lower cost); see
`benchmark/` for the numbers. An unrecognised name falls back to `default` with
a warning. Setting `profile` in a project `./.trimwire.toml` is allowed (it only
affects pruning; `upstream` stays global-only).

## `[server]`

```toml
[server]
listen   = "127.0.0.1:8765"          # address:port the gateway binds
upstream = "https://api.anthropic.com" # where slimmed requests are forwarded
```

If you change `listen`, set `ANTHROPIC_BASE_URL` to match.

## Strategies

They run in this fixed order: `failed_input_purge` → `stale_input_cap` →
`cross_turn_dedup` → `stale_reads` → `simhash_dedup` → `bloat_cap` →
`sliding_window` → `image_strip` → `thinking_strip`. Each has an `enabled`
flag. `simhash_dedup` is the only one OFF in both shipped profiles (opt-in);
`thinking_strip` is ON in both (its struct default is off, but the profiles
enable it). Tool-name lists support `*` globs. `trimwire stats` shows which
fire and how much each saves.

### `[strategies.cross_turn_dedup]` — workhorse, on by default

Same tool called again with identical arguments (re-reading a file, re-running
a grep) → keep only the most recent result; replace earlier identical ones with
a marker. Safe: only provably-superseded duplicates are removed.

```toml
[strategies.cross_turn_dedup]
enabled = true
# exempt_tools = []   # tools never deduped (default: none)
# stub = "[trimwire: superseded by a later identical call]"
```

### `[strategies.failed_input_purge]` — workhorse, on by default

For tool calls whose result errored, **reduce** the (often large) `input` after
a few turns: keep the small scalar fields (`command`, `file_path`, flags up to
~512 B) so the model still knows *which* call failed, and replace only the bulk
(large strings and nested arrays/objects: heredocs, file bodies, stdin) with
a content-free size marker. A small input is left untouched. The tool name, id,
and the error text are always preserved.

```toml
[strategies.failed_input_purge]
enabled = true
keep_recent_turns = 4   # struct default; the `default` profile sets this to 2
# exempt_tools = []
```

### `[strategies.bloat_cap]` — on by default

Trim a single oversized `tool_result` (e.g. a huge build log) to
head + tail + a `[trimwire: trimmed N bytes]` marker, but only once it's older
than `keep_recent_turns`. Skipped if the trim wouldn't actually shrink it.
Never touches file-editing tools. For **array-content** results (e.g. multi-block
MCP output) it salvages the bulky text blocks in place (same head/tail trim),
keeping structure, small blocks, and images; a pure non-text/image array over
threshold is replaced by a single size marker.

```toml
[strategies.bloat_cap]
enabled = true
threshold_bytes   = 16384   # struct default; the `default` profile sets this to 4096
head_bytes        = 2048    # bytes kept from the start
tail_bytes        = 2048    # bytes kept from the end
keep_recent_turns = 4       # struct default; the `default` profile sets this to 2
exempt_tools      = ["Read", "Edit", "Write", "MultiEdit", "Task"]
# --- opt-in levers (all default 0 / empty = OFF; zero behaviour change unless set) ---
catastrophic_bytes = 0          # if >0, also caps a RECENT result this large (it can't
                                # fit the context window anyway, so it would brick the
                                # session). Set it WELL ABOVE threshold_bytes. Uses a
                                # generous head/tail floor and a distinct marker.
stub_age_turns     = 0          # if > keep_recent_turns, results older than this are
                                # FULLY stubbed (not head+tail). Must exceed
                                # keep_recent_turns or it has no effect. String content
                                # only; trades old-result fidelity for size.
protected_file_patterns = []    # globs of file paths never trimmed (mirror the same
                                # globs in [strategies.stale_reads] for full protection)
```

### `[strategies.sliding_window]` — most aggressive, browser-only by default

Stub denylisted `tool_use`/`tool_result` pairs older than `keep_recent_turns`,
regardless of whether they're duplicates. More aggressive than dedup, so it
defaults to browser-automation tools only. Add e.g. `"Bash"`/`"Grep"` only if
you want **all** their old results dropped (this can remove a unique result the
model still wanted; dedup is the safer general win).

```toml
[strategies.sliding_window]
enabled = true
keep_recent_turns = 4   # struct default; the `default` profile sets this to 2
# the shipped `default` value — verb-class globs matching browser automation
# (`*browser_act*`, NOT a bare `*act*`, which would also match extract/interact/redact):
denylist_tools = ["*screenshot*", "*navigate*", "*click*", "*browser_act*", "Grep"]
exempt_tools   = ["Read", "Edit", "Write", "MultiEdit", "Task"]  # never stubbed, even if denylisted
# stub = "[trimwire: elided, older than sliding window]"
```

### `[strategies.image_strip]` — on by default

Replace base64 image payloads (e.g. screenshots) older than the K most-recent
matching results with a marker.

```toml
[strategies.image_strip]
enabled = true
applies_to_tools  = ["*screenshot*"]
keep_recent_count = 3   # struct default; the `default` profile sets this to 1
# stub = "[trimwire: image stripped]"
```

### `[strategies.stale_input_cap]` — on in `default`, off in `gentle`

Caps the bulky *input* of an old **successful** tool call (the failed-input
counterpart is `failed_input_purge`). Protects the most recent turns.

```toml
[strategies.stale_input_cap]
enabled = true
keep_recent_turns = 2            # turns protected from capping
# shipped default — authoring/sub-agent tools are never capped (Write/Edit/MultiEdit/
# NotebookEdit are ALSO a hard floor; Task is only protected via this list):
exempt_tools = ["Task", "Write", "Edit", "MultiEdit", "NotebookEdit"]  # tool names (globs) never capped
```

### `[strategies.stale_reads]` — on in `default`, off in `gentle`

Elides an old `Read` whose file was later superseded (re-read / Write / Edit on
the same path), and demand-pages the *last* read of a path once it exceeds
`page_min_bytes`. Never breaks a tool pair.

```toml
[strategies.stale_reads]
enabled = true
keep_recent_turns = 4
page_min_bytes = 32768           # default profile: only page reads larger than this (32 KB)
exempt_tools = []
protected_file_patterns = []     # opt-in (default empty = off): globs of file paths that
                                 # are never superseded-elided OR demand-paged. Mirror the
                                 # same globs in [strategies.bloat_cap] for full protection.
# stub = "[trimwire: stale read…]"
```

### `[strategies.thinking_strip]` — on in BOTH profiles (struct default off)

Drops old `thinking` / `redacted_thinking` blocks (reasoning for
already-solved problems). Removals are replayed by reprune by signature, so the
cache still holds. `default` keeps the last 4 turns of reasoning, `gentle` the
last 8.

```toml
[strategies.thinking_strip]
enabled = true
keep_recent_turns = 4
```

### `[strategies.simhash_dedup]` — OPT-IN (off in both profiles)

Catches *near*-duplicate tool_results that `cross_turn_dedup` (exact-match)
misses, using a SimHash within `hamming_threshold` bits. Off by default; enable
it if you have very repetitive tool output.

```toml
[strategies.simhash_dedup]
enabled = false
keep_recent_turns = 4            # struct default (simhash is opt-in; no profile sets it)
hamming_threshold = 3            # ≤ this many differing bits (of 64) = duplicate
min_bytes = 512                  # ignore results smaller than this
exempt_tools = []
# stub = "[trimwire: near-duplicate…]"
```

### `[strategies.system_shape_normalize]` — OPT-IN (off by default)

Lifts a stray `messages[0]` whose `role` is `"system"` up into the top-level
`system` field. Some flows (after `/compact`, `/clear`, or a mid-session model
switch) can emit a leading system-role message, which the API rejects with a 400.
Off by default; enable it only if you actually hit that 400 (it mutates request
shape, so it stays opt-in to keep the default path transparent).

```toml
[strategies.system_shape_normalize]
enabled = false
```

## `[reprune]` — stable-prefix re-pruning (on in both shipped profiles)

```toml
[reprune]
enabled = true    # on in both shipped profiles; set false to disable
threshold = 8     # re-prune cadence: new messages before a full re-prune (~2× keep_recent)
max_sessions = 1024
ttl_secs = 3600
```

By default trimwire re-prunes every request from scratch; as messages age out of
the recent window, the pruned prefix shifts turn-to-turn and busts Anthropic's
prompt cache. With `reprune` on, trimwire keeps per-session state and, while the
conversation is **append-only**, re-uses the previous turn's pruning decisions so
the pruned prefix stays byte-identical. It recomputes only once the tail grows
past `threshold` messages, or when history is rewritten (compaction). This **cuts
prompt-cache churn and cost on long / churn-heavy sessions** (offline-measured
−30–40%), at the cost of trimming the most-recent batch one checkpoint later.

Both shipped profiles (`default` and `gentle`) turn reprune on. That's what keeps
the aggressive default cache-stable. Set `[reprune] enabled = false` to override.
It can never produce wrong output: any prefix change forces a full re-prune
identical to the stateless path (worst case is a cache miss). State is bounded by
`max_sessions` (LRU) and `ttl_secs` (idle eviction).

## `[summarizer]` — optional model summary of OLD content (off by default)

**Start with the wizard:** `trimwire summarizer setup` writes this block for you.
The knobs below are for hand-tuning `~/.config/trimwire.toml` afterward.

Opt-in. When `engine` is not `"model-free"` (and `[reprune]` is on), trimwire sends
the OLD prunable slice to a **local ollama** model or your **own API key** and replays
the resulting summary in place of that slice — a clean summary the model can read
instead of lossy elision markers. Requires reprune (it carries the summary across
turns). See [`docs/SUMMARIZER.md`](docs/SUMMARIZER.md) for the full guide; this is the
knob reference. **ToS:** trimwire never originates calls on your Claude *subscription*
token — only on a local model or an API key you provide.

```toml
[summarizer]
engine        = "model-free"  # "model-free" (off) | "local" | a provider id below
fallback      = []            # ordered engine ids to try if the primary fails
mode          = "default"     # "default" | "gentle" (a slower, more conservative cadence)
trigger_bytes = 204800        # only engage once the request body exceeds this (200 KB)
timeout_secs  = 180           # hard timeout per summarizer call
keep_recent_turns = 6         # recent assistant turns never summarized (working set)
resummarize_after_bytes = 32768   # re-summarize once this much new old-content accrues
accumulator   = true          # append frozen delta segments instead of replacing
max_summary_segments = 128    # frozen-segment cap (raise for very long sessions; cache-safe)
# slice_char_budget = 131072  # max serialized bytes summarized per segment. Unset =
#                             # per-engine default: local ~60 KB (num_ctx/OOM-safe, from
#                             # the 25600 default below), API-only chain ~128 KB.
#                             # Bigger = the summary owns more old content (API engines).
accept_ratio  = 1.0           # 1.0 = strict (keep summary only if smaller than model-
#                             # free). >1.0 (e.g. 1.5, for strong API engines) keeps a
#                             # higher-fidelity summary up to ratio× the model-free size
#                             # (capped at +16 KB). Keep 1.0 for weak local models.

[summarizer.local]            # used when engine/fallback includes "local"
endpoint = "http://localhost:11434"
model    = "qwen3.5:4b"       # an approved local tag; weaker tags are warned/refused
max_num_ctx = 25600           # ollama num_ctx + local slice budget (≈max_num_ctx×2.5−2000
#                             # chars ≈ 60 KB). Raise to 40000 (≈96 KB) on a GPU/high-RAM
#                             # box for more coverage (qwen3.5:4b held ~92% near this size);
#                             # KV cache only grows when a slice is actually that big.
#                             # Clamped at 131072.
keep_alive_secs = 0           # 0 = unload the model from RAM after each summarizer call
#                             # (RAM-saving default). Raise (e.g. 60) to keep it warm
#                             # between calls on a GPU/high-RAM box.

# [[summarizer.providers]]    # one block per cloud provider; reference by `id`
# id          = "myapi"
# style       = "anthropic"   # "anthropic" | "openai" (auth header + payload shape)
# base_url    = "https://api.example.com"   # API root; style path /v1/... is appended
#                                           # (no trailing /v1 for openai)
# full_url    = "..."         # OPTIONAL exact POST URL — bypasses base_url + /v1 for
#                             # non-standard paths (Z.ai /paas/v4, Azure deployments).
#                             # When set, base_url is ignored; style still applies.
# model       = "claude-haiku-4-5"
# api_key_env = "MYAPI_KEY"   # env var holding the key (never inline the key)
```

See [docs/SUMMARIZER.md](docs/SUMMARIZER.md) "Provider recipes" for copy-paste configs
(OpenAI, OpenRouter, Anthropic, Z.ai anthropic + openai, Azure, self-hosted vLLM).

To let the summary own a large fraction of old content on a strong cloud model, use an
**API-only** chain (no `"local"` in `engine`/`fallback`) with `accept_ratio = 1.5`; the
128 KB API budget then covers far more per segment (raise `slice_char_budget` for more,
mindful of the provider's context window). The deterministic strategies still run and
remain the full fallback when the summarizer is off or fails.

## `[ledger]`

Per-request savings + cache-prefix telemetry (byte counts and hashes only, no
message content). Pruned to `retain_days` at startup. Set `enabled = false` to
record nothing.

```toml
[ledger]
enabled     = true
db_path     = "~/.trimwire/ledger.db"
retain_days = 365
```

## `[share]` — opt-in anonymous telemetry (off by default)

Only used by `trimwire share stats` and `trimwire share benchmark`.

Opt in with `trimwire share enable` (persists `enabled = true`); opt out with
`trimwire share disable`. Once enabled, `trimwire share stats` uploads without
`--yes` each run. `--yes` also works as a per-run override or first-time
confirmation. `--force` bypasses the once-per-day throttle.

The built-in community stats collector URL ships in the binary and points at
`https://api.trimwire.dev/ingest`. `[share] endpoint` exists as an override for
self-hosting or testing; it is not the normal path. The benchmark endpoint
(`[share] benchmark_endpoint`) is not yet deployed — `trimwire share benchmark`
dry-runs until configured.

The payload is coarse, bucketed, and anonymized client-side; it never contains
prompts, code, paths, ids, IPs, or raw counts. See
[`docs/TELEMETRY.md`](docs/TELEMETRY.md) for the full field-by-field contract.

```toml
[share]
enabled           = false   # set true via `trimwire share enable` to persist consent
endpoint          = ""      # override for self-hosting/testing only; leave empty for the community collector
benchmark_endpoint = ""     # same, for `trimwire share benchmark`
```

## Seeing your savings

`trimwire stats` always works (reads the ledger; no setup). For an in-session
view there are two pieces:

### Live bar (the statusline) — opt-in

`trimwire install` **never touches your statusline**. Adding the bar is a
separate, explicit step (Claude Code's statusline is a *single command* with no
"field" API, so the bar can only live in `statusLine`):

```bash
trimwire statusline add      # if you have NO statusline → makes trimwire your bar
trimwire statusline wrap     # if you ALREADY have one → adds a trimwire row beneath it
trimwire statusline remove   # undo either (restores a wrapped original exactly)
```

- `add` writes `statusLine → trimwire statusline render` to
  `~/.claude/settings.json` (atomic write; refuses if the file has JSON comments
  it can't parse). If a statusline already exists it **leaves it untouched** and
  points you to `wrap`.
- `wrap` makes trimwire the `statusLine`, runs your original bar (feeding it the
  same input), and prints a trimwire row beneath it. Your original command is
  stashed in `~/.trimwire/statusline-wrapped.cmd`; `remove` (and `uninstall`)
  restore it exactly.
- (`trimwire statusline render` is the command those write into settings.json;
  Claude Code runs it. You never type it, so it's hidden from `--help`.)

The bar prints `⊡ trimwire 128 KB (~32K tok) · 41% ↓ · 87 reqs` for the current
session (`· ready` before any traffic; a yellow `⚠ not responding` when it's
set-but-down). It's intentionally terser than an in-session AI assistant block (no
"messages/tools compressed" or topic; trimwire is a byte-level proxy). The full
per-strategy breakdown is in `trimwire stats`.

> **Heads-up on wrapping:** Claude Code runs only one `statusLine`, and the
> *last* tool to write that key wins it. If `claude-statusline` is reinstalled
> after you wrap, it'll overwrite the key and trimwire's row disappears. Just
> re-run `trimwire statusline wrap`.

### Manual integration into your own statusline script

If you'd rather embed trimwire *inside* your own script instead of wrapping (e.g.
[`claude-statusline`](https://github.com/Flagrare/claude-statusline), a single
shell script with no plugin hook), add trimwire as a *segment*. **Both read
Claude Code's JSON from stdin, and stdin can only be consumed once**, so reuse
the `$input` your script already captured; don't re-read stdin:

```bash
# in statusline.sh, right after the existing `input=$(cat)` line:
tw_seg=$(printf '%s' "$input" | trimwire statusline render 2>/dev/null)

# then add "$tw_seg" to a row's join_segs call, e.g.:
row2_right=$(join_segs "$cost_seg" "$tw_seg")
```

`join_segs` skips empty segments, so when the ledger is empty or trimwire is
off the segment just vanishes. (If your `justify` helper isn't ANSI-aware, the
colored segment may offset right-alignment. Drop the color by piping through
`sed 's/\x1b\[[0-9;]*m//g'` if so.)

### Health alert hook (works without any statusline)

Wire `trimwire hook` as a `SessionStart` (and/or `UserPromptSubmit`) hook. It
stays silent when healthy and emits a visible `systemMessage` only when trimwire
is configured but not serving, so a silent "set but not pruning, paying full
price" failure can't go unnoticed even if you don't watch the statusline:

```json
{ "hooks": { "SessionStart": [ { "hooks": [ { "type": "command", "command": "trimwire hook" } ] } ] } }
```

## Runtime environment variables (not config-file keys)

These tune the running daemon, separate from the config layers above:

- `TRIMWIRE_AUDIT=<file>`: opt-in **wire audit**. When set, the daemon appends
  one JSONL line per `/v1/messages` describing the *shape* of each request
  (counts, sizes, flags, the model, the `anthropic-beta` header, and the session
  id) and **never any message content, tool input, or result text**. It also
  records the **cache-prefix structure** (structural identifiers only): the
  ordered tool-definition names (`tool_names`), which `tools`/`system` blocks
  carry a `cache_control` breakpoint, the `system` shape, and the `messages[0]`
  block-type sequence (`first_msg_blocks`) — used to investigate prompt-cache
  behaviour. Off by default (no cost when unset). Useful to see exactly what
  Claude Code sends on the wire and what trimwire forwards. Also available as
  `--audit <file>` on `trimwire serve` / `trimwire run`.
- `TRIMWIRE_LOG=info|debug|warn`: gateway log verbosity (`tracing` filter).

## Tuning notes

The shipped defaults (`threshold_bytes`, `keep_recent_turns`, the
`sliding_window` denylist) are conservative starting points, not
telemetry-tuned values. Run a few real sessions, check `trimwire stats`, and
adjust. Per-profile savings/cost numbers and the methodology are in
[`benchmark/`](benchmark/README.md) (start with its headline tables).
