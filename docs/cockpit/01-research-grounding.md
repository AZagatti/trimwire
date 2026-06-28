# 01 — Research & Grounding: what the cockpit builds on

> What trimwire exposes **today**, and the gaps a cockpit must fill. All claims are
> code-grounded (file:line) from a read-only exploration pass. This is the factual base
> the rest of the plan stands on.

## 1. Control surface (what you can drive today)

trimwire runs as a daemon: `trimwire serve` (alias `daemon`), binding **`127.0.0.1:8765`** by
default (`src/config.rs` `ServerConfig::default`), upstream `https://api.anthropic.com`.
Service lifecycle auto-detects **systemd** (Linux), **launchd** (macOS), or a **supervisor**
fallback (WSL2, pidfile `~/.trimwire/daemon.pid`) — `src/cli/service.rs`. systemd/launchd use
**socket activation**.

**The entire HTTP surface today is two endpoints** (`src/proxy/gateway.rs`):

- `GET /healthz` → `{"ok":true,"version":"x.y.z"}`, answered locally, never forwarded.
- `POST /v1/messages` → the Anthropic proxy passthrough (this connection carries the OAuth
  `Authorization: Bearer` token).

**There is no management/control API.** Everything else is CLI:

| Group | Commands |
|---|---|
| Lifecycle | `install`, `uninstall`, `on`, `off`, `status`, `doctor`, `update`/`upgrade` |
| Inspect | `stats` (`--json`), `recall` (`--json`), `preview` (`--json`), `dashboard` (HTML report) |
| Summarizer | `summarizer setup/status/benchmark/probe` |
| Share | `share enable/disable/stats/benchmark` |
| Maintenance | `sweep list/all/file/undo`, `config show/edit` |
| Shell | `statusline`, `completions`, `man` |
| Hidden | `serve`/`daemon`, `run`, `hook` |

**Config** is a figment merge of global `~/.config/trimwire.toml` + project `./.trimwire.toml`
+ `TRIMWIRE_*` env. Crucially, **`[server] upstream` is never read from a project file** —
that key decides where the OAuth token is sent, so honoring it from a cloned repo would let a
project redirect your token (`src/config.rs`, with a regression test). **The daemon reads
config only at startup** — any change needs an `off → on` restart. There is no config *write*
API; only `config edit` ($EDITOR).

### Gaps the cockpit must fill

1. **No local control API** — only `/healthz`. No config read/write, no service toggle over
   HTTP, no strategy tuning.
2. **No live event stream** — state can only be polled.
3. **No hot-reload** — config changes require a restart.
4. **No config-validation endpoint** (only `doctor`, CLI-only).
5. **No structured live metrics export** beyond `stats --json` snapshots.

## 2. Data surface (what a dashboard can show)

The **ledger** is a local SQLite DB (`~/.trimwire/ledger.db`, `src/ledger.rs`) with three
tables: `requests` (one row per `POST /v1/messages`), `summarizer_events`, `upstream_errors`.

**Content-free guarantee (confirmed in code):** the ledger stores only byte counts, token
counts (input / cache_read / cache_creation / output), **prefix hashes** (with `messages`
removed), timestamps, `session_id`, model name, strategy names + per-strategy elided bytes,
TTFT (µs), and status codes. **Never** message content, prompts, tool results, or file paths.
*This is structural: the data to leak simply isn't there.*

Already-available metrics (all via existing `--json`):

- **`stats --json`**: totals, `bytes_saved`, `reduction_pct`, `est_tokens_removed`, `per_day`
  timeseries, `per_strategy` (count + bytes), `cache_stability` ratio, `response_metrics`
  (avg TTFT, token buckets, `cache_hit_pct`), summarizer outcomes, upstream errors.
- **`stats --session <id> --json`**: per-session, per-model breakdown.
- **`recall --json`**: recent sessions (id, last_day, requests, bytes, tokens, model).
- **`preview <file> --json`**: deterministic what-if prune estimate per profile — powers a
  live "what would be pruned" pane *without touching the daemon* (strategies are pure fns).
- **`trimwire dashboard`**: already emits a **self-contained HTML report**
  (`src/cli/dashboard.rs` + `src/cli/dashboard_template.html`).
- **`statusline`**: a live per-session reduction signal Claude Code renders after each turn.

**The 9 strategies** (`src/strategies/mod.rs`): `failed_input_purge`, `stale_input_cap`,
`cross_turn_dedup`, `stale_reads`, `simhash_dedup` (opt-in), `bloat_cap`, `sliding_window`,
`image_strip`, `thinking_strip`. **Two profiles**: `default` (aggressive) and `gentle`
(conservative). The product guardrails forbid a third profile and any intensity dial.

### Useful data that does NOT exist yet (dashboard opportunities, not v1 blockers)

Per-strategy latency cost; strategy fire-rate trend; cache-busting root cause; compression by
model; summarizer cost/latency; reprune checkpoint timeline; sub-agent (sidechain) metrics;
ledger growth/retention pressure. All are additive ledger columns or queries — defer until a
pane proves it needs them.

## 3. Web & telemetry surface (what to align with / reuse)

**Site** (`site/`): **Astro 6 + Starlight 0.40**, TypeScript, **vanilla DOM (no React/Vue)**,
Vitest+jsdom, Playwright. Deployed static (trimwire.dev). It already has interactive,
dependency-free components built exactly like the cockpit needs:

- `CommunityDashboard.astro` + `dashboard.ts` — sortable sticky tables, in-cell bars, KPI
  strip, expandable detail rows.
- `BenchmarkTable.astro`, `Hero.astro` (flow diagram + before/after bars).

**Design system** (`site/src/styles/custom.css`): accent teal **`#2aa39c`** (dark `#178a83` /
light `#9fe7e2`); semantic good `#22c87a` / warn `#e0a000` / bad `#e85d3f`; radius `0.55rem`;
3px top-accent cap; tight headings (`-0.02em`); `tabular-nums` data tables; dark/light parity
via `:root[data-theme]`; `prefers-reduced-motion` respected.

**Collector** (`collector/`): a Cloudflare Worker + D1 that ingests **k-anonymous, content-free
community aggregates** (`POST /ingest`, `GET /aggregates.json`, benchmark equivalents). This is
the *community* backend — **the local cockpit reads the local `ledger.db`, not the collector.**
The collector is a model for privacy discipline, not a data source for the cockpit.

**Brand voice:** restrained, technical, honest ("headroom, not dollars"), data-first, minimal
ornament. The cockpit should honor the *cockpit metaphor* but pick an on-brand, non-hype
visible name (see doc 04).

## 4. Constraints inherited from AGENTS.md (carried into every later doc)

- Single static binary, **no heavy runtime deps**; latency is acceptable, weight is not.
- Transparent **ToS-compliant** proxy: never originate model calls on the subscription token;
  **no detection-evasion** ("transparent" = faithful, not undetectable).
- `main` is protected; every change is a PR through green CI: `fmt + clippy + test`,
  MSRV 1.85, **Python parity oracle**, cargo-deny/audit, 3 cross-platform builds.
- Layer rules: `gateway.rs` must not contain mutation logic; `strategies/*` are pure (no I/O);
  `ledger.rs` is the only SQLite I/O; new modules require an `ARCHITECTURE.md` update.

## Takeaway

The cockpit is **~80% presentation over data and verbs that already exist**, plus **one new
backend component** (the control API, doc 03). That framing — reuse the HTML-report data, the
vanilla-DOM components, the design system, and the CLI verbs — is what keeps the build
proportionate to trimwire's size and ethos.
