// Ingest-side payload validation for the trimwire telemetry collector.
//
// This MIRRORS the Rust client's content-free guarantee (src/cli/share.rs):
// the payload must have EXACTLY the allowed keys for the current SCHEMA_VERSION,
// each a coarse bucket within its closed set / range. Anything else is rejected so a
// malformed or hostile upload never enters the aggregate. Pure + dependency-free
// so it unit-tests without the Workers runtime.

// Starts at 1 (v0.1.0, the first release). The collector validates this exact
// version only — no dual-version path yet. A breaking change bumps it in lockstep
// with the Rust client (share.rs); add a transition window if v1 rows exist.
export const SCHEMA_VERSION = 1;

// MIRRORS `src/cli/share.rs` ALLOWED_KEYS across the language boundary — the two
// must stay byte-identical (kept in sync by hand; a drift means the client sends
// a field this rejects with HTTP 400). Update both together.
export const ALLOWED_KEYS = [
  "schema_version",
  "sent_day",
  "trimwire_version",
  "harness",
  "model_family",
  "profile",
  "summarizer_backend",
  "summarizer_family",
  "conversation_length_bucket",
  "reduction_pct_bucket",
  "cache_hit_pct_bucket",
  "cache_stability_bucket",
  "bytes_saved_bucket",
  "strategy_share",
  // marginals (not in the k-anon grouping key):
  "reprune_enabled",
  "simhash_enabled",
  "accumulator_enabled",
  "os_family",
  "native_compaction_rate_bucket",
  "strategies_fired",
  "summarizer_size_bucket",
  "strategy_any_fired_pct_bucket",
  "summarizer_accept_rate_bucket",
  "summarizer_trigger_rate_bucket",
  // §3.4 marginal: max (not median/avg) session length bucket.
  "max_session_length_bucket",
  // §3.1 day-scoped dedup token (HMAC-SHA256 of install_id + sent_day).
  "dedup_token",
  // §8C/Q4 marginal: which engine actually won after fallback cascade.
  "summarizer_backend_won",
] as const;

// MIRRORS `src/ledger.rs` KNOWN_STRATEGIES (the Rust source of truth) across the
// language boundary — keep in sync by hand when a strategy is added/renamed.
export const KNOWN_STRATEGIES = [
  "failed_input_purge",
  "stale_input_cap",
  "cross_turn_dedup",
  "stale_reads",
  "simhash_dedup",
  "bloat_cap",
  "sliding_window",
  "image_strip",
  "thinking_strip",
] as const;

/** A name `strategy_share` / `strategies_fired` may contain. */
export type KnownStrategy = (typeof KNOWN_STRATEGIES)[number];

// model_family uses a shape check (not a closed list) so future model versions
// don't need a code change. Accepts `claude-(opus|sonnet|haiku)-<major>-<minor>` or `other`.
const MODEL_FAMILY_RE = /^claude-(opus|sonnet|haiku)-\d{1,2}-\d{1,2}$/;
const PROFILES = ["default", "gentle", "other"];
/** Closed value set for `harness` — the agent harness trimwire proxied. Always
 *  "claude-code" today; the rest are reserved for the roadmap'd multi-harness
 *  adapters. Deployed with the FULL set so a future client release can emit a new
 *  value with no collector change. MIRRORS `HARNESSES` in src/cli/share.rs. */
export const HARNESSES = ["claude-code", "aider", "opencode", "cline", "codex", "other"];
/** Closed value set for `summarizer_backend` (§3.4 rename of old `local_model`).
 *  "off" = model-free; "local" = local ollama/llama.cpp; "api" = cloud API. */
const SUMMARIZER_BACKENDS = ["off", "local", "api"];
const LENGTH_BUCKETS = ["<10", "10-50", "50-200", ">200"];
const BYTES_BUCKETS = ["<100kb", "100kb-1mb", "1mb-10mb", "10mb-100mb", ">100mb"];
const OS_FAMILIES = ["linux", "macos", "windows", "other"];
/** Ollama model families for the local backend + API-style sentinels for the api
 *  backend ("anthropic", "openai"). "none" and "other" are structural values. */
