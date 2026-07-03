# AI code review

A panel of LLMs reviews every PR and posts a single sticky comment. It's
model-agnostic and provider-agnostic (z.ai GLM + OpenRouter by default), needs no
Claude billing, and is **advisory only** — it comments, it does not gate merges.

## How it works

Two workflows, split for fork safety (a privileged job must never execute
untrusted PR code — the "pwn request" problem):

```
ai-review-gather.yml   on: pull_request       (NO secrets, read-only token)
   └─ captures the diff + PR metadata → uploads an artifact
                              │  workflow_run (completed)
ai-review-post.yml     on: workflow_run        (secrets, pull-requests: write)
   └─ checks out the trusted base repo (script + prompts)
   └─ downloads the artifact (untrusted *data* only — never executed)
   └─ scripts/ai_review.py:  panel → aggregator → review.md
   └─ posts/updates the sticky comment
```

The privileged job only ever ingests the PR **diff as text**, which it treats as
untrusted (the reviewer prompt refuses instructions embedded in the diff). This is
the safe alternative to `pull_request_target`.

A third workflow, **`ai-review-manual.yml`**, is a `workflow_dispatch` you run from
the Actions tab: enter a PR number and the **exact models** you want (three panel
slots + an aggregator, any OpenRouter ids). Because dispatching requires write access,
it's trusted and runs as a single job. Use it to throw heavier models at a big PR —
e.g. `anthropic/claude-opus-4.8`, `openai/gpt-5.5`, `google/gemini-3.5-flash`.

## The panel — routed personas

Rather than three generalist reviewers, the diff is routed to **specialist personas** —
checklist modules (SENTINEL/logic, WARDEN/security, CHRONICLER/contracts, FERRUS/Rust
`unsafe`, PYTHIA/Python, GATEKEEPER/GHA-security, VANGUARD/frontend, ARGUS/accessibility,
PACER/web-vitals, SENTRY/supply-chain, SCRIBE/docs, SCOUT/test-quality). A deterministic
router (file globs) picks the relevant personas per PR and **composes them by model-lane**
— one call per model. Cost therefore scales with the PR: a docs-only change is one call,
a cross-stack change ~5. Findings are then **deduplicated deterministically** (by
`file:line` + title) — no paid LLM aggregator, no recall leak.

Personas + their assigned models live in
[`scripts/ai_review_personas.py`](../scripts/ai_review_personas.py); the legacy
3-generalist panel + LLM aggregator is still available via `AI_REVIEW_LEGACY_PANEL=1`.

**Default assignments** — GLM-5.2 (free on z.ai) for the logic/baseline personas
(SENTINEL/SCOUT/GATEKEEPER/FERRUS), Qwen3-Coder-30B (~$0.0019/call) for ARGUS/PACER/SENTRY,
DeepSeek-V4-Flash (~$0.0021) for PYTHIA/VANGUARD, DeepSeek-V3.2 for CHRONICLER/SCRIBE, and
GPT-5-mini only for WARDEN. A review runs **<$0.02/PR**.

**Heavy review** (manual workflow) — throw stronger OpenRouter models at a big PR via the
dispatch inputs; all reachable through the one OpenRouter key.

## How the personas & models were chosen

Personas are **standards-grounded checklists** (CWE Top 25, WCAG 2.2, Rust API Guidelines,
GitHub Actions hardening, web.dev Core Web Vitals). Model→persona fit was picked by a
**67-case real-fix-PR classification** — each case a distinct merged fix-PR with a known
defect — scored by a blinded LLM judge on recall, then stress-tested on a **holistic bench**
(the full panel over whole ambiguous PRs, scored for recall *and* false-positives).
Highlights:

- **The checklist beats the model on most personas**: given a concrete checklist, a
  near-free model matches an expensive one — tuning the accessibility checklist lifted a
  $0.0019 model from 0.60 to 1.00 recall and replaced GPT-5-mini. The knowledge lives in
  the checklist, so the model can be cheap.
- **Synthetic ≠ real, and honestly so**: isolated planted-bug recall (~80–100%) massively
  overstates real-PR recall. On whole ambiguous PRs the panel catches **~27%** of subtle
  issues at ~19% noise. So this reviewer is **advisory** — a cheap second pair of eyes that
  stays quiet and defers to CI. **Completeness belongs to deterministic tools** (clippy,
  `cargo-semver-checks`, `tsc`, ruff) + a green-CI gate, not the LLM.
- **Reasoning level matters**: each model runs at its validated level (a model's default
  reasoning varies wildly and changes review quality).

## Cost & anti-spam

- Runs on `opened` / `synchronize` / `reopened`; `concurrency` cancels superseded runs.
- **Skipped:** draft PRs, bot PRs, `skip-ai-review`-labelled PRs, and PRs from
  outside authors (only owner/member/collaborator auto-run — a one-off past merged
  PR earns `CONTRIBUTOR`, which is **not** trusted to spend LLM budget, so outside
  authors are reviewed on demand via the manual workflow). GitHub's built-in approval
  gate for outside-contributor workflows is the first line of defense; this is
  belt-and-suspenders.
- **`ai-review-strict` label:** low-noise mode — the sticky comment shows only
  findings ≥ 2 models agree on (security findings are always shown; hidden solo
  findings remain in the collapsed raw-panel section).
- **`AI_REVIEW_SAMPLES` repo variable (opt-in recall boost):** default `1` (single
  deterministic pass — the low-noise default). Set to `2`–`5` to sample each model
  N times at spread temperatures and union the findings; higher-temperature passes
  surface bugs a single pass misses (evidence: multi-pass raises catch rate ~+15-25%),
  at ~N× cost and more noise. `consensus` badges then count cross-sample agreement, so
  pairing it with the `ai-review-strict` label keeps only the findings multiple passes
  agree on.
- The diff is filtered (lockfiles, snapshots, generated/min files) and budgeted
  (per-file + total caps) before any model is called.

## Accepted / false-positive tracking

A weekly job (`ai-review-track.yml`) mines the bot's inline-comment threads for maintainer
signals — a 👍/👎 reaction or a reply like "good catch" / "false positive" — and publishes
per-persona acceptance rates to a dedicated **`ai-review-data`** branch (no noise on `main`).
Each inline comment carries a hidden `ai-review-meta` marker so a finding can be attributed
to the persona(s) that raised it. Over time this shows which personas are noisy, so their
checklists can be tightened. React on the bot's comments to feed it — nothing else needed.

## Setup

1. Add repo secrets: `ZAI_API_KEY`, `OPENROUTER_API_KEY`.
2. Confirm the z.ai base URL for your plan in `PROVIDERS` (`ZAI_BASE_URL`).

Prompts live in [`.github/ai-review/`](../.github/ai-review/) (`REVIEWER.md`,
`AGGREGATOR.md`) — edit them without touching code.
