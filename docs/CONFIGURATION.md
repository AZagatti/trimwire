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

- **Bypass trimwire for ONE session** (gateway stays up for everyone else):
  `trimwire run --bypass -- <claude args>` — it points that one `claude` straight
  at Anthropic (no pruning) without touching global state. (Equivalent by hand:
  `ANTHROPIC_BASE_URL= claude …` or `unset ANTHROPIC_BASE_URL` then `claude`.)
- **Use trimwire for ONE session without the always-on service:**
  `trimwire run …` — it scopes `ANTHROPIC_BASE_URL` to just that child
  process (reusing the running gateway if one is up, else starting a private one)
  and tears it down afterward.

So "active for this session, off for that one" is just which shells export
`ANTHROPIC_BASE_URL`, or a single `trimwire run --bypass`. (Note: `trimwire
pause` globally puts the gateway in **bypass** — it keeps serving but forwards
unmodified, so every shell keeps working with no pruning; `trimwire resume`
resumes. It does **not** edit your rc. `trimwire off`, by contrast, fully
disengages: it stops the gateway and removes the `ANTHROPIC_BASE_URL` wiring so
Claude Code talks directly to Anthropic — use it when you need a host-gated
feature like Remote Control (or, to keep pruning too, enable `[server]
remote_control` — see below); `trimwire on` re-engages.)

## `profile` — the one knob most people need


```toml
profile = "default"   # "default" | "gentle"
```

A profile seeds every strategy knob below it, so you usually don't touch the
`[strategies.*]` tables at all. It sits *below* your explicit keys in the merge
order; anything you set by hand still wins.

