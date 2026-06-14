# trimwire telemetry: `share stats` (opt-in, anonymous, content-free)

> **TL;DR:** Nothing is ever uploaded by default. Opt in with `trimwire share
> enable`; opt out with `trimwire share disable`. Once enabled, `trimwire share
> stats` uploads without `--yes` each run. `--yes` works as a per-run override.
> `--force` bypasses the once-per-day throttle only; it never bypasses consent.
>
> The built-in community stats collector URL ships in the binary at
> `https://api.trimwire.dev/ingest`. `trimwire share stats` uploads when
> consent is given (`share enable` or `--yes`); without consent it dry-runs.
> The benchmark collector ships in the binary at
> `https://api.trimwire.dev/ingest-benchmark`; `trimwire share benchmark`
> uploads only with `--yes`, otherwise it dry-runs.

**Sample payload** (the complete set of fields — nothing else is ever sent):

```json
{
  "schema_version": 1,
  "sent_day": "2026-06-09",
  "trimwire_version": "0.1",
  "harness": "claude-code",
  "model_family": "claude-sonnet-4-6",
  "profile": "default",
  "summarizer_backend": "off",
  "summarizer_family": "none",
  "conversation_length_bucket": "50-200",
  "reduction_pct_bucket": 40,
  "cache_hit_pct_bucket": 70,
  "cache_stability_bucket": 9,
  "bytes_saved_bucket": "1mb-10mb",
  "strategy_share": {"bloat_cap": 60, "sliding_window": 40},
  "reprune_enabled": true,
  "simhash_enabled": false,
  "accumulator_enabled": false,
  "os_family": "linux",
  "native_compaction_rate_bucket": 20,
  "strategies_fired": ["bloat_cap", "sliding_window"],
  "summarizer_size_bucket": "none",
  "strategy_any_fired_pct_bucket": 80,
  "summarizer_accept_rate_bucket": "none",
  "summarizer_trigger_rate_bucket": 0,
  "max_session_length_bucket": "50-200",
  "dedup_token": "a3f1e2b4c5d6e7f8a9b0c1d2e3f4a5b6c7d8e9f0a1b2c3d4e5f6a7b8c9d0e1f2",
  "summarizer_backend_won": "off"
}
```

**Status: opt-in, off by default.** No data is ever sent without explicit consent.
This document is the single source of truth for exactly what the payload contains
and why it cannot identify you. The telemetry collector is a separate service from
the trimwire binary.

## The one-paragraph promise

