// trimwire telemetry collector — Cloudflare Worker.
//
// The ONLY public surface in front of D1 (the database is never internet-
// exposed). Two routes:
//   POST /ingest          validate a content-free payload, store one row.
//   GET  /aggregates.json compute the k-anonymous, intensive-only dashboard JSON.
//
// INERT BY DEFAULT: nothing is deployed by this repo. The maintainer owns the
// Cloudflare account, `wrangler deploy`, the real endpoint URL, the domain, and
// the privacy policy (see collector/README.md + docs/TELEMETRY.md). The Worker
// never logs or stores client IPs (the optional rate-limiter uses a salted hash
// of the IP as a TTL'd counter key only — the IP itself is never persisted).

import {
  validatePayload,
  SCHEMA_VERSION,
  HARNESSES,
  sanitizeStrategyShare,
  sanitizeStrategiesFired,
} from "./validate";
import { aggregate } from "./aggregate";
import type { KnownStrategy, TelemetryRow } from "./validate";

export interface Env {
  DB: D1Database;
  /** k-anonymity threshold (string from wrangler vars); default 10, floor 5. */
  K?: string;
  /** REQUIRED-in-production KV namespace for abuse rate-limiting. When unbound,
   *  /ingest fails closed (503) unless ALLOW_UNTHROTTLED="true". */
  RATE_LIMIT?: KVNamespace;
  /** Max accepted uploads per IP per UTC day. */
  MAX_PER_DAY?: string;
  /** Max /aggregates.json CACHE-MISS (D1-scanning) reads per IP per UTC day — a
   *  backstop against a GET flood exhausting the D1 read quota when the edge cache
   *  is cold or absent (workers.dev). Generous: legit dashboard reads are served
   *  from the edge cache and never counted. Default 2000. */
  MAX_AGG_PER_DAY?: string;
  /** Escape hatch for local dev / tests ONLY: "true" allows /ingest to run
   *  without the rate-limiter bound. NEVER set in production. */
  ALLOW_UNTHROTTLED?: string;
}

const MAX_BODY_BYTES = 8192; // a valid payload is < 1 KB; anything larger is junk.
const PAGE = 1000; // D1 .all() returns at most 1000 rows — paginate by id cursor.
const RATE_LIMIT_TTL_SECONDS = 172800; // counter expires after 2 days
const AGGREGATE_TTL_SECONDS = 600; // edge-cache the dashboard JSON ~10 min

function utcToday(): string {
  return new Date().toISOString().slice(0, 10);
}

function kFromEnv(env: Env): number {
  const parsed = Math.floor(Number(env.K ?? "10"));
  if (!Number.isFinite(parsed)) return 10;
  return Math.max(5, parsed); // never below the floor
}

/** Per-IP daily-cap gate for /ingest. Returns a Response to short-circuit with
 *  (503 / 400 / 429), or null to proceed.
 *
 *  FAIL CLOSED: the rate-limiter is REQUIRED. An unthrottled public endpoint is
 *  a D1 write-quota DoS + aggregate-poisoning vector, so when the RATE_LIMIT KV
 *  namespace isn't bound /ingest is refused (503) unless ALLOW_UNTHROTTLED is
 *  explicitly "true" (local dev / tests only). The IP is hashed with a per-day
 *  salt and only a TTL'd counter is stored — the IP itself is never kept. */
/** Day-scoped SHA-256 of `ip|day` (hex). Used ONLY for the rate-limit KV key;
 *  the IP is never stored in D1. Different days → different hashes, so even the
 *  rate-limit key can't be used for cross-day identity. */
async function dayIpHash(ip: string, day: string): Promise<string> {
  const digest = await crypto.subtle.digest(
    "SHA-256",
    new TextEncoder().encode(`${ip}|${day}`),
  );
  return [...new Uint8Array(digest)]
    .map((b) => b.toString(16).padStart(2, "0"))
    .join("");
}

async function rateLimitGate(request: Request, env: Env): Promise<Response | null> {
  if (!env.RATE_LIMIT) {
    if (env.ALLOW_UNTHROTTLED === "true") {
      // Surfaces in Worker logs so an accidental production deploy with this
      // escape set (e.g. via `wrangler deploy --var`) is visible.
      console.warn("ALLOW_UNTHROTTLED active — rate-limiter disabled; do not use in production");
      return null;
    }
    return new Response("rate limiter not configured", { status: 503 });
  }
  const ip = request.headers.get("CF-Connecting-IP");
  if (!ip) {
    // Behind Cloudflare this header is always present; its absence means the
    // request didn't traverse the edge and can't be fairly throttled → refuse.
    return new Response("invalid request", { status: 400 });
  }
  const day = utcToday();
  const hex = await dayIpHash(ip, day);
  const key = `rl:${day}:${hex}`;
  const cap = Math.max(1, Math.floor(Number(env.MAX_PER_DAY ?? "50")) || 50);
  // Soft cap: the read-then-write is racy, so concurrent requests can overshoot
  // by the degree of concurrency. Acceptable — this is abuse-bounding, not a
  // hard quota, and a small overcount per IP per day is harmless.
  const current = Number.parseInt((await env.RATE_LIMIT.get(key)) ?? "0", 10) || 0;
  if (current >= cap) return new Response("rate limited", { status: 429 });
  await env.RATE_LIMIT.put(key, String(current + 1), {
    expirationTtl: RATE_LIMIT_TTL_SECONDS,
  });
  return null;
}

