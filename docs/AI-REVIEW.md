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

## The panel

Each model reviews the diff independently; an **aggregator** merges, deduplicates,
consensus-scores, and ranks the findings into one comment. Panel members and the
aggregator are configured at the top of [`scripts/ai_review.py`](../scripts/ai_review.py)
(or overridden with the `AI_REVIEW_PANEL` / `AI_REVIEW_AGGREGATOR` env vars).

**Default (standard) panel** — GLM-5-Turbo (z.ai) + Gemini-3.5-Flash + Nex-N2-Pro
(OpenRouter), aggregated by Gemini-3.5-Flash. Three model lineages. GLM-5-Turbo is
free on the z.ai subscription; the two OpenRouter legs plus the aggregator run
~$0.02–0.04/PR at current Gemini-3.5-Flash pricing (after OpenRouter's implicit
prompt caching on the stable system prefix).

**Heavy review** (manual workflow) — type whichever OpenRouter models you want into
the three panel slots + aggregator (e.g. Opus 4.8, GPT-5.5, Gemini-3.5-Flash). All
reachable via the one OpenRouter key, so no extra secrets. Use it for large or
high-stakes PRs where the extra cost is worth a stronger read.

## How the models were chosen

Picks came from an offline **dogfood** that ranks candidates on *review quality*, not
on general benchmarks or summarization ability (those are different skills — a great
summarizer or coding model is often a mediocre reviewer). The harness plants known
bugs (plus clean controls and a prompt-injection trap) in realistic Rust diffs, has
each candidate review them with the production prompt, and grades the output with a
blinded LLM judge on: bug recall, false-positive rate, injection resistance, and
latency. Highlights:

- Two strong models already **saturate recall** on typical bugs, so the third panel
  slot is for consensus confidence, not coverage.
- **Latency matters**: panel calls run concurrently, so per-PR latency ≈ the slowest
  member. GLM-5-Turbo anchors because it matches heavier GLM models on quality at a
  fraction of the time.
- Coder-specialist and summarizer-optimized models consistently under-review — they
  were tested and dropped.

## Cost & anti-spam

- Runs on `opened` / `synchronize` / `reopened`; `concurrency` cancels superseded runs.
- **Skipped:** draft PRs, bot PRs, `skip-ai-review`-labelled PRs, and PRs from
  first-time / outside authors (only owner/member/collaborator/returning-contributor
  auto-run — others are reviewed on demand via the manual workflow). GitHub's built-in
  approval gate for outside-contributor workflows is the first line of defense; this
  is belt-and-suspenders.
- The diff is filtered (lockfiles, snapshots, generated/min files) and budgeted
  (per-file + total caps) before any model is called.

## Setup

1. Add repo secrets: `ZAI_API_KEY`, `OPENROUTER_API_KEY`.
2. Confirm the z.ai base URL for your plan in `PROVIDERS` (`ZAI_BASE_URL`).

Prompts live in [`.github/ai-review/`](../.github/ai-review/) (`REVIEWER.md`,
`AGGREGATOR.md`) — edit them without touching code.
