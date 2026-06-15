// HTTP-gate / integration tests for the Worker (src/index.ts), running in the
// real Workers runtime (workerd) with an in-memory D1. These cover what the pure
// validate/aggregate unit tests cannot: that the routing actually CALLS the
// validator (fail-closed 400), actually WRITES to D1 (204), and actually applies
// k-anonymity at the GET boundary before returning anything. For a privacy tool
// this enforcement layer is the part that must not silently regress.
import { env as providedEnv, createExecutionContext, waitOnExecutionContext } from "cloudflare:test";
import { beforeAll, beforeEach, describe, it, expect } from "vitest";
import worker, { type Env } from "../src/index";
import schemaSql from "../schema.sql?raw";
import { validTelemetry, validBenchmark } from "./fixtures";

// The pool injects `env` typed as the generated `Cloudflare.Env`; this Worker
// declares its own `Env` (DB + vars). They describe the same bindings — cast
// once here so the rest of the file is fully typed (env.DB, worker.fetch(...)).
const env = providedEnv as unknown as Env;

/** Seed the in-memory D1 from the real schema.sql (no drift from production):
 *  strip `--` line comments, split on `;`, run each statement. Runs in beforeAll
 *  so it becomes the baseline every isolated-storage test stacks on top of. */
beforeAll(async () => {
  const statements = schemaSql
    .replace(/--[^\n]*/g, "")
    .split(";")
    .map((s) => s.trim())
    .filter(Boolean);
  for (const s of statements) await env.DB.prepare(s).run();
});

// Belt-and-suspenders: each test starts from an empty table AND a cold edge
// cache, so contributor counts are deterministic. (The GET handlers cache the
// computed JSON keyed on path only; without busting it, a suppressed response
// from one test would be served to the next.)
beforeEach(async () => {
  await env.DB.prepare("DELETE FROM telemetry").run();
  await env.DB.prepare("DELETE FROM benchmark").run();
  await caches.default.delete("https://api.test/aggregates.json");
  await caches.default.delete("https://api.test/benchmarks.json");
});

// telemetry rows dedup on `dedup_token` (INSERT OR REPLACE), so distinct
// contributors need distinct tokens — a 64-hex string derived from an index.
function telemetryWith(i: number) {
  return { ...validTelemetry(), dedup_token: i.toString(16).padStart(64, "0") };
}

/** Drive the Worker's fetch handler with a fresh execution context. */
async function call(request: Request): Promise<Response> {
  const ctx = createExecutionContext();
  const res = await worker.fetch(request, env, ctx);
  await waitOnExecutionContext(ctx);
  return res;
}

function postJson(path: string, body: unknown): Request {
  return new Request(`https://api.test${path}`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: typeof body === "string" ? body : JSON.stringify(body),
  });
}

describe("POST /ingest (telemetry)", () => {
  it("accepts a well-formed payload with 204 and writes one row", async () => {
    const res = await call(postJson("/ingest", validTelemetry()));
    expect(res.status).toBe(204);
    const { results } = await env.DB.prepare("SELECT COUNT(*) AS n FROM telemetry").all();
    expect((results[0] as { n: number }).n).toBe(1);
  });

  it("fail-closed: a payload with a smuggled key is rejected 400 and stored nowhere", async () => {
    const res = await call(postJson("/ingest", { ...validTelemetry(), leaked_path: "/home/me/secret" }));
    expect(res.status).toBe(400);
    const { results } = await env.DB.prepare("SELECT COUNT(*) AS n FROM telemetry").all();
    expect((results[0] as { n: number }).n).toBe(0);
  });

  it("rejects non-JSON with 400", async () => {
    const res = await call(postJson("/ingest", "not json{"));
    expect(res.status).toBe(400);
  });

  it("rejects an over-size body with 413 before parsing it", async () => {
    // > MAX_BODY_BYTES (8192). The size gate runs before JSON.parse, so a junk/
    // abusive body is cheaply refused without being decoded or stored.
    const huge = "x".repeat(9000);
    const res = await call(postJson("/ingest", huge));
    expect(res.status).toBe(413);
    const { results } = await env.DB.prepare("SELECT COUNT(*) AS n FROM telemetry").all();
    expect((results[0] as { n: number }).n).toBe(0);
  });

  it("dedups same-day re-uploads on dedup_token (INSERT OR REPLACE keeps 1 row)", async () => {
    // Privacy-relevant: a client re-uploading the same day must not inflate the
    // contributor count. Same token twice → still one row.
    const row = telemetryWith(42);
    expect((await call(postJson("/ingest", row))).status).toBe(204);
    expect((await call(postJson("/ingest", { ...row, reduction_pct_bucket: 60 }))).status).toBe(204);
    const { results } = await env.DB.prepare("SELECT COUNT(*) AS n FROM telemetry").all();
    expect((results[0] as { n: number }).n).toBe(1);
  });

  it("fails CLOSED with 503 when the rate-limiter is unbound and not explicitly allowed", async () => {
    // The advertised invariant: an unthrottled public ingest endpoint is a DoS +
    // poisoning vector, so with no KV bound and ALLOW_UNTHROTTLED unset, /ingest
    // must refuse. Temporarily clear the test-only escape hatch to exercise it.
    const saved = env.ALLOW_UNTHROTTLED;
    (env as { ALLOW_UNTHROTTLED?: string }).ALLOW_UNTHROTTLED = undefined;
    try {
      const res = await call(postJson("/ingest", validTelemetry()));
      expect(res.status).toBe(503);
      const { results } = await env.DB.prepare("SELECT COUNT(*) AS n FROM telemetry").all();
      expect((results[0] as { n: number }).n).toBe(0);
    } finally {
      (env as { ALLOW_UNTHROTTLED?: string }).ALLOW_UNTHROTTLED = saved;
    }
  });
});