/** Per-IP daily cap for the EXPENSIVE /aggregates.json path (a cache MISS that
 *  scans D1). Called only after the edge-cache check, so legit dashboard reads —
 *  served from cache — are never counted; this purely bounds a GET flood from
 *  rescanning D1 and burning the read quota. Unlike `rateLimitGate` it does NOT
 *  fail closed: a public read endpoint with no KV bound should still serve (the
 *  edge cache is the primary defense), and a non-edge request (no CF-Connecting-IP)
 *  is let through rather than darkening the dashboard. Returns 429 or null. */
async function aggregatesRateLimit(request: Request, env: Env): Promise<Response | null> {
  if (!env.RATE_LIMIT) return null; // no KV → rely on the edge cache; don't fail closed
  const ip = request.headers.get("CF-Connecting-IP");
  if (!ip) return null; // can't fairly throttle a non-edge request; let it through
  const day = utcToday();
  const key = `agg:${day}:${await dayIpHash(ip, day)}`;
  const cap = Math.max(1, Math.floor(Number(env.MAX_AGG_PER_DAY ?? "2000")) || 2000);
  const current = Number.parseInt((await env.RATE_LIMIT.get(key)) ?? "0", 10) || 0;
  if (current >= cap) return new Response("rate limited", { status: 429 });
  await env.RATE_LIMIT.put(key, String(current + 1), { expirationTtl: RATE_LIMIT_TTL_SECONDS });
  return null;
}

/** Baseline security headers on EVERY response: nosniff defeats MIME sniffing;
 *  no-referrer stops the (cross-origin) site leaking request URLs. */
function withSecurityHeaders(resp: Response): Response {
  const r = new Response(resp.body, resp);
  r.headers.set("X-Content-Type-Options", "nosniff");
  r.headers.set("Referrer-Policy", "no-referrer");
  return r;
}

async function handleIngest(request: Request, env: Env): Promise<Response> {
  const gate = await rateLimitGate(request, env);
  if (gate) return gate;
  // Cheap pre-check on the DECLARED length to reject obvious oversizes without
  // reading the body. A missing/non-numeric header just falls through — the
  // arrayBuffer byteLength check below is the authoritative size limit.
  const declared = Number(request.headers.get("content-length") ?? "0");
  if (Number.isFinite(declared) && declared > MAX_BODY_BYTES) {
    return new Response("payload too large", { status: 413 });
  }
  // Measure the real byte length (not UTF-16 code units) before decoding.
  const buf = await request.arrayBuffer();
  if (buf.byteLength > MAX_BODY_BYTES) {
    return new Response("payload too large", { status: 413 });
  }
  let body: unknown;
  try {
    body = JSON.parse(new TextDecoder().decode(buf));
  } catch {
    return new Response("invalid JSON", { status: 400 });
  }
  const result = validatePayload(body);
  if (!result.ok) {
    return new Response(`rejected: ${result.error}`, { status: 400 });
  }
  const r = result.value;
  // INSERT OR REPLACE on the client-provided dedup_token:
  // - The token is HMAC-SHA256(install_id, sent_day) — day-scoped so it rotates
  //   daily and cannot link two different days (no cross-day identity).
  // - INSERT OR REPLACE means a same-day re-upload overrides the prior row (the
  //   client sees 204 either way). This is intentional: a re-upload with new data
  //   (e.g. after more sessions) takes precedence over the earlier upload.
  // - IP is used ONLY for rate-limiting (in rateLimitGate above) and is never
  //   stored in D1.
  await env.DB.prepare(
    `INSERT OR REPLACE INTO telemetry (
       dedup_token,
       received_day, schema_version, sent_day, trimwire_version, harness, model_family,
       profile, summarizer_backend, summarizer_family, conversation_length_bucket,
       reduction_pct_bucket, cache_hit_pct_bucket, cache_stability_bucket,
       bytes_saved_bucket, strategy_share,
       reprune_enabled, simhash_enabled, accumulator_enabled, os_family,
       native_compaction_rate_bucket, strategies_fired,
       summarizer_size_bucket, strategy_any_fired_pct_bucket,
       summarizer_accept_rate_bucket, summarizer_trigger_rate_bucket,
       max_session_length_bucket, summarizer_backend_won
     ) VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)`,
  )
    .bind(
      r.dedup_token,
      utcToday(),
      r.schema_version,
      r.sent_day,
      r.trimwire_version,
      r.harness,
      r.model_family,
      r.profile,
      r.summarizer_backend,
      r.summarizer_family,
      r.conversation_length_bucket,
      r.reduction_pct_bucket,
      r.cache_hit_pct_bucket,
      r.cache_stability_bucket,
      r.bytes_saved_bucket,
      JSON.stringify(r.strategy_share),
      r.reprune_enabled ? 1 : 0,
      r.simhash_enabled ? 1 : 0,
      r.accumulator_enabled ? 1 : 0,
      r.os_family,
      r.native_compaction_rate_bucket,
      JSON.stringify(r.strategies_fired),
      r.summarizer_size_bucket,
      r.strategy_any_fired_pct_bucket,
      r.summarizer_accept_rate_bucket,
      r.summarizer_trigger_rate_bucket,
      r.max_session_length_bucket,
      r.summarizer_backend_won,
    )
    .run();
  // 204: accepted, nothing to return. No echo of the stored data.
  return new Response(null, { status: 204 });
}

