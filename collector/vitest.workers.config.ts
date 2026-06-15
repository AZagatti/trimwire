import { defineConfig } from "vitest/config";
import { cloudflareTest } from "@cloudflare/vitest-pool-workers";

// Route/integration tests for the Worker's HTTP gate (src/index.ts), run inside
// the real Workers runtime (workerd) via @cloudflare/vitest-pool-workers (v4
// plugin API: `cloudflareTest()` plugin + plain `defineConfig`). This is where
// the privacy guarantees are *enforced*: ingest fail-closed validation (400),
// k-anonymity at the GET boundary, and the security headers. The bindings below
// mirror wrangler.toml but use an in-memory D1 (seeded from schema.sql in the
// test) and NO KV — POST sets ALLOW_UNTHROTTLED so the rate-limiter (which fails
// closed without KV) is bypassed; the GET path already degrades open.
export default defineConfig({
  plugins: [
    cloudflareTest({
      miniflare: {
        compatibilityDate: "2026-06-12",
        d1Databases: ["DB"],
        bindings: {
          K: "10",
          BENCH_K: "5",
          MAX_PER_DAY: "50",
          // Local/CI ONLY: lets POST /ingest run without the KV rate-limiter.
          ALLOW_UNTHROTTLED: "true",
        },
      },
    }),
  ],
  test: {
    include: ["test/routes.test.ts"],
  },
});