| profile | strategies | who it's for |
|---|---|---|
| `default` *(shipped)* | All eight cache-safe strategies with aggressive knobs: mostly `keep_recent_turns=2` (a few wider — `stale_reads`/`thinking_strip` keep 4), bloat cap 4 KB, image keep 1, verb-class denylist for browser tools, plus `stale_input_cap`, `stale_reads`, `thinking_strip`, and reprune on. (Exact per-strategy values are in the sections below.) | Most people; especially long or tool-heavy sessions. Cleanest context, kept cache-stable by reprune. |
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
listen   = "127.0.0.1:8765"          # numeric IP:port the gateway binds
upstream = "https://api.anthropic.com" # where slimmed requests are forwarded
```

`listen` must be a **numeric IP:port** (e.g. `127.0.0.1:8765` or `[::1]:8765`),
not a hostname — `localhost:8765` is rejected (it falls back to the default with
a warning). If you change `listen`, set `ANTHROPIC_BASE_URL` to match.

### `remote_control` — pruning **and** Remote Control on the same session (opt-in, off)

Claude Code refuses to start **Remote Control** (driving your session from your
phone / claude.ai) whenever `ANTHROPIC_BASE_URL` points at a non-Anthropic host —
so trimwire's normal wiring and Remote Control are otherwise mutually exclusive.

```toml
[server]
remote_control = true
```

With this on, `trimwire install` / `trimwire on` wire trimwire a **different way**:
instead of exporting `ANTHROPIC_BASE_URL`, they export
`BUN_OPTIONS=--preload ~/.trimwire/coexist-shim.js` and leave the base URL **unset**
(so Remote Control's gate is satisfied). A tiny preload shim inside Claude Code
reroutes only `POST /v1/messages` through the gateway for pruning; everything else —
including the Remote Control channel — goes straight to Anthropic. Re-run
`trimwire on` after changing this, and open a new shell (or `source` your rc).

**Opt-in and fragile, by design.** It relies on Claude Code's Bun runtime + `fetch`
internals — it works today, but a Claude Code update can break it (it fails safe,
falling back to going direct) — and it works around a deliberate Anthropic
restriction. All traffic still goes **only to Anthropic**; trimwire just prunes
context, exactly like the default path. Requires a **loopback** `listen` (trimwire
refuses to wire this mode for a non-local gateway). `trimwire doctor` verifies the
wiring.

**GUI/editor-launched Claude Code.** The shell rc only reaches terminal-launched
sessions. For Claude Code launched outside a shell (the VS Code panel, the desktop
app), `trimwire install`/`on` also writes a process-local launcher at
`~/.trimwire/claude-launch.sh` — it execs the real `claude` with the same preload
set **only in that process** (no session-global env var, so no other Bun process is
touched). Point your editor's Claude "process wrapper" at it: in VS Code set
`claudeCode.claudeProcessWrapper` to `~/.trimwire/claude-launch.sh` (Remote-WSL/SSH:
put it in `~/.vscode-server/data/Machine/settings.json` on the remote host). Terminal
and JetBrains sessions already prune via the rc block. See also the
[FAQ entry](FAQ.md#can-i-use-claude-codes-remote-control-control-your-session-from-your-phone-with-trimwire).

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
# exempt_tools = ["Task", "Agent"]   # tools never deduped (default: subagent tools)
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

**Subagent (`Task`/`Agent`) results are age-gated on a wider window** (not exempt
at every age). A subagent's findings/blocker list is consumed across many follow-up
turns, so it stays exempt while within `subagent_keep_recent_turns` (default 8, vs
the tight global `keep_recent_turns`), then head+tail-salvaged like any old result —
top findings + conclusion kept, the dense middle trimmed. Add `Task`/`Agent` back to
`exempt_tools` to restore the legacy all-ages exemption (this is what the `gentle`
profile does). Authoring results (Write/Edit/MultiEdit) stay all-ages exempt.

**`Read` gets its own, wider recent window** (`exempt_recent_only_keep_turns`, default
4 in the `default` profile). A medium file read (4–16 KB) is protected by `stale_reads`
for 4 turns but was head+tail-trimmed by `bloat_cap` after only 2 — so its middle
vanished at age 3 while the model might still reference it. Matching this to
`stale_reads`' 4-turn window closes that gap (a recoverable trim — the file is
re-readable — so the tradeoff is read-heavy savings, not correctness). The effective
window is `max(exempt_recent_only_keep_turns, keep_recent_turns)`; `0` falls back to the
global window. Bash/MCP results keep the tight global window (savings preserved).

```toml
[strategies.bloat_cap]
enabled = true
threshold_bytes   = 16384   # struct default; the `default` profile sets this to 4096
head_bytes        = 2048    # bytes kept from the start
tail_bytes        = 2048    # bytes kept from the end
keep_recent_turns = 4       # struct default; the `default` profile sets this to 2
exempt_tools      = ["Edit", "Write", "MultiEdit"]  # never trimmed at ANY age (authoring — eliding them corrupts sessions)
subagent_keep_recent_turns = 8        # Task/Agent results exempt within this WIDER window, then head+tail-salvaged once old
exempt_recent_only_tools = ["Read"]   # exempt only while RECENT; an OLD large Read IS trimmed (the "Read coverage gap" fix)
exempt_recent_only_keep_turns = 4     # Read's own recent window (>= keep_recent_turns); the `default` profile sets 4; 0 = fall back to keep_recent_turns
# --- opt-in levers (all default 0 / empty = OFF; zero behaviour change unless set) ---
catastrophic_bytes = 0          # if >0, also caps a RECENT result this large (it can't
                                # fit the context window anyway, so it would brick the
                                # session). Set it WELL ABOVE threshold_bytes. Uses a
                                # generous head/tail floor and a distinct marker.
stub_age_turns     = 16         # the `default` profile sets this to 16 (struct default 0).
                                # A string result older than this is FULLY stubbed to a
                                # marker (not head+tail). Must exceed keep_recent_turns or
                                # it has no effect. String content only. See the note below.
protected_file_patterns = []    # globs of file paths never trimmed (mirror the same
                                # globs in [strategies.stale_reads] for full protection)