interface DbRow {
  id: number;
  trimwire_version: string;
  harness: string;
  model_family: string;
  profile: string;
  summarizer_backend: string;
  summarizer_family: string;
  conversation_length_bucket: string;
  reduction_pct_bucket: number;
  cache_hit_pct_bucket: number;
  cache_stability_bucket: number;
  bytes_saved_bucket: string;
  strategy_share: string;
  reprune_enabled: number;
  simhash_enabled: number;
  accumulator_enabled: number;
  os_family: string;
  native_compaction_rate_bucket: number;
  strategies_fired: string;
  summarizer_size_bucket: string;
  strategy_any_fired_pct_bucket: number;
  summarizer_accept_rate_bucket: string;
  summarizer_trigger_rate_bucket: number;
  max_session_length_bucket: string;
  summarizer_backend_won: string;
}

function rowFromDb(d: DbRow): TelemetryRow {
  // Re-validate on the READ path too (defense-in-depth): a row written before a
  // rule existed or inserted out-of-band is SANITIZED, never trusted — so one
  // bad row can't skew the aggregate or break the page. A corrupt JSON string
  // contributes empty rather than throwing.
  let share: Partial<Record<KnownStrategy, number>> = {};
  try {
    share = sanitizeStrategyShare(JSON.parse(d.strategy_share));
  } catch {
    console.error(`corrupt strategy_share in row id=${d.id}; counting as empty`);
  }
  let fired: KnownStrategy[] = [];
  try {
    fired = sanitizeStrategiesFired(JSON.parse(d.strategies_fired));
  } catch {
    console.error(`corrupt strategies_fired in row id=${d.id}; counting as none`);
  }
  return {
    // schema_version + sent_day aren't used by aggregation; carry the current
    // version rather than a stale literal.
    schema_version: SCHEMA_VERSION,
    sent_day: "", // not needed for aggregation
    trimwire_version: d.trimwire_version,
    // DEFAULT 'claude-code' covers any row written before this column existed;
    // re-validate against the closed set too (read-path sanitization) so an
    // out-of-band-inserted row can't push an arbitrary value into a cohort label.
    harness: HARNESSES.includes((d.harness ?? "claude-code") as (typeof HARNESSES)[number])
      ? (d.harness ?? "claude-code")
      : "other",
    model_family: d.model_family,
    profile: d.profile,
    summarizer_backend: d.summarizer_backend,
    summarizer_family: d.summarizer_family,
    conversation_length_bucket: d.conversation_length_bucket,
    reduction_pct_bucket: d.reduction_pct_bucket,
    cache_hit_pct_bucket: d.cache_hit_pct_bucket,
    cache_stability_bucket: d.cache_stability_bucket,
    bytes_saved_bucket: d.bytes_saved_bucket,
    strategy_share: share,
    reprune_enabled: d.reprune_enabled === 1,
    simhash_enabled: d.simhash_enabled === 1,
    accumulator_enabled: d.accumulator_enabled === 1,
    os_family: d.os_family,
    native_compaction_rate_bucket: d.native_compaction_rate_bucket,
    strategies_fired: fired,
    summarizer_size_bucket: d.summarizer_size_bucket ?? "none",
    strategy_any_fired_pct_bucket: d.strategy_any_fired_pct_bucket ?? 0,
    summarizer_accept_rate_bucket: d.summarizer_accept_rate_bucket ?? "none",
    summarizer_trigger_rate_bucket: d.summarizer_trigger_rate_bucket ?? 0,
    max_session_length_bucket: d.max_session_length_bucket ?? "<10",
    // dedup_token is not needed for aggregation — it's the conflict key on write.
    dedup_token: "",
    // §8C/Q4: winning engine kind; DEFAULT 'off' covers rows pre-dating the column.
    summarizer_backend_won: d.summarizer_backend_won ?? "off",
  };
}

