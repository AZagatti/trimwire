import { test, expect } from "@playwright/test";
import AxeBuilder from "@axe-core/playwright";

/**
 * E2E a11y: axe-core scan of both interactive pages after client JS has run.
 * Checks ZERO serious/critical violations on /benchmark/ and /dashboard/.
 *
 * This catches regressions that the integration tests miss: axe needs a real
 * browser (CSSOM, focus management, live-region announcements).
 */

test.describe("accessibility (axe-core)", () => {
  test("/benchmark/ has no serious or critical axe violations after mount", async ({ page }) => {
    await page.goto("/benchmark/");
    // No collector URL in the build → the page renders the empty-state CTA.
    await page.waitForSelector(".tw-bench-empty");

    const results = await new AxeBuilder({ page })
      .exclude("iframe") // exclude any third-party iframes
      .analyze();

    const serious = results.violations.filter((v) =>
      v.impact === "serious" || v.impact === "critical",
    );

    if (serious.length > 0) {
      // Surface the violation details in the failure message.
      const summary = serious
        .map((v) => `[${v.impact}] ${v.id}: ${v.description} — ${v.nodes.length} node(s)`)
        .join("\n");
      expect.soft(serious, `Axe violations on /benchmark/:\n${summary}`).toHaveLength(0);
    }

    expect(serious).toHaveLength(0);
  });

  test("/dashboard/ has no serious or critical axe violations after mount", async ({ page }) => {
    await page.goto("/dashboard/");
    // No collector URL in the build → the page renders the empty-state CTA.
    await page.waitForSelector(".tw-dash-empty");

    const results = await new AxeBuilder({ page })
      .exclude("iframe")
      .analyze();

    const serious = results.violations.filter((v) =>
      v.impact === "serious" || v.impact === "critical",
    );

    if (serious.length > 0) {
      const summary = serious
        .map((v) => `[${v.impact}] ${v.id}: ${v.description} — ${v.nodes.length} node(s)`)
        .join("\n");
      expect.soft(serious, `Axe violations on /dashboard/:\n${summary}`).toHaveLength(0);
    }

    expect(serious).toHaveLength(0);
  });

  test("/ (home) has no serious or critical axe violations", async ({ page }) => {
    await page.goto("/");
    // Wait for the custom landing hero heading to render.
    await page.waitForSelector("main h1");
    // Let the terminal island mount + finish its run so axe scans the settled
    // state (text at full opacity), which is what a real visitor / Lighthouse sees.
    await page.waitForSelector(".termwin");
    await page.waitForSelector(".termwin .fwd", { timeout: 20000 });
    await page.waitForTimeout(2500);

    const results = await new AxeBuilder({ page })
      .exclude("iframe")
      .analyze();

    const serious = results.violations.filter((v) =>
      v.impact === "serious" || v.impact === "critical",
    );

    if (serious.length > 0) {
      const summary = serious
        .map((v) => `[${v.impact}] ${v.id}: ${v.description} — ${v.nodes.length} node(s)`)
        .join("\n");
      expect.soft(serious, `Axe violations on /:\n${summary}`).toHaveLength(0);
    }

    expect(serious).toHaveLength(0);
  });

  test("/guides/faq/ has no serious or critical axe violations", async ({ page }) => {
    await page.goto("/guides/faq/");
    // Wait for the Starlight doc page title (stable across Starlight versions).
    await page.waitForSelector("h1#_top");

    const results = await new AxeBuilder({ page })
      .exclude("iframe")
      .analyze();

    const serious = results.violations.filter((v) =>
      v.impact === "serious" || v.impact === "critical",
    );

    if (serious.length > 0) {
      const summary = serious
        .map((v) => `[${v.impact}] ${v.id}: ${v.description} — ${v.nodes.length} node(s)`)
        .join("\n");
      expect.soft(serious, `Axe violations on /guides/faq/:\n${summary}`).toHaveLength(0);
    }

    expect(serious).toHaveLength(0);
  });
});
