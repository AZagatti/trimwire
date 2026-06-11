# AGENTS.md

Rules and pointers for any AI agent (Claude Code, OpenCode, Cursor, etc.)
working in this repo. **Single source of truth — [`CLAUDE.md`](CLAUDE.md)
defers to this file.**

## First session in this repo? Read in this order (~10 min)

0. **ai-memory** (auto-loaded at session start) carries the live project state —
   read it first. If absent, the docs below are the fallback.
1. **This file (`AGENTS.md`)** — repo conventions, product guardrails, the
   **Working conventions** section (subagents / council / DS), what NOT to do, layer rules.
2. **[`ARCHITECTURE.md`](ARCHITECTURE.md)** — module-level design + layer-rules table.
   (The strategy list/diagram is mid-update; the authoritative run order is the doc-comment
   in `src/strategies/mod.rs`.)
3. **[`.agents/skills/trimwire-gateway/SKILL.md`](.agents/skills/trimwire-gateway/SKILL.md)** —
   per-task workflow recipes (invoke when touching `src/proxy/gateway.rs`,
   `src/strategies/*`, `src/pairing.rs`, `src/ledger.rs`).
4. **`SPIKE.md` + `DEVELOPMENT.md`** — design/build history (useful but partially
   stale; trust the code + ai-memory over them).

> **Maintainer-local docs:** the planning/research/backlog docs (LAUNCH-BACKLOG,
> BACKLOG-SCORED, COMPETITIVE-SCAN, IDEAS-BACKLOG, MULTI-HARNESS context,
> LOCAL-MODEL-* notes, etc.) live in a **gitignored `internal/` folder** — present
> on the maintainer's machine, **not in the public repo**. ai-memory is the live
> "what's next".

Sanity-check that the workspace builds:

```bash
cargo check --all-targets
make phase0    # auto-creates a venv + installs pytest on first run
```

`cargo check` is the load-bearing one for Rust work; `make phase0` is
the Python invariant suite that the Rust strategies will mirror. If
both pass, the workspace is healthy.

To resume, find the next-up step in `DEVELOPMENT.md` "Phase 1" and start
there. Each step lists acceptance criteria — when those pass, commit and
move on.


## Project state

trimwire is a Rust HTTP gateway for Claude Code context pruning.

> **⚠️ NOT SHIPPED — NO USERS YET.** trimwire has never been released, tagged, or
> published — there are **zero installs in the wild** and **no configs to protect**.
> Do NOT spend effort on backward-compatibility, config migration, deprecation
> warnings/aliases, "don't surprise existing users," or staged rollouts. Just make the
> change cleanly and update the docs/tests. (This burned cycles before — e.g. a profile
> rename got migration-alias machinery it never needed.) When a "what about existing
> users?" worry comes up: there are none — proceed. The real review bars are correctness,
> the guardrails below, and the maintainer's product sign-off — not back-compat.

- **Spec:** [`SPIKE.md`](SPIKE.md) — full design, empirical validation,
  build/document split, two-phase build plan. **Architecture and tier
  decisions live there; cite the section when you make a change that
  contradicts it.**
- **Architecture overview:** [`ARCHITECTURE.md`](ARCHITECTURE.md) —
  module-level design, layer rules, decision log.
- **Reference POCs:** [`pocs/`](pocs/) — throwaway Python + bash that
  validated the architecture during the spike. Not part of the build;
  do not import or invoke from Rust code.
- **Target:** ~1000-1200 LOC Rust binary using `hyper` + `serde_json` +
  `tokio` + `rusqlite`. Single static binary distributed via a cross-platform
  GitHub Actions release matrix (see `.github/workflows/release.yml`).

## Product direction & guardrails (maintainer, 2026-06)

