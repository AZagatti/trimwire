import { describe, it, expect } from "vitest";
import { validateBenchmarkPayload } from "../src/validate";
import { aggregateBenchmark } from "../src/aggregate";
import type { BenchmarkRow } from "../src/validate";

function localBenchmark() {
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

function apiBenchmark() {
  return {
    ...localBenchmark(),
    backend: "api",
    provider_style: "anthropic",
    provider_route: "anthropic",
    model_family: "claude-haiku",
    model_bucket: "claude-haiku-4-5",
    model_size_bucket: "api",
  };
}

describe("validateBenchmarkPayload", () => {
  it("accepts a well-formed LOCAL row", () => {
    expect(validateBenchmarkPayload(localBenchmark()).ok).toBe(true);
  });

  it("accepts a well-formed API row (claude bucket)", () => {
    expect(validateBenchmarkPayload(apiBenchmark()).ok).toBe(true);
  });

  it("accepts an API openai/openrouter row (gpt bucket)", () => {
    const p = { ...apiBenchmark(), provider_style: "openai", provider_route: "openrouter", model_family: "gpt", model_bucket: "gpt-4.1-mini" };
    expect(validateBenchmarkPayload(p).ok).toBe(true);
  });

  it("rejects an unexpected key (fail closed on smuggling)", () => {
    expect(validateBenchmarkPayload({ ...localBenchmark(), raw_model_tag: "qwen3.5:4b" })).toEqual({
      ok: false,
      error: "unexpected key",
    });
  });

  it("rejects a missing key", () => {
    const p: Record<string, unknown> = localBenchmark();
    delete p.backend;
    expect(validateBenchmarkPayload(p)).toEqual({ ok: false, error: "missing key" });
  });

  it("rejects a wrong schema_version", () => {
    expect(validateBenchmarkPayload({ ...localBenchmark(), schema_version: 2 })).toEqual({
      ok: false,
      error: "schema_version mismatch",
    });
  });

  it("rejects an out-of-set backend / provider_style / provider_route / error_kind", () => {
    expect(validateBenchmarkPayload({ ...localBenchmark(), backend: "cloud" })).toEqual({ ok: false, error: "bad backend" });
    expect(validateBenchmarkPayload({ ...apiBenchmark(), provider_style: "ollama" })).toEqual({ ok: false, error: "bad provider_style" });
    expect(validateBenchmarkPayload({ ...apiBenchmark(), provider_route: "self" })).toEqual({ ok: false, error: "bad provider_route" });
    expect(validateBenchmarkPayload({ ...localBenchmark(), error_kind: "explode" })).toEqual({ ok: false, error: "bad error_kind" });
  });

  it("rejects an api-dry-run backend (placeholder rows are never real data)", () => {
    // An API model requested without --yes yields a display-only dry-run placeholder
    // (no provider calls made). The CLI never uploads it; the collector also refuses
    // it fail-closed so it can never enter /benchmarks.json or D1 by any path.
    expect(validateBenchmarkPayload({ ...apiBenchmark(), backend: "api-dry-run" })).toEqual({ ok: false, error: "bad backend" });
  });

  it("rejects a non-bucketed retention (must be a 10pp step)", () => {
    expect(validateBenchmarkPayload({ ...localBenchmark(), retention_bucket: 95 })).toEqual({
      ok: false,
      error: "bad retention_bucket",
    });
  });

  it("rejects an out-of-set false_done_count / failed_slice_count / slice_count_bucket", () => {
    expect(validateBenchmarkPayload({ ...localBenchmark(), false_done_count: "3" })).toEqual({ ok: false, error: "bad false_done_count" });
    expect(validateBenchmarkPayload({ ...localBenchmark(), failed_slice_count: "5" })).toEqual({ ok: false, error: "bad failed_slice_count" });
    expect(validateBenchmarkPayload({ ...localBenchmark(), slice_count_bucket: "7" })).toEqual({ ok: false, error: "bad slice_count_bucket" });
  });

  // --- provider-as-family rejection (the core of correct API support) ---
  it("rejects provider names as API model_family", () => {
    for (const provider of ["anthropic", "openai", "openrouter"]) {
      expect(validateBenchmarkPayload({ ...apiBenchmark(), model_family: provider })).toEqual({
        ok: false,
        error: "bad model_family",
      });
    }
  });

  it("rejects provider names as API model_bucket", () => {
    for (const provider of ["anthropic", "openai", "openrouter"]) {
      expect(validateBenchmarkPayload({ ...apiBenchmark(), model_bucket: provider })).toEqual({
        ok: false,
        error: "bad model_bucket",
      });
    }
  });

  it("rejects model_family 'none' on a local row (a benchmarked model has a real tag)", () => {
    expect(validateBenchmarkPayload({ ...localBenchmark(), model_family: "none" })).toEqual({
      ok: false,
      error: "bad model_family",
    });
  });

  it("API row must carry size 'api' (not a local size tier)", () => {
    expect(validateBenchmarkPayload({ ...apiBenchmark(), model_size_bucket: "3-4b" })).toEqual({
      ok: false,
      error: "bad model_size_bucket",
    });
  });

  it("API row must carry a real provider_style (not 'none')", () => {
    expect(validateBenchmarkPayload({ ...apiBenchmark(), provider_style: "none" })).toEqual({
      ok: false,
      error: "bad provider_style",
    });
  });

  it("LOCAL row must carry provider_style/route 'none'", () => {
    expect(validateBenchmarkPayload({ ...localBenchmark(), provider_style: "anthropic" })).toEqual({
      ok: false,
      error: "bad provider for local row",
    });
  });

  it("rejects a local size 'api' / 'none'", () => {
    expect(validateBenchmarkPayload({ ...localBenchmark(), model_size_bucket: "api" })).toEqual({ ok: false, error: "bad model_size_bucket" });
    expect(validateBenchmarkPayload({ ...localBenchmark(), model_size_bucket: "none" })).toEqual({ ok: false, error: "bad model_size_bucket" });
  });

  it("rejects a malformed API model_bucket shape", () => {
    expect(validateBenchmarkPayload({ ...apiBenchmark(), model_bucket: "claude-haiku" })).toEqual({ ok: false, error: "bad model_bucket" }); // missing version
    expect(validateBenchmarkPayload({ ...apiBenchmark(), model_bucket: "gpt-this-is-way-too-long-and-weird" })).toEqual({ ok: false, error: "bad model_bucket" });
  });

  // --- OpenRouter open-model families preserved (not collapsed to "other") ---
  it("accepts open-model api families/buckets with size variants", () => {
    const cases = [
      ["qwen3", "qwen3-32b"],
      ["qwen3", "qwen3-4b"],
      ["gemma-3", "gemma-3-27b"],
      ["llama-3.1", "llama-3.1-8b"],
      ["mistral-small", "mistral-small"],
      ["qwen2.5", "qwen2.5-7b"],
    ];
    for (const [family, bucket] of cases) {
      const p = { ...apiBenchmark(), provider_style: "openai", provider_route: "openrouter", model_family: family, model_bucket: bucket };
      expect(validateBenchmarkPayload(p).ok, `${family}/${bucket}`).toBe(true);
    }
  });

  it("rejects a size on a no-size base + an unknown open family", () => {
    expect(validateBenchmarkPayload({ ...apiBenchmark(), provider_style: "openai", provider_route: "openrouter", model_family: "mistral-small", model_bucket: "mistral-small-22b" })).toEqual({ ok: false, error: "bad model_bucket" });
    expect(validateBenchmarkPayload({ ...apiBenchmark(), provider_style: "openai", provider_route: "openrouter", model_family: "frobnicate", model_bucket: "frobnicate-9b" })).toEqual({ ok: false, error: "bad model_family" });
  });

  // --- cross-field consistency (Point 1) ---
  it("rejects api row with provider_route=none", () => {
    expect(validateBenchmarkPayload({ ...apiBenchmark(), provider_route: "none" })).toEqual({ ok: false, error: "bad provider_route" });
  });

  it("rejects inconsistent failed_slice_count/error_kind", () => {
    expect(validateBenchmarkPayload({ ...localBenchmark(), failed_slice_count: "0", error_kind: "timeout" })).toEqual({ ok: false, error: "inconsistent failed_slice_count/error_kind" });
    expect(validateBenchmarkPayload({ ...localBenchmark(), failed_slice_count: "1", error_kind: "none" })).toEqual({ ok: false, error: "inconsistent failed_slice_count/error_kind" });
  });

  it("rejects inconsistent benchmark_scope/slice_count_bucket", () => {
    expect(validateBenchmarkPayload({ ...localBenchmark(), benchmark_scope: "full_corpus", slice_count_bucket: "2-4" })).toEqual({ ok: false, error: "inconsistent benchmark_scope/slice_count_bucket" });
    expect(validateBenchmarkPayload({ ...localBenchmark(), benchmark_scope: "partial_corpus", slice_count_bucket: "full" })).toEqual({ ok: false, error: "inconsistent benchmark_scope/slice_count_bucket" });
  });

  // --- model_family ↔ model_bucket consistency ---
  it("rejects a local row where model_family !== model_bucket", () => {
    expect(validateBenchmarkPayload({ ...localBenchmark(), model_family: "qwen3.5", model_bucket: "llama3.1" })).toEqual({
      ok: false,
      error: "inconsistent model_family/model_bucket",
    });
  });

  it("rejects api rows where model_family is not the one derived from model_bucket", () => {
    const cases: [string, string][] = [
      ["qwen3", "llama-3.1-8b"],
      ["claude-haiku", "gpt-4.1-mini"],
      ["gpt", "o3-mini"],
      ["gemma-3", "qwen3-32b"],
    ];
    for (const [family, bucket] of cases) {
      // both are individually valid; only the pair is inconsistent.
      expect(validateBenchmarkPayload({ ...apiBenchmark(), provider_style: "openai", provider_route: "openrouter", model_family: family, model_bucket: bucket }), `${family}/${bucket}`).toEqual({
        ok: false,
        error: "inconsistent model_family/model_bucket",
      });
    }
  });

  it("accepts api rows where family matches the derived family", () => {
    const cases: [string, string][] = [
      ["claude-haiku", "claude-haiku-4-5"],
      ["gpt", "gpt-5-mini"],
      ["o-series", "o3-mini"],
      ["qwen3", "qwen3-32b"],
      ["gemma-3", "gemma-3-27b"],
      ["llama-3.1", "llama-3.1-8b"],
      ["mistral-small", "mistral-small"],
      ["deepseek-r1", "deepseek-r1"],
      ["other", "other"],
    ];
    for (const [family, bucket] of cases) {
      expect(validateBenchmarkPayload({ ...apiBenchmark(), provider_style: "openai", provider_route: "openrouter", model_family: family, model_bucket: bucket }).ok, `${family}/${bucket}`).toBe(true);
    }
  });
});

describe("aggregateBenchmark provider_route", () => {
  it("reports a single route when the group agrees, else 'mixed'", () => {
    const api = (route: string): BenchmarkRow =>
      brow({ backend: "api", provider_style: "anthropic", provider_route: route, model_family: "claude-haiku", model_bucket: "claude-haiku-4-5", model_size_bucket: "api" });
    // all anthropic
    const single = aggregateBenchmark([api("anthropic"), api("anthropic"), api("anthropic")], 3);
    expect(single.models[0].provider_route).toBe("anthropic");
    // same model via anthropic + openrouter → one cell, route "mixed"
    const mixed = aggregateBenchmark([api("anthropic"), api("openrouter"), api("anthropic")], 3);
    expect(mixed.models).toHaveLength(1);
    expect(mixed.models[0].provider_route).toBe("mixed");
  });
});

function brow(over: Partial<BenchmarkRow> = {}): BenchmarkRow {
  return { ...(localBenchmark() as unknown as BenchmarkRow), ...over };
}

describe("aggregateBenchmark", () => {
  it("suppresses a group below k", () => {
    const agg = aggregateBenchmark([brow(), brow(), brow()], 5);
    expect(agg.models).toHaveLength(0);
    expect(agg.suppressed_groups).toBe(1);
  });

  it("never merges local and api into one cell, even with same model_bucket", () => {
    const rows = [
      ...Array.from({ length: 5 }, () => brow()), // local qwen3.5
      ...Array.from({ length: 5 }, () =>
        brow({ backend: "api", provider_style: "anthropic", provider_route: "anthropic", model_family: "claude-haiku", model_bucket: "claude-haiku-4-5", model_size_bucket: "api" }),
      ),
    ];
    const agg = aggregateBenchmark(rows, 5);
    expect(agg.models).toHaveLength(2);
    const backends = agg.models.map((m) => m.backend).sort();
    expect(backends).toEqual(["api", "local"]);
    const api = agg.models.find((m) => m.backend === "api")!;
    expect(api.provider_route).toBe("anthropic");
    expect(api.model_bucket).toBe("claude-haiku-4-5");
    expect(api.model_size_bucket).toBe("api");
  });

  it("separates full vs partial corpus into different cells", () => {
    const rows = [
      ...Array.from({ length: 5 }, () => brow({ backend: "api", provider_style: "anthropic", provider_route: "anthropic", model_family: "claude-haiku", model_bucket: "claude-haiku-4-5", model_size_bucket: "api", benchmark_scope: "full_corpus" })),
      ...Array.from({ length: 5 }, () => brow({ backend: "api", provider_style: "anthropic", provider_route: "anthropic", model_family: "claude-haiku", model_bucket: "claude-haiku-4-5", model_size_bucket: "api", benchmark_scope: "partial_corpus", slice_count_bucket: "2-4" })),
    ];
    const agg = aggregateBenchmark(rows, 5);
    expect(agg.models).toHaveLength(2);
    expect(agg.models.map((m) => m.benchmark_scope).sort()).toEqual(["full_corpus", "partial_corpus"]);
  });

  it("computes intensive rates incl. failed_rate", () => {
    const rows = [
      brow({ retention_bucket: 100, compression_bucket: 50 }),
      brow({ retention_bucket: 90, compression_bucket: 50 }),
      brow({ retention_bucket: 100, compression_bucket: 60, false_done_count: "1", failed_slice_count: "1" }),
    ];
    const agg = aggregateBenchmark(rows, 3);
    expect(agg.models).toHaveLength(1);
    const m = agg.models[0];
    expect(m.contributors).toBe(3);
    expect(m.avg_retention).toBeCloseTo((100 + 90 + 100) / 3, 1);
    expect(m.false_done_rate).toBeCloseTo(33.3, 1);
    expect(m.failed_rate).toBeCloseTo(33.3, 1);
    expect(m.usable_pct).toBe(100);
  });

  it("empty input → empty leaderboard", () => {
    expect(aggregateBenchmark([], 5)).toEqual({
      schema_version: 1,
      k: 5,
      corpus_version: null,
      suppressed_groups: 0,
      models: [],
    });
  });
});
