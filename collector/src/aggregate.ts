// k-anonymity aggregation for the trimwire telemetry dashboard.
//
// Pure (no I/O) so it unit-tests without the Workers runtime. Enforces the
// privacy rules from docs/TELEMETRY.md:
//   - group by the quasi-identifier key (version, model_family, profile,
//     summarizer_backend, conversation_length_bucket);
//   - SUPPRESS any group with fewer than K contributing uploads;
//   - publish ONLY intensive metrics (means / shares / distributions) — never
//     extensive sums — so repeat uploads can't inflate a headline total;
//   - l-diversity: a marginal's distribution is shown only when it has >= 3
//     distinct values within the (already K-safe) group.

import { KNOWN_STRATEGIES, SCHEMA_VERSION, type TelemetryRow } from "./validate";

export interface GroupAggregate {
  trimwire_version: string;
  model_family: string;
  profile: string;
  summarizer_backend: string;
  conversation_length_bucket: string;
  /** §3.2: now part of the grouping key. `"none"` for all summarizer_backend=off rows. */
  summarizer_size_bucket: string;
  /** Number of uploads in this group (>= k). An *approximation* of distinct
   *  contributors — telemetry is identity-free (see docs/TELEMETRY.md). */
  contributors: number;
  avg_reduction_pct: number;
  avg_cache_hit_pct: number;
  avg_cache_stability: number; // 0..10
  /** Histogram (counts) of reduction buckets, or null when l-diversity (>=3
   *  distinct values) is not met. Divide by `contributors` for shares. */
  reduction_distribution: Record<string, number> | null;
  /** Mean share (0..100) of each strategy across the group; rows where a
   *  strategy didn't fire count as 0. Only strategies with a non-zero mean are
   *  included. */
  strategy_share_avg: Record<string, number>;
  /** Histogram (counts) of summarizer families within the group, or null when
   *  l-diversity is not met. */
  summarizer_distribution: Record<string, number> | null;
  // ---- v2 marginals ----
  /** % of the group with stable-prefix re-pruning enabled (0..100). */
  reprune_on_pct: number;
  /** % of the group with the opt-in simhash_dedup strategy enabled. */
  simhash_on_pct: number;
  /** % of the group with the local-model accumulator enabled. */
  accumulator_on_pct: number;
  /** Mean of native_compaction_rate_bucket (0..100) — how often Anthropic's own
   *  context_management fired, i.e. the "is trimwire redundant?" signal. */
  avg_native_compaction_rate: number;
  /** Histogram (counts) of os_family within the group, or null when l-diversity
   *  is not met. */
  os_distribution: Record<string, number> | null;
  /** For each strategy, the % of the group's sessions where it fired (0..100).
   *  Covers EVERY strategy that fired at all — including small byte-contributors
   *  absent from strategy_share_avg. Only non-zero rates are included. */
  strategy_fire_rate: Record<string, number>;
  /** Mean of strategy_any_fired_pct_bucket (0..100) — how often any trimwire
   *  strategy fired on average. */
  avg_strategy_any_fired_pct: number;
  /** Histogram (counts) of summarizer_size_bucket within the group, or null when
   *  l-diversity is not met (mirrors summarizer_distribution). */
  summarizer_size_distribution: Record<string, number> | null;
  /** Histogram (counts) of summarizer_accept_rate_bucket (the install rate: "none"
   *  / "0".."100") within the group, or null when l-diversity is not met. */
  summarizer_accept_rate_distribution: Record<string, number> | null;
  /** Mean of summarizer_trigger_rate_bucket (0..100) — how often the summarizer
   *  attempted a model call, on average. */
  avg_summarizer_trigger_rate: number;
  /** Histogram (counts) of max_session_length_bucket within the group, or null
   *  when l-diversity (>=3 distinct values) is not met.  Answers "how long does
   *  the longest session get?" as a tail/pressure signal the median hides.  Same
   *  l-diversity gate as the other length/size distributions. */
  max_session_length_distribution: Record<string, number> | null;
  /** §8C/Q4 marginal: histogram (counts) of the winning backend engine
   *  (`summarizer_backend_won`) within the group, or null when l-diversity is not
   *  met. Answers "which engine actually won" — distinct from `summarizer_backend`
   *  (the *configured* primary) when a fallback fired. NOT in the grouping key. */
  summarizer_backend_won_distribution: Record<string, number> | null;
}

