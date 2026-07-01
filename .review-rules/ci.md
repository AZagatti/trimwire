---
description: CI / workflows / shell review rules
globs: [".github/**", "**/*.sh", "Makefile"]
---

- Actions pinned by full commit SHA + a version comment (repo policy; `actionlint` gates CI).
- Least-privilege `permissions:` per job. Never interpolate PR-controlled data into a
  `run:` block — pass it via `env:` (script-injection). `set -euo pipefail` in shell steps.
- No `pull_request_target` that checks out PR head code. Don't write untrusted input to
  `$GITHUB_ENV`/`$GITHUB_OUTPUT` unescaped.
