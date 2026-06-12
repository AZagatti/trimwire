import { describe, it, expect } from "vitest";
import { aggregate } from "../src/aggregate";
import type { TelemetryRow } from "../src/validate";

function row(over: Partial<TelemetryRow> = {}): TelemetryRow {
  return {
    schema_version: 1,
    sent_day: "2026-06-06",
    trimwire_version: "0.1",
    harness: "claude-code",
    model_family: "claude-opus",
    profile: "default",
    summarizer_backend: "off",
    summarizer_family: "none",
    conversation_length_bucket: "50-200",
    reduction_pct_bucket: 40,
    cache_hit_pct_bucket: 70,
    cache_stability_bucket: 9,
    bytes_saved_bucket: "1mb-10mb",
    strategy_share: { bloat_cap: 60, sliding_window: 40 },
    reprune_enabled: true,
    simhash_enabled: false,
    accumulator_enabled: false,
    os_family: "linux",
    native_compaction_rate_bucket: 0,
    strategies_fired: ["bloat_cap", "sliding_window"],
    summarizer_size_bucket: "none",
    strategy_any_fired_pct_bucket: 0,
    summarizer_accept_rate_bucket: "none",
    summarizer_trigger_rate_bucket: 0,
    max_session_length_bucket: "<10",
    dedup_token: "a".repeat(64),
    summarizer_backend_won: "off",
    ...over,
  };
}

