-- trimwire telemetry collector — Cloudflare D1 (SQLite) schema.
-- Content-free by construction: every column is a coarse bucket already
-- anonymized client-side (see docs/TELEMETRY.md). No IP, no identity, no
-- timestamp finer than a calendar day.

CREATE TABLE IF NOT EXISTS telemetry (
  id                          INTEGER PRIMARY KEY AUTOINCREMENT,
  -- Server-side UTC date the row was received (defense-in-depth coarse clock;
  -- still day-granular, still content-free).
  received_day                TEXT    NOT NULL,
  schema_version              INTEGER NOT NULL,
  sent_day                    TEXT    NOT NULL,
  trimwire_version            TEXT    NOT NULL,
  -- Agent harness this row came from. Always 'claude-code' today; reserved
  -- values cover the roadmap'd multi-harness adapters. Part of the grouping key.
  harness                     TEXT    NOT NULL DEFAULT 'claude-code',
  model_family                TEXT    NOT NULL,
  profile                     TEXT    NOT NULL,
  -- "off" (model-free) | "local" (ollama) | "api" (cloud provider)
  summarizer_backend          TEXT    NOT NULL,
  summarizer_family           TEXT    NOT NULL,
  conversation_length_bucket  TEXT    NOT NULL,
  reduction_pct_bucket        INTEGER NOT NULL,
  cache_hit_pct_bucket        INTEGER NOT NULL,
  cache_stability_bucket      INTEGER NOT NULL,
  bytes_saved_bucket          TEXT    NOT NULL,
  -- JSON object {strategy_name: share_pct}; keys restricted to the 9 known
  -- strategies, values are 5..100 multiples of 5 (validated at ingest).
  strategy_share              TEXT    NOT NULL,
  -- v2 marginals (booleans stored as 0/1; never in the grouping key).
  reprune_enabled             INTEGER NOT NULL DEFAULT 0,
  simhash_enabled             INTEGER NOT NULL DEFAULT 0,
  accumulator_enabled         INTEGER NOT NULL DEFAULT 0,
  os_family                   TEXT    NOT NULL DEFAULT 'other',
  native_compaction_rate_bucket INTEGER NOT NULL DEFAULT 0,
  -- v3: JSON array of the known strategy names that fired this window.
  strategies_fired            TEXT    NOT NULL DEFAULT '[]',
  -- v4 marginals: size tier of the local summarizer model; % of requests where any strategy fired.
  summarizer_size_bucket        TEXT    NOT NULL DEFAULT 'none',
  strategy_any_fired_pct_bucket INTEGER NOT NULL DEFAULT 0,
  -- v5 marginals: local-model summarizer install rate ("none"/"0".."100") + trigger rate.
  summarizer_accept_rate_bucket  TEXT    NOT NULL DEFAULT 'none',
  summarizer_trigger_rate_bucket INTEGER NOT NULL DEFAULT 0,
  -- §3.4: max session length (same bucket scheme as conversation_length_bucket).
  -- Answers "how long does the longest session in this window get?" — a tail signal.
  max_session_length_bucket      TEXT    NOT NULL DEFAULT '<10',
  -- §8C/Q4 marginal: which engine actually WON the fallback cascade this window
  -- ("off" = no accepted summaries) — distinct from summarizer_backend (the
  -- configured primary). Same closed value set; never in the grouping key.
  summarizer_backend_won         TEXT    NOT NULL DEFAULT 'off',
  -- §3.1: client-generated day-scoped HMAC-SHA256 dedup token.
  -- The client computes hex(HMAC-SHA256(install_id, sent_day)).  Rotating daily means
  -- it can't link two different days (no cross-day identity — see docs/TELEMETRY.md).
  -- INSERT OR REPLACE on this column: a same-day re-upload overrides the prior row.
  -- NULL for pre-§3.1 rows (legacy rows kept as-is; NULL rows never conflict).
  dedup_token                    TEXT
);

-- The k-anonymity grouping key (quasi-identifier). §3.2: summarizer_size_bucket is
-- part of the key so the local-model sub-population splits by model size tier.
-- `harness` is part of the key too (a primary cohort dimension); today every row
-- is 'claude-code' so it's one shared cell with no k-anonymity impact, splitting
-- cleanly once multi-harness adapters land.
CREATE INDEX IF NOT EXISTS idx_group ON telemetry
  (trimwire_version, harness, model_family, profile, summarizer_backend,
   conversation_length_bucket, summarizer_size_bucket);

