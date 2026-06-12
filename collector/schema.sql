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
