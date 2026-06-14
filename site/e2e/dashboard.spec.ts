import { test, expect } from "@playwright/test";

/**
 * E2E: /dashboard/ page.
 *
 * The page is built without PUBLIC_AGGREGATES_URL, so init() shows the honest
 * empty-state + opt-in CTA — NOT seeded sample data dressed up as real. These
 * specs verify the real production behaviour end-to-end in a browser:
 *   1. The empty-state renders (no fake rows, no "Preview with sample data" banner).
 *   2. The opt-in CTA (`trimwire share stats` + the guide link) is present.
 *
 * The interactive table (sorting, filtering, expand/collapse, k-anon empty state)
 * is covered by the unit tests, which mount the renderer against a fixture payload.
 */

test.describe("/dashboard/ page", () => {
  test.beforeEach(async ({ page }) => {
    await page.goto("/dashboard/");
    // Wait for the client-side script to render the empty-state.
    await page.waitForSelector(".tw-dash-empty");
  });

  test("shows the honest empty-state, not seeded sample data", async ({ page }) => {
    // No fake table rows before the collector is live.
    await expect(page.locator("table.twd")).toHaveCount(0);
    await expect(page.locator(".tw-dash-kpi .tw-kpi")).toHaveCount(0);
    // And the old "Preview with sample data" banner is gone for good.
    await expect(page.locator(".tw-dash-status")).not.toContainText("Preview with sample data");
  });

  test("shows the opt-in CTA (command + guide link)", async ({ page }) => {
    const empty = page.locator(".tw-dash-empty");
    await expect(empty.locator(".tw-empty-cmd")).toHaveText("trimwire share stats");
    // The empty-state has two links (the guide + the ?demo preview), so assert the
    // guide link specifically — a bare `a` locator is a strict-mode violation.
    await expect(empty.locator('a[href="/guides/telemetry/"]')).toBeVisible();
  });
});