const SUMMARIZER_FAMILIES = [
  "qwen3.5", "qwen3", "qwen2.5", "granite4.1", "granite3", "llama3.1", "llama3",
  "mistral", "phi4", "phi3", "gemma3", "gemma2",
  // api-backend sentinels (§3.4):
  "anthropic", "openai",
];
/** "none" when backend=off; "api" when backend=api; otherwise a size tier from
 *  the local model tag.
 *
 *  NOTE: "≤2b" and "≥10b" contain intentional non-ASCII Unicode characters
 *  (U+2264 LESS-THAN OR EQUAL TO, U+2265 GREATER-THAN OR EQUAL TO). These are
 *  wire-format values that MUST stay in sync with `src/cli/share.rs`
 *  `SUMMARIZER_SIZE_BUCKETS`. Do NOT silently replace them with ASCII equivalents
 *  (`<=2b` / `>=10b`) — that would silently break Rust↔TS parity. */
const SUMMARIZER_SIZE_BUCKETS = ["none", "≤2b", "3-4b", "5-9b", "≥10b", "unknown", "api"];
// Summarizer install rate: "none" (no quality-relevant attempts) or a 10pp bucket.
const SUMMARIZER_ACCEPT_RATE_BUCKETS = [
  "none", "0", "10", "20", "30", "40", "50", "60", "70", "80", "90", "100",
];

export interface TelemetryRow {
  schema_version: number;
  sent_day: string;
  trimwire_version: string;
  /** Agent harness this row came from. Always "claude-code" today; part of the
   *  k-anon grouping key. See HARNESSES. */
  harness: string;
  model_family: string;
  profile: string;
  /** "off" | "local" | "api" — which summarizer engine is active (§3.4 rename
   *  of the old `local_model` field). */
  summarizer_backend: string;
  summarizer_family: string;
  conversation_length_bucket: string;
  reduction_pct_bucket: number;
  cache_hit_pct_bucket: number;
  cache_stability_bucket: number;
  bytes_saved_bucket: string;
  strategy_share: Partial<Record<KnownStrategy, number>>;
  // marginals
  reprune_enabled: boolean;
  simhash_enabled: boolean;
  accumulator_enabled: boolean;
  os_family: string;
  native_compaction_rate_bucket: number;
  // strategies fired this window (per-strategy fire-rate)
  strategies_fired: KnownStrategy[];
  // v4 marginals
  /** Coarse size tier of the summarizer: "none" when backend=off; "api" when backend=api;
   *  otherwise a size tier from the local model tag. */
  summarizer_size_bucket: string;
  summarizer_accept_rate_bucket: string;
  summarizer_trigger_rate_bucket: number;
  /** % of requests where any strategy fired, floored to nearest 10 pp (0..100). */
  strategy_any_fired_pct_bucket: number;
  /** Maximum session length bucket (same scheme as conversation_length_bucket). */
  max_session_length_bucket: string;
  /** Day-scoped HMAC-SHA256 dedup token: hex(HMAC-SHA256(install_id, sent_day)).
   *  64 lowercase hex chars. Rotates daily — no cross-day identity. */
  dedup_token: string;
  /** §8C/Q4 marginal: which backend engine actually won the fallback cascade and
   *  produced accepted summaries this window. Same closed set as `summarizer_backend`:
   *  "off" = no accepted summaries; "local" = local engine won; "api" = API engine won. */
  summarizer_backend_won: string;
}

export type ValidateResult =
  | { ok: true; value: TelemetryRow }
  | { ok: false; error: string };

// Compile-time guard: ALLOWED_KEYS must equal `keyof TelemetryRow` EXACTLY. Add a
// field to one but not the other and this stops compiling (the type resolves to
// `never`, so `true` is no longer assignable) — catching the documented Rust↔TS
// drift on the TS side at build time. Exported so noUnusedLocals doesn't flag it.
type Exact<A extends string, B extends string> = [A] extends [B]
  ? [B] extends [A]
    ? true
    : never
  : never;
export const _ALLOWED_KEYS_MATCH_ROW: Exact<
  (typeof ALLOWED_KEYS)[number],
  keyof TelemetryRow
> = true;

function isPlainObject(v: unknown): v is Record<string, unknown> {
  return typeof v === "object" && v !== null && !Array.isArray(v);
}

/** A real calendar date in `YYYY-MM-DD`, within [2024-01-01, today+1d]. Rejects
 *  impossible dates ("2026-99-99") and absurd future/past values. */
function isSaneDay(s: unknown): s is string {
  if (typeof s !== "string") return false;
  if (!/^\d{4}-\d{2}-\d{2}$/.test(s)) return false;
  const t = Date.parse(`${s}T00:00:00Z`);
  if (Number.isNaN(t)) return false;
  // round-trip guard: Date.parse is lenient, so confirm it re-serializes the same
  if (new Date(t).toISOString().slice(0, 10) !== s) return false;
  const minT = Date.parse("2024-01-01T00:00:00Z");
  const maxT = Date.now() + 86_400_000; // allow up to 1 day ahead (TZ skew)
  return t >= minT && t <= maxT;
}

