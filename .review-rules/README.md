# `.review-rules/` — the AI reviewer's memory

Committed, path-scoped conventions the reviewer loads per PR. This is the **memory
layer** — git *is* the store, so it's persistent and free (no VM/DB needed) and every
change is itself reviewable.

## How it's loaded

`_manifest.toml` maps globs → persona + rule file. For a PR, the reviewer loads
`base.md` (always) **plus** every category whose globs match the changed files, and the
highest-priority match sets the single shared panel persona. Rule text is injected into
the prompt inside delimiters and treated as trusted project content.

## Maintained by the AI (human-gated)

The reviewer proposes rule updates instead of a human hand-writing all of them:

1. When it repeatedly sees a convention (or a repeatedly-dismissed finding), it emits a
   `rule_suggestions` entry in its JSON output: `{category, glob, rule, why}`.
2. The `ai-review-rules` maintenance workflow collects accepted suggestions and **opens a
   small PR** adding/updating the relevant `.review-rules/*.md`.
3. A human merges it. So the memory sharpens itself over time, but **AI-authored rules
   never land unreviewed**.

Why human-gated (not auto-commit to main): rule text goes straight into the model's
prompt, so an auto-committed rule is a prompt-injection vector. A PR + merge keeps a
person in the loop. Write access to this dir should also be pinned via `CODEOWNERS`.

## Editing by hand

Just add a bullet to the relevant file (or a new `[category]` in `_manifest.toml`). Keep
rules as tight bullet lists — dense context beats prose for model attention and cost.
