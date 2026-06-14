# site/ — trimwire's public website (Astro + Starlight)

Landing page, docs, the **community telemetry dashboard**, and a performance
page — one Astro + Starlight site, deployable to Cloudflare Pages.

Starlight was chosen by council + Disagree-Seeking. The "dependency-light,
no-toolchain" ethos applies to
the **binary**, not this marketing/docs site (same reasoning as `collector/`),
so a Node build step here is fine. We still keep it lean: docs are
single-sourced from the repo, and the dashboard renders with **vanilla JS + CSS
bars — no chart library**.

## Develop

```sh
cd site
npm install
npm run dev      # runs sync-docs first, then astro dev
npm run build    # production build → dist/
npm run preview
```

## How it's wired

- **Landing** — `src/content/docs/index.mdx` (Starlight splash). Ported from the
  old hand-written `site/index.html`.
- **Docs** — synced at build time from the repo's `docs/*.md` by
  `scripts/sync-docs.mjs` into `src/content/docs/guides/` (a **gitignored,
  generated** dir). The repo `docs/*.md` are the single source of truth — no
  second copy to drift. Curated to the user-facing docs (FAQ, Telemetry,
  Troubleshooting, Alternatives, Roadmap); internal dev docs are excluded.
- **Community dashboard** — `src/content/docs/dashboard.mdx`. Client-side fetch
  of a public `aggregates.json` (k-anonymous, content-free; see
  `docs/TELEMETRY.md` + `collector/`). The endpoint is read from the
  `PUBLIC_AGGREGATES_URL` build env; **with none set it shows a clearly-labelled
  sample preview**, so it's never blank and never points anywhere by default.
- **Performance** — `src/content/docs/performance.mdx` (honest, offline-replay
  framing; links to `benchmark/results/`).

## Note on search

Starlight's search is powered by **pagefind**, which downloads a small
platform-specific binary on first build. In a fully offline/sandboxed build that
download fails (non-fatal — the rest of the site builds fine, search just has no
index); on Cloudflare Pages or any networked CI it works normally.

## Deploy / config

- **`PUBLIC_AGGREGATES_URL` / `PUBLIC_BENCHMARK_URL`** — wired in `.env` to the
  live collector (`https://api.trimwire.dev/aggregates.json` and
  `/benchmarks.json`). The dashboard + leaderboard fetch these at runtime and show
  an honest empty-state until a cohort crosses the k-anonymity threshold.
- **Domain + deploy** — live at `trimwire.dev` via Cloudflare Workers Builds
  (auto-deploys on push to `main`); `astro.config.mjs` `site:` is `trimwire.dev`.
- **Branding** — palette lives in `src/styles/custom.css`; no logo is shipped.
