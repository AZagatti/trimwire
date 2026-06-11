# Regression & bench sweep (subagent workflow)

A reusable, mostly-offline workflow to run **before a release, after a meaningful
change set, or periodically** — to prove we didn't break anything, catch
savings/perf regressions, and surface doc/memory drift and coverage gaps.

It fans work out to subagents by domain, then the lead reconciles into one
scorecard and applies the clear fixes. Everything except one harness is offline
and deterministic, so the whole sweep runs unattended.

## When to run it

- Before the maintainer tags a release.
- After incorporating any feature/fix to `main` (or a feature branch).
- On a periodic health check (e.g. a `/loop`).

## Ground rules (inherit the repo conventions)

- **Subagents are UNBIASED** — judges of pass/fail and regression, not skewed
  conservative or lenient. They report facts (numbers, diffs), the lead decides.
- **Offline by default.** Only `examples/api_harm` needs a live provider key
  (`/tmp/openrouter_key` or `/tmp/zai_key` + `TRIMWIRE_API_HARM_*`); skip it
  unless the maintainer asks and a key is present. Everything else is deterministic.
- **Never push/tag** (maintainer owns release). **Never touch live `~/.claude`** —
  copy any transcript to `/tmp` first.
- Commit any fix per-increment with the standard trailer.
- Use no-worktree subagents for read-mostly audits; use a worktree only if a
  subagent must build/run in isolation concurrently.

## Phase 0 — baseline (lead, ~1 min)

Record the starting point so regressions are measurable:

```bash
git rev-parse --short HEAD && git branch --show-current && git status --short
cargo test --lib 2>&1 | tail -1     # current green count
```

Note the prior baseline numbers to compare against: last recorded bench
savings/cost (`docs/BENCHMARK.md`), test count, binary size (CI limit 12 MB).

## Phase 1 — fan out (6 subagents, parallel)

Spawn these as separate subagents. Each returns a short verdict block:
`PASS/FAIL/REGRESSION`, the numbers, and any must-fix.

| # | Agent | Does | Pass criteria |
|---|---|---|---|
| **A** | **Build & gate** | `cargo fmt --all -- --check`; `cargo clippy --all-targets -- -D warnings`; `cargo test --no-fail-fast`; `cargo build --release`; `cargo doc --no-deps` (`RUSTDOCFLAGS=-D warnings`); `cargo package --locked` | all clean; test count ≥ baseline; binary ≤ 12 MB |
| **B** | **Invariant harnesses** (offline) | `cargo run --release --example longitudinal_harm`; then `compaction_harm`, `density_harm`, `needle`, `residual_profile` | every assertion holds (recent-verbatim, orphan-free pairing, frozen-replay no-amplification, bounded collapse, needle retention) |
| **C** | **Bench / savings regression** (offline) | `cargo run --release --example bench` (the headline savings/cost sweep — compare to `benchmark/results/RESULTS.md`); `session_profile` (profiles real `~/.claude` sessions). **Note:** `cost_replay`/`runway` need a reconstructed `BODY.json` arg and `compaction_bench` needs a long fixture — only run those if a body is supplied, else skip. | size win on every corpus; cost behaviour matches the known profile (long-session win, short-session loss is expected); deterministic numbers match `RESULTS.md` (timing is host-dependent — not a regression) |
| **D** | **Parity oracle** | `make phase0` then `make phase0-verify` | Python invariant suite green; committed fixtures not drifted |
| **E** | **Docs/memory drift** | Cross-check code vs docs: config keys (`src/config.rs` ↔ `CONFIGURATION.md`), CLI (`src/main.rs` ↔ `docs/CLI.md`), strategies (`src/strategies/` ↔ `ARCHITECTURE.md`/`AGENTS.md`), summarizer knobs (↔ `SUMMARIZER.md`). Confirm `MEMORY.md` RESUME-HERE pointer + `CHANGELOG.md` are current. | every shipped flag/CLI/strategy is documented; no stale or wrong values; no dead references |
| **F** | **Gap / coverage scan** | New `pub` fns/strategies without a test; untested config flags; `TODO`/`FIXME`/`dbg!`; behaviour added since the last sweep that isn't covered or documented | no untested new surface; no stray debug; gaps listed with a recommendation |

Notes for agents:
- The expected cost profile is **size win always, cost win only on long sessions**
  (short corpora are a cost *loss* by design — not a regression). Don't flag it.
- `[trimwire: …]`/elision markers are intentional. Don't flag them as content drift.
- Run from a clean tree; if an example needs a fixture, it's under the repo, no network.

## Phase 2 — reconcile (lead)

Collect the six verdicts into one scorecard:

```
SWEEP <short-sha> (<branch>) — <date>
A build/gate:     PASS | <n> tests, clippy clean, bin <x> MB
B invariants:     PASS | longitudinal/compaction/density/needle all hold
C bench:          PASS | size −<x>%; cost profile as expected (no drift)
D parity:         PASS | phase0 green, fixtures current
E docs/memory:    <FINDINGS — flags/CLI/strategies documented? pointer current?>
F gaps:           <FINDINGS — untested surface / TODOs / missing docs>
MUST-FIX:         <list, or none>
DOC/MEMORY TODO:  <list, or none>
MISSING/FOLLOW-UP:<list, or none>
```

Then:
- **Apply the clear must-fixes** (failing test, lint, a doc that contradicts code,
  a missing config-default) — build + re-gate, commit per increment.
- **Flag genuine big-shift or product decisions** for the maintainer; don't act.
- If a bench regressed, bisect to the cause before "fixing" the number.

## Phase 3 — record (lead)

- Update `CHANGELOG.md`/docs for anything the sweep changed.
- If the sweep was meaningful, add a one-line memory note (new `CONTINUATION`)
  with the scorecard summary and any follow-ups; update the `MEMORY.md`
  RESUME-HERE pointer if state moved.
- Report the scorecard to the maintainer. Do **not** push or tag.

## Quick one-shot (no subagents)

For a fast local check without the fan-out, the gate + the offline harnesses are
what CI runs:

```bash
cargo fmt --all -- --check && \
cargo clippy --all-targets -- -D warnings && \
cargo test --no-fail-fast && \
cargo run --release --example longitudinal_harm && \
make phase0 && make phase0-verify
```

The subagent fan-out adds the bench-regression, docs/memory-drift, and
coverage-gap passes that CI does not cover.