async function handleAggregates(
  request: Request,
  env: Env,
  ctx: ExecutionContext,
): Promise<Response> {
  // Edge-cache the computed JSON so a miss (a full D1 cursor scan) is rare — a
  // GET flood otherwise burns the D1 read quota.
  // NOTE: caches.default is a no-op on *.workers.dev — it only works behind a
  // custom domain/route (hence workers_dev=false in wrangler.toml). Without one,
  // every GET rescans D1.
  const cache = caches.default;
  // Key on the PATH ONLY (origin + pathname), never the query string: the
  // response ignores query params, so keying on the full URL would let
  // `/aggregates.json?x=1`, `?x=2`, … each miss the cache and force a fresh D1
  // scan, bypassing the edge cache (and the post-cache rate-limiter) — a cheap
  // amplification. A canonical key collapses them to one cached entry.
  const canonicalUrl = new URL(request.url);
  canonicalUrl.search = "";
  const cacheKey = new Request(canonicalUrl.toString(), { method: "GET" });
  const hit = await cache.match(cacheKey);
  if (hit) return hit; // already carries security headers (stored wrapped)

  // Cache MISS → we're about to scan D1. Backstop a GET flood here (legit reads
  // are served from the cache above and never reach this point).
  const gate = await aggregatesRateLimit(request, env);
  if (gate) return withSecurityHeaders(gate);

  const k = kFromEnv(env);
  // D1 .all() caps at PAGE rows; page by id cursor so the aggregate is computed
  // over the FULL table (a partial read could wrongly satisfy/deny k-anonymity).
  // For high volume the upgrade is a scheduled SQL GROUP BY → KV (see README).
  const rows: TelemetryRow[] = [];
  let cursor = 0;
  for (;;) {
    const { results } = await env.DB.prepare(
      `SELECT id, trimwire_version, harness, model_family, profile, summarizer_backend,
              summarizer_family, conversation_length_bucket, reduction_pct_bucket,
              cache_hit_pct_bucket, cache_stability_bucket, bytes_saved_bucket,
              strategy_share, reprune_enabled, simhash_enabled, accumulator_enabled,
              os_family, native_compaction_rate_bucket, strategies_fired,
              summarizer_size_bucket, strategy_any_fired_pct_bucket,
              summarizer_accept_rate_bucket, summarizer_trigger_rate_bucket,
              max_session_length_bucket, summarizer_backend_won
         FROM telemetry WHERE id > ? ORDER BY id LIMIT ${PAGE}`,
    )
      .bind(cursor)
      .all<DbRow>();
    const batch = results ?? [];
    if (batch.length === 0) break;
    for (const d of batch) rows.push(rowFromDb(d));
    cursor = batch[batch.length - 1].id;
    if (batch.length < PAGE) break;
  }
  const agg = aggregate(rows, k);
  const payload = { generated_at: new Date().toISOString(), ...agg };
  // Wrap with the baseline security headers BEFORE caching, so a cache hit
  // serves identical headers without depending on the caller to re-wrap.
  const resp = withSecurityHeaders(
    new Response(JSON.stringify(payload), {
      headers: {
        "content-type": "application/json; charset=utf-8",
        // public aggregate; let the static site (other origin) fetch it.
        "access-control-allow-origin": "*",
        // max-age for the browser; s-maxage drives the edge Cache API entry.
        "cache-control": `public, max-age=${AGGREGATE_TTL_SECONDS}, s-maxage=${AGGREGATE_TTL_SECONDS}`,
        // JSON is data, not a document — lock it down so it can't be framed/run.
        "content-security-policy": "default-src 'none'",
      },
    }),
  );
  // Store a clone without blocking the response.
  ctx.waitUntil(cache.put(cacheKey, resp.clone()));
  return resp;
}

export default {
  async fetch(request: Request, env: Env, ctx: ExecutionContext): Promise<Response> {
    const url = new URL(request.url);
    if (request.method === "POST" && url.pathname === "/ingest") {
      return withSecurityHeaders(await handleIngest(request, env));
    }
    if (request.method === "GET" && url.pathname === "/aggregates.json") {
      // Self-wraps (and caches the wrapped copy) — don't double-wrap here.
      return handleAggregates(request, env, ctx);
    }
    return withSecurityHeaders(new Response("not found", { status: 404 }));
  },
} satisfies ExportedHandler<Env>;
