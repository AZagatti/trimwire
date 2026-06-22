// Single-source-of-truth docs sync. Copies a curated set of the repo's
// docs/*.md into the Starlight content collection at build time so the site
// never holds a second copy that can drift. The generated guides/ dir is
// gitignored. Run automatically by `predev` / `prebuild`.
//
// WRITE-IF-CHANGED, stable directory: we do NOT rm+recreate the output dir.
// Recreating a directory that lives inside Astro's watched content tree makes
// the dev file-watcher re-emit `add` events for every file at startup, which
// Starlight's docs loader reports as "Duplicate id" warnings under `astro dev`
// (a chokidar/FSEvents quirk on macOS + WSL2). Keeping the dir's inode stable
// and only writing files whose content actually changed avoids that entirely.

import {
  readFileSync,
  writeFileSync,
  mkdirSync,
  readdirSync,
  unlinkSync,
  existsSync,
} from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const __dirname = dirname(fileURLToPath(import.meta.url));
const repoRoot = join(__dirname, "..", "..");
const outDir = join(__dirname, "..", "src", "content", "docs", "guides");

// [source file, sidebar title, sidebar order]. Only USER-facing docs are synced;
// other repo docs (ARCHITECTURE, SPIKE, the contributor workflows) stay
// GitHub-only, and the maintainer's planning/research notes live in the
// gitignored `internal/` folder — neither is published to the site.
const DOCS = [
  ["OVERVIEW.md", "What is trimwire?", 0],
  ["FOR-AGENTS.md", "For agents (LLMs)", 12],
  ["FAQ.md", "FAQ & Trust", 1],
  ["SUMMARIZER.md", "Summarizer (optional)", 2],
  ["TELEMETRY.md", "Telemetry (share stats)", 3],
  ["BENCHMARK.md", "Benchmark a local model", 4],
  ["TROUBLESHOOTING.md", "Troubleshooting", 5],
  ["ALTERNATIVES.md", "Alternatives", 6],
  ["VS-ANTHROPIC-NATIVE.md", "vs. Anthropic native", 7],
  ["ROADMAP.md", "Roadmap", 8],
  ["CLI.md", "CLI Reference", 9],
  ["MODEL-COMPATIBILITY.md", "Model compatibility", 10],
  ["PRIVACY.md", "Privacy policy", 11],
];

mkdirSync(outDir, { recursive: true }); // idempotent — no-op (stable inode) if it exists

const expectedSlugs = new Set(DOCS.map(([file]) => file.replace(/\.md$/, "").toLowerCase()));
let wrote = 0;
let unchanged = 0;

for (const [file, title, order] of DOCS) {
  const src = join(repoRoot, "docs", file);
  // Fail loudly + early (not a raw ENOENT stack mid-loop) if a source doc was
  // renamed/removed in the repo — the curated list above must stay in sync.
  if (!existsSync(src)) {
    console.error(`sync-docs: missing source doc: ${src} (update DOCS in this script)`);
    process.exit(1);
  }
  let md = readFileSync(src, "utf8");
  // Drop a leading H1 — Starlight renders the frontmatter title as the page H1,
  // so keeping the body's would duplicate it.
  md = md.replace(/^#\s+.*\r?\n+/, "");
  // Cross-doc links between synced guides are written same-directory in
  // docs/*.md (works on GitHub) — rewrite them to the published guide URLs so
  // they don't 404 as /guides/<this>/<OTHER>.md on the site.
  for (const [other] of DOCS) {
    const otherSlug = other.replace(/\.md$/, "").toLowerCase();
    md = md.replaceAll(`](${other}`, `](/guides/${otherSlug}/`);
  }
  const fm =
    `---\n` +
    `title: ${JSON.stringify(title)}\n` +
    `sidebar:\n  order: ${order}\n` +
    `editUrl: https://github.com/AZagatti/trimwire/edit/main/docs/${file}\n` +
    `---\n\n`;
  const slug = file.replace(/\.md$/, "").toLowerCase();
  const dest = join(outDir, `${slug}.md`);
  const next = fm + md;
  // Write ONLY when content differs — an unchanged file is left untouched so the
  // content-watcher sees no change (no needless reload / duplicate-id churn).
  const prev = existsSync(dest) ? readFileSync(dest, "utf8") : null;
  if (prev === next) {
    unchanged++;
  } else {
    writeFileSync(dest, next);
    wrote++;
  }
}

// Targeted cleanup: drop any generated guide whose source was removed from DOCS,
// without touching (recreating) the directory itself.
for (const entry of readdirSync(outDir)) {
  if (!entry.endsWith(".md")) continue;
  const slug = entry.replace(/\.md$/, "");
  if (!expectedSlugs.has(slug)) {
    unlinkSync(join(outDir, entry));
    console.log(`sync-docs: removed stale guide ${entry}`);
  }
}

console.log(
  `sync-docs: ${DOCS.length} guide(s) — ${wrote} written, ${unchanged} unchanged.`,
);
