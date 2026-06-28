# Flightdeck cockpit — static preview (mock data)

`index.html` is a **self-contained, mock-data preview** of the cockpit UI (docs 04/09). It has
**no daemon, no backend, no build step** — every figure is canned, content-free sample data — so
the UI can be opened from a static link to review the look, layout, and the planned screens
(Live · Savings · Strategies · Sessions · Config).

## Preview link

Served via githack straight from this branch (no deploy needed):

**https://raw.githack.com/AZagatti/trimwire/claude/trimwire-cockpit-ui-8s69wn/docs/cockpit/demo/index.html**

(Or open `docs/cockpit/demo/index.html` locally — it's a single file.) It can also be dropped into
`site/public/cockpit-demo/` to ride the site's Cloudflare deploy at `/cockpit-demo/`.

## What this is / isn't

- **Is:** a faithful preview of the cockpit's design + information architecture, using the real
  teal design tokens and the planned panes, with realistic *content-free* numbers.
- **Isn't:** the real cockpit. The working POC is served by the `trimwire` binary on a loopback
  control API (`src/admin/`, run `trimwire cockpit`) and reads the **local content-free ledger** —
  it never renders prompts, tool results, or file paths. This demo mirrors that: only counts,
  bytes, tokens, model names, and strategy names appear.

## Relationship to the POC

The embedded POC (`src/admin/cockpit.html`) is the *minimal real* UI wired to live endpoints;
this demo is the *fuller* UI with mock data for previewing the vision without a running daemon.
Both share the same design language; the production build collapses them into one frontend
(doc 05 §5 — one artifact, browser + PWA + Tauri).
