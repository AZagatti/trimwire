import { describe, it, expect } from "vitest";
import {
  validatePayload,
  sanitizeStrategyShare,
  sanitizeStrategiesFired,
} from "../src/validate";

function goodPayload() {
  return {
    schema_version: 1,
    sent_day: "2026-06-06",
    trimwire_version: "0.1",
    harness: "claude-code",
    // §3.3: model_family is now tier + major.minor (no closed enum, shape check)
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
    // §3.4: max session length bucket (same scheme as conversation_length_bucket)
    max_session_length_bucket: "50-200",
    // §3.1: day-scoped dedup token (64 lowercase hex chars)
    dedup_token: "a".repeat(64),
    // §8C/Q4: which engine actually won after fallback cascade
    summarizer_backend_won: "off",
  };
}

describe("validatePayload", () => {
  it("accepts a well-formed payload", () => {
    const r = validatePayload(goodPayload());
    expect(r.ok).toBe(true);
  });

  it("rejects an unexpected (smuggled) key", () => {
    const p = { ...goodPayload(), leaked_path: "/home/me/secret" };
    const r = validatePayload(p);
    expect(r.ok).toBe(false);
  });

  it("rejects a missing key", () => {
    const p = goodPayload() as Record<string, unknown>;
    delete p.model_family;
    expect(validatePayload(p).ok).toBe(false);
  });

  it("rejects a wrong schema_version", () => {
    expect(validatePayload({ ...goodPayload(), schema_version: 0 }).ok).toBe(false);
    expect(validatePayload({ ...goodPayload(), schema_version: 2 }).ok).toBe(false);
  });

  it("validates strategies_fired (array of known names only)", () => {
    expect(validatePayload({ ...goodPayload(), strategies_fired: [] }).ok).toBe(true);
    expect(validatePayload({ ...goodPayload(), strategies_fired: "bloat_cap" }).ok).toBe(false);
    expect(
      validatePayload({ ...goodPayload(), strategies_fired: ["exfiltrate"] }).ok,
    ).toBe(false);
  });

  it("validates the v2 marginal fields", () => {
    expect(validatePayload({ ...goodPayload(), os_family: "plan9" }).ok).toBe(false);
    expect(validatePayload({ ...goodPayload(), reprune_enabled: "yes" }).ok).toBe(false);
    expect(
      validatePayload({ ...goodPayload(), native_compaction_rate_bucket: 37 }).ok,
    ).toBe(false);
    expect(
      validatePayload({ ...goodPayload(), native_compaction_rate_bucket: 100 }).ok,
    ).toBe(true);
  });

  it("rejects a high-cardinality value in a closed enum", () => {
    const p = { ...goodPayload(), model_family: "claude-opus-4-5-20251101" };
    expect(validatePayload(p).ok).toBe(false);
  });

  it("rejects a non-bucketed (raw) percentage", () => {
    expect(validatePayload({ ...goodPayload(), reduction_pct_bucket: 42 }).ok).toBe(false);
    expect(validatePayload({ ...goodPayload(), cache_hit_pct_bucket: 73 }).ok).toBe(false);
  });

  it("rejects out-of-range stability", () => {
    expect(validatePayload({ ...goodPayload(), cache_stability_bucket: 11 }).ok).toBe(false);
  });

  it("rejects a bad sent_day format", () => {
    expect(validatePayload({ ...goodPayload(), sent_day: "2026-6-6T12:00" }).ok).toBe(false);
  });

  it("rejects impossible and absurd dates", () => {
    expect(validatePayload({ ...goodPayload(), sent_day: "2026-99-99" }).ok).toBe(false);
    expect(validatePayload({ ...goodPayload(), sent_day: "9999-12-31" }).ok).toBe(false);
    expect(validatePayload({ ...goodPayload(), sent_day: "1999-01-01" }).ok).toBe(false);
  });

  it("rejects version strings with leading zeros / overlong components", () => {
    expect(validatePayload({ ...goodPayload(), trimwire_version: "01.02" }).ok).toBe(false);
    expect(
      validatePayload({ ...goodPayload(), trimwire_version: "99999.1" }).ok,
    ).toBe(false);
    expect(validatePayload({ ...goodPayload(), trimwire_version: "dev" }).ok).toBe(true);
  });

  it("rejects an unknown strategy name", () => {
    const p = { ...goodPayload(), strategy_share: { exfiltrate: 50 } };
    expect(validatePayload(p).ok).toBe(false);
  });

  it("rejects a bad strategy share value", () => {
    const p = { ...goodPayload(), strategy_share: { bloat_cap: 42 } };
    expect(validatePayload(p).ok).toBe(false);
  });

  it("rejects a strategy_share that sums implausibly high", () => {
    // every key valid on its own, but the shares can't all be ~100 — that would
    // skew every per-strategy mean in the aggregate.
    const p = {
      ...goodPayload(),
      strategy_share: { bloat_cap: 100, sliding_window: 100, stale_reads: 100 },
    };
    expect(validatePayload(p).ok).toBe(false);
    // a normal partition (sums to 100) is fine
    expect(validatePayload(goodPayload()).ok).toBe(true);
  });

  it("rejects duplicate / overlong strategies_fired", () => {
    expect(
      validatePayload({ ...goodPayload(), strategies_fired: ["bloat_cap", "bloat_cap"] }).ok,
    ).toBe(false);
    // more entries than there are known strategies
    const tooMany = Array(10).fill("bloat_cap");
    expect(validatePayload({ ...goodPayload(), strategies_fired: tooMany }).ok).toBe(false);
  });

  it("validates summarizer_backend_won (same closed set as summarizer_backend)", () => {
    for (const v of ["off", "local", "api"]) {
      expect(validatePayload({ ...goodPayload(), summarizer_backend_won: v }).ok).toBe(true);
    }
    expect(validatePayload({ ...goodPayload(), summarizer_backend_won: "unknown" }).ok).toBe(false);
    expect(validatePayload({ ...goodPayload(), summarizer_backend_won: "" }).ok).toBe(false);
  });

  it("accepts summarizer_backend=local and a known ollama summarizer family", () => {
    const p = {
      ...goodPayload(),
      summarizer_backend: "local",
      summarizer_family: "qwen3.5",
    };
    expect(validatePayload(p).ok).toBe(true);
  });

  it("accepts summarizer_backend=api with api-style summarizer_family", () => {
    const p = {
      ...goodPayload(),
      summarizer_backend: "api",
      summarizer_family: "anthropic",
      summarizer_size_bucket: "api",
    };
    expect(validatePayload(p).ok).toBe(true);

    const p2 = {
      ...goodPayload(),
      summarizer_backend: "api",
      summarizer_family: "openai",
      summarizer_size_bucket: "api",
    };
    expect(validatePayload(p2).ok).toBe(true);
  });

  it("validates summarizer_size_bucket (closed set)", () => {
    // valid values (includes "api" for the api backend)
    for (const v of ["none", "≤2b", "3-4b", "5-9b", "≥10b", "unknown", "api"]) {
      expect(validatePayload({ ...goodPayload(), summarizer_size_bucket: v }).ok).toBe(true);
    }
    // invalid value
    expect(validatePayload({ ...goodPayload(), summarizer_size_bucket: "giant" }).ok).toBe(false);
    expect(validatePayload({ ...goodPayload(), summarizer_size_bucket: 4 }).ok).toBe(false);
  });

  it("validates strategy_any_fired_pct_bucket (0..100 step 10)", () => {
    expect(validatePayload({ ...goodPayload(), strategy_any_fired_pct_bucket: 0 }).ok).toBe(true);
    expect(validatePayload({ ...goodPayload(), strategy_any_fired_pct_bucket: 100 }).ok).toBe(true);
    expect(validatePayload({ ...goodPayload(), strategy_any_fired_pct_bucket: 50 }).ok).toBe(true);
    // not a multiple of 10
    expect(validatePayload({ ...goodPayload(), strategy_any_fired_pct_bucket: 37 }).ok).toBe(false);
    // out of range
    expect(validatePayload({ ...goodPayload(), strategy_any_fired_pct_bucket: 110 }).ok).toBe(false);
    expect(validatePayload({ ...goodPayload(), strategy_any_fired_pct_bucket: -10 }).ok).toBe(false);
  });

  it("validates summarizer rate buckets", () => {
    // accept rate: closed "none"|"0".."100" set
    for (const v of ["none", "0", "70", "100"]) {
      expect(validatePayload({ ...goodPayload(), summarizer_accept_rate_bucket: v }).ok).toBe(true);
    }
    expect(validatePayload({ ...goodPayload(), summarizer_accept_rate_bucket: "75" }).ok).toBe(false);
    expect(validatePayload({ ...goodPayload(), summarizer_accept_rate_bucket: 70 }).ok).toBe(false);
    // trigger rate: int 0..100 step 10
    expect(validatePayload({ ...goodPayload(), summarizer_trigger_rate_bucket: 0 }).ok).toBe(true);
    expect(validatePayload({ ...goodPayload(), summarizer_trigger_rate_bucket: 100 }).ok).toBe(true);
    expect(validatePayload({ ...goodPayload(), summarizer_trigger_rate_bucket: 25 }).ok).toBe(false);
  });

  it("rejects non-objects", () => {
    expect(validatePayload(null).ok).toBe(false);
    expect(validatePayload([1, 2, 3]).ok).toBe(false);
    expect(validatePayload("nope").ok).toBe(false);
  });
});

describe("read-path sanitizers (a stored bad row can't skew/break the page)", () => {
  it("sanitizeStrategyShare drops unknown keys and out-of-range shares", () => {
    const out = sanitizeStrategyShare({
      bloat_cap: 60, // kept
      sliding_window: 40, // kept
      exfiltrate: 50, // unknown → dropped
      stale_reads: 42, // not a step of 5 → dropped
      image_strip: 3, // below 5 → dropped
    });
    expect(out).toEqual({ bloat_cap: 60, sliding_window: 40 });
  });

  it("sanitizeStrategyShare tolerates non-objects", () => {
    expect(sanitizeStrategyShare(null)).toEqual({});
    expect(sanitizeStrategyShare("nope")).toEqual({});
    expect(sanitizeStrategyShare([1, 2])).toEqual({});
  });

  it("sanitizeStrategiesFired keeps known names, deduped", () => {
    expect(
      sanitizeStrategiesFired(["bloat_cap", "bloat_cap", "exfiltrate", "stale_reads"]),
    ).toEqual(["bloat_cap", "stale_reads"]);
    expect(sanitizeStrategiesFired("bloat_cap")).toEqual([]);
    expect(sanitizeStrategiesFired(null)).toEqual([]);
  });
});
