# AI code review (multi-model)

A panel of LLMs reviews every PR; an aggregator merges their findings into one
sticky comment. Built model-agnostic and provider-agnostic (z.ai GLM + OpenRouter
by default), with no Claude billing.

## How it works

Two workflows, split for fork safety (the "pwn request" problem — a privileged
job must never execute untrusted PR code):

```
ai-review-gather.yml   on: pull_request        (NO secrets, read-only token)
   └─ captures pr-files.json + pr-meta.json → uploads artifact
                              │  workflow_run (completed)
ai-review-post.yml     on: workflow_run         (secrets, pull-requests: write)
   └─ checks out the TRUSTED base repo (script + prompts)
   └─ downloads the artifact (untrusted *data* only — never executed)
   └─ scripts/ai_review.py:  panel → aggregator → review.md
   └─ posts/updates the sticky comment (marker <!-- ai-code-review -->)
```

Why not `pull_request_target`? Same privileges, but it's one careless `checkout`
of PR head away from leaking secrets. The `workflow_run` split keeps the secret-
holding job from ever touching PR code.

## Configuring the panel

Edit `PANEL` / `AGGREGATOR` at the top of [`scripts/ai_review.py`](../../scripts/ai_review.py),
or override at runtime with the `AI_REVIEW_PANEL` / `AI_REVIEW_AGGREGATOR` env
vars (JSON). Each member is `{name, provider, model}`; providers and their base
URLs / key env vars live in `PROVIDERS`.

Defaults (chosen for COMPLEMENTARITY on real PRs — see `internal/ai-review-bench/`;
each member catches different real issues, at its real-code-optimal reasoning level):
**panel** = DeepSeek-V3.2 @off (thoroughness/tests) + GPT-5-mini @medium (security
breadth) + GLM-5.2 @fast (architecture/config, free z.ai) — 3 lineages;
**aggregator** = Gemini-3.5-Flash @medium. ~$0.005/PR.

## Prompts

- [`REVIEWER.md`](REVIEWER.md) — the per-model reviewer system prompt
  (trimwire layer rules, severity tags, injection defense). Edit without touching code.
- [`AGGREGATOR.md`](AGGREGATOR.md) — how findings are merged, deduped, and ranked.

## Setup

1. Add repo secrets: `ZAI_API_KEY`, `OPENROUTER_API_KEY`.
2. Confirm the z.ai base URL for your plan in `PROVIDERS` (`ZAI_BASE_URL`).

Workflow actions are SHA-pinned (repo policy — `actionlint` runs in CI).

## Cost & noise controls

- Runs on `opened`/`synchronize`/`reopened`; `concurrency` cancels superseded runs.
- Skips bot PRs and any PR labeled `skip-ai-review`.
- Diff is filtered (drops lockfiles, snapshots, generated/min files) and budgeted
  (per-file + total char caps) before any model is called.
- Advisory only — posts a comment, is **not** a required merge check.
