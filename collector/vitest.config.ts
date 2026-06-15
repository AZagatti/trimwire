import { defineConfig } from "vitest/config";

// Pure-logic tests (validate + aggregate + benchmark). These exercise the
// privacy-critical decision code WITHOUT the Workers runtime, so `npm test` is
// fast and needs no Cloudflare account. The HTTP gate that *enforces* those
// decisions (src/index.ts: routing, D1 I/O, k-anon at the boundary) is covered
// separately by `npm run test:routes` (vitest.workers.config.ts), which runs in
// the real workerd runtime — hence routes.test.ts is excluded here.
export default defineConfig({
  test: {
    include: ["test/**/*.test.ts"],
    exclude: ["test/routes.test.ts", "**/node_modules/**"],
  },
});