The north star: be **opencode-dcp-like, adapted to a transparent proxy** —
**automagic** (on by default, prunes every turn), **proactive** (prunes harder
as a session grows so it helps even on huge 1M-context sessions where the window
isn't near full — the fight is *context rot*, not the wall), **especially useful
on code sessions**, and **don't lose what was pruned** (recoverable, so the agent
doesn't "forget what happened" and get dumber). "Proactive" is **not** a new
fill-gate — trimwire already prunes every request; the lever is *aggressive
defaults + stable re-pruning on by default* (reprune keeps aggression cache-safe;
spike shows 80-94% prefix reuse at every length).

**Guardrails — do not drift from the core:**
- **Stay lightweight.** trimwire is a small, fast, single static binary. **No
  heavy runtime dependencies** — a served/bundled local LLM (e.g. ollama) is
  **rejected** (too heavy for a normal dev box / WSL). Any ML must be tiny,
  in-process, and **behind an opt-in cargo feature** that never bloats the
  default binary or forces a first-run model download for non-opt-in users.
- **Transparent-proxy limits are real.** No in-band tools/nudges (can't make the
  model call a "compress" tool, can't inject "please compact" messages — those
  break the prompt-cache prefix). Deliver the *value* out-of-band instead.
- **Latency/delay is ACCEPTABLE (maintainer, 2026-06).** A batched/checkpoint prune
  that adds an occasional mid-session delay is fine — do NOT treat latency as a
  constraint that limits how aggressively we batch. Prefer batching (prune less
  often, more each time, holding the prefix stable between checkpoints) over
  per-turn pruning: it's both cache-cheaper and the latency cost is a non-issue.
- **Cost ≠ bytes.** "X% lighter" is BYTES; token COST depends on the prompt cache
  (cache_read ≈ 0.1× vs cache_creation ≈ 1.25×, invalidated from the first changed
  byte). Block REMOVAL (thinking_strip) USED to bust the cache every turn (reprune bypass),
  but reprune now REPLAYS thinking-block removals by signature (`apply_thinking_removals`), so
  the pruned prefix is byte-identical between checkpoints and the cache holds — one bust per
  checkpoint, not per turn (live-confirmed 92% cache-hit). So thinking_strip is now cache-safe
  and ON in `default`. Content/input OVERWRITE strategies (incl. the new superseded-input
  collapse + demand-paging) are reprune-replayable too. The remaining cache rule: don't add a
  NEW removal-based strategy without teaching reprune to replay it.
- **Keep the load-bearing invariants:** orphan-safety (PairingIndex pre/post),
  prompt-cache prefix stability (reprune), no message content in the ledger,
  no spend/routing decisions for the user, deterministic core (Rust↔Python
  parity oracle) — a non-deterministic ML path must be opt-in and excluded from
  the oracle, never in a default profile.
- **"Summary of pruned content" without a model:** the closest model-free value
  is **offload-to-artifact** (move the verbatim old result to a local sidecar the
  agent can re-read) — not a lossy generated summary (extractive summarizers
  re-emit dropped content → leak/stale/parity problems; rejected). The other half
  of the intent is **transparency** — a deterministic, ledger-backed "what got
  pruned" report in `stats`/`preview`, not an in-band summary.
- **Smarter pruning ("prune the right things") — pick the lightest tool that
  works:** start with **no-model lexical relevance (BM25, pure-Rust, deterministic,
  zero download)** — sufficient for most code sessions (paths/symbols/errors are
  precise terminology). Escalate to **static embeddings (model2vec — no ONNX
  runtime, ~tens of MB, microsecond, deterministic)** only if the semantic tail is
  worth it. **Avoid heavier ML** (fastembed/ONNX `ort`, candle, and definitely
  rust-bert/libtorch) — fastembed was the maintainer's first guess but is
  over-built for ranking a handful of strings per request. Any model is opt-in,
  behind a cargo feature, never in a default profile / parity oracle.
- **Don't over-engineer the surface.** The profile story must stay **simple** —
  **two profiles**: `default` (aggressive + reprune) and `gentle` (dedup + purge +
  a *conservative* bloat_cap at a high threshold / large keep_recent + reprune —
  NOT dedup+purge-only, so it's still >0% on bloat-heavy sessions). NOT the
  confusing low/medium/high intensity dial; no `pro`. One good default + one
  gentler escape hatch. Keep the tool focused on its core: deterministic,
  transparent, cache-safe context pruning. When tempted to expand scope, re-read
  this section.
- **Coexist with Claude Code's OWN built-in microcompact (resolved, not a
  blocker):** CC's cold-path microcompact rewrites its own messages[] client-side
  and sends `tool_result.content == "[Old tool result content cleared]"` on the
  wire (no `context_management`/beta signal, so detect-and-defer won't catch it).
  trimwire must **skip already-cleared results** — add `is_already_cleared()` and
  extend the sliding_window/bloat_cap idempotence guard (the same pattern that
  skips trimwire's own stub). Ship it WITH the aggressive flip. Cache churn, not
  corruption; reprune's prefix guard absorbs most of it.
- **CC already offloads big results to disk** (it writes `tool-results/*.txt`
  sidecars and may send a reference inline). So trimwire's planned offload-to-
  artifact is **possibly redundant** — VERIFY whether CC's offload already shrinks
  what trimwire sees on the wire before building trimwire's own. (Update §13C,
  2026-06-10: `bloat_cap` now also handles multi-block **array** `tool_result`s —
  it salvages the bulky text blocks in place and total-erases only pure non-text/
  image arrays — so the former string-only coverage gap is closed.)
- **"Aggression is cache-safe" is CC-only, and sub-agents share the session-id
  (CONFIRMED by a real wire capture).** reprune engages only with the
  `x-claude-code-session-id` header, and CC sends sub-agent + background (haiku/
  sonnet) calls under the *same* session-id → reprune thrashes to stateless.
  **Fix: key reprune on `session_id + model`** (won't fully separate two same-model
  streams — accept or refine). Non-CC clients get stateless aggressive pruning.
- **ToS reality (researched + cited; DO NOT cross):** the Claude **subscription
  OAuth token** that flows through trimwire is "exclusively for ordinary use of
  Claude Code and other native Anthropic applications." trimwire **must NOT
  originate its own model calls** on that token (e.g. an LLM summary/compaction) —
  it's prohibited and **actively enforced** (server-side block killed
  OpenClaw/OpenCode/Cline in 2026). ⇒ **model-summary is OFF the table** unless the
  user supplies their own API key. trimwire's *current* core (prune + forward the
  user's own CC requests) is a **gray area** — likely fine (local proxy, own
  session, preserves auth, no extra calls; Anthropic's own LLM-gateway docs bless
  `ANTHROPIC_BASE_URL` proxies; enforcement only hit redistributed multi-user
  token-reuse), but enforcement *could* broaden. Eyes open.
