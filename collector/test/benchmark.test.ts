import { describe, it, expect } from "vitest";
import { validateBenchmarkPayload } from "../src/validate";
import { aggregateBenchmark } from "../src/aggregate";
import type { BenchmarkRow } from "../src/validate";

function goodBenchmark() {
  return {
    schema_version: 1,
    sent_day: "2026-06-13",
    trimwire_version: "0.2",
    corpus_version: "1",
    model_family: "qwen3.5",
    model_size_bucket: "3-4b",
    retention_bucket: 100,
    compression_bucket: 50,
    false_done_count: "0",
    produced_usable_summary: true,
    os_family: "linux",
  };
}

describe("validateBenchmarkPayload", () => {
  it("accepts a well-formed content-free row", () => {
    const r = validateBenchmarkPayload(goodBenchmark());
    expect(r.ok).toBe(true);
  });

  it("rejects an unexpected key (fail closed on smuggling)", () => {
    const r = validateBenchmarkPayload({ ...goodBenchmark(), raw_model_tag: "qwen3.5:4b" });
    expect(r).toEqual({ ok: false, error: "unexpected key" });
  });

  it("rejects a missing key", () => {
    const p: Record<string, unknown> = goodBenchmark();
    delete p.corpus_version;
    expect(validateBenchmarkPayload(p)).toEqual({ ok: false, error: "missing key" });
  });

  it("rejects a wrong schema_version", () => {
    expect(validateBenchmarkPayload({ ...goodBenchmark(), schema_version: 2 })).toEqual({
      ok: false,
      error: "schema_version mismatch",
    });
  });

  it("rejects a non-bucketed retention (must be a 10pp step)", () => {
    expect(validateBenchmarkPayload({ ...goodBenchmark(), retention_bucket: 95 })).toEqual({
      ok: false,
      error: "bad retention_bucket",
    });
  });

  it("rejects an out-of-set false_done_count", () => {
    expect(validateBenchmarkPayload({ ...goodBenchmark(), false_done_count: "3" })).toEqual({
      ok: false,
      error: "bad false_done_count",
    });
  });

  it("rejects a raw model tag in model_family (only families/none/other allowed)", () => {
    expect(validateBenchmarkPayload({ ...goodBenchmark(), model_family: "qwen3.5:4b" })).toEqual({
      ok: false,
      error: "bad model_family",
    });
  });

  it("rejects junk corpus_version", () => {
    expect(validateBenchmarkPayload({ ...goodBenchmark(), corpus_version: "v1" })).toEqual({
      ok: false,
      error: "bad corpus_version",
    });
  });

  it("rejects a non-boolean produced_usable_summary", () => {
    expect(validateBenchmarkPayload({ ...goodBenchmark(), produced_usable_summary: 1 })).toEqual({
      ok: false,
      error: "bad produced_usable_summary",
    });
  });
});

function brow(over: Partial<BenchmarkRow> = {}): BenchmarkRow {
  return {
    schema_version: 1,
    sent_day: "2026-06-13",
    trimwire_version: "0.2",
    corpus_version: "1",
    model_family: "qwen3.5",
    model_size_bucket: "3-4b",
    retention_bucket: 100,
    compression_bucket: 50,
    false_done_count: "0",
    produced_usable_summary: true,
    os_family: "linux",
    ...over,
  };
}

describe("aggregateBenchmark", () => {
  it("suppresses a group below k and publishes nothing", () => {
    const rows = [brow(), brow(), brow()]; // 3 < k=5
    const agg = aggregateBenchmark(rows, 5);
    expect(agg.models).toHaveLength(0);
    expect(agg.suppressed_groups).toBe(1);
    expect(agg.corpus_version).toBe("1");
  });

  it("publishes a group at/above k with intensive means and rates", () => {
    const rows = [
      brow({ retention_bucket: 100, compression_bucket: 50 }),
      brow({ retention_bucket: 90, compression_bucket: 50 }),
      brow({ retention_bucket: 100, compression_bucket: 60, false_done_count: "1" }),
    ];
    const agg = aggregateBenchmark(rows, 3);
    expect(agg.models).toHaveLength(1);
    const m = agg.models[0];
    expect(m.model_family).toBe("qwen3.5");
    expect(m.model_size_bucket).toBe("3-4b");
    expect(m.contributors).toBe(3);
    expect(m.avg_retention).toBeCloseTo((100 + 90 + 100) / 3, 1);
    expect(m.avg_compression).toBeCloseTo((50 + 50 + 60) / 3, 1);
    // one of three rows had a non-zero false_done_count
    expect(m.false_done_rate).toBeCloseTo(33.3, 1);
    expect(m.usable_pct).toBe(100);
  });

  it("only publishes the latest corpus_version (cross-corpus rows aren't comparable)", () => {
    const rows = [
      ...Array.from({ length: 5 }, () => brow({ corpus_version: "1" })),
      ...Array.from({ length: 5 }, () => brow({ corpus_version: "2", model_size_bucket: "5-9b" })),
    ];
    const agg = aggregateBenchmark(rows, 5);
    expect(agg.corpus_version).toBe("2");
    expect(agg.models).toHaveLength(1);
    expect(agg.models[0].model_size_bucket).toBe("5-9b");
    // the corpus-1 group is a different corpus, not counted as suppressed here
    expect(agg.suppressed_groups).toBe(0);
  });

  it("returns an empty leaderboard for no rows", () => {
    const agg = aggregateBenchmark([], 5);
    expect(agg).toEqual({
      schema_version: 1,
      k: 5,
      corpus_version: null,
      suppressed_groups: 0,
      models: [],
    });
  });
});
