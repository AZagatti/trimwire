---
name: trimwire
description: Use when the user asks — inside a Claude Code session — how much context trimwire is saving, for cache/token stats, to find or inspect a past session, to preview what pruning would do, to benchmark/share a summarizer model, to opt in/out of the community dashboard, OR when something trimwire did looks wrong (content went missing, a re-read loop, a summary looks fabricated) and you want to explain it / file a report. It shells the local `trimwire` CLI (safe, local, content-free) and explains the output. Triggers like "how much is trimwire saving?", "is trimwire working?", "trimwire stats", "what would pruning trim here?", "find my session from earlier", "share my benchmark", "opt in to the dashboard", "something trimwire did looks wrong", "report a trimwire bug".
---

# trimwire — in-session visibility

trimwire is a local proxy that prunes Claude Code's conversation context on every
API call. This skill surfaces what it's doing **in-session** by running its
**safe, local, content-free** CLI commands via the shell and interpreting the
results. It never writes code or edits the pruning config. Most commands here are
**safe + local + read-only**; the exceptions are narrow: `trimwire dashboard --out
FILE` writes a local HTML report, and `trimwire share enable`/`disable` flip a
single consent flag (`[share] enabled`) in the global config — only run those when
the user explicitly asks to opt in/out. Network uploads (`share stats` /
`share benchmark`) never happen without that consent (`share enable` or `--yes`).

## When to use this skill

- "How much context/cost is trimwire saving?" → `trimwire stats`
- "Is trimwire actually running / on the wire?" → `trimwire status`, then `trimwire doctor`
- "Show this session's cache hit rate / per-model token split" → `trimwire stats --session`
- "Which of my recent sessions was biggest / used the most cache?" / "find that session from earlier" → `trimwire recall [query]`
- "What would pruning trim on this (or a past) session?" (no wire, no token) → `trimwire preview --last`
- "Something looks off with trimwire" → `trimwire doctor` (one-shot setup diagnosis)
- "trimwire trimmed something I needed / file content went missing / this summary looks fabricated" → explain it (see *Recognizing trimwire in-session* below), then offer `trimwire report` to file a content-free issue

## Commands (safe + local; run via the shell)

Prefer the `--json` variants when you need to compute or compare numbers; use the
plain form when you just want to show the user the human report.

| Goal | Command |
|------|---------|
| All-time savings ledger (saved bytes, reduction %, ~tokens, per-strategy bars, cache-prefix stability) | `trimwire stats` · `trimwire stats --json` |
| Savings within a UTC date window ("this week", "since a date") | `trimwire stats --since YYYY-MM-DD [--until YYYY-MM-DD]` (add `--json`) |
| Per-session, per-model cache/token report (cache-read vs cache-creation, hit %) — omit the id for the most recent session | `trimwire stats --session [<id>]` |
| List recent sessions (date, requests, in→out + reduction %, cache-hit %, model), newest first; optional id/model substring filter | `trimwire recall [query] [--limit N]` · `trimwire recall --json` |
| What-if: what pruning *would* trim on a recorded session, without touching the file or the network | `trimwire preview --last` · `trimwire preview <session.jsonl>` · add `--profile gentle` or `--json` |
| Write a self-contained local HTML stats dashboard (content-free; opens via file://) | `trimwire dashboard [--out FILE]` |
| Show the anonymous, content-free telemetry payload (prints it; **uploads if the user already opted in**, else dry-run — see Notes) | `trimwire share stats` |
| Score a summarizer model and preview the content-free leaderboard row it *would* share (dry run without `--yes`) | `trimwire share benchmark` |
| Opt in / out of community uploads (persists consent; after `enable`, `share stats` uploads without `--yes` each run) | `trimwire share enable` · `trimwire share disable` |
| Is the gateway running and serving? | `trimwire status` |
| One-shot setup diagnosis (config + active profile, gateway health, `ANTHROPIC_BASE_URL` wiring, ledger) | `trimwire doctor` |
| Print a pre-filled, content-free GitHub issue URL to report a trimwire problem (versions + OS + coarse cache-stability only; no paths/content) — show the user the link, they fill in the description and submit | `trimwire report` · `trimwire report --url-only` |

## How to interpret the output

- **Zero rows in `stats`** is normal on a fresh setup or a short text-only session
  — it means no prunable traffic has flowed yet, not a failure. Suggest running a
  `claude` turn (or `trimwire doctor`) before concluding anything's wrong.
- **Cache-hit %** = `cache_read / total_input_tokens`. Read it *alongside* cache
  writes (creation tokens cost ~1.25–2×, reads ~0.1×) — a high hit % is good.
  trimwire deliberately reports **headroom, not dollar savings** (net cost is
  non-monotonic under prompt caching), so don't translate it into a money figure.
- **`recall`** gives the session id to pass to `stats --session`.
- **`preview`** is the safe way to demonstrate the effect with zero risk (no
  network, no token, file untouched).

## Recognizing trimwire in-session

When something looks wrong mid-session, check whether **trimwire** (not the tool, not the user)
is the cause, then explain it and offer to report it:

- **A `[trimwire: …]` marker** at the start of a tool result or text block is trimwire. It names
  what was removed (`stale read`, `paged out — Read <path>`, `trimmed N bytes`, `image stripped`,
  `superseded by a later identical call`, `summarized turns A..B`). The original is still on
  disk / re-runnable — **re-read the file or re-run the tool to recover the detail.** If the same
  source keeps getting trimmed, or content clearly still needed was removed, that may be
  over-trimming — tell the user and run `trimwire report`.
- **Aged `thinking` blocks are dropped with no marker** (expected, not a bug — don't report).
  With a configured `sliding_window` denylist, an old tool's `input` is blanked — a large one
  becomes a `{"_trimwire": "[trimwire: input elided …]"}` breadcrumb, a small one falls back to a
  bare `{}` (either way it's trimwire, not a tool genuinely called with no arguments).
- **A `[trimwire: summarized turns …]` block** is the opt-in local-model summarizer; its accept
  gate is size-only, so it can be **lossy or wrong** — verify any "done"/result claims by
  re-reading/re-running before relying on them, and offer `trimwire report` if it looks fabricated.

`trimwire report` only ever emits versions + OS/arch + a coarse cache-stability bucket — never file
paths or session content. Show the user the link; they write the description and submit.

## Notes

- These commands need the `trimwire` binary on `PATH` (built/installed from this
  repo). If a command isn't found, say so plainly rather than guessing numbers —
  never fabricate stats.
- This skill is for visibility, not changes. Changing the profile or pruning
  config is `trimwire config` (leave that to the user). The only commands here
  that write anything are `dashboard --out` (a local HTML file) and
  `share enable`/`disable` (which flip the `[share] enabled` consent flag) — run
  the latter only when the user explicitly asks to opt in/out. See `docs/FAQ.md`
  for trust/ToS questions.
- **`share stats` is not always a dry run.** It uploads when consent is already
  enabled (after a prior `share enable`) — *and* `--yes` forces an upload. It only
  *prints* the payload (no network) when sharing is **off** and you pass no
  `--yes`. So don't run `share stats` to "just preview" unless you know sharing is
  off; if unsure, check `trimwire config show` for `[share] enabled` first, and
  never pass `--yes` yourself — opting in is the user's explicit choice. The
  binary ships a built-in community endpoint (`api.trimwire.dev`); the gate is
  **consent**, not a missing destination. Same applies to `share benchmark`
  (uploads only with `--yes`). See `docs/TELEMETRY.md` for each payload.