function intInRange(v: unknown, min: number, max: number, step: number): v is number {
  return (
    typeof v === "number" &&
    Number.isInteger(v) &&
    v >= min &&
    v <= max &&
    v % step === 0
  );
}

/** Validate one decoded JSON payload. Pure; no I/O. */
export function validatePayload(body: unknown): ValidateResult {
  if (!isPlainObject(body)) return { ok: false, error: "payload is not an object" };

  // Exact key set — no missing, no extra (fail closed on drift / smuggling).
  // Error strings never echo attacker-controlled values (no reflection oracle).
  const keys = Object.keys(body);
  for (const k of keys) {
    if (!ALLOWED_KEYS.includes(k as (typeof ALLOWED_KEYS)[number])) {
      return { ok: false, error: "unexpected key" };
    }
  }
  for (const k of ALLOWED_KEYS) {
    if (!(k in body)) return { ok: false, error: "missing key" };
  }

  if (body.schema_version !== SCHEMA_VERSION) {
    return { ok: false, error: "schema_version mismatch" };
  }
  if (!isSaneDay(body.sent_day)) {
    return { ok: false, error: "bad sent_day" };
  }
  // MAJOR.MINOR (no leading zeros, <=4 digits each) or "dev".
  if (
    typeof body.trimwire_version !== "string" ||
    !/^((0|[1-9]\d{0,3})\.(0|[1-9]\d{0,3})|dev)$/.test(body.trimwire_version)
  ) {
    return { ok: false, error: "bad trimwire_version" };
  }
  // model_family: shape check (tier + major.minor) rather than closed list.
  if (
    typeof body.model_family !== "string" ||
    (body.model_family !== "other" && !MODEL_FAMILY_RE.test(body.model_family))
  ) {
    return { ok: false, error: "bad model_family" };
  }
  const enums: [string, string[]][] = [
    ["harness", HARNESSES],
    ["profile", PROFILES],
    ["summarizer_backend", SUMMARIZER_BACKENDS],
    ["conversation_length_bucket", LENGTH_BUCKETS],
    ["bytes_saved_bucket", BYTES_BUCKETS],
    ["max_session_length_bucket", LENGTH_BUCKETS],
  ];
  for (const [field, set] of enums) {
    const v = (body as Record<string, unknown>)[field];
    if (typeof v !== "string" || !set.includes(v)) {
      return { ok: false, error: `bad ${field}` };
    }
  }
  const sf = body.summarizer_family;
  if (
    typeof sf !== "string" ||
    (sf !== "none" && sf !== "other" && !SUMMARIZER_FAMILIES.includes(sf))
  ) {
    return { ok: false, error: "bad summarizer_family" };
  }
  if (!intInRange(body.reduction_pct_bucket, 0, 100, 5)) {
    return { ok: false, error: "bad reduction_pct_bucket" };
  }
  if (!intInRange(body.cache_hit_pct_bucket, 0, 100, 10)) {
    return { ok: false, error: "bad cache_hit_pct_bucket" };
  }
  if (!intInRange(body.cache_stability_bucket, 0, 10, 1)) {
    return { ok: false, error: "bad cache_stability_bucket" };
  }
  if (!isPlainObject(body.strategy_share)) {
    return { ok: false, error: "bad strategy_share" };
  }
  let shareSum = 0;
  for (const [name, share] of Object.entries(body.strategy_share)) {
    if (!KNOWN_STRATEGIES.includes(name as (typeof KNOWN_STRATEGIES)[number])) {
      return { ok: false, error: "unknown strategy" };
    }
    if (!intInRange(share, 5, 100, 5)) {
      return { ok: false, error: "bad strategy share" };
    }
    shareSum += share;
  }
  // Shares are a partition of bytes saved: a legit set sums to <=100 (each
  // floored down, sub-5 dropped). Allow 10pp of rounding slack, then reject —
  // a crafted row of nine 100s passes the per-key check but would skew every
  // per-strategy mean in the aggregate. (Belt to rowFromDb's read-path braces.)
  if (shareSum > 110) {
    return { ok: false, error: "strategy_share sum too large" };
  }

  // marginals
  for (const field of ["reprune_enabled", "simhash_enabled", "accumulator_enabled"]) {
    if (typeof (body as Record<string, unknown>)[field] !== "boolean") {
      return { ok: false, error: `bad ${field}` };
    }
  }
  if (typeof body.os_family !== "string" || !OS_FAMILIES.includes(body.os_family)) {
    return { ok: false, error: "bad os_family" };
  }
  if (!intInRange(body.native_compaction_rate_bucket, 0, 100, 10)) {
    return { ok: false, error: "bad native_compaction_rate_bucket" };
  }
  // strategies_fired: an array of known strategy names, each at most once.
  if (!Array.isArray(body.strategies_fired)) {
    return { ok: false, error: "bad strategies_fired" };
  }
  // Cap length first to bound the work (the CLI sorts+dedups; a manual POST
  // might not), then require uniqueness so dupes can't inflate a fire-rate.
  if (body.strategies_fired.length > KNOWN_STRATEGIES.length) {
    return { ok: false, error: "too many strategies_fired" };
  }
  const seenFired = new Set<string>();
  for (const s of body.strategies_fired) {
    if (!KNOWN_STRATEGIES.includes(s as (typeof KNOWN_STRATEGIES)[number])) {
      return { ok: false, error: "unknown strategy in strategies_fired" };
    }
    if (seenFired.has(s as string)) {
      return { ok: false, error: "duplicate strategy in strategies_fired" };
    }
    seenFired.add(s as string);
  }

  // v4 marginals
  if (
    typeof body.summarizer_size_bucket !== "string" ||
    !SUMMARIZER_SIZE_BUCKETS.includes(body.summarizer_size_bucket)
  ) {
    return { ok: false, error: "bad summarizer_size_bucket" };
  }
  if (!intInRange(body.strategy_any_fired_pct_bucket, 0, 100, 10)) {
    return { ok: false, error: "bad strategy_any_fired_pct_bucket" };
  }
  if (
    typeof body.summarizer_accept_rate_bucket !== "string" ||
    !SUMMARIZER_ACCEPT_RATE_BUCKETS.includes(body.summarizer_accept_rate_bucket)
  ) {
    return { ok: false, error: "bad summarizer_accept_rate_bucket" };
  }
  if (!intInRange(body.summarizer_trigger_rate_bucket, 0, 100, 10)) {
    return { ok: false, error: "bad summarizer_trigger_rate_bucket" };
  }
  // dedup_token: 64 lowercase hex chars (HMAC-SHA256 output = 32 bytes = 64 hex).
  if (
    typeof body.dedup_token !== "string" ||
    !/^[0-9a-f]{64}$/.test(body.dedup_token)
  ) {
    return { ok: false, error: "bad dedup_token" };
  }

  // summarizer_backend_won: same closed set as summarizer_backend.
  const sbw = (body as Record<string, unknown>).summarizer_backend_won;
  if (typeof sbw !== "string" || !SUMMARIZER_BACKENDS.includes(sbw)) {
    return { ok: false, error: "bad summarizer_backend_won" };
  }

  return { ok: true, value: body as unknown as TelemetryRow };
}

