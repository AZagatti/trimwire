---
description: TypeScript / frontend review rules (Astro site + Cloudflare Worker collector)
globs: ["site/**", "collector/**", "**/*.ts", "**/*.svelte", "**/*.astro"]
---

- No `any`; prefer precise types. No floating promises — handle/await rejections.
- `collector/` runs on **Cloudflare Workers**: no Node-only APIs, respect bindings and
  the `fetch`/handler signatures; watch for unbounded work (CPU/subrequest limits).
- `site/` is **Astro**: keep client JS minimal; flag layout-shift regressions and
  accessibility gaps (labels, roles, keyboard) on interactive components.
- Don't restate what `tsc`/eslint/prettier already enforce.
