import { test, expect } from "@playwright/test";
import AxeBuilder from "@axe-core/playwright";
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, resolve } from "node:path";

/**
 * A11y gate: axe-core scan of EVERY page in the built sitemap.
 *
 * Coverage is driven off `dist/sitemap-0.xml`, so a new page (docs or custom)
 * gets an a11y test automatically — no per-page spec to maintain.
 *
 * The hard gate stays at `serious | critical` on purpose. We render the bulk of
 * our markup through Starlight, which we don't control at the violation level; a
 * single upstream patch that introduces a `moderate` finding (icon button label,
 * landmark edge case, generated-element contrast) would otherwise fail all 15
 * doc pages at once and the gate would get disabled. serious/critical is the
 * calibration that fails on real, actionable regressions and stays green across
 * Starlight minor bumps. Lighthouse's a11y category score is the coarse net for
 * the rest.
 *
 * axe needs a real browser (CSSOM, focus management, live regions) and runs
 * after client JS settles — it catches what the static integration tests can't.
 */

const __dirname = dirname(fileURLToPath(import.meta.url));

// Pages whose JS islands must settle before axe scans them. Key: pathname,
// value: a selector that only appears once the page has reached its stable
// interactive state.
const WAIT_FOR: Record<string, string> = {
  "/": ".termwin .fwd", // terminal island has finished typing
  "/benchmark/": ".tw-bench-empty", // no collector URL → empty-state CTA
  "/dashboard/": ".tw-dash-empty", // no collector URL → empty-state CTA
};

// Starlight doc pages are SSG HTML; the stable title anchor is enough.
const STARLIGHT_READY = "h1#_top";

/**
 * Parse the built sitemap and return local-preview pathnames. Falls back to a
 * hard-coded core set if the sitemap is missing (e.g. a partial build), so the
 * suite still protects the important pages rather than silently testing nothing.
 */
function pagesFromSitemap(): string[] {
  try {
    const xml = readFileSync(resolve(__dirname, "../dist/sitemap-0.xml"), "utf-8");
    const paths = [...xml.matchAll(/<loc>([^<]+)<\/loc>/g)].map(
      (m) => new URL(m[1]).pathname,
    );
    if (paths.length > 0) return paths;
  } catch {
    // fall through to the core set
  }
  return ["/", "/benchmark/", "/dashboard/", "/performance/", "/guides/overview/"];
}

const PAGES = pagesFromSitemap();

test.describe("accessibility (axe-core — every sitemap page)", () => {
  for (const pathname of PAGES) {
    test(`${pathname} — no serious or critical axe violations`, async ({ page }) => {
      await page.goto(pathname);

      const waitSelector = WAIT_FOR[pathname] ?? STARLIGHT_READY;
      await page.waitForSelector(waitSelector, { timeout: 20_000 });

      // The terminal animation needs a beat to reach full opacity — scan the
      // settled state a real visitor (and Lighthouse) sees.
      if (pathname === "/") await page.waitForTimeout(2_500);

      const results = await new AxeBuilder({ page })
        .exclude("iframe") // third-party iframes are out of our control
        .analyze();

      const serious = results.violations.filter(
        (v) => v.impact === "serious" || v.impact === "critical",
      );

      const summary = serious
        .map(
          (v) =>
            `[${v.impact}] ${v.id}: ${v.description} (${v.nodes.length} node(s))\n  ${v.helpUrl}`,
        )
        .join("\n");

      expect(serious, `Axe violations on ${pathname}:\n${summary}`).toHaveLength(0);
    });
  }
});
