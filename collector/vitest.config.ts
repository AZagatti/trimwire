import { defineConfig } from "vitest/config";

// Pure-logic tests only (validate + aggregate). These exercise the privacy-
// critical code WITHOUT the Workers runtime, so `npm test` is fast and needs no
// Cloudflare account. The Worker wiring (src/index.ts) is exercised manually
// with `wrangler dev` (see README).
export default defineConfig({
  test: {
    include: ["test/**/*.test.ts"],
  },
});
