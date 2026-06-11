import { defineConfig, devices } from "@playwright/test";

/**
 * E2E configuration — the trophy tip. Small, focused, non-flaky.
 * Tests run against `npm run build` output served by `npx serve dist --listen 4321`.
 * Because both pages rely on client-side JS (no data-* URL ⟹ EXAMPLE fallback),
 * every spec gets a fully-rendered page without a live collector.
 *
 * Locally:   `npm run test:e2e`  (builds + serves + runs)
 * CI:        see .github/workflows/site.yml  (e2e job)
 */
export default defineConfig({
  testDir: "./e2e",
  /* Fail fast on CI; run in parallel locally. */
  workers: process.env.CI ? 1 : undefined,
  /* Retry once on CI to absorb minor serve-startup flakiness. */
  retries: process.env.CI ? 1 : 0,
  reporter: [["html", { open: "never" }], ["list"]],

  use: {
    baseURL: "http://localhost:4321",
    /* Screenshots / traces on failure only — keeps artifacts small. */
    screenshot: "only-on-failure",
    trace: "retain-on-failure",
  },

  projects: [
    {
      name: "chromium",
      use: { ...devices["Desktop Chrome"] },
    },
  ],

  webServer: {
    /* Build is assumed done before test:e2e runs (see package.json script). */
    command: "npx serve dist --listen 4321 --no-clipboard",
    url: "http://localhost:4321",
    reuseExistingServer: !process.env.CI,
    stdout: "ignore",
    stderr: "pipe",
  },
});
