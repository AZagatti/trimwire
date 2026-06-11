import { defineConfig } from "astro/config";
import starlight from "@astrojs/starlight";
import starlightLlmsTxt from "starlight-llms-txt";

// The real domain is the maintainer's call; this placeholder only affects
// absolute-URL generation (sitemap/canonical) and is harmless if changed later.
export default defineConfig({
  site: "https://trimwire.example",
  integrations: [
    starlight({
      title: "trimwire",
      favicon: "/favicon.svg",
      logo: {
        src: "./public/logo.svg",
        alt: "",
        replacesTitle: false,
      },
      description:
        "A tiny local gateway that prunes Claude Code's API context on every request.",
      social: [
        {
          icon: "github",
          label: "GitHub",
          href: "https://github.com/AZagatti/trimwire",
        },
      ],
      customCss: ["./src/styles/custom.css"],
      components: {
        // Open the GitHub icon link in a new tab.
        SocialIcons: "./src/components/SocialIcons.astro",
        // Replace the <select> theme picker with a sun/moon/auto cycle button.
        ThemeSelect: "./src/components/ThemeSelect.astro",
        // Custom hero: right-side stat panel + tighter button hierarchy.
        Hero: "./src/components/Hero.astro",
        // Page actions row (Copy as Markdown / Open in ChatGPT / Open in Claude)
        // rendered only on /guides/* content pages.
        PageTitle: "./src/components/PageTitle.astro",
        // Extends Starlight's <head> with an accessibility fix: adds tabindex="0"
        // to scrollable <pre> code blocks so they are keyboard-reachable
        // (axe rule: scrollable-region-focusable).
        Head: "./src/components/Head.astro",
      },
      // Emit /llms.txt + /llms-full.txt so trimwire's own docs are consumable by
      // LLMs/agents — fitting for a context-efficiency tool. Build-time only.
      // (starlight-llms-txt is by the Starlight lead maintainer; pinned to 0.6.x
      // for Astro 5 — 0.8+ needs Astro 6.)
      plugins: [starlightLlmsTxt()],
      // `guides/` is synced from the repo's docs/*.md at build time (single
      // source of truth — see scripts/sync-docs.mjs), so it autogenerates.
      sidebar: [
        {
          label: "Start here",
          items: [
            { label: "Overview", link: "/" },
            { label: "Community dashboard", link: "/dashboard/" },
            { label: "Model benchmark", link: "/benchmark/" },
            { label: "Performance", link: "/performance/" },
          ],
        },
        { label: "Guides", autogenerate: { directory: "guides" } },
      ],
    }),
  ],
});
