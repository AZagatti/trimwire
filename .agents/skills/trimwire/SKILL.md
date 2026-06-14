---
name: trimwire
description: Use when the user asks — inside a Claude Code session — how much context trimwire is saving, for cache/token stats, to find or inspect a past session, to preview what pruning would do, to benchmark/share a summarizer model, or to opt in/out of the community dashboard. It shells the local `trimwire` CLI (safe, local, content-free) and explains the output. Triggers like "how much is trimwire saving?", "is trimwire working?", "trimwire stats", "what would pruning trim here?", "find my session from earlier", "share my benchmark", "opt in to the dashboard".
---

# trimwire — in-session visibility

trimwire is a local proxy that prunes Claude Code's conversation context on every
API call. This skill surfaces what it's doing **in-session** by running its
**safe, local, content-free** CLI commands via the shell and interpreting the
results. It does **not** write code or change config. Almost everything here only
reads; the one exception is `trimwire dashboard --out FILE`, which writes a local
HTML report. Network uploads (`share stats` / `share benchmark`) never happen
without the user's explicit consent (`share enable` or `--yes`).

## When to use this skill

- "How much context/cost is trimwire saving?" → `trimwire stats`
- "Is trimwire actually running / on the wire?" → `trimwire status`, then `trimwire doctor`
- "Show this session's cache hit rate / per-model token split" → `trimwire stats --session`
- "Which of my recent sessions was biggest / used the most cache?" / "find that session from earlier" → `trimwire recall [query]`
- "What would pruning trim on this (or a past) session?" (no wire, no token) → `trimwire preview --last`
- "Something looks off with trimwire" → `trimwire doctor` (one-shot setup diagnosis)

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
| Preview the anonymous, content-free telemetry that `share stats` *would* upload (dry run — prints the exact payload, sends nothing) | `trimwire share stats` |
| Score a summarizer model and preview the content-free leaderboard row it *would* share (dry run without `--yes`) | `trimwire share benchmark` |
| Opt in / out of community uploads (persists consent; after `enable`, `share stats` uploads without `--yes` each run) | `trimwire share enable` · `trimwire share disable` |
| Is the gateway running and serving? | `trimwire status` |
| One-shot setup diagnosis (config + active profile, gateway health, `ANTHROPIC_BASE_URL` wiring, ledger) | `trimwire doctor` |

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

## Notes

- These commands need the `trimwire` binary on `PATH` (built/installed from this
  repo). If a command isn't found, say so plainly rather than guessing numbers —
  never fabricate stats.
- This skill is for visibility, not changes. Changing the profile or config is
  `trimwire config` (leave that to the user). The only command here that writes
  anything is `dashboard --out` (a local HTML file). See `docs/FAQ.md` for
  trust/ToS questions.
- `trimwire share stats` / `share benchmark` **without** `--yes` are safe to run
  — they only *print* the anonymous, content-free payload they would upload (a
  dry run; nothing is sent). The binary *does* ship a built-in community endpoint
  (`api.trimwire.dev`), so the dry-run is gated on **consent**, not a missing
  destination: nothing uploads until the user runs `share enable` or passes
  `--yes`. **Do not** run them with `--yes` — opting in is the user's explicit
  choice. See `docs/TELEMETRY.md` for exactly what each payload contains.
