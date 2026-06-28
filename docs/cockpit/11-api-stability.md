# 11 — API Stability: the cockpit won't break when CLI commands change

> Maintainer requirement: *"we need to guarantee when the CLI commands change the
> cockpit won't break."* This doc states how that guarantee is enforced — by
> architecture, by a versioned contract, and by tests that fail CI on drift.

## The principle: two consumers, one library

trimwire already separates **pure library logic** (`src/{config,ledger,strategies,
sweep,...}.rs`) from **binary-private command bodies** (`src/cli/*`). The cockpit adds a
**third consumer of the same library** — the control API (`src/admin/*`):

```
            ┌─────────────── trimwire library (serde_json::Value builders) ───────────────┐
            │  config · ledger Report · preview estimate · sweep · service status ...      │
            └──────────────┬───────────────────────────────────────┬──────────────────────┘
                           │                                        │
                  src/cli/* (CLI)                          src/admin/* (control API)
              `trimwire stats --json`                      `GET /api/v1/stats`
                           │                                        │
                    human / scripts                        the cockpit (web + app)
```

**The cockpit never talks to the CLI.** It never shells out to `trimwire <cmd>` and never
parses CLI stdout. It speaks only the versioned **`/api/v1` HTTP contract**. Therefore:

- Renaming a CLI flag, reordering subcommands, changing help text, or reformatting the
  *human* output of `trimwire stats` — **cannot** affect the cockpit. Those are CLI surface,
  a different consumer.
- The CLI `--json` output and the API response are built by the **same** library function
  (doc 03 PR-1 extracts the `serde_json::Value` builders out of the `println!` wrappers so
  both call one source). They move together by construction; they cannot silently diverge.

So "CLI commands change" is, for the cockpit, a **non-event** — unless the change reaches the
shared *JSON shape*. That single remaining risk is what the rest of this doc closes.

## The three guarantees

### 1. A versioned contract (`/api/v1`)

The cockpit pins the API version. Evolution rules inside a version are **additive-only**:

- ✅ Add a new endpoint or a new field → fine; the frontend ignores unknown fields.
- ❌ Rename/remove a field, or change its type/meaning → **not allowed in `/api/v1`**; it
  goes to `/api/v2`, and the server can serve both during a transition.

`GET /api/v1/version` returns `"control_api": "v1"`, so the client can detect the contract it
is talking to and degrade gracefully (show "update your cockpit") on a major mismatch.

### 2. Contract tests that fail CI on drift

The shared builders are pinned by tests, so a refactor that would change the wire shape
**fails loudly** instead of silently breaking the cockpit. In the POC today
(`src/admin/mod.rs`):

- `version_payload_contract_keys_are_stable` — asserts the exact top-level key set of
  `/api/v1/version`. Change the shape → the test fails → you make a *conscious* additive
  change or bump the version.
- `stats_payload_missing_ledger_is_available_false_and_pathless` — pins the stable
  `available:false` shape and re-asserts the content-free red line (`db_path` never exposed).

The production plan (doc 03) adds the matching guard for the read endpoints: a snapshot test
asserting **`GET /api/v1/stats` is byte-equal to `trimwire stats --json`** (minus the
API-stripped `db_path`). Because they come from one builder, that test both documents the
contract and proves the CLI and API can't drift apart. The repo already uses `insta`
snapshots and has a `stats --json` shape test to extend.

### 3. The frontend codes against the contract, not the CLI

The web UI / app consume typed responses from `/api/v1`. They never assume a CLI flag exists,
never scrape CLI output, and tolerate extra fields. A field they rely on is, by rule (1), only
ever added or version-bumped — never removed under them.

## What this means in practice

| If a maintainer… | Cockpit impact | Why |
|---|---|---|
| renames a CLI flag (`--json` → `--format json`) | **none** | CLI surface, not the API contract |
| reformats `trimwire stats` human output | **none** | the cockpit reads `/api/v1/stats`, not stdout |
| adds a new field to the ledger `Report` | **none** (frontend ignores unknowns) | additive change inside `v1` |
| renames a field in the shared JSON builder | **CI fails** (contract test) → fix or `/api/v2` | the one real risk, now gated |
| adds a whole new CLI subcommand | **none** until exposed | the API exposes endpoints deliberately, one by one |

## Status in the POC

- ✅ Cockpit talks only to `/api/v1` (no CLI shell-out anywhere in `src/admin/`).
- ✅ `"control_api": "v1-poc"` version marker on `/api/v1/version`.
- ✅ Two contract tests pinning the `version`/`stats` shapes.
- ⏳ (Production, doc 03 PR-1) extract the shared `--json` builders so CLI and API provably
  share one source; add the byte-equality snapshot test.

The guarantee, in one sentence: **the cockpit depends on a versioned, contract-tested JSON API
— not on the CLI — so CLI commands can change freely, and the one thing that could break it
(a shared-shape change) fails CI before it ships.**
