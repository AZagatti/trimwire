// Shared, known-VALID payload factories for the collector tests. One source of
// truth so a schema change updates the wire contract in exactly one place — the
// validation tests, the aggregation tests, and the HTTP route tests all build on
// these. Each factory returns a fresh object so callers can spread + override
// (e.g. `{ ...validTelemetry(), profile: "gentle" }`) without mutating others.

/** A well-formed telemetry (`stats --share`) row: passes `validatePayload`. */
export function validTelemetry() {
  return {
    schema_version: 1,
    sent_day: "2026-06-06",
    trimwire_version: "0.1",
    harness: "claude-code",
    model_family: "claude-opus-4-5",
    profile: "default",
    summarizer_backend: "off",
    summarizer_family: "none",
    conversation_length_bucket: "50-200",
    reduction_pct_bucket: 40,
    cache_hit_pct_bucket: 70,
    cache_stability_bucket: 9,
    bytes_saved_bucket: "1mb-10mb",
    strategy_share: { bloat_cap: 60, sliding_window: 30, stale_reads: 10 },
    reprune_enabled: true,
    simhash_enabled: false,
    accumulator_enabled: false,
    os_family: "linux",
    native_compaction_rate_bucket: 20,
    strategies_fired: ["bloat_cap", "sliding_window", "stale_reads"],
    summarizer_size_bucket: "none",
    strategy_any_fired_pct_bucket: 60,
    summarizer_accept_rate_bucket: "none",
    summarizer_trigger_rate_bucket: 0,
    max_session_length_bucket: "50-200",
    // day-scoped dedup token (64 lowercase hex chars); stripped before storage.
    dedup_token: "a".repeat(64),
    summarizer_backend_won: "off",
  };
}

/** A well-formed LOCAL benchmark (`share benchmark`) row: passes
 *  `validateBenchmarkPayload`. */
export function validBenchmark() {
  return {
    schema_version: 1,
    sent_day: "2026-06-14",
    trimwire_version: "0.2",
    corpus_version: "1",
    backend: "local",
    provider_style: "none",
    provider_route: "none",
    model_family: "qwen3.5",
    model_bucket: "qwen3.5",
    model_size_bucket: "3-4b",
    retention_bucket: 100,
    compression_bucket: 50,
    false_done_count: "0",
    produced_usable_summary: true,
    benchmark_scope: "full_corpus",
    slice_count_bucket: "full",
    failed_slice_count: "0",
    error_kind: "none",
    os_family: "linux",
  };
}

/** A well-formed API/provider benchmark row (claude bucket). */
export function validApiBenchmark() {
  return {
    ...validBenchmark(),
    backend: "api",
    provider_style: "anthropic",
    provider_route: "anthropic",
    model_family: "claude-haiku",
    model_bucket: "claude-haiku-4-5",
    model_size_bucket: "api",
  };
}
