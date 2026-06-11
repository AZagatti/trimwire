# Pre-publish audit (subagent workflow)

Run this **before the repo is first pushed public, and before any release**.
Pushing makes the **entire repo + its full git history** public and (with
release-plz) can trigger a publish — so this is the gate that catches secrets,
personal data, internal content that shouldn't be public, stale docs, and stale
assets *before* they're irreversible.

Fan out to subagents by domain, then the lead reconciles into one scorecard,
fixes what's fixable, and flags the rest (assets, content calls) for the maintainer.

## Ground rules

- **Subagents are UNBIASED** and **read-only** (they report; the lead fixes).
- **History counts.** A secret ever committed is public once pushed — scan git
  history, not just the working tree.
- **Internal ≠ secret, but is it OK public?** Many `docs/*.md` are internal-dev
  (not synced to the site) but still ship in the repo. Judge each for content that
  shouldn't be public (credentials, spend figures, names, candid disparagement).
- Never push/tag (maintainer does). Fix per-increment with the standard trailer.

## Phase 1 — fan out (6 agents, parallel)

| # | Agent | Checks | Flags |
|---|---|---|---|
| **A** | **Secrets & history** | Working tree + `git log -p`/`git grep` across history for API keys, tokens, private keys, `.env`, passwords, connection strings, the project's own inference keys (`/tmp/*_key`), bearer/auth values. | any real credential in tree or history |
| **B** | **PII / personal data** | Emails, home paths (`/home/<user>`), machine names, real names beyond the author, IPs, hardcoded local paths in committed configs/fixtures/tests. | anything identifying or machine-specific |
| **C** | **Internal-content review** | Every repo doc that becomes public — esp. the *non-site* repo docs (AGENTS, SPIKE, DEVELOPMENT, MULTI-HARNESS-PLAN, the contributor workflows). Look for: spend/cost figures, competitor disparagement, unverified claims, embarrassing/half-baked notes, anything strategy- or finance-sensitive. (Candid planning/research notes belong in the gitignored `internal/` folder, not the public repo.) | per doc: keep / sanitize / move to `internal/` / delete |
| **D** | **Doc accuracy & staleness** | Stale claims vs code; **old project name leakage** (`cc-dcp`) in public-facing docs; broken/relative links; placeholder values (`trimwire.example`, wrangler `REPLACE_…`, empty endpoint — note which are intentional-pre-deploy vs must-fix); version refs; README correctness. | wrong/stale/misleading public text |
| **E** | **Assets & branding** | README/`demo.gif` and any screenshots/favicon: do they show the current name, CLI, and UI, or something outdated? Asset sizes. | stale assets needing regeneration (maintainer/manual) |
| **F** | **Repo hygiene / wrong files** | `git ls-files` for files that **shouldn't be committed at all** — OS/editor cruft (`.DS_Store`, `Thumbs.db`, `.idea/`, `.vscode/`), build artifacts (`target/`, `dist/`, `node_modules/`), logs, `*.db`/`*.sqlite`, test outputs, scratch/temp, `/tmp` artifacts, backups (`*.bak`, `*~`), accidental dumps, oversized blobs, anything generated-and-gitignored that slipped in. Plus `.gitignore` correctness, LICENSE(s) present, `pocs/` content, and `Cargo.toml` `include` (what the crate ships). | wrong/accidental committed files, cruft, packaging leaks |

## Phase 2 — reconcile (lead)

Collect into one scorecard: `MUST-FIX (blocks push)` / `FIX` / `MAINTAINER` /
`OK`. Apply the clear fixes (delete cruft, scrub a path, correct stale text),
flag asset regeneration and content-judgment calls to the maintainer, and
**block the push** until every MUST-FIX is cleared.

## Phase 3 — record

Update memory + report the scorecard. Re-run this whole audit before each release.