`trimwire share stats` uploads a **single small JSON of coarse, bucketed,
aggregate numbers** derived from your local ledger (the same content-free
counters `trimwire stats` already shows you). It contains **no** prompts, code,
file paths, file names, message text, session ids, machine ids, install ids,
raw IP data, timestamps finer than a calendar day, or any raw byte/token
counts. The `dedup_token` is a day-scoped HMAC digest that rotates daily;
the install id used as the HMAC key stays on your machine. Every number is **bucketed on your machine before it leaves**, so even
the raw row that reaches the collector is already anonymized. The public dashboard shows **only aggregates across many contributors**, with
small groups suppressed (**k-anonymity**: a group is only published when it
contains at least K distinct contributors, so no individual's data is surfaced).

## Hard invariants

1. **Opt-in, off by default.** Requires explicit consent (`trimwire share enable`
   or `--yes`). Never runs as a side effect of any other command.
2. **Dry-run without consent.** Without explicit consent (`trimwire share enable`
   or `--yes`), `trimwire share stats` always dry-runs: it prints the payload and
   exits without network I/O. The stats collector is deployed at
   `https://api.trimwire.dev/ingest`. The benchmark endpoint is
   `https://api.trimwire.dev/ingest-benchmark` — `trimwire share benchmark`
   uploads only with `--yes`, otherwise it dry-runs.
   `[share] endpoint` / `[share] benchmark_endpoint` exist as overrides for
   self-hosting or testing.
3. **Content-free.** Only ledger-derived metadata; never message content/paths.
4. **No cross-day identity.** A random install id lives only on your machine and
   is **never transmitted**. The `dedup_token` sent with each upload is
   `HMAC(install_id, sent_day)`, rotating daily so uploads on different days
   produce unrelated tokens and cannot be linked. A same-day re-upload produces
   the same token (same-day idempotency only). See "No cross-day identity" below.
5. **Client-side coarsening.** All percentages/sizes are bucketed in the Rust
   client *before* the POST. The wire payload and the stored row are identical
   and already anonymized; the collector never sees a raw value to leak.
6. **Aggregate-only, k-suppressed output.** The public surface is a
   pre-aggregated JSON; raw rows are never exposed. Buckets below `K` distinct
   contributors are hidden. Marginal distributions are additionally
   **l-diversity**-gated (requires at least 3 distinct values in a column within
   a group, so a rare value can't single out a contributor).

## The payload (schema_version 1)

Exactly these fields, nothing else is included; a test asserts the serialized
payload contains no other keys. The **marginals** at the
bottom (config flags, OS, native-compaction rate, per-strategy fire list) are
shown only within already-k-anon-safe groups, so the grouping key is unchanged
and k-anonymity is not weakened.

| Field | Type | Client-side normalization |
|------|------|------|
| `schema_version` | int | Literal `1`. |
| `sent_day` | string | UTC calendar date `YYYY-MM-DD`. **No** sub-day time. |
| `trimwire_version` | string | **`MAJOR.MINOR`** of the build's semver (the patch component is dropped to lower cardinality); **debug builds report `"dev"`** (a one-off from-source build would be near-unique). |
| `harness` | enum | The agent harness whose traffic trimwire proxied: `claude-code` \| `aider` \| `opencode` \| `cline` \| `codex` \| `other`. **Always `claude-code` today** (trimwire is a Claude Code gateway); the rest are reserved for the roadmap'd multi-harness adapters. **In the grouping key** — a primary cohort dimension. |
| `model_family` | string | The session's Claude model coarsened to **family + major.minor**: e.g. `claude-opus-4-5`, `claude-sonnet-4-6`, `claude-haiku-3-5`. Only the trailing dated build suffix (e.g. `-20251101`) is dropped (we keep the version granularity needed to distinguish `opus-4-5` from `opus-4-8`). Anything not matching `claude-(opus\|sonnet\|haiku)-<major>-<minor>` → `other`. |
| `profile` | enum | `default` \| `gentle` \| `other`. |
| `summarizer_backend` | enum | `off` \| `local` \| `api`. `"off"` = model-free (no summarizer); `"local"` = local ollama/llama.cpp; `"api"` = cloud API backend. |
| `summarizer_family` | enum | `none` when backend=off; an ollama family when backend=local (`qwen3.5`, `granite4.1`, `llama3`, …, else `other`); the API style when backend=api (`"anthropic"` or `"openai"`). Size tiers (`:4b`/`:8b`) are dropped. **Marginal only** (not in the grouping key). |
| `conversation_length_bucket` | enum | From request count: `<10` \| `10-50` \| `50-200` \| `>200`. Raw counts/bytes never sent. |
| `reduction_pct_bucket` | int | Overall reduction floored to the nearest **5** pp (0–100). No raw float. |
| `cache_hit_pct_bucket` | int | `cache_read/(cache_read+cache_creation)` floored to nearest **10** pp. No raw float. |
| `cache_stability_bucket` | int | `floor(stable_prefix_ratio × 10)`, 0–10. No raw float. |
| `bytes_saved_bucket` | enum | Log-scale: `<100kb` \| `100kb-1mb` \| `1mb-10mb` \| `10mb-100mb` \| `>100mb`. Raw byte count never sent. |
| `strategy_share` | object | For each of the 9 known strategies (8 enabled in the default profile, plus the opt-in simhash_dedup) that earned ≥ a floor: its **share of total bytes saved**, floored to nearest **5** pp. Answers "which strategy earns its keep." Zeros omitted. Raw fire-counts and raw per-strategy bytes are **never** sent. **Marginal only.** |
| `reprune_enabled` | bool | Stable-prefix re-pruning on? Lets the dashboard cross-tab cache stability against reprune. **Marginal.** |
| `simhash_enabled` | bool | The opt-in `simhash_dedup` strategy on? Adoption signal independent of whether it fired. **Marginal.** |
| `accumulator_enabled` | bool | Summarizer accumulator on? Always `false` when `summarizer_backend=off` (no presence fingerprint). **Marginal.** |
| `os_family` | enum | `linux` \| `macos` \| `windows` \| `other` (detected from the operating system). Platform-investment signal. **Marginal** (and l-diversity-gated as a distribution). |
| `native_compaction_rate_bucket` | int | Fraction of requests where Anthropic's **own** `context_management` fired, floored to nearest **10** pp (0–100). A **rate**, never a raw/magnitude count (answers the strategically critical "is trimwire redundant with native compaction?"). **Marginal.** |
| `strategies_fired` | array | Which of the 9 known strategies (8 enabled in the default profile, plus the opt-in simhash_dedup) fired ≥1× this window (sorted, deduped names only). The dashboard turns it into each strategy's **fire-rate** across sessions, so *every* strategy is represented, including ones too small to appear in `strategy_share`. **Marginal.** |
| `summarizer_size_bucket` | enum | Coarse size tier of the summarizer: `"none"` when backend=off; `"api"` when backend=api (parameter count is meaningless for cloud models); otherwise parsed from the local model tag (e.g. `"qwen3.5:4b"` → `"3-4b"`): `"≤2b"` \| `"3-4b"` \| `"5-9b"` \| `"≥10b"` \| `"unknown"`. **Marginal.** |
| `strategy_any_fired_pct_bucket` | int | % of requests where ANY pruning strategy fired (vs pass-through), floored to nearest **10** pp (0–100). Answers "how often is trimwire actively pruning?". **Marginal.** |
| `summarizer_accept_rate_bucket` | enum | Summarizer **accept rate**: of the summaries produced, the % that beat model-free pruning and were kept ("accepted"), floored to **10** pp: `"none"` \| `"0"`…`"100"`. `"none"` = no quality-relevant attempts (feature off, or all errored); not the same as `"0"`. A structural signal (did the summary win on size), **never** a content-based quality score (see "Quality: what we cannot measure"). **Marginal.** |
| `summarizer_trigger_rate_bucket` | int | How often the summarizer attempted a model call, as a % of requests, floored to **10** pp (0–100). Paired with the accept rate: many triggers + low accepts ⇒ weak model / thresholds too low; few triggers + high accepts ⇒ thresholds too conservative. **Marginal.** |
| `max_session_length_bucket` | enum | Maximum session length across the window (same bucketing as `conversation_length_bucket`): `<10` \| `10-50` \| `50-200` \| `>200`. Captures the tail — "how long does the longest session in this window get?", a context-pressure signal the median hides. **Marginal.** |
| `dedup_token` | string | Day-scoped dedup token: `hex(HMAC-SHA256(install_id, sent_day))`. The install id stays local; only this 64-hex digest is sent. Different days → completely different tokens (no cross-day identity). A same-day re-upload produces the same token, letting the collector `INSERT OR REPLACE` to override the prior row rather than ignore it. |
| `summarizer_backend_won` | enum | §8C/Q4: which engine actually **won** the fallback cascade and produced accepted summaries this window. Same closed set as `summarizer_backend`: `"off"` = no accepted summaries in the window; `"local"` = local ollama/llama.cpp engine won; `"api"` = cloud API engine won. Differs from `summarizer_backend` (the configured primary) when a fallback fired. **Marginal.** |

Everything else the ledger holds (raw `in_bytes`/`out_bytes`, raw token counts,
`ttft_us`, `prefix_hash_*`, `session_id`, `db_path`, per-day rows) is
**excluded**. A unit test enumerates the allowed keys and fails on any extra.

### Explicitly excluded (tempting but dangerous)

Raw `bytes_saved` / `total_in_bytes` (project-size fingerprint) · raw token
counts (codebase-size fingerprint) · `ttft_us` (geo/time-of-day fingerprint) ·
raw `reduction`/`cache` floats (quasi-unique) · per-strategy raw counts/bytes ·
exact model id · ollama size tier · session/machine/install ids · any nonce ·
IP (the collector must not log or store it) · locale/timezone · sub-day
timestamps · file paths/names · any message content.

**Explicitly considered and excluded:**
- `arch` (CPU architecture): `aarch64` ≈ Apple-Silicon, so combined with model
  family + version it forms near-unique tuples at low adoption; little signal
  beyond `os_family`.
- `lifetime_total_requests`: a monotonically-only-growing counter is a soft
  cross-upload tracking signal (undercuts "no linkage"), and the top bucket is
  near-unique at low adoption.
- `max_summary_segments`: fingerprints config-editors within the already-small
  `summarizer_backend≠off` subpopulation, for low marginal value.
- `tokens_removed_bucket`: `est_tokens_removed = bytes_saved/4`, a deterministic
  transform of `bytes_saved_bucket` → zero new information (compute at display).
- native-compaction **magnitude** (tokens/turns cleared): a compound fingerprint
  (correlates with length + model + project size); we keep only the **rate**.

## Quality: what we honestly *cannot* measure

There is **no content-free way** to measure summarization *correctness*
(fact-retention, "false-done" over-claiming). Measuring it requires reading
message content, which violates invariant 3. So the payload carries **no quality
score**, and the dashboard does **not** claim one. What it *can* show, honestly
labeled as **"cache health,"** is the structural signals already in the ledger:
`cache_stability_bucket` (did pruning preserve a stable, cacheable prefix) and
`cache_hit_pct_bucket`. A true quality metric stays in the **offline harm
harness** (`examples/compaction_harm.rs`, `tests/harm.rs`) and is **deferred**
from production telemetry indefinitely. (Consistent with the project's standing
"headroom, not dollars" honesty and the rejected-FCS-metric note.)

## Benchmark sharing (`trimwire share benchmark`)

A **separate**, opt-in payload to a **separate** benchmark collector route
(`/ingest-benchmark`) and dataset (the [model-benchmark page](https://trimwire.dev/benchmark/), not
the stats dashboard). It is
the one place a *directional* quality signal is shared, and it stays content-free
because the model summarizes a **bundled synthetic corpus**, never your session.
Measuring fact-retention and false-done there reads no user content (invariant
3 holds; the "cannot measure quality" caveat above is about *production* sessions).

Both **local** (ollama) and **API/provider** models can be benchmarked + shared
— but they are **never mixed**: every row carries an explicit `backend`, and the
leaderboard ranks and filters `local` and `api` separately (API scores are a
directional cross-check, not comparable to local ones). For API rows the
`model_family`/`model_bucket` are derived from the **real model** (e.g.
`claude-haiku-4-5`, `gpt-4.1-mini`) — never the provider id/name; the provider is
captured only as a coarse `provider_style` + `provider_route` bucket.

Same discipline as the stats payload: values are coarsened client-side, a content-free guard validates the payload, and a test enforces the allowed fields. One row **per benchmarked model**:

| Field | Type | Client-side normalization |
| --- | --- | --- |
| `schema_version` | int | Literal `1`. |
| `sent_day` | string | UTC calendar date `YYYY-MM-DD`. **No** sub-day time. |
| `trimwire_version` | string | `MAJOR.MINOR` of the build (or `"dev"`). |
| `corpus_version` | string | Which bundled corpus produced the score (rows across versions aren't comparable). |
| `backend` | enum | `local` (ollama) \| `api` (cloud provider). The leaderboard ranks these **separately**. |
| `provider_style` | enum | `none` (local) \| `anthropic` \| `openai` (the API protocol). Never a provider id. |
| `provider_route` | enum | `none` (local) \| `anthropic` \| `openai` \| `openrouter` \| `azure` \| `other`. A coarse bucket derived from the provider URL — **never the raw base_url, host, key, or env-var name**. |
| `model_family` | enum | Broad family. local: ollama family (`qwen3.5`, …, else `other`); api: `claude-{tier}` \| `gpt` \| `o-series` \| `other`. A provider name is **not** a valid value. |
| `model_bucket` | string | Public coarse model id. local: the ollama family; api: derived from the real model — `claude-tier-N-N` \| `gpt-<ver>[-mini/nano/turbo]` \| `o<n>[-mini]` \| `other`. Vendor prefixes (`anthropic/…` from OpenRouter) + dated suffixes stripped. |
| `model_size_bucket` | enum | local: `≤2b` \| `3-4b` \| `5-9b` \| `≥10b` \| `unknown`; api: `api` (param count meaningless for cloud). |
| `retention_bucket` | int | Fact retention floored to nearest **10** pp (0–100). |
| `compression_bucket` | int | Summary compression (`1 − out/in`) floored to nearest **10** pp (0–100). |
| `false_done_count` | enum | Unsupported completion claims, **capped**: `"0"` \| `"1"` \| `"2+"`. |
| `produced_usable_summary` | bool | Did every slice yield a usable (non-empty, non-verbatim) summary? |
| `benchmark_scope` | enum | `full_corpus` \| `partial_corpus` (e.g. an API run capped with `--max-calls`). Partial rows are ranked + labeled apart from full-corpus rows. |
| `slice_count_bucket` | enum | How many slices were scored: `1` \| `2-4` \| `full`. |
| `failed_slice_count` | enum | Provider/model **call** failures, capped: `"0"` \| `"1"` \| `"2+"`. A reliability signal, distinct from model quality. |
| `error_kind` | enum | Coarse error class across failed slices: `none` \| `timeout` \| `http_status` \| `malformed` \| `empty` \| `unreachable` \| `auth_or_config` \| `other`. **Never a raw message/stack trace.** |
| `os_family` | enum | `linux` \| `macos` \| `windows` \| `other`. |

No raw model tag/id, no provider URLs/hosts/keys/env-var names, no summary text,
no per-slice detail, no paths/ids/raw counts, no error messages. Rows whose run
had call failures are **not** uploaded (the CLI prints a report-an-issue hint
instead — error auto-upload isn't enabled). Sharing is blocked unless the bundled
corpus matches a pinned, verified hash, so modified builds can't inject results
into the shared dataset. Off by default: without `--yes` (or with `[share]
benchmark_endpoint` set empty), `trimwire share benchmark` only prints the row.
See [the benchmark guide](BENCHMARK.md).

The leaderboard groups by `(corpus_version, backend, model_family, model_bucket,
model_size_bucket, benchmark_scope)` and suppresses any group below **k=5**
uploads. Unlike the stats payload, the benchmark row carries **no dedup token**
and the collector never stores an IP — so there is no per-identity dedup and the
leaderboard's `N` counts uploaded rows, not distinct people. That is a deliberate
trade-off (no cross-day identity is ever retained) and is why the page is framed
as a *directional* ranking, not an authoritative one. **Local and API rows are
never combined into one ranking** (different `backend` ⟹ different group), and
partial-corpus rows are kept apart from full-corpus rows.

## k-anonymity & how the dashboard is computed

- **Grouping key (quasi-identifier):**
  `(trimwire_version, harness, model_family, profile, summarizer_backend, conversation_length_bucket, summarizer_size_bucket)`.
  `harness` is in the key as a primary cohort dimension; today every row is
  `claude-code` so it's one shared cell with no k-anonymity impact, splitting cleanly
  once multi-harness adapters land.
  `summarizer_size_bucket` was added so the local-model sub-population is split by
  model size tier.  For `summarizer_backend=off` rows the bucket is always `"none"`, so
  those rows still share one cell and k-anonymity is unchanged for the majority case.
- **K (currently 10, minimum 5):** a group is shown only
  when it has **≥ K** contributing uploads. Smaller groups are hidden entirely;
  there is no *per-group* "suppressed" marker that would reveal which combination
  was small. (The response does carry a single **global** `suppressed_groups`
  integer for transparency: it discloses only "N combinations currently have
  1..K-1 uploads", not which ones.) The dashboard is **intentionally sparse at
  launch** and fills in as adoption grows. That is the correct, safe behavior,
  not a bug.
- **Marginals** (`reduction_pct_bucket`, `cache_*`, `strategy_share`,
  `summarizer_family`, `os_family`, …) are shown **only
  within an already-K-safe group**. A marginal **distribution** is published only
  if, after dropping every **singleton/too-small bucket** (count < 2, so a group
  can never reveal a *sole* member of a rare bucket, e.g. the only macOS user),
  it still has **≥ 3 distinct values** (l-diversity); otherwise it's withheld.
- **Intensive metrics only.** The dashboard publishes rates, shares,
  distributions and per-bucket **contributor counts**, never extensive **sums**
  (e.g. never "total bytes saved across all users"). This is what makes repeat
  uploads harmless: re-uploading can't inflate a total because no totals are
  published.
- **Where enforced:** suppression runs at the **aggregate/read** layer; the
  collector publishes a pre-computed `aggregates.json` and never exposes raw
  rows. Because the client already coarsened everything, even the raw table is
  bucketed + identity-free.

### No cross-day identity (and the honest limitation)

Each upload includes a `dedup_token` computed on your machine as
`hex(HMAC-SHA256(install_id, sent_day))`.  The install id is a random string
stored only in your trimwire data directory and **never transmitted**.  Only the
daily HMAC digest reaches the collector.  Because the day is part of the input,
the token is different on every UTC day; two uploads on different days produce
completely unrelated tokens, so the collector cannot link them to the same person.
A same-day re-upload produces the same token, which lets the collector use
`INSERT OR REPLACE` to keep at most one row per token per day (overriding the
prior row with fresh data rather than silently ignoring the re-upload).

The cost: we still can't perfectly distinguish "10 different users" from "1
enthusiastic user who ran `trimwire share stats` 10 times on the same day with different
install ids."  We blunt this with (a) a **client-side ≤ 1 upload/UTC-day** throttle
(the last-shared date is stored locally and **never transmitted**), and
(b) publishing only intensive metrics so over-weighting can't distort a headline
number.  The IP is used only for rate-limiting and is **never stored in D1** (Cloudflare's
managed SQLite database, used here as the collector's row store).
We treat "uploads in a bucket" as an *approximation* of contributors and say so.
This is the honest floor of identity-free telemetry: no cross-day tracking, no
stable id, no IP in the database.

## Forward compatibility

`schema_version` is the first field. The first breaking change bumps it.
Bucket edges and the version/model allowlists live in code, are auditable in this
open-source repo, and are documented here.