- **Keep these alternatives OPEN (don't design them away)** in case the gray area
  closes or needs change: (a) cross-client (Cursor/Cline/Codex) + API-key users
  (fully compliant; model-summary becomes possible there); (b) lean on
  `trimwire sweep` (on-disk transcript cleaning — no wire, zero ToS exposure);
  (c) an in-agent plugin (opencode-dcp style, sanctioned in-band) vs a wire proxy.
- **Live decisions + current build state + next steps: [`internal/BACKLOG-SCORED.md`](internal/BACKLOG-SCORED.md)**
  (BUILD SEQUENCE, code-first) + **ai-memory** (auto-loaded). Read those first on resume.
  (The old PIVOT-STATE / REDUCTION-OPTIONS / IMPROVEMENTS-RESEARCH / RESEARCH-context-strategy
  docs were deleted 2026-06-05 — superseded by BACKLOG-SCORED + COMPETITIVE-SCAN + memory.) OLD
  **thinking blocks** are ~8–16% of a code session's wire body (the once-cited "22%" was an
  atypically heavy transcript); `thinking_strip` is now **ON in BOTH profiles** (default
  keep_recent=4; gentle keep_recent=8, 2026-06-05) — API-safe (live-confirmed) and cache-stable
  (reprune replays its removals). Real-session reduction (measured via examples/session_profile
  on reconstructed sessions): **default 15–72%, gentle 5–42%** of messages[]. BM25 relevance
  eviction was DROPPED (AUC 0.39 < recency); the model-free frontier = **density-aware
  select_slice** (de-risked: not a no-op on real sessions) + the depth menu (denoise pre-pass /
  idle-path demand-paging / byte-budget gate / token-aware cutoff) — see internal/BACKLOG-SCORED.md.
