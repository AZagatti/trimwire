---
description: Python review rules (benchmark harness + CI scripts)
globs: ["benchmark/**", "scripts/**/*.py", "**/*.py"]
---

- Type-annotate new functions; keep imports tidy; no bare `except:`.
- Benchmark/harness code must stay **offline and reproducible** — flag hidden network
  calls, wall-clock/`random` nondeterminism, or unpinned model/data assumptions.
- Prefer stdlib; avoid adding heavy deps to throwaway tooling.
