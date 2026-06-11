# Privacy policy

**Short version: trimwire collects nothing by default. It runs entirely on your
machine, and it never sees, stores, or transmits your code or conversations.**
The only data that ever leaves your machine is an *opt-in*, anonymized, content-free
statistics upload that you turn on yourself — described in full below.

_Last updated: 2026-06-11._

## The trimwire tool

trimwire is a local proxy. Your Claude Code (or other harness) traffic passes
through it on your own machine, on its way to your model provider on **your own
API key**.

- It reads each request **only to prune it in memory** before forwarding it, then
  discards it. It does **not** store your prompts, code, file contents, file
  names, or model responses.
- It keeps a small local **ledger** (a SQLite file in your data directory) of
  *content-free* counters — bytes saved, cache-hit rate, which strategies fired —
  the same numbers `trimwire stats` shows you. This never leaves your machine
  unless you explicitly opt in to sharing (below).
- It makes **no network calls of its own** on the default path. (The optional
  summarizer, if you enable it, calls a model **you choose on your own key** — see
  [Summarizer](SUMMARIZER.md).)

## Opt-in telemetry (`trimwire share`)

Nothing is uploaded unless you run `trimwire share enable` (or pass `--yes`). You
can opt out any time with `trimwire share disable`. Until you opt in — and until
the project's community collector is deployed — `trimwire share stats` only prints
the payload and sends nothing.

When you do opt in, each upload is a **single small JSON of coarse, bucketed,
aggregate numbers** derived from your local ledger. By design it contains **no**
prompts, code, file paths or names, message text, session IDs, machine or install
IDs, raw IP, timestamps finer than a calendar day, or any raw byte/token counts.

How your privacy is protected:

- **Content-free.** Only ledger-derived metadata is ever sent — never message
  content or paths.
- **Coarsened on your machine.** Every percentage and size is bucketed *before* it
  leaves, so even the row that reaches the collector is already anonymized.
- **No cross-day identity.** A random install ID stays on your machine and is never
  transmitted. Each upload carries only a daily-rotating HMAC token, so uploads on
  different days cannot be linked to you.
- **No IP stored.** The collector uses your IP only for transient rate-limiting and
  never writes it to its database.
- **Aggregate-only public dashboard.** The public dashboard shows only aggregates
  across many contributors, with small groups suppressed (k-anonymity), so no
  individual's data is surfaced.

The complete, field-by-field list of exactly what is and isn't sent — the single
source of truth — is in [Telemetry](TELEMETRY.md). A test in the codebase fails
the build if the payload ever contains a field not on that list.

**Benchmark sharing** (`trimwire share benchmark`) is a separate opt-in upload that
scores a model against a *bundled synthetic corpus* — never your session — and is
likewise content-free. See [Telemetry](TELEMETRY.md#benchmark-sharing).

## The website

This site is static and adds **no analytics, tracking pixels, or tracking
cookies**. It's served by a CDN host (Cloudflare), which — like any web host —
processes requests and may keep standard, short-lived access logs for security and
abuse-prevention. The community dashboard fetches only the pre-aggregated,
anonymized statistics described above.

## Changes

trimwire is open source: this policy, the exact telemetry fields, and the code that
produces them are all auditable in the repository. Material changes will be noted
in the [changelog](https://github.com/AZagatti/trimwire/blob/main/CHANGELOG.md) and
reflected here.
