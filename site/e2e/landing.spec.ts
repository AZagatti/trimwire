import { test, expect, devices } from "@playwright/test";

/**
 * Launch-critical checks for the landing page (`/`) — the marketing surface that
 * gets pasted into HN / X / Slack and carries the terminal hero island.
 *
 * These guard the failure modes that lose clicks/credibility when a share goes
 * viral but are invisible to the build, to Lighthouse, and to axe:
 *   1. Social-share card (og:/twitter: tags + the og.png asset actually exists)
 *   2. No console errors / uncaught exceptions after the island hydrates
 *   3. The terminal island actually mounts and paints (not a blank box)
 *   4. The install command + GitHub CTA are present and correct
 *   5. No horizontal overflow on a phone viewport (share traffic skews mobile)
 */

const OG_IMAGE = "https://trimwire.dev/og.png";

test.describe("landing page — social + hero integrity", () => {
  test("/ exposes a complete social-share card", async ({ page, request }) => {
    await page.goto("/");

    // OG (Facebook/LinkedIn/Slack/Discord)
    await expect(page.locator('meta[property="og:title"]')).toHaveAttribute(
      "content",
      /.+/,
    );
    await expect(page.locator('meta[property="og:description"]')).toHaveAttribute(
      "content",
      /.+/,
    );
    await expect(page.locator('meta[property="og:image"]')).toHaveAttribute(
      "content",
      OG_IMAGE,
    );
    // Twitter/X large card
    await expect(page.locator('meta[name="twitter:card"]')).toHaveAttribute(
      "content",
      "summary_large_image",
    );
    await expect(page.locator('meta[name="twitter:image"]')).toHaveAttribute(
      "content",
      OG_IMAGE,
    );

    // The og:image must resolve, or the card is blank just the same. The tag
    // points at production (absolute URL required by the platforms); verify the
    // asset actually shipped by fetching it from THIS build's preview server.
    const local = await request.get("/og.png");
    expect(local.status(), "/og.png missing from the build").toBe(200);
    expect(local.headers()["content-type"]).toContain("image/png");
  });

  test("/ hydrates with no console errors or uncaught exceptions", async ({ page }) => {
    const errors: string[] = [];
    page.on("console", (msg) => {
      if (msg.type() === "error") errors.push(`console.error: ${msg.text()}`);
    });
    page.on("pageerror", (err) => errors.push(`pageerror: ${err.message}`));

    await page.goto("/");
    // Wait for the terminal island to finish its run, then a beat to let any
    // deferred work throw.
    await page.waitForSelector(".termwin .fwd", { timeout: 20_000 });
    await page.waitForTimeout(2_500);

    expect(errors, `Console/JS errors on /:\n${errors.join("\n")}`).toHaveLength(0);
  });

  test("/ terminal island mounts and paints", async ({ page }) => {
    await page.goto("/");
    const termwin = page.locator(".termwin");
    await expect(termwin).toBeVisible();
    // A hydration failure leaves a collapsed/empty box. Real content is tall.
    const box = await termwin.boundingBox();
    expect(box?.height ?? 0).toBeGreaterThan(200);
  });

  test("/ shows the install path and GitHub link", async ({ page }) => {
    await page.goto("/");
    await expect(page.getByText("cargo install trimwire")).toBeVisible();
    // The primary repo CTA must point at the real repo (trust signal).
    const repo = page.locator('a[href="https://github.com/AZagatti/trimwire"]').first();
    await expect(repo).toHaveCount(1);
  });

  test("/ has no horizontal overflow on a phone viewport", async ({ browser }) => {
    // Scoped to one fresh mobile context so we don't double the whole suite with
    // a global mobile project.
    const ctx = await browser.newContext({ ...devices["Pixel 5"] });
    const page = await ctx.newPage();
    try {
      await page.goto("/");
      await page.waitForSelector("main h1");
      const { scrollWidth, clientWidth } = await page.evaluate(() => ({
        scrollWidth: document.documentElement.scrollWidth,
        clientWidth: document.documentElement.clientWidth,
      }));
      // +1px tolerance for sub-pixel rounding.
      expect(scrollWidth, "page overflows the viewport horizontally").toBeLessThanOrEqual(
        clientWidth + 1,
      );
    } finally {
      await ctx.close();
    }
  });
});