describe("aggregate k-anonymity", () => {
  it("handles an empty table", () => {
    const res = aggregate([], 10);
    expect(res.groups).toEqual([]);
    expect(res.suppressed_groups).toBe(0);
    expect(res.k).toBe(10);
  });

  it("suppresses groups below k", () => {
    const rows = [row(), row(), row(), row()]; // 4 in one group
    const res = aggregate(rows, 5);
    expect(res.groups.length).toBe(0);
    expect(res.suppressed_groups).toBe(1);
  });

  it("publishes a group at exactly k", () => {
    const rows = Array.from({ length: 5 }, () => row());
    const res = aggregate(rows, 5);
    expect(res.groups.length).toBe(1);
    expect(res.groups[0].contributors).toBe(5);
  });

  it("separates distinct quasi-identifier groups and suppresses small ones", () => {
    const rows = [
      ...Array.from({ length: 6 }, () => row({ profile: "default" })),
      ...Array.from({ length: 3 }, () => row({ profile: "gentle" })),
    ];
    const res = aggregate(rows, 5);
    expect(res.groups.length).toBe(1);
    expect(res.groups[0].profile).toBe("default");
    expect(res.suppressed_groups).toBe(1);
  });

  it("separates cohorts by harness (a grouping-key field)", () => {
    const rows = [
      ...Array.from({ length: 5 }, () => row({ harness: "claude-code" })),
      ...Array.from({ length: 5 }, () => row({ harness: "aider" })),
    ];
    const res = aggregate(rows, 5);
    expect(res.groups.length).toBe(2);
    expect(new Set(res.groups.map((g) => g.harness))).toEqual(
      new Set(["claude-code", "aider"]),
    );
  });

  it("computes intensive means (not sums) so repeats don't inflate totals", () => {
    const rows = Array.from({ length: 6 }, (_, i) =>
      row({ reduction_pct_bucket: i < 3 ? 40 : 60 }),
    );
    const res = aggregate(rows, 5);
    expect(res.groups[0].avg_reduction_pct).toBe(50); // mean of 40 and 60
    // no field is an extensive sum of bytes/sessions
    expect(res.groups[0]).not.toHaveProperty("total_bytes_saved");
  });

  it("withholds reduction distribution until l-diversity (>=3 buckets, each >=2)", () => {
    const same = Array.from({ length: 5 }, () => row({ reduction_pct_bucket: 40 }));
    expect(aggregate(same, 5).groups[0].reduction_distribution).toBeNull();

    // all-singleton buckets are suppressed → withheld even though 5 distinct
    const singletons = [
      row({ reduction_pct_bucket: 30 }),
      row({ reduction_pct_bucket: 40 }),
      row({ reduction_pct_bucket: 50 }),
      row({ reduction_pct_bucket: 60 }),
      row({ reduction_pct_bucket: 70 }),
    ];
    expect(aggregate(singletons, 5).groups[0].reduction_distribution).toBeNull();

    // 3 buckets each with >=2 contributors → published
    const diverse = [
      row({ reduction_pct_bucket: 30 }), row({ reduction_pct_bucket: 30 }),
      row({ reduction_pct_bucket: 40 }), row({ reduction_pct_bucket: 40 }),
      row({ reduction_pct_bucket: 50 }), row({ reduction_pct_bucket: 50 }),
    ];
    const dist = aggregate(diverse, 5).groups[0].reduction_distribution;
    expect(dist).not.toBeNull();
    expect(Object.keys(dist as object).length).toBe(3);
  });

  it("suppresses a singleton bucket so a k-safe group can't reveal a sole member", () => {
    // 8 linux + 1 macos + 1 windows: l-diversity is met (3 distinct) but the
    // two singletons must be dropped → only linux survives → withheld entirely.
    const rows = [
      ...Array.from({ length: 8 }, () => row({ os_family: "linux" })),
      row({ os_family: "macos" }),
      row({ os_family: "windows" }),
    ];
    expect(aggregate(rows, 5).groups[0].os_distribution).toBeNull();
  });

  it("aggregates v2 marginals as intensive rates + l-diversity-gated os dist", () => {
    const rows = [
      ...Array.from({ length: 3 }, () => row({ reprune_enabled: true, os_family: "linux", native_compaction_rate_bucket: 20 })),
      ...Array.from({ length: 2 }, () => row({ reprune_enabled: false, os_family: "macos", native_compaction_rate_bucket: 0 })),
    ];
    const g = aggregate(rows, 5).groups[0];
    expect(g.reprune_on_pct).toBe(60); // 3/5
    expect(g.simhash_on_pct).toBe(0);
    // mean native compaction rate = (20*3 + 0*2)/5 = 12
    expect(g.avg_native_compaction_rate).toBe(12);
    // only 2 distinct os values (<3) → l-diversity withholds the distribution
    expect(g.os_distribution).toBeNull();
  });

  it("publishes os distribution once >=3 buckets each have >=2 contributors", () => {
    const rows = [
      row({ os_family: "linux" }), row({ os_family: "linux" }),
      row({ os_family: "macos" }), row({ os_family: "macos" }),
      row({ os_family: "windows" }), row({ os_family: "windows" }),
    ];
    const g = aggregate(rows, 6).groups[0];
    expect(g.os_distribution).not.toBeNull();
    expect((g.os_distribution as Record<string, number>).linux).toBe(2);
  });

  it("computes per-strategy fire-rate across the group (every strategy, incl. small ones)", () => {
    const rows = [
      ...Array.from({ length: 3 }, () => row({ strategies_fired: ["bloat_cap", "stale_reads"] })),
      ...Array.from({ length: 1 }, () => row({ strategies_fired: ["bloat_cap"] })),
      ...Array.from({ length: 1 }, () => row({ strategies_fired: [] })),
    ];
    const g = aggregate(rows, 5).groups[0];
    // bloat_cap fired in 4/5 = 80%; stale_reads in 3/5 = 60%
    expect(g.strategy_fire_rate.bloat_cap).toBe(80);
    expect(g.strategy_fire_rate.stale_reads).toBe(60);
    // a strategy that never fired is omitted (not 0-noise)
    expect(g.strategy_fire_rate.image_strip).toBeUndefined();
  });

  it("averages strategy share treating a missing strategy as 0", () => {
    const rows = [
      row({ strategy_share: { bloat_cap: 100 } }),
      ...Array.from({ length: 4 }, () => row({ strategy_share: {} })),
    ];
    const res = aggregate(rows, 5);
    // bloat_cap mean = 100 / 5 = 20 (the four rows without it count as 0)
    expect(res.groups[0].strategy_share_avg.bloat_cap).toBe(20);
  });

  it("computes avg_strategy_any_fired_pct and summarizer_size_distribution", () => {
    // 3 rows fired 60%, 1 fired 100%, 1 fired 0% → mean = (60*3+100+0)/5 = 56
    const rows = [
      ...Array.from({ length: 3 }, () => row({ strategy_any_fired_pct_bucket: 60 })),
      row({ strategy_any_fired_pct_bucket: 100 }),
      row({ strategy_any_fired_pct_bucket: 0 }),
    ];
    const g = aggregate(rows, 5).groups[0];
    expect(g.avg_strategy_any_fired_pct).toBe(56);

    // §3.2: summarizer_size_bucket is now part of the grouping key, so within a
    // group all rows share the same size bucket value → always 1 distinct bucket →
    // l-diversity gate always withholds the within-group distribution (it's null).
    // The meaningful cross-group signal comes from which groups exist in the result.
    expect(g.summarizer_size_distribution).toBeNull();

    // Rows with different size buckets go into SEPARATE groups (one per bucket).
    // Each group of 2 is suppressed at k=3 but visible at k=2.
    const mixed = [
      row({ summarizer_size_bucket: "none" }), row({ summarizer_size_bucket: "none" }),
      row({ summarizer_size_bucket: "3-4b" }), row({ summarizer_size_bucket: "3-4b" }),
      row({ summarizer_size_bucket: "5-9b" }), row({ summarizer_size_bucket: "5-9b" }),
    ];
    const result = aggregate(mixed, 2);
    // 3 separate groups (one per bucket), each with 2 rows — all published at k=2.
    expect(result.groups.length).toBe(3);
    // Each group's summarizer_size_bucket is the key value (not a distribution).
    const buckets = result.groups.map((g) => g.summarizer_size_bucket).sort();
    expect(buckets).toEqual(["3-4b", "5-9b", "none"]);
    // Within each group, the size distribution is always null (1 distinct bucket → no l-diversity).
    for (const g of result.groups) {
      expect(g.summarizer_size_distribution).toBeNull();
    }
  });

  it("computes the summarizer install-rate distribution + mean trigger rate", () => {
    // trigger rate: (20*2 + 40*2 + 60) / 5 = 36; install-rate buckets l-diversity-gated.
    const rows = [
      row({ summarizer_trigger_rate_bucket: 20, summarizer_accept_rate_bucket: "70" }),
      row({ summarizer_trigger_rate_bucket: 20, summarizer_accept_rate_bucket: "70" }),
      row({ summarizer_trigger_rate_bucket: 40, summarizer_accept_rate_bucket: "80" }),
      row({ summarizer_trigger_rate_bucket: 40, summarizer_accept_rate_bucket: "80" }),
      row({ summarizer_trigger_rate_bucket: 60, summarizer_accept_rate_bucket: "90" }),
    ];
    const g = aggregate(rows, 5).groups[0];
    expect(g.avg_summarizer_trigger_rate).toBe(36);
    // 3 distinct accept-rate buckets, but only "70" and "80" have >= 2 → after the
    // singleton drop only 2 buckets survive (< l-diversity 3) → withheld.
    expect(g.summarizer_accept_rate_distribution).toBeNull();
  });
  it("distributes summarizer_backend_won as a marginal (not grouping key)", () => {
    // 3 "off" + 4 "local" + 3 "api" in the same group → l-diversity satisfied (3 buckets, each >=2)
    const rows = [
      ...Array.from({ length: 3 }, () => row({ summarizer_backend_won: "off" })),
      ...Array.from({ length: 4 }, () => row({ summarizer_backend_won: "local" })),
      ...Array.from({ length: 3 }, () => row({ summarizer_backend_won: "api" })),
    ];
    const g = aggregate(rows, 5).groups[0];
    expect(g).toBeDefined();
    // Different summarizer_backend_won values must NOT split into different groups
    // (it is a marginal, not part of the grouping key).
    expect(g.contributors).toBe(10);
    expect(g.summarizer_backend_won_distribution).toEqual({ off: 3, local: 4, api: 3 });
  });

  it("withholds summarizer_backend_won_distribution when l-diversity not met", () => {
    // 8 "off" + 2 "local" — after MIN_BUCKET_COUNT (>=2) filter, 2 buckets survive (<3).
    const rows = [
      ...Array.from({ length: 8 }, () => row({ summarizer_backend_won: "off" })),
      ...Array.from({ length: 2 }, () => row({ summarizer_backend_won: "local" })),
    ];
    const g = aggregate(rows, 5).groups[0];
    expect(g.summarizer_backend_won_distribution).toBeNull();
  });
});