// ---- read-path sanitizers -------------------------------------------------
//
// A row already stored in D1 (written before a validation rule existed, or
// inserted out-of-band) can't be REJECTED — it's already in the table. So the
// read path SANITIZES instead: drop anything that wouldn't pass ingest today,
// so one bad/legacy row can never skew the aggregate or break the dashboard.

/** Keep only known strategy names whose share is an int in [5,100] stepped by 5. */
export function sanitizeStrategyShare(parsed: unknown): Partial<Record<KnownStrategy, number>> {
  const out: Partial<Record<KnownStrategy, number>> = {};
  if (!isPlainObject(parsed)) return out;
  for (const [name, share] of Object.entries(parsed)) {
    if (KNOWN_STRATEGIES.includes(name as KnownStrategy) && intInRange(share, 5, 100, 5)) {
      out[name as KnownStrategy] = share;
    }
  }
  return out;
}

/** Keep only known strategy names, deduped (implicitly capped at KNOWN length). */
export function sanitizeStrategiesFired(parsed: unknown): KnownStrategy[] {
  if (!Array.isArray(parsed)) return [];
  const out: KnownStrategy[] = [];
  const seen = new Set<string>();
  for (const s of parsed) {
    if (KNOWN_STRATEGIES.includes(s as KnownStrategy) && !seen.has(s)) {
      seen.add(s);
      out.push(s as KnownStrategy);
    }
  }
  return out;
}