export interface AggregateResult {
  schema_version: number;
  k: number;
  /** A single GLOBAL count of groups hidden for being below k. Not a per-group
   *  label (those stay silent) — see docs/TELEMETRY.md. */
  suppressed_groups: number;
  groups: GroupAggregate[];
}

const L_DIVERSITY_MIN = 3;
/** Drop histogram buckets with fewer than this many contributors. Prevents a
 *  k-safe group's distribution from revealing a SOLE member of a rare bucket
 *  (e.g. the only macOS user in a version×model×profile cell). */
const MIN_BUCKET_COUNT = 2;

/** A distribution (histogram of counts) gated for publication: singleton (and
 *  otherwise too-small) buckets are dropped, then the whole thing is withheld
 *  (null) unless >= L_DIVERSITY_MIN distinct buckets survive. */
function gatedDistribution(
  hist: Record<string, number>,
): Record<string, number> | null {
  const filtered: Record<string, number> = {};
  for (const [key, count] of Object.entries(hist)) {
    if (count >= MIN_BUCKET_COUNT) filtered[key] = count;
  }
  return Object.keys(filtered).length >= L_DIVERSITY_MIN ? filtered : null;
}

/** The 6 quasi-identifier fields that form the k-anon grouping key.
 *
 *  §3.2: `summarizer_size_bucket` is now part of the key so that the local-model
 *  sub-population is split by model size tier.  For `summarizer_backend=off` rows
 *  the bucket is always `"none"`, so they still share one cell — no k-anonymity impact.
 *
 *  Both `TelemetryRow` and `GroupAggregate` structurally satisfy this, so `keyOf`
 *  needs no cast at either call site. */
type GroupKey = Pick<
  TelemetryRow,
  | "trimwire_version"
  | "model_family"
  | "profile"
  | "summarizer_backend"
  | "conversation_length_bucket"
  | "summarizer_size_bucket"
>;

function keyOf(r: GroupKey): string {
  // Unit separator so no concatenation of two field values can collide with a
  // different field split (a bucket value never contains a control char).
  return [
    r.trimwire_version,
    r.model_family,
    r.profile,
    r.summarizer_backend,
    r.conversation_length_bucket,
    r.summarizer_size_bucket,
  ].join("|");
}

function round1(n: number): number {
  return Math.round(n * 10) / 10;
}

