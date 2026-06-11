import { test, expect } from "@playwright/test";

/**
 * E2E: /benchmark/ page.
 *
 * The page is built without PUBLIC_BENCHMARK_URL, so init() shows the honest
 * empty-state + opt-in CTA — NOT seeded sample data dressed up as real. These
 * specs verify the real production behaviour end-to-end in a browser:
 *   1. The empty-state renders (no table, no fake rows, no "preview" banner).
 *   2. The opt-in CTA (`trimwire share benchmark` + the guide link) is present.
 *
 * The interactive table (sorting, search, family filter, keyboard nav) is covered
 * by the unit tests, which mount the renderer against a fixture payload directly.
 */

test.describe("/benchmark/ page", () => {
  test.beforeEach(async ({ page }) => {
    await page.goto("/benchmark/");
    // Wait for the client-side script to render the empty-state.
    await page.waitForSelector(".tw-bench-empty");
  });

  test("shows the honest empty-state, not seeded sample data", async ({ page }) => {
    // No fake table and no fake KPI cards before the collector is live.
    await expect(page.locator("table.twb")).toHaveCount(0);
    await expect(page.locator(".tw-bench-kpi .tw-kpi")).toHaveCount(0);
    // And the old "Preview with sample data" banner is gone for good.
    await expect(page.locator(".tw-bench-status")).not.toContainText("Preview with sample data");
  });

  test("shows the opt-in CTA (command + guide link)", async ({ page }) => {
    const empty = page.locator(".tw-bench-empty");
    await expect(empty.locator(".tw-empty-cmd")).toHaveText("trimwire share benchmark");
    await expect(empty.locator("a")).toHaveAttribute("href", "/guides/benchmark/");
  });
});