describe("GET /aggregates.json (k-anonymity at the boundary)", () => {
  it("suppresses a below-k group: no raw rows or sub-k group are ever returned", async () => {
    // 3 distinct contributors — below the k=10 floor. The HTTP layer must
    // withhold them.
    for (let i = 0; i < 3; i++) await call(postJson("/ingest", telemetryWith(i)));
    const res = await call(new Request("https://api.test/aggregates.json"));
    expect(res.status).toBe(200);
    const body = (await res.json()) as { groups: unknown[]; suppressed_groups: number };
    expect(body.groups).toHaveLength(0);
    expect(body.suppressed_groups).toBeGreaterThanOrEqual(1);
    // The raw quasi-identifiers must never appear in the public JSON.
    const text = JSON.stringify(body);
    expect(text).not.toContain("claude-opus-4-5");
  });

  it("publishes a group only once it reaches k contributors", async () => {
    for (let i = 0; i < 10; i++) await call(postJson("/ingest", telemetryWith(i)));
    const res = await call(new Request("https://api.test/aggregates.json"));
    const body = (await res.json()) as { groups: { contributors: number }[] };
    expect(body.groups.length).toBeGreaterThanOrEqual(1);
    expect(body.groups[0].contributors).toBeGreaterThanOrEqual(10);
  });

  it("serves the public JSON with CORS + a locked-down CSP + security headers", async () => {
    // The cross-origin site widget depends on `access-control-allow-origin: *`;
    // a refactor that drops it would silently break the dashboard with green CI.
    // CSP `default-src 'none'` keeps the JSON from being framed/executed.
    await call(postJson("/ingest", validTelemetry()));
    const res = await call(new Request("https://api.test/aggregates.json"));
    expect(res.headers.get("access-control-allow-origin")).toBe("*");
    expect(res.headers.get("content-security-policy")).toBe("default-src 'none'");
    expect(res.headers.get("X-Content-Type-Options")).toBe("nosniff");
    expect(res.headers.get("content-type")).toContain("application/json");
  });
});

describe("POST /ingest-benchmark", () => {
  it("accepts a valid local benchmark row with 204 and writes it", async () => {
    const res = await call(postJson("/ingest-benchmark", validBenchmark()));
    expect(res.status).toBe(204);
    const { results } = await env.DB.prepare("SELECT COUNT(*) AS n FROM benchmark").all();
    expect((results[0] as { n: number }).n).toBe(1);
  });

  it("fail-closed: an api-dry-run placeholder is rejected 400 and never stored", async () => {
    // Locks the dry-run invariant at the HTTP boundary: even if a malformed
    // client POSTed a dry-run placeholder, the collector must refuse it so it can
    // never reach /benchmarks.json or D1.
    const res = await call(postJson("/ingest-benchmark", { ...validBenchmark(), backend: "api-dry-run" }));
    expect(res.status).toBe(400);
    const { results } = await env.DB.prepare("SELECT COUNT(*) AS n FROM benchmark").all();
    expect((results[0] as { n: number }).n).toBe(0);
  });
});

describe("GET /benchmarks.json (k-anonymity at the boundary)", () => {
  it("suppresses a below-BENCH_K model group", async () => {
    for (let i = 0; i < 2; i++) await call(postJson("/ingest-benchmark", validBenchmark()));
    const res = await call(new Request("https://api.test/benchmarks.json"));
    expect(res.status).toBe(200);
    const body = (await res.json()) as { models: unknown[]; suppressed_groups: number };
    expect(body.models).toHaveLength(0);
    expect(body.suppressed_groups).toBeGreaterThanOrEqual(1);
  });

  it("publishes a model group once it reaches BENCH_K (5) contributors", async () => {
    for (let i = 0; i < 5; i++) await call(postJson("/ingest-benchmark", validBenchmark()));
    const res = await call(new Request("https://api.test/benchmarks.json"));
    const body = (await res.json()) as { models: { contributors: number }[] };
    expect(body.models.length).toBeGreaterThanOrEqual(1);
    expect(body.models[0].contributors).toBeGreaterThanOrEqual(5);
  });
});

describe("baseline hardening", () => {
  it("unknown routes 404 and every response carries the security headers", async () => {
    const res = await call(new Request("https://api.test/nope"));
    expect(res.status).toBe(404);
    expect(res.headers.get("X-Content-Type-Options")).toBe("nosniff");
    expect(res.headers.get("Referrer-Policy")).toBe("no-referrer");
  });
});