- **"Transparent" means FAITHFUL, not UNDETECTABLE — and that's the brand.**
  trimwire is positioned as the *ToS-compliant* context tool: it forwards the
  user's own requests faithfully (preserve headers/auth, originate no calls on
  the subscription token) and never tries to *disguise* itself from Anthropic
  telemetry. Do NOT build detection-evasion (e.g. spoofing the official client's
  JA3/JA4 TLS fingerprint, header-order mimicry, hiding the proxy) — that turns a
  faithful proxy into an impersonation/evasion tool, escalates ToS risk from
  passive-gray to active-circumvention, and risks account bans. Being the
  *compliant* one is the moat (it's what got OpenClaw/OpenCode blocked). If a use
  needs to go beyond the gray zone, use a compliant path (API key / cross-client /
  sweep / in-agent plugin), never evasion.

## Skills

Installed locally (see [`skills-lock.json`](skills-lock.json) and
[`.agents/skills/`](.agents/skills/)):

- **`rust-engineer`** — primary skill for all implementation. Invoke for
  ownership, borrowing, lifetimes, async patterns, trait design, error
  handling, performance.
- **`find-skills`** — discover and install new skills if a workflow
  emerges that warrants one.

Globally available (per user's setup):

- **`context7`** — fetch current library/framework docs (Rust: hyper, tokio,
  rustls, serde_json, clap, rusqlite; frontend: Astro, Starlight, Vite;
  Cloudflare Workers/D1; etc.). **Always consult context7 before relying on
  training data** for any library/framework API, config, or version-specific
  behaviour — your cutoff lags releases. This applies to **subagents too**:
  when you spawn one to implement or review against a library, tell it to use
  context7 as its doc source.

## Working conventions (how we actually work — subagents / council / DS / review)

These are the maintainer's standing expectations. Follow them by default; they are not optional niceties.

- **Use subagents liberally** for research, broad searches ("find where/whether X"), and reviews — don't do large fan-out reads inline. Spawn them; relay only the conclusions.
- **Council = N subagents (2–3) given the *same* task, independently**, then **reconcile their divergence** (the disagreement IS the signal). A council is NOT one agent, and NOT N agents split across different sub-tasks (that's parallel specialists — fine for breadth research, but call it that, not a council). Reconcile by *rank* when their scales differ.
- **Disagree-Seeking (DS) pass:** after a council (or any notable proposal), run a dedicated subagent to **steelman the case AGAINST** it. Don't agree blindly with the council *or* the DS — exercise your own judgment and diverge from both where the evidence/goal warrants. Use DS whenever a result is interesting/unanimous (unanimity = groupthink risk).
- **Measure, don't guess.** When a choice is empirical (thresholds, savings, latency), measure it on real data (the `examples/*` profilers + real reconstructed sessions) before deciding. The whole project runs on measured truth.
- **Review after every big change** (a subagent code review), then apply its findings before moving on. Small changes: tests suffice.
- **Verify subagent claims against the real code/data** — they overclaim; the orchestrator (you) is the check (catch "data not available at call-site", false precision, stale assumptions).
- **Real sessions:** never operate on the live `~/.claude` transcripts — **copy them to a working dir and work on the copies** (`benchmark/reconstruct_session.py` on copies → bodies).
- **Result-impacting code changes ship WITH regression/smoke tests**; strategy/prompt changes go through the **harm gate** (`tests/harm.rs` + the false-done detector + blind real-slice gut-read) and need maintainer greenlight before merge.
- **Never push or tag** — the maintainer releases manually. Commit on `main` (pre-release, no branch needed). Commit trailer: `Co-Authored-By: Claude Opus 4.8 (1M context) <noreply@anthropic.com>`.
- **Priority order: CODE > docs/DX.** A skill is doc-like (not a code item). API-provider/portability is after the tool is peak. See `internal/BACKLOG-SCORED.md` BUILD SEQUENCE.
- **Docs source = context7.** Before writing/reviewing code against any library or framework (Astro/Starlight/Vite for the site, Cloudflare Workers/D1 for the collector, clap/hyper/tokio/rusqlite for the binary), consult the `context7` skill rather than training data — and instruct subagents to do the same.
- **Regression & bench sweep:** before a release, after a meaningful change set, or periodically, run the subagent sweep in [`docs/REGRESSION-WORKFLOW.md`](docs/REGRESSION-WORKFLOW.md) — a 6-agent fan-out (build/gate, invariant harnesses, bench regression, parity oracle, docs/memory drift, coverage gaps) reconciled into one scorecard. Mostly offline/deterministic (only `examples/api_harm` needs a provider key). It catches what CI doesn't: savings drift, doc/memory drift, and untested new surface.

## Execution rules

- **Conventional commits.** `feat:`, `fix:`, `refactor:`, `docs:`,
  `test:`, `chore:`. Scope in parens optional (e.g. `feat(pairing):`).
- **Never bypass pre-commit hooks.** No `--no-verify`. If `cargo clippy`
  or `cargo test` fails, fix the underlying issue.
- **Strict clippy.** CI runs `cargo clippy --all-targets -- -D warnings`.
  Local lefthook does the same on staged files. Promote warnings to
  errors; don't suppress without comment.
- **Strict rustfmt.** Edition 2024 settings in [`rustfmt.toml`](rustfmt.toml).
  CI runs `cargo fmt --check`.
- **Tests required.** Every new strategy module needs at least snapshot
  tests over fixture JSON. Every new public function in `src/pairing.rs`
  needs unit coverage — that module is THE load-bearing correctness layer
  ([`SPIKE.md` §5](SPIKE.md)).
- **Update [`ARCHITECTURE.md`](ARCHITECTURE.md) whenever you refactor
  modules, change layer boundaries, or add a module.** It is the design
  document; code follows it.
- **Update [`SPIKE.md`](SPIKE.md) only when a discovery contradicts the
  spike's claims.** Don't restate things that are already in the spike.
- **Public vs internal docs.** A curated subset of `docs/*.md` is PUBLISHED to the
  website — the source of truth for which is the `DOCS` array in
  [`site/scripts/sync-docs.mjs`](site/scripts/sync-docs.mjs) (currently FAQ,
  SUMMARIZER, TELEMETRY, BENCHMARK, TROUBLESHOOTING, ALTERNATIVES, ROADMAP, CLI,
  MODEL-COMPATIBILITY). **Those are user-facing — keep them concise and free of
  implementation internals** (file paths, trait signatures, effort estimates).
  Everything else (root `*.md` like AGENTS/ARCHITECTURE/SPIKE, and `docs/*.md` not
  in that list — IDEAS-BACKLOG, LAUNCH-BACKLOG, BACKLOG-SCORED, COMPETITIVE-SCAN,
  REGRESSION-WORKFLOW, MULTI-HARNESS-PLAN, VS-ANTHROPIC-NATIVE, …) is INTERNAL.
  Detailed engineering plans go in an internal doc; the public doc gets a short
  summary that links to it **by full GitHub URL** (a relative `](INTERNAL.md` link
  404s on the site — the sync only rewrites links between published docs). When you
  add/rename a published doc, update the `DOCS` array.

## Layer rules (enforced by structure + clippy)

- `src/main.rs` — CLI entry only (clap parse + dispatch). No business logic.
- `src/cli/*.rs` — binary-private command bodies (mostly one file per
  subcommand: `daemon`, `run`, `install`, `config_edit` (`config` / `config show`),
  `stats` (`--json`), `statusline`, `hook`, `sweep`, plus `service` which backs
  `on`/`off`/`status`/`uninstall`, and `doctor` (in `cli/mod.rs`) for diagnostics).
  Wiring + process/FS orchestration only.
- `src/proxy/gateway.rs` — HTTP server + request/response routing. Must NOT
  contain mutation logic (delegates to `strategies::apply_to_body`).
- `src/proxy/{upstream,proxy_stream}.rs` — HTTPS client + SSE response pipe.
- `src/strategies/*.rs` — pure functions over `&mut [Value]` plus the
  pairing index. Must NOT do any I/O. (Shared message-array walk helpers
  `role`/`block_mut`/`serialized_len` live in `strategies/mod.rs`.)
- `src/pairing.rs` — index building + invariant validation. Must NOT
  mutate messages (only read). The load-bearing correctness module.
- `src/ledger.rs` — SQLite I/O. Only place that touches the ledger DB.
- `src/config.rs` — typed config + figment loader. `src/error.rs` — error types.

Note: LOC figures elsewhere in this repo's docs are rough estimates, not
hard caps. Prefer splitting by concern into files/folders for navigability.

Rationale: every module should be unit-testable without spinning up the
HTTP server or hitting the network. If a test needs `tokio::test`, the
code probably belongs in `proxy/`; pure logic stays pure.

## What NOT to do

- **Do not touch the `system` field of the request body.** Modifying the
  Claude Code system prompt would trigger Anthropic's Jan 2026 third-party
  detection pattern. See [`SPIKE.md` §1](SPIKE.md) "Anthropic's stance".
- **Do not buffer response bodies.** Anthropic uses SSE; buffering breaks
  streaming UX. See [`SPIKE.md` §3](SPIKE.md) "Request buffered, response
  streamed".
- **Do not orphan `tool_use_id` references.** Every mutation must use the
  pairing index and validate pre/post. See [`SPIKE.md` §5](SPIKE.md).
- **Do not skip the cache-prefix hash log.** Cache-prefix thrashing is the
  top silent-failure risk. See [`SPIKE.md` §9](SPIKE.md). The ledger records
  in/out prefix hashes on **every** `POST /v1/messages` — including no-ops;
  the no-strategy-fired cohort is exactly what the stability ratio measures,
  so never gate recording on whether a strategy mutated. A test asserts the
  ratio is 1.0 when no strategy fires.
- **Do not introduce a new module without updating
  [`ARCHITECTURE.md`](ARCHITECTURE.md).**

## Build / defer / document tier split

See [`SPIKE.md` §8](SPIKE.md) for the full table.

- **T1 (gateway)** — build for v0.1.
- **T2 (HTTPS proxy alternative)** — documented in README only; no code
  from us.
- **T3 (sweep)** — ✅ **shipped in v0.1.0** (`trimwire sweep`, `src/cli/sweep.rs`);
  ported + hardened from the POC at [`pocs/tier2-sweep.py`](pocs/tier2-sweep.py).
- **T4 (tmux restart)** — POC at [`pocs/tier3-restart.sh`](pocs/tier3-restart.sh);
  documented as a reference snippet only.

If you find yourself implementing T2/T3/T4 in the Rust binary,
**stop and re-read the spike.** Either the spike needs updating
(empirical data showed something) or you're scope-creeping.

## Pre-commit setup

Hooks defined in [`lefthook.yml`](lefthook.yml). To activate:

```bash
# Install lefthook if you don't have it (Linux/macOS)
brew install lefthook   # or: cargo install lefthook
# Wire the hooks
lefthook install
```

Pre-commit gates: `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`,
`cargo test --lib`. All parallel.

## Two-phase build plan (where we are)

Per [`SPIKE.md` §10](SPIKE.md):

- **Phase 0** (deferred): Python test harness against real session fixtures.
  ~1200 LOC, ~1 evening. Locks down mutation semantics before Rust port.
- **Phase 1** (✅ complete — v0.1.0 ready): Rust MVP, built in strict order.
  All 7 steps landed (pass-through gateway → pairing → SlidingWindow →
  ImageStrip → ledger+stats → CLI+installer → release readiness). See
  `DEVELOPMENT.md` "Where we are right now" for the per-step record.
- **Phase 2** (✅ shipped in v0.1.0): the originally-deferred strategies
  (`CrossTurnDedup`, `FailedInputPurge`, `BloatCap`) and `trimwire sweep` were all
  pulled forward into v0.1.0 — a deliberate product call to land the full
  feature set, made ahead of the telemetry that was meant to gate them (not
  because that telemetry arrived). What remains genuinely deferred is
  **tuning only** (default thresholds, `keep_recent_turns`, sliding_window
  denylist) — that, still, only when usage warrants; do not add preemptively.
  See [`docs/ROADMAP.md`](docs/ROADMAP.md) "Future / possible directions" for
  the (unscheduled, evidence-gated) idea list.

The current next-step pointer always lives in `DEVELOPMENT.md`, not here.

## Custom skill for trimwire work

A focused skill at
[`.agents/skills/trimwire-gateway/SKILL.md`](.agents/skills/trimwire-gateway/SKILL.md)
captures workflows specific to this project (request-path mutation,
response-path streaming, pairing-index invariants, ledger conventions).
Invoke explicitly when working on the gateway internals.

## ATDD / BDD note

User mentioned a custom ATDD skill exists for product work. trimwire is a
small tool, not a user-story-driven product — its testing discipline is
**fixture-driven snapshot testing + integration tests against wiremock**,
not user-story acceptance tests. The ATDD skill is not currently in scope
for this repo. If the project grows a user-facing surface that warrants it,
revisit.