```

**The age ladder (`stub_age_turns`, on at 16 in `default`).** Once a string result is
older than `stub_age_turns` assistant turns, it's replaced by a bare
`[trimwire: aged-out result N bytes]` marker instead of a head+tail trim — it keeps the
size marker but loses the head/tail glimpse *and* the salvaged error/warning signal
lines. This is a concentrated win on **long** result-heavy sessions (the ones past 16
turns, where trimwire's value is highest) and has **zero effect on short sessions**. The
tradeoff is fidelity on very-old results: a `Read` is re-readable, but an old Bash/MCP
output stubbed this way is gone. 16 (rather than a tighter value) deliberately keeps the
12–16 turn band as head+tail so a still-referenced recent-ish error survives. Set it to
`0` to disable (very-old results then keep their head+tail). `gentle` leaves it off.

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
exempt_tools   = ["Read", "Edit", "Write", "MultiEdit", "NotebookEdit", "Task", "Agent"]  # never stubbed, even if denylisted
# stub = "[trimwire: elided, older than sliding window]"
```

### `[strategies.image_strip]` — on by default

Replace base64 image payloads (e.g. screenshots, accessibility/DOM/heap snapshots)
older than the K most-recent matching results with a marker.

```toml
[strategies.image_strip]
enabled = true
applies_to_tools  = ["*screenshot*", "*snapshot*"]   # screenshots + snapshot/heapsnapshot/DOM image blobs
keep_recent_count = 3   # struct default; the `default` profile sets this to 1
# stub = "[trimwire: image stripped]"
```

### `[strategies.stale_input_cap]` — on in `default`, off in `gentle`

Caps the bulky *input* of an old **successful** tool call (the failed-input
counterpart is `failed_input_purge`). Protects the most recent turns.

**Authoring tools (Write/Edit/MultiEdit/NotebookEdit) are age-gated with a
recoverable marker**, not permanently exempt. Their authored body is kept verbatim
while recent — within the wider `authoring_keep_recent_turns` window (default 6 vs.
2 for ordinary inputs) — so content the model is actively editing is never touched.
Once it ages past that window it is replaced with `[trimwire: wrote <path> (<N>B) —
read the file to restore]` instead of the generic size marker: the call succeeded,
so the content is on disk and the model re-reads to recover it. A *failed* authored
call's body is never touched here (it never hit disk — that floor stays in
`failed_input_purge`).

**Subagent (`Task`/`Agent`) prompts are reduced once old and successful** — no
default exemption. After a subagent call succeeds its result captures the outcome,
so the verbatim sub-task prompt is dead weight; the short `description` is kept and
only the bulky `prompt` is elided to a size marker. (A *failed* subagent call keeps
its exemption in `failed_input_purge`, where a retry may re-emit the prompt.) Add a
tool name back to `exempt_tools` if you want to keep its input verbatim.

```toml
[strategies.stale_input_cap]
enabled = true
keep_recent_turns = 2            # ordinary inputs (Bash stdin, MCP args, subagent prompts)
authoring_keep_recent_turns = 6  # authored bodies — wider; recoverable "read the file" marker once old
exempt_tools = []                # nothing exempt by default; add a tool to protect its input
```

### `[strategies.stale_reads]` — on in `default`, off in `gentle`

Elides an old `Read` whose file was later superseded (re-read / Write / Edit on
the same path), and demand-pages the *last* read of a path once it exceeds
`page_min_bytes`. Never breaks a tool pair.

**Recency gate (`keep_recent_turns`):** a superseded `Read` is elided only once it
ages past the keep-recent window — never on the turn the model is using it. A read
the model re-reads or edits one or two turns later is still in the active working
set; eliding it immediately would force the model to re-issue the same `Read`
(re-billing the bytes and slowing the session). The same window gates both
superseded-elision and demand-paging, so trimwire only ever acts "some rounds
later."

**Hot-path guard:** a path the model has `Read` **more than once** in the session
is never demand-paged. Paging the current view of a file the model keeps needing
would just force another re-read (paged out again next turn) — a read-spiral. So
demand-paging only pages genuinely one-shot large reads; the *current* view of a
file you keep re-reading is left verbatim. (This suppresses demand-paging only —
an *earlier*, superseded read of that same path is still elided once it ages past
`keep_recent_turns`.)