-- §3.1: INSERT OR REPLACE conflict target — at most one row per dedup_token per day.
-- A same-day re-upload overrides the prior row (the client sees 204 either way).
-- Partial index: NULL dedup_token rows (legacy/pre-§3.1) never participate in the conflict.
CREATE UNIQUE INDEX IF NOT EXISTS idx_dedup ON telemetry (dedup_token)
  WHERE dedup_token IS NOT NULL;

-- ---------------------------------------------------------------------------
-- `trimwire share benchmark` — the model-quality leaderboard (a SEPARATE
-- dataset + route from the stats telemetry above). One row per benchmarked
-- model, scored against a bundled synthetic corpus (never user content).
-- Content-free by construction: only the coarse rank-table columns. Mirrors the
-- BenchmarkPayload wire shape in src/cli/share.rs.
--
-- NOTE: the benchmark payload carries NO dedup token and the collector never
-- stores an IP, so there is no per-identity dedup here — `contributors` counts
-- uploaded rows. This is a deliberately directional leaderboard (and the more
-- private choice: no cross-day identity is retained at all). The k-anonymity
-- suppression in aggregate.ts still hides any group below BENCH_K rows.
CREATE TABLE IF NOT EXISTS benchmark (
  id                       INTEGER PRIMARY KEY AUTOINCREMENT,
  -- Server-side UTC receive date (coarse, content-free; defense in depth).
  received_day             TEXT    NOT NULL,
  schema_version           INTEGER NOT NULL,
  sent_day                 TEXT    NOT NULL,
  trimwire_version         TEXT    NOT NULL,
  -- Which bundled corpus produced the score (rows across versions aren't comparable).
  corpus_version           TEXT    NOT NULL,
  -- "local" (ollama) | "api" (cloud provider). API rows are ranked SEPARATELY.
  backend                  TEXT    NOT NULL DEFAULT 'local',
  -- API style: "none" (local) | "anthropic" | "openai".
  provider_style           TEXT    NOT NULL DEFAULT 'none',
  -- Coarse route bucket from the provider URL (never the raw URL/host):
  -- "none" (local) | anthropic | openai | openrouter | azure | other.
  provider_route           TEXT    NOT NULL DEFAULT 'none',
  -- Broad family. local: ollama family (qwen3.5 / …); api: claude-{tier} | gpt | o-series | other.
  model_family             TEXT    NOT NULL,
  -- Public coarse model id. local: ollama family; api: claude-tier-N-N | gpt-… | o… | other.
  -- Derived from the REAL model — a provider name is never a valid value here.
  model_bucket             TEXT    NOT NULL DEFAULT 'other',
  -- Size tier (local: ≤2b | 3-4b | 5-9b | ≥10b | unknown) or "api" (cloud).
  model_size_bucket        TEXT    NOT NULL,
  -- Fact retention floored to nearest 10 pp (0..100).
  retention_bucket         INTEGER NOT NULL,
  -- Summary compression (1 − out/in) floored to nearest 10 pp (0..100).
  compression_bucket       INTEGER NOT NULL,
  -- Unsupported-completion-claim count, capped client-side: "0" | "1" | "2+".
  false_done_count         TEXT    NOT NULL,
  -- Did every slice yield a usable (non-empty, non-verbatim) summary? (0/1)
  produced_usable_summary  INTEGER NOT NULL,
  -- "full_corpus" | "partial_corpus" — partial (e.g. --max-calls) runs ranked apart.
  benchmark_scope          TEXT    NOT NULL DEFAULT 'full_corpus',
  -- How many slices were scored: "1" | "2-4" | "full".
  slice_count_bucket       TEXT    NOT NULL DEFAULT 'full',
  -- Provider/model call failures across slices, capped: "0" | "1" | "2+".
  failed_slice_count       TEXT    NOT NULL DEFAULT '0',
  -- Coarse error kind across failed slices (closed set; never a raw message).
  error_kind               TEXT    NOT NULL DEFAULT 'none',
  os_family                TEXT    NOT NULL DEFAULT 'other'
);

-- Leaderboard grouping key: a published model row is
-- (corpus_version, backend, model_family, model_bucket, model_size_bucket, benchmark_scope).
CREATE INDEX IF NOT EXISTS idx_bench_group ON benchmark
  (corpus_version, backend, model_family, model_bucket, model_size_bucket, benchmark_scope);
