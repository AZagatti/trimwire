---
description: Project-wide conventions for trimwire (always loaded)
---

- trimwire is a transparent LLM-proxy CLI (Rust core) + Astro site + Cloudflare
  Worker collector (TS) + Python benchmark harness. Match the lens to the file.
- Review only added/changed lines. **Silence = approve** — an empty findings list
  with verdict `approve` is the correct, expected result for a clean PR. Never
  invent a finding to look useful.
- Don't flag anything a formatter/linter already enforces (rustfmt, `clippy -D
  warnings`, prettier, eslint, tsc). If CI signals are provided, defer to them.
- Conventional commits; `CHANGELOG.md` is owned by release-plz — never hand-edit it.
- Module/layer-boundary changes should update `ARCHITECTURE.md`; config changes
  should update `docs/CONFIGURATION.md`.
