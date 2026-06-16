# Testing strategy

Short version: **test where the risk is.** trimwire is a correctness- and
privacy-critical tool, so the gates concentrate on (1) never dropping load-bearing
context, (2) never leaking content in telemetry, and (3) the CLI working
end-to-end. Everything below runs in CI.

## Rust (the proxy + CLI)

| Layer | Where | What it proves |
|-------|-------|----------------|
| Unit | `src/**` (`#[test]`, ~600) | each strategy/transform in isolation |
| Parity oracle | `tests/phase0/*` + `make phase0` | the Rust port matches the Python reference, byte-for-byte, on committed fixtures |
| Harm gate | `tests/harm.rs`, `tests/false_done_gate.rs` | both safety directions: `harm.rs` — no profile drops a unique-dependency / recent fact (lower bound on retention); `false_done_gate.rs` — no summary injects a false completion ("tests passed" / "committed") the source slice doesn't support, while honest hedged phrasing still passes |
| Efficiency | `tests/integration.rs` | aggressive profiles **actually shrink** the body (`gateway_prunes_*`, `image_strip_shrinks_*`) — the upper-bound complement to the harm gate |
| Gateway integration | `tests/integration.rs` (wiremock) | real HTTP in → pruned, orphan-free, smaller body out |
| Snapshots | `insta` (`tests/snapshots/`) | stable structural output; serialize via `BTreeMap`, never `HashMap` order |
| CLI e2e | `tests/cli.rs` (Unix) | the built binary: `run` launches claude with env + forwards args + propagates exit, `install` is idempotent, `doctor`/`stats`/`config`/`summarizer setup` behave |
| Cross-platform smoke | `ci.yml` matrix | daemon binds a port on linux/macOS/windows; binary-size budget |

Gated by `.github/workflows/ci.yml` (fmt, clippy `-D warnings`, `cargo nextest run`
+ `cargo test --doc`, doc, package, MSRV, cargo-deny/audit, parity, cross-platform)
on every PR **and** push to `main`. The runner is `cargo nextest` for per-test
process isolation (matters for the gateway/port-binding integration tests);
because nextest doesn't run doctests, `cargo test --doc` runs alongside it.

## Collector (Cloudflare Worker — the telemetry privacy gate)

Two tiers, both gated by `.github/workflows/collector.yml` (path-filtered to
`collector/**`, on PR + push to `main`):

- **Pure-logic** (`npm test`, plain vitest): `validate.ts` (content-free / closed-enum
  fail-closed validation) and `aggregate.ts` (k-anonymity + l-diversity suppression).
  Fast, no Cloudflare account.
- **HTTP gate** (`npm run test:routes`, `@cloudflare/vitest-pool-workers` in real
  `workerd`): `index.ts` routing + D1 I/O + **k-anonymity enforced at the boundary**
  (ingest → 400 on bad input, GET suppresses sub-k groups, security headers). This is
  the layer that *enforces* the guarantees the pure-logic tests *decide*; it seeds an
  in-memory D1 from `schema.sql` so there's no drift from production.

Valid wire-payload shapes live once in `collector/test/fixtures.ts` (shared by all
collector tests) so a schema change updates the contract in exactly one place.

## Site (Astro/Starlight docs + dashboard) — the Testing Trophy

`static (astro check) → unit/integration (vitest + happy-dom) → e2e (Playwright +
axe-core) → perf/a11y budget (Lighthouse CI)`. Gated by `.github/workflows/site.yml`,
path-filtered to `site/**`. Cost-aware: the **full trophy runs on PRs**; a direct
push to `main` runs only the cheap tier (typecheck + vitest + build) — `e2e` and
`lighthouse` are `if: github.event_name == 'pull_request'`.

Prefer happy-dom for data-in → DOM-out logic (fast, no build); reserve Playwright for
what happy-dom can't see (CSSOM, focus, accessibility). Chromium-only on purpose.

## Deferred (researched, not yet adopted — pick up by value)

Reviewed against current Rust-testing practice and Kent C. Dodds' Testing Trophy.
None are blockers; ordered by payoff (`cargo nextest` — #1 below — is now adopted):

1. `rstest` `#[case]` for the profile-parameterized loops (already a dev-dep).
2. `proptest` for a pruning invariant (e.g. pure-pruning output ⊆ input) — design
   the invariant carefully; summarizer mode *replaces* content, so "subsequence"
   only holds for non-summarizing strategies.
3. One Playwright screenshot per dashboard page (CSS-regression net that axe +
   Lighthouse miss).
4. `insta` redaction filters once any snapshot starts carrying timestamps/ids.