```toml
[strategies.stale_reads]
enabled = true
keep_recent_turns = 4
page_min_bytes = 16384           # default profile: only page reads larger than this (16 KB; lowered from 32 KB in v0.3.0). struct default 0 = off
exempt_tools = []
protected_file_patterns = []     # opt-in (default empty = off): globs of file paths that
                                 # are never superseded-elided OR demand-paged. Mirror the
                                 # same globs in [strategies.bloat_cap] for full protection.
# stub = "[trimwire: stale read — superseded by a newer view later]"
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
recheckpoint_result_bytes = 131072  # default profile (128 KB); 0 = off (gentle + struct default).
                                    # Forces a re-checkpoint when the tail carries > this many bytes of
                                    # tool_result content, with no-advance-on-no-op rollback (the
                                    # byte-based re-checkpoint from the "Read coverage gap" fix).
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
turns). See [`SUMMARIZER.md`](SUMMARIZER.md) for the full guide; this is the
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
#                             # chars ≈ 60 KB). KEEP THE DEFAULT for qwen3.5:4b: raising to
#                             # 40000 (≈96 KB) FAILS it (N=10: 0/10, drops the same ~2 facts
#                             # every run) — see MODEL-COMPATIBILITY.md. Only raise on a model
#                             # you've gated at the larger size with `summarizer probe`.
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
# # Key source (pick one; trimwire stores the NAME/PATH, never the key):
# api_key_file = "~/.myapi_key"  # RECOMMENDED: read at runtime, so it works with the
#                             # always-up service `trimwire install` sets up (a daemon
#                             # can't see your shell's exported env vars). ~/ → $HOME; chmod 600.
# api_key_env = "MYAPI_KEY"   # OR an env-var name — fine for foreground `trimwire run`,
#                             # invisible to a background service.
```

See [SUMMARIZER.md](SUMMARIZER.md) "Provider recipes" for copy-paste configs
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

Note `db_path` defaults to `~/.trimwire/`, **not** an `XDG_DATA_HOME` location —
so setting `XDG_DATA_HOME` alone does **not** redirect the ledger. To point the
ledger somewhere else (e.g. to isolate a test run or manual canary from your real
data), set the env override `TRIMWIRE_LEDGER__DB_PATH=/path/to/ledger.db`.

## `[share]` — opt-in anonymous telemetry (off by default)

Only used by `trimwire share stats` and `trimwire share benchmark`.

Opt in with `trimwire share enable` (persists `enabled = true`); opt out with
`trimwire share disable`. Once enabled, `trimwire share stats` uploads without
`--yes` each run. `--yes` also works as a per-run override or first-time
confirmation. `--force` bypasses the once-per-day throttle.

The built-in community stats collector URL ships in the binary and points at
`https://api.trimwire.dev/ingest`. `[share] endpoint` exists as an override for
self-hosting or testing; it is not the normal path. The benchmark collector
ships the same way (built-in URL `https://api.trimwire.dev/ingest-benchmark`,
overridable via `[share] benchmark_endpoint`); `trimwire share benchmark` uploads
only with `--yes`, otherwise it dry-runs.

The payload is coarse, bucketed, and anonymized client-side; it never contains
prompts, code, paths, ids, IPs, or raw counts. See
[`TELEMETRY.md`](TELEMETRY.md) for the full field-by-field contract.

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

### Auto-report hook (opt-in, files a content-free issue on a detected anomaly)

Wire `trimwire report --auto` as a **`Stop`** hook. At the end of each session it
checks that session for a trimwire anomaly (currently: an HTTP ≥400 response on a
turn where pruning fired — i.e. a prune that may have broken the request) and, if
found and not already filed, opens a **content-free** GitHub issue via `gh`
(versions + OS + a coarse cache-stability bucket only — no paths or session
content), deduped in `<ledger-dir>/filed-issues`. It is **silent when clean**,
never fails a session (a hung `gh` call is bounded to 15 s), and falls back to
printing a pre-filled report URL if `gh` isn't available. Requires the `gh` CLI
authenticated with issue-write access.

```json
{ "hooks": { "Stop": [ { "hooks": [ { "type": "command", "command": "trimwire report --auto 2>/dev/null || true" } ] } ] } }
```

Run `trimwire report` (no `--auto`) any time to print a pre-filled issue URL you
submit yourself.

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
[`benchmark/`](https://github.com/AZagatti/trimwire/blob/main/benchmark/README.md) (start with its headline tables).
