# collector/ — trimwire telemetry collector

A Cloudflare **Worker + D1** that receives the opt-in, content-free payloads from
`trimwire share stats` (community stats) and `trimwire share benchmark` (the
model-quality leaderboard), stores one coarse row per upload, and serves
**k-anonymous, aggregate-only** JSON for the public dashboard + leaderboard. The
payload contract is `docs/TELEMETRY.md`.

> **Deployed at `api.trimwire.dev`.** The repo CI does NOT auto-deploy — the
> maintainer runs `wrangler deploy` by hand (see "Deploy" below). The Rust client
> only uploads on explicit opt-in (`trimwire share enable` / `--yes`); nothing
> phones home on its own. Forks: swap in your own account/D1/KV ids.

## What it does

```
trimwire share stats     ──POST /ingest──────────▶ Worker (the only public surface)
trimwire share benchmark ──POST /ingest-benchmark─▶   ├─ per-IP daily rate-limit (REQUIRED — fail-closed)
                                                      ├─ size cap + validate*()  (mirror of the
                                                      │   client's content-free guarantee; rejects any
                                                      │   extra key / out-of-set value / unknown strategy,
                                                      │   an implausible strategy_share sum, or dup fires)
                                                      ├─ never logs or stores the client IP
                                                      └─ INSERT one coarse row → D1 (never internet-exposed)

GET /aggregates.json ──▶ aggregate(rows, K=10)       [edge-cached ~10 min via Cache API]
GET /benchmarks.json ──▶ aggregateBenchmark(rows, BENCH_K=5)
                           ├─ re-sanitize each row on read (legacy/bad row can't skew)
                           ├─ GROUP BY the quasi-identifier key
                           ├─ SUPPRESS groups with < K contributors (k-anonymity)
                           ├─ l-diversity gate (>=3 distinct) on marginal distributions (stats)
                           └─ INTENSIVE metrics only (means/shares/distributions, never sums)
```

Every response carries `X-Content-Type-Options: nosniff` + `Referrer-Policy:
no-referrer`; the aggregate adds `Content-Security-Policy: default-src 'none'`.
The static dashboard fetches `/aggregates.json` (CORS-open). The Cache API entry
means a GET flood doesn't rescan D1; for very high traffic the maintainer can
still move the aggregate to a scheduled (cron) Worker that writes
`aggregates.json` to KV/R2 — the `aggregate()` logic is unchanged.

## Files

- `src/validate.ts` — pure ingest validation (the privacy gate), stats + benchmark. Unit-tested.
- `src/aggregate.ts` — pure k-anonymity + l-diversity + intensive aggregation, stats + benchmark. Unit-tested.
- `src/index.ts` — the Worker wiring (all 4 routes, D1 reads/writes). Run via `wrangler dev`.
- `schema.sql` — the D1 tables (`telemetry` + `benchmark`) + grouping indexes. Content-free columns only.
- `wrangler.toml` — the **canonical deployed config** (real account-scoped, non-secret D1/KV ids + `K`/`BENCH_K`). Forks: replace the ids/bindings with your own.

## Local development / test

```sh
cd collector
npm install
npm test        # pure-logic tests (validate + aggregate) — no Cloudflare account needed
npm run typecheck

# exercise the Worker end-to-end locally (needs the wrangler CLI):
npm run db:init:local                 # apply schema.sql to a local D1
# /ingest fails closed (503) without the rate-limiter; allow it for local dev:
npx wrangler dev --var ALLOW_UNTHROTTLED:true   # local Worker + local D1
#   POST a sample payload (ALL keys are required — exact key set):
curl -s -X POST localhost:8787/ingest -H 'content-type: application/json' \
  -d '{
    "schema_version": 1,
    "sent_day": "2026-06-09",
    "trimwire_version": "0.1",
    "harness": "claude-code",
    "model_family": "claude-sonnet-4-6",
    "profile": "default",
    "summarizer_backend": "off",
    "summarizer_family": "none",
    "conversation_length_bucket": "50-200",
    "reduction_pct_bucket": 40,
    "cache_hit_pct_bucket": 70,
    "cache_stability_bucket": 9,
    "bytes_saved_bucket": "1mb-10mb",
    "strategy_share": {"bloat_cap": 60, "sliding_window": 40},
    "reprune_enabled": true,
    "simhash_enabled": false,
    "accumulator_enabled": false,
    "os_family": "linux",
    "native_compaction_rate_bucket": 20,
    "strategies_fired": ["bloat_cap", "sliding_window"],
    "summarizer_size_bucket": "none",
    "strategy_any_fired_pct_bucket": 80,
    "summarizer_accept_rate_bucket": "none",
    "summarizer_trigger_rate_bucket": 0,
    "max_session_length_bucket": "50-200",
    "dedup_token": "a3f1e2b4c5d6e7f8a9b0c1d2e3f4a5b6c7d8e9f0a1b2c3d4e5f6a7b8c9d0e1f2",
    "summarizer_backend_won": "off"
  }'
curl -s localhost:8787/aggregates.json   # (sparse until a group reaches K)
```

