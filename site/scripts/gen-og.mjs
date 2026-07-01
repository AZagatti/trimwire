// Generates the static social-share card at public/og.png (1200×630).
//
// Run with `npm run gen-og`. The output is committed so the build and the
// landing-page <meta og:image> are deterministic — we don't rasterize on every
// build. Re-run this only when the card copy or branding changes.
//
// Uses `sharp` (already a site dependency) to rasterize an inline SVG. Keep the
// SVG self-contained (no external fonts/images) so rendering is reproducible.

import sharp from "sharp";
import { fileURLToPath } from "node:url";
import { dirname, resolve } from "node:path";

const __dirname = dirname(fileURLToPath(import.meta.url));
const OUT = resolve(__dirname, "../public/og.png");

const BG = "#04100f";
const PANEL = "#07211f";
const ACCENT = "#2aa39c";
const ACCENT_HI = "#3ec8bf";
const TEXT = "#e6f2f0";
const MUTED = "#7fa8a3";

// 1200×630 is the canonical OG/Twitter summary_large_image ratio.
const svg = `
<svg width="1200" height="630" viewBox="0 0 1200 630" xmlns="http://www.w3.org/2000/svg">
  <defs>
    <linearGradient id="bg" x1="0" y1="0" x2="1" y2="1">
      <stop offset="0" stop-color="${BG}"/>
      <stop offset="1" stop-color="#02201d"/>
    </linearGradient>
  </defs>
  <rect width="1200" height="630" fill="url(#bg)"/>
  <rect x="48" y="48" width="1104" height="534" rx="20" fill="${PANEL}" stroke="#0e3a36" stroke-width="1.5"/>

  <!-- brand mark + wordmark -->
  <g transform="translate(96, 104)">
    <rect x="0" y="0" width="44" height="44" rx="12" fill="${ACCENT}"/>
    <path d="M10 22h14" stroke="#fff" stroke-width="6.2" stroke-linecap="round"/>
    <path d="M30.8 22h7.2" stroke="#fff" stroke-width="6.2" stroke-linecap="round" opacity="0.74"/>
    <text x="62" y="33" font-family="Arial, Helvetica, sans-serif" font-size="32" font-weight="700" fill="${TEXT}">trimwire</text>
  </g>

  <!-- headline -->
  <text x="96" y="290" font-family="Arial, Helvetica, sans-serif" font-size="68" font-weight="800" fill="${TEXT}">Prune your agent's context</text>
  <text x="96" y="372" font-family="Arial, Helvetica, sans-serif" font-size="68" font-weight="800" fill="${ACCENT_HI}">on every request.</text>

  <!-- subhead -->
  <text x="98" y="436" font-family="Arial, Helvetica, sans-serif" font-size="30" font-weight="400" fill="${MUTED}">A tiny local gateway that trims stale context before it reaches the model.</text>

  <!-- terminal line -->
  <g transform="translate(96, 486)">
    <text x="0" y="28" font-family="'DejaVu Sans Mono', 'Liberation Mono', monospace" font-size="30" font-weight="700" fill="${ACCENT}">$ cargo install trimwire</text>
  </g>

  <!-- footer -->
  <text x="96" y="556" font-family="Arial, Helvetica, sans-serif" font-size="26" font-weight="600" fill="${TEXT}">trimwire.dev</text>
  <text x="1104" y="556" text-anchor="end" font-family="Arial, Helvetica, sans-serif" font-size="24" font-weight="400" fill="${MUTED}">signed, verifiable releases</text>
</svg>`;

await sharp(Buffer.from(svg)).png().toFile(OUT);
console.log(`gen-og: wrote ${OUT}`);