/** Aggregate raw rows into k-anonymous, intensive-only group summaries. */
export function aggregate(rows: TelemetryRow[], k: number): AggregateResult {
  const buckets = new Map<string, TelemetryRow[]>();
  for (const r of rows) {
    const key = keyOf(r);
    const arr = buckets.get(key);
    if (arr) arr.push(r);
    else buckets.set(key, [r]);
  }

  const groups: GroupAggregate[] = [];
  let suppressed = 0;

  for (const group of buckets.values()) {
    if (group.length < k) {
      suppressed++;
      continue;
    }
    const n = group.length;
    const first = group[0];

    const avg = (sel: (r: TelemetryRow) => number) =>
      group.reduce((acc, r) => acc + sel(r), 0) / n;

    // reduction distribution + l-diversity gate
    const redHist: Record<string, number> = {};
    for (const r of group) {
      const b = String(r.reduction_pct_bucket);
      redHist[b] = (redHist[b] ?? 0) + 1;
    }
    const reduction_distribution = gatedDistribution(redHist);

    // summarizer distribution + l-diversity gate
    const sumHist: Record<string, number> = {};
    for (const r of group) {
      sumHist[r.summarizer_family] = (sumHist[r.summarizer_family] ?? 0) + 1;
    }
    const summarizer_distribution = gatedDistribution(sumHist);

    // summarizer_size_bucket distribution + l-diversity gate
    const sumSizeHist: Record<string, number> = {};
    for (const r of group) {
      sumSizeHist[r.summarizer_size_bucket] =
        (sumSizeHist[r.summarizer_size_bucket] ?? 0) + 1;
    }
    const summarizer_size_distribution = gatedDistribution(sumSizeHist);

    // summarizer install-rate distribution + l-diversity gate. NOTE: a cohort
    // that never ran the summarizer is all-"none" → 1 distinct bucket, which the
    // l-diversity gate (>= 3) withholds. That gate is what keeps the all-"none"
    // (and 2-distinct) cases from publishing — don't lower L_DIVERSITY_MIN below 3
    // without re-checking this.
    const sumAcceptHist: Record<string, number> = {};
    for (const r of group) {
      sumAcceptHist[r.summarizer_accept_rate_bucket] =
        (sumAcceptHist[r.summarizer_accept_rate_bucket] ?? 0) + 1;
    }
    const summarizer_accept_rate_distribution = gatedDistribution(sumAcceptHist);

    // os_family distribution + l-diversity gate
    const osHist: Record<string, number> = {};
    for (const r of group) {
      osHist[r.os_family] = (osHist[r.os_family] ?? 0) + 1;
    }
    const os_distribution = gatedDistribution(osHist);

    // max_session_length_bucket distribution + l-diversity gate
    const maxSessHist: Record<string, number> = {};
    for (const r of group) {
      maxSessHist[r.max_session_length_bucket] =
        (maxSessHist[r.max_session_length_bucket] ?? 0) + 1;
    }
    const max_session_length_distribution = gatedDistribution(maxSessHist);

    // summarizer_backend_won distribution + l-diversity gate
    const sbwHist: Record<string, number> = {};
    for (const r of group) {
      sbwHist[r.summarizer_backend_won] = (sbwHist[r.summarizer_backend_won] ?? 0) + 1;
    }
    const summarizer_backend_won_distribution = gatedDistribution(sbwHist);

    const pctTrue = (sel: (r: TelemetryRow) => boolean) =>
      round1((group.filter(sel).length / n) * 100);

    // mean strategy share (missing strategy in a row counts as 0)
    const strategy_share_avg: Record<string, number> = {};
    for (const s of KNOWN_STRATEGIES) {
      const mean =
        group.reduce((acc, r) => acc + (r.strategy_share[s] ?? 0), 0) / n;
      if (mean > 0) strategy_share_avg[s] = round1(mean);
    }

    // % of the group's sessions where each strategy fired at all.
    const strategy_fire_rate: Record<string, number> = {};
    for (const s of KNOWN_STRATEGIES) {
      const fired = group.filter((r) => r.strategies_fired.includes(s)).length;
      if (fired > 0) strategy_fire_rate[s] = round1((fired / n) * 100);
    }

    groups.push({
      trimwire_version: first.trimwire_version,
      model_family: first.model_family,
      profile: first.profile,
      summarizer_backend: first.summarizer_backend,
      conversation_length_bucket: first.conversation_length_bucket,
      summarizer_size_bucket: first.summarizer_size_bucket,
      contributors: n,
      avg_reduction_pct: round1(avg((r) => r.reduction_pct_bucket)),
      avg_cache_hit_pct: round1(avg((r) => r.cache_hit_pct_bucket)),
      avg_cache_stability: round1(avg((r) => r.cache_stability_bucket)),
      reduction_distribution,
      strategy_share_avg,
      summarizer_distribution,
      reprune_on_pct: pctTrue((r) => r.reprune_enabled),
      simhash_on_pct: pctTrue((r) => r.simhash_enabled),
      accumulator_on_pct: pctTrue((r) => r.accumulator_enabled),
      avg_native_compaction_rate: round1(avg((r) => r.native_compaction_rate_bucket)),
      os_distribution,
      strategy_fire_rate,
      avg_strategy_any_fired_pct: round1(avg((r) => r.strategy_any_fired_pct_bucket)),
      summarizer_size_distribution,
      summarizer_accept_rate_distribution,
      avg_summarizer_trigger_rate: round1(avg((r) => r.summarizer_trigger_rate_bucket)),
      max_session_length_distribution,
      summarizer_backend_won_distribution,
    });
  }

  // Stable, deterministic ordering (largest groups first, then by key).
  groups.sort(
    (a, b) =>
      b.contributors - a.contributors || keyOf(a).localeCompare(keyOf(b)),
  );

  return { schema_version: SCHEMA_VERSION, k, suppressed_groups: suppressed, groups };
}