`npm test` is the fast privacy-critical gate and needs nothing external. The
`wrangler` steps are optional and only needed to drive the live HTTP path.

## Deploy (reference — already done for api.trimwire.dev; steps for a fork/redeploy)

1. `wrangler d1 create trimwire-telemetry` → paste the printed `database_id` into
   `wrangler.toml`.
2. `wrangler d1 execute trimwire-telemetry --remote --file=./schema.sql`
   (`--remote` applies to the PROD D1 — without it the schema only lands on the
   local dev DB).
3. **Create the REQUIRED rate-limiter KV namespace** and paste its id into the
   `[[kv_namespaces]]` block in `wrangler.toml`:
   `wrangler kv namespace create RATE_LIMIT`. The Worker fails closed at
   **runtime** (every `/ingest` returns 503) until a real namespace resolves —
   `wrangler deploy` itself does not validate the id, so don't skip this. Do
   **not** set `ALLOW_UNTHROTTLED` in production.
4. `compatibility_date` is set to a recent date and the **custom domain**
   (`api.trimwire.dev`) is declared as a `[[routes]]` block with `workers_dev =
   false`, so `wrangler deploy` binds the domain and creates DNS automatically
   (the zone is in the same account). The `/aggregates.json` edge cache and a
   trustworthy `CF-Connecting-IP` both depend on this (no-op / spoofable on a
   bare `*.workers.dev`).
5. `wrangler deploy`.
6. Set the resulting Worker URL as the Rust client's `[share] endpoint` (and
   publish it in the docs) **only after** writing a privacy policy.
7. Optionally raise `K` / `MAX_PER_DAY` in `wrangler.toml` as the user base grows.

For the canonical instance this is all done (live at `api.trimwire.dev`, schema
includes both the `telemetry` and `benchmark` tables, privacy policy published).
These steps are the reference for a fork or a redeploy — see `docs/TELEMETRY.md`.

## Pre-deploy security checklist (v3 council review)

A 3-agent review found the Worker architecturally sound (parameterized D1, ingest
fails closed, strict payload allowlist, read-path re-sanitization). Confirm these
deploy-time decisions before `wrangler deploy`:

- [ ] **Custom domain, `workers_dev = false`.** Two things depend on it: (a) the
  `/aggregates.json` edge cache (a no-op on `*.workers.dev` → every GET rescans D1),
  and (b) `CF-Connecting-IP` is only trustworthy behind Cloudflare's edge — on a
  bare `workers.dev` route it can be spoofed, bypassing the rate-limiters.
- [ ] **`RATE_LIMIT` KV bound; `ALLOW_UNTHROTTLED` NOT set.** Ingest fails closed
  (503) without it; the GET backstop (`MAX_AGG_PER_DAY`, default 2000 cache-miss
  reads/IP/day) also needs it.
- [ ] **Accept the soft rate-limit semantics.** The per-IP counters are read-then-write
  (racy), so a concurrent burst can overshoot the cap by ~the degree of concurrency.
  This is deliberate abuse-bounding, not a hard quota — confirm `MAX_PER_DAY` (50) and
  `MAX_AGG_PER_DAY` (2000) are the budgets you want.
- [ ] **k-anonymity `K` ≥ 10** (floored at 5 in code) for the stats cohort threshold.
- [ ] **`BENCH_K` ≥ 5** (floored at 3 in code) for the benchmark-leaderboard threshold.
- Known-latent, fix-if-needed-later (not blockers): `/ingest` has no CORS/OPTIONS
  handler (only matters if a *browser* ever POSTs — the CLI doesn't); a non-SSE
  upstream error response yields a 0-token ledger row (metrics only, not the collector).
