//! `trimwire share stats` / `trimwire share benchmark` — OPT-IN, anonymous, content-free telemetry upload.
//!
//! Builds a single small JSON of *coarse, bucketed, aggregate* numbers from the
//! local ledger and (only with an endpoint configured AND `--yes`) POSTs it to a
//! community collector. Inert by default: no endpoint ⇒ dry run, no network I/O.
//!
//! Every value is bucketed **here, before the POST**, so even the row that
//! reaches the collector is already anonymized. The payload contains no prompts,
//! paths, ids, IPs, sub-day timestamps, or raw byte/token counts. The full spec
//! and privacy rationale live in `docs/TELEMETRY.md`. Both a unit test and a
//! runtime guard assert the serialized payload carries only the allowed keys.

use std::collections::BTreeMap;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::Serialize;

use trimwire::config::{Config, global_config_path};
use trimwire::ledger::{self, KNOWN_STRATEGIES, Ledger, Report, SessionRow};

/// schema_version of the wire payload — a wire-format guard, not a migration:
/// the collector accepts only this exact value. Stays at 1: v0.1.0–0.1.2 shipped
/// with an empty endpoint (dry-run only), so NO v1 rows were ever collected and
/// the `harness` field could be added to the v1 shape without a transition. A
/// future breaking field/bucket change bumps it; the collector must then accept
/// both old and new for a window (or reject old clients).
const SCHEMA_VERSION: u32 = 1;

/// Built-in community stats collector endpoint for `trimwire share stats`.
///
/// Live: the Cloudflare Worker collector deployed at `api.trimwire.dev` (see
/// `collector/`). `share stats` still only uploads with explicit `--yes`; without
/// it the command dry-runs (prints what it would send).
///
/// Resolution order: an explicit `[share] endpoint` in the user's config
/// overrides this constant (for self-hosting or testing). If both are empty →
/// dry run, no network I/O.
const COMMUNITY_STATS_ENDPOINT: &str = "https://api.trimwire.dev/ingest";

/// Built-in community benchmark collector endpoint for `trimwire share benchmark`.
///
/// Live: the Cloudflare Worker collector deployed at `api.trimwire.dev` (see
/// `collector/`, route `POST /ingest-benchmark`; the leaderboard reads
/// `GET /benchmarks.json`). `share benchmark` still only uploads with explicit
/// `--yes`; without it the command dry-runs (prints the row it would send).
///
/// Resolution order: an explicit `[share] benchmark_endpoint` in config overrides
/// this constant. Both empty → dry run.
///
/// Referenced by `resolve_benchmark_endpoint` below (used from `benchmark.rs`).
const COMMUNITY_BENCHMARK_ENDPOINT: &str = "https://api.trimwire.dev/ingest-benchmark";

/// The exact top-level keys the payload is allowed to contain. The content-free
/// guarantee is enforced structurally (the `SharePayload` type), and this list
/// is what the regression test checks the serialized JSON against. MIRRORS
/// `collector/src/validate.ts` `ALLOWED_KEYS` across the wire — the two must stay
/// in lock-step (kept in sync by hand; the collector rejects any drift).
const ALLOWED_KEYS: &[&str] = &[
    "schema_version",
    "sent_day",
    "trimwire_version",
    "harness",
    "model_family",
    "profile",
    "summarizer_backend",
    "summarizer_family",
    "conversation_length_bucket",
    "reduction_pct_bucket",
    "cache_hit_pct_bucket",
    "cache_stability_bucket",
    "bytes_saved_bucket",
    "strategy_share",
    // marginals (not in the k-anon grouping key):
    "reprune_enabled",
    "simhash_enabled",
    "accumulator_enabled",
    "os_family",
    "native_compaction_rate_bucket",
    "strategies_fired",
    "summarizer_size_bucket",
    "strategy_any_fired_pct_bucket",
    "summarizer_accept_rate_bucket",
    "summarizer_trigger_rate_bucket",
    // §3.4 marginal: max (not median/avg) session length bucket.
    "max_session_length_bucket",
    // §3.1 day-scoped dedup token (HMAC-SHA256 of install_id + sent_day).
    "dedup_token",
    // §8C/Q4 marginal: which engine actually won after fallback cascade.
    "summarizer_backend_won",
];

// The strategy names that may appear in `strategy_share` come from
// `ledger::KNOWN_STRATEGIES` (the single Rust source of truth) — anything else is
// dropped (defense against a future strategy name leaking through unbucketed).

/// Coarse ollama summarizer families we recognize; everything else → "other".
const SUMMARIZER_FAMILIES: &[&str] = &[
    "qwen3.5",
    "qwen3",
    "qwen2.5",
    "granite4.1",
    "granite3",
    "llama3.1",
    "llama3",
    "mistral",
    "phi4",
    "phi3",
    "gemma3",
    "gemma2",
];

// model_family validation uses is_valid_model_family() — a shape check accepting
// `claude-(opus|sonnet|haiku)-<major>-<minor>` or `other` — instead of a closed list,
// because future model versions are valid but cannot all be enumerated at compile time.
const PROFILES: &[&str] = &["default", "gentle", "other"];
/// Closed value set for `harness` — the agent harness whose traffic trimwire is
/// proxying. Today trimwire only proxies Claude Code, so the client always emits
/// `"claude-code"`; the rest are reserved for the roadmap'd multi-harness adapters
/// (see docs/ROADMAP.md). The collector is deployed with the FULL set so a future
/// client release can emit a new value with no collector change or D1 migration.
/// Part of the k-anonymity grouping key (a primary cohort dimension).
const HARNESSES: &[&str] = &[
    "claude-code",
    "aider",
    "opencode",
    "cline",
    "codex",
    "other",
];
/// Closed value set for `summarizer_backend` (§3.4 rename of old `local_model`).
/// `"off"` = model-free (no summarizer); `"local"` = local ollama/llama.cpp;
/// `"api"` = cloud API backend.
const SUMMARIZER_BACKENDS: &[&str] = &["off", "local", "api"];
const LENGTH_BUCKETS: &[&str] = &["<10", "10-50", "50-200", ">200"];
const BYTES_BUCKETS: &[&str] = &["<100kb", "100kb-1mb", "1mb-10mb", "10mb-100mb", ">100mb"];
const OS_FAMILIES: &[&str] = &["linux", "macos", "windows", "other"];
/// Closed value set for `summarizer_size_bucket`. Size tiers for the local model;
/// "none" when backend=off; "api" when backend=api (parameter count is meaningless
/// for a cloud model); otherwise one of the size tiers parsed from the ollama tag.
///
/// NOTE: `"≤2b"` and `"≥10b"` contain intentional non-ASCII Unicode characters
/// (U+2264 LESS-THAN OR EQUAL TO, U+2265 GREATER-THAN OR EQUAL TO). These are
/// wire-format values that MUST stay in sync with `collector/src/validate.ts`
/// `SUMMARIZER_SIZE_BUCKETS`. Do NOT silently replace them with ASCII equivalents
/// (`<=2b` / `>=10b`) — that would silently break collector parity.
const SUMMARIZER_SIZE_BUCKETS: &[&str] = &["none", "≤2b", "3-4b", "5-9b", "≥10b", "unknown", "api"];
/// Closed value set for `summarizer_accept_rate_bucket` — the % of summarizer
/// attempts whose summary was installed (beat model-free pruning), floored to
/// 10 pp. "none" = no quality-relevant attempts (feature off, or every attempt
/// errored), which is NOT the same as 0%.
const SUMMARIZER_ACCEPT_RATE_BUCKETS: &[&str] = &[
    "none", "0", "10", "20", "30", "40", "50", "60", "70", "80", "90", "100",
];

#[derive(Debug, Serialize, PartialEq)]
pub struct SharePayload {
    schema_version: u32,
    /// UTC calendar date only — never a finer timestamp.
    sent_day: String,
    /// `MAJOR.MINOR` of the released semver; debug builds report `"dev"`.
    trimwire_version: String,
    /// The agent harness this traffic came from. Always `"claude-code"` today
    /// (trimwire only proxies Claude Code); reserved values cover the roadmap'd
    /// multi-harness adapters. Part of the k-anon grouping key. See [`HARNESSES`].
    harness: String,
    /// Claude model coarsened to family (claude-opus/sonnet/haiku/other).
    model_family: String,
    /// default | gentle | other.
    profile: String,
    /// off | local | api — which summarizer engine is active (§3.4 rename of
    /// the old `local_model` field; "off" = model-free, no summarizer).
    summarizer_backend: String,
    /// none, or a coarse family when summarizer_backend != off.
    /// For backend=local: ollama family (qwen3.5, llama3, …, else "other").
    /// For backend=api: the API style ("anthropic" or "openai").
    summarizer_family: String,
    /// Typical conversation length bucket (median recent session, by requests).
    conversation_length_bucket: String,
    /// Overall reduction floored to nearest 5 pp (0..100).
    reduction_pct_bucket: i64,
    /// cache_read/(read+creation) floored to nearest 10 pp (0..100).
    cache_hit_pct_bucket: i64,
    /// floor(stable_prefix_ratio * 10), 0..10.
    cache_stability_bucket: i64,
    /// Log-scale size bucket of bytes saved.
    bytes_saved_bucket: String,
    /// Per-strategy share of bytes saved, floored to nearest 5 pp; <5 dropped.
    strategy_share: BTreeMap<String, i64>,
    // ---- marginals (shown only within k-anon-safe groups) ----
    /// Stable-prefix re-pruning on? (`[reprune] enabled`).
    reprune_enabled: bool,
    /// Opt-in `simhash_dedup` strategy on?
    simhash_enabled: bool,
    /// Summarizer accumulator on? (false unless the summarizer is enabled).
    accumulator_enabled: bool,
    /// Coarse OS family: linux | macos | windows | other.
    os_family: String,
    /// Fraction of requests where Anthropic's own context_management fired,
    /// floored to nearest 10 pp (0..100). The "is trimwire redundant with
    /// native compaction?" signal — a rate, never a raw/magnitude count.
    native_compaction_rate_bucket: i64,
    /// Which of the 9 known strategies fired at least once this window (sorted,
    /// deduped). The dashboard turns this into each strategy's fire-rate across
    /// sessions — so *every* strategy is represented, even ones whose byte share
    /// is too small to appear in `strategy_share`.
    strategies_fired: Vec<String>,
    /// Coarse size tier of the summarizer model: "none" when backend=off;
    /// "api" when backend=api (parameter count meaningless for cloud models);
    /// otherwise parsed from the local model tag (e.g. "qwen3.5:4b" → "3-4b").
    summarizer_size_bucket: String,
    /// % of requests where ANY pruning strategy fired (vs pass-through), floored
    /// to nearest 10 pp (0..100). Answers "how often is trimwire doing anything?".
    strategy_any_fired_pct_bucket: i64,
    /// Local-model summarizer INSTALL rate: of the summaries it produced, the %
    /// that beat model-free pruning and were kept, floored to 10 pp. A structural
    /// quality signal (NOT a content score). "none" = no quality-relevant attempts
    /// (feature off, or all attempts errored) — distinct from "0".
    summarizer_accept_rate_bucket: String,
    /// How often the summarizer attempted (a model call) per request, floored to
    /// 10 pp (0..100). Pairs with the install rate: many triggers + low installs =
    /// a weak model or too-low thresholds; few triggers + high installs = thresholds
    /// too conservative.
    summarizer_trigger_rate_bucket: i64,
    /// Maximum session length bucket (same bucketing as conversation_length_bucket,
    /// but computed from the MAX request count rather than the median).  Answers
    /// "how long does the longest session in this window get?" — a tail-latency /
    /// context-pressure signal the median hides.
    max_session_length_bucket: String,
    /// Day-scoped HMAC-SHA256 dedup token: `hex(HMAC-SHA256(install_id, sent_day))`.
    /// The install id never leaves the machine; only this daily-rotating digest is
    /// sent. Rotating daily means two uploads on different days produce unrelated
    /// tokens — no cross-day identity. A same-day re-upload produces the same token
    /// so the collector can `INSERT OR REPLACE` (override rather than duplicate).
    dedup_token: String,
    /// §8C/Q4: which engine actually won the fallback cascade (same closed set as
    /// `summarizer_backend`): `"off"` = no summary accepted in this window;
    /// `"local"` = local ollama/llama.cpp engine won most accepted summaries;
    /// `"api"` = cloud API engine won most accepted summaries. **Marginal.**
    summarizer_backend_won: String,
}

// ---- pure bucketing helpers (unit-tested) ---------------------------------

/// `MAJOR.MINOR` of the build's version; debug builds collapse to `"dev"` (a
/// from-source dev build is near-unique and must never be charted on its own).
pub(super) fn version_bucket() -> String {
    if cfg!(debug_assertions) {
        return "dev".to_owned();
    }
    let v = env!("CARGO_PKG_VERSION");
    let mut it = v.split('.');
    match (it.next(), it.next()) {
        (Some(maj), Some(min)) => format!("{maj}.{min}"),
        _ => "dev".to_owned(),
    }
}

/// Coarsen a raw Claude model id to `claude-<tier>-<major>-<minor>` or `"other"`.
///
/// Only the trailing `-YYYYMMDD` date suffix is dropped. Examples:
/// - `claude-opus-4-5-20251101` → `claude-opus-4-5`
/// - `claude-sonnet-4-6`       → `claude-sonnet-4-6`
/// - `claude-haiku-3-5`        → `claude-haiku-3-5`
/// - `gpt-4o`                  → `other`
///
/// This is low-cardinality (few model versions in the wild) and content-free
/// (no dated build suffix that could fingerprint early adopters of a new version).
fn model_family(raw: Option<&str>) -> String {
    let lower = raw.unwrap_or("").to_ascii_lowercase();
    // Drop a trailing runtime marker Claude Code appends to the model id, e.g. the
    // 1M-context auto-bump `claude-opus-4-8[1m]`. It is NOT part of the model
    // version, and left in place it makes the minor segment (`8[1m]`) fail to parse
    // → the model is miscoarsened to "other" (silently under-counting it in
    // telemetry). Everything from the first `[` on is stripped.
    let m = lower.split('[').next().unwrap_or("");
    // Match `claude-(opus|sonnet|haiku)-<major>-<minor>` and drop any trailing `-YYYYMMDD`.
    // We parse manually to stay dependency-free (no regex crate).
    for tier in ["opus", "sonnet", "haiku"] {
        let prefix = format!("claude-{tier}-");
        if let Some(rest) = m.strip_prefix(prefix.as_str()) {
            // rest is now e.g. "4-5-20251101" or "4-6" or "3-5-20241022"
            let parts: Vec<&str> = rest.split('-').collect();
            if parts.len() >= 2 {
                // First two segments must be decimal integers (major, minor).
                let ok = parts[0].parse::<u32>().is_ok() && parts[1].parse::<u32>().is_ok();
                if ok {
                    return format!("claude-{tier}-{}-{}", parts[0], parts[1]);
                }
            }
        }
    }
    "other".to_owned()
}

/// Returns true iff `s` matches the model_family shape: `claude-(opus|sonnet|haiku)-D-D`
/// or exactly `"other"`. Used by the runtime guard (replaces the old closed-list check).
fn is_valid_model_family(s: &str) -> bool {
    if s == "other" {
        return true;
    }
    for tier in ["opus", "sonnet", "haiku"] {
        let prefix = format!("claude-{tier}-");
        if let Some(rest) = s.strip_prefix(prefix.as_str()) {
            let parts: Vec<&str> = rest.split('-').collect();
            if parts.len() == 2 {
                return parts[0].parse::<u32>().is_ok() && parts[1].parse::<u32>().is_ok();
            }
        }
    }
    false
}

/// The harness whose traffic this build proxies. Constant `"claude-code"` for now
/// — trimwire is a Claude Code gateway. When multi-harness adapters land (see
/// docs/ROADMAP.md), this becomes a detection point (e.g. from the request shape
/// or a configured adapter), emitting one of [`HARNESSES`]; the wire field and the
/// collector already accept the full set, so that change needs no schema bump.
fn harness() -> String {
    "claude-code".to_owned()
}

fn profile_bucket(raw: Option<&str>) -> String {
    match raw.unwrap_or("default") {
        "default" => "default",
        "gentle" => "gentle",
        _ => "other",
    }
    .to_owned()
}

/// `summarizer_family` value for the LOCAL backend. Strips the `:size` tag and
/// maps to a known ollama family, else "other". `None`/empty ⇒ "none".
fn summarizer_family(tag: Option<&str>) -> String {
    let Some(tag) = tag else {
        return "none".to_owned();
    };
    let base = tag.split(':').next().unwrap_or("").to_ascii_lowercase();
    if base.is_empty() {
        return "none".to_owned();
    }
    if SUMMARIZER_FAMILIES.contains(&base.as_str()) {
        base
    } else {
        "other".to_owned()
    }
}

/// `summarizer_family` for the stats payload, taking the backend into account.
///
/// - backend=off → "none"
/// - backend=local → ollama family coarsening (via [`summarizer_family`])
/// - backend=api → the API style: "anthropic" | "openai" (from the primary provider's `style`)
fn summarizer_family_for_backend(backend: &str, tag: Option<&str>, api_style: &str) -> String {
    match backend {
        "off" => "none".to_owned(),
        "api" => match api_style {
            "anthropic" => "anthropic".to_owned(),
            "openai" => "openai".to_owned(),
            _ => "other".to_owned(),
        },
        _ => summarizer_family(tag), // "local" or any future value
    }
}

fn reduction_bucket(pct: f64) -> i64 {
    let p = pct.clamp(0.0, 100.0);
    ((p / 5.0).floor() as i64) * 5
}

fn cache_pct_bucket(pct: f64) -> i64 {
    let p = pct.clamp(0.0, 100.0);
    ((p / 10.0).floor() as i64) * 10
}

fn stability_bucket(ratio: f64) -> i64 {
    let r = ratio.clamp(0.0, 1.0);
    ((r * 10.0).floor() as i64).clamp(0, 10)
}

fn bytes_saved_bucket(bytes: i64) -> String {
    let b = bytes.max(0);
    let s = if b < 100_000 {
        "<100kb"
    } else if b < 1_000_000 {
        "100kb-1mb"
    } else if b < 10_000_000 {
        "1mb-10mb"
    } else if b < 100_000_000 {
        "10mb-100mb"
    } else {
        ">100mb"
    };
    s.to_owned()
}

/// Coarse size tier of the summarizer model, taking the backend into account.
///
/// - backend=off → `"none"` (no model in use)
/// - backend=api → `"api"` (parameter count is meaningless for a cloud model)
/// - backend=local → parses the FIRST `:<number>b` token from `tag`:
///   the segment after the first `:` that matches `^\d+(\.\d+)?b` (case
///   insensitive). Maps: n ≤ 2 → `"≤2b"`, 3–4 → `"3-4b"`, 5–9 → `"5-9b"`,
///   ≥ 10 → `"≥10b"`. No parseable suffix → `"unknown"`.
///
/// The `local_model` parameter name is kept for backward-compat with the benchmark
/// path which calls this with `"default"` (never `"api"`).
fn summarizer_size_bucket(local_model: &str, tag: Option<&str>) -> String {
    if local_model == "off" {
        return "none".to_owned();
    }
    if local_model == "api" {
        return "api".to_owned();
    }
    let Some(tag) = tag else {
        return "unknown".to_owned();
    };
    // Walk the colon-separated segments looking for one that starts with a
    // number followed by 'b' (e.g. "4b", "0.8b", "14b"); first match wins.
    for seg in tag.split(':').skip(1) {
        // Strip an optional quantization suffix (e.g. "-q8_0") to get the bare
        // size token, then try to parse as f64 followed by 'b'.
        let lower = seg.to_ascii_lowercase();
        let bare = lower.split('-').next().unwrap_or(&lower);
        if let Some(num_str) = bare.strip_suffix('b') {
            if let Ok(n) = num_str.parse::<f64>() {
                let tier = if n <= 2.0 {
                    "≤2b"
                } else if n <= 4.0 {
                    "3-4b"
                } else if n < 10.0 {
                    "5-9b"
                } else {
                    "≥10b"
                };
                return tier.to_owned();
            }
        }
    }
    "unknown".to_owned()
}

/// `std::env::consts::OS` mapped to a coarse family.
fn os_family() -> String {
    match std::env::consts::OS {
        "linux" => "linux",
        "macos" => "macos",
        "windows" => "windows",
        _ => "other",
    }
    .to_owned()
}

/// Fraction of requests where Anthropic's native context_management fired,
/// floored to nearest 10 pp (0..100). 0 when there are no requests.
fn native_compaction_rate_bucket(requests_with_applied_edits: u64, total_requests: u64) -> i64 {
    if total_requests == 0 {
        return 0;
    }
    let pct = requests_with_applied_edits as f64 / total_requests as f64 * 100.0;
    cache_pct_bucket(pct) // same floor-to-10pp, clamped 0..100
}

fn length_bucket(requests: u64) -> String {
    let s = if requests < 10 {
        "<10"
    } else if requests < 50 {
        "10-50"
    } else if requests <= 200 {
        "50-200"
    } else {
        ">200"
    };
    s.to_owned()
}

/// Median request count across the recent sessions (typical conversation
/// length). Empty ⇒ 0.
///
/// Uses the LOWER median (`(len-1)/2`) on even-length arrays so the result is
/// always an actual element of the input and the function is conservative (it
/// does not round up). Example: `[3, 7]` → index 0 → 3, not 7.
fn median_requests(sessions: &[SessionRow]) -> u64 {
    if sessions.is_empty() {
        return 0;
    }
    let mut reqs: Vec<u64> = sessions.iter().map(|s| s.requests).collect();
    reqs.sort_unstable();
    reqs[(reqs.len() - 1) / 2]
}

/// Maximum request count across the recent sessions (tail / longest session).
/// Empty ⇒ 0.
fn max_requests(sessions: &[SessionRow]) -> u64 {
    sessions.iter().map(|s| s.requests).max().unwrap_or(0)
}

/// Per-strategy share of total bytes saved, floored to nearest 5 pp, shares <5
/// dropped, unknown strategy names dropped.
fn strategy_share(per_strategy_bytes: &[(String, i64)]) -> BTreeMap<String, i64> {
    let total: i64 = per_strategy_bytes
        .iter()
        .filter(|(n, b)| *b > 0 && KNOWN_STRATEGIES.contains(&n.as_str()))
        .map(|(_, b)| *b)
        .sum();
    let mut out = BTreeMap::new();
    if total <= 0 {
        return out;
    }
    for (name, bytes) in per_strategy_bytes {
        if *bytes <= 0 || !KNOWN_STRATEGIES.contains(&name.as_str()) {
            continue;
        }
        let pct = (*bytes as f64) / (total as f64) * 100.0;
        let bucket = ((pct / 5.0).floor() as i64) * 5;
        if bucket >= 5 {
            out.insert(name.clone(), bucket);
        }
    }
    out
}

/// The known strategies that fired ≥1× this window, sorted + deduped. Unknown
/// names are dropped (defense against a future strategy leaking through).
fn strategies_fired(per_strategy: &[(String, u64)]) -> Vec<String> {
    let mut v: Vec<String> = per_strategy
        .iter()
        .filter(|(name, count)| *count > 0 && KNOWN_STRATEGIES.contains(&name.as_str()))
        .map(|(name, _)| name.clone())
        .collect();
    v.sort();
    v.dedup();
    v
}

/// Summarizer INSTALL rate as a closed-set bucket: accepted/(accepted+rejected)
/// floored to 10 pp ("0".."100"), or "none" when there were no quality-relevant
/// attempts (denominator 0 — feature off or every attempt errored). Errors are
/// excluded from the denominator (infra failure ≠ a quality signal).
fn summarizer_accept_rate_bucket(accepted: u64, rejected: u64) -> String {
    let denom = accepted + rejected;
    if denom == 0 {
        return "none".to_owned();
    }
    let pct = accepted as f64 / denom as f64 * 100.0;
    (((pct / 10.0).floor() as i64) * 10).to_string()
}

/// Summarizer TRIGGER rate: (accepted+rejected+errored) model calls per request,
/// floored to 10 pp (0..100). 0 when nothing triggered.
fn summarizer_trigger_rate_bucket(attempts: u64, total_requests: u64) -> i64 {
    if total_requests == 0 {
        return 0;
    }
    cache_pct_bucket(attempts as f64 / total_requests as f64 * 100.0)
}

/// Read-or-create the local install id file at `<data_dir>/install-id`.
///
/// The file contains a 32-hex random string (16 random bytes → 32 hex chars).
/// It is NEVER transmitted — only used as the HMAC key for `dedup_token`.
/// Returns an error if the file can't be created or read.
fn read_or_create_install_id(data_dir: &std::path::Path) -> Result<String> {
    use std::io::Write as _;

    let path = data_dir.join("install-id");
    if path.exists() {
        let s = std::fs::read_to_string(&path)
            .with_context(|| format!("read install id from {}", path.display()))?;
        let s = s.trim().to_owned();
        if s.len() == 32 && s.chars().all(|c| c.is_ascii_hexdigit()) {
            return Ok(s);
        }
        // File exists but is corrupt — regenerate.
    }
    // Generate 16 random bytes using the OS CSPRNG via SystemTime-seeded mixing
    // (no dependency on rand). We XOR the current time with itself rotated as a
    // cheap deterministic seed, then use the OS random source via std::io.
    // Actually use std::fs::read from /dev/urandom (Linux/macOS) or fallback.
    let raw: [u8; 16] = generate_random_bytes();
    let hex_id = hex::encode(raw);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create dir {}", parent.display()))?;
    }
    // Write atomically: write to a temp file, rename.  On failure, fall back to
    // a direct write (Windows rename semantics differ but POSIX rename is atomic).
    let tmp = path.with_extension("tmp");
    let write_result = (|| -> Result<()> {
        let mut f =
            std::fs::File::create(&tmp).with_context(|| format!("create tmp {}", tmp.display()))?;
        // Tighten BEFORE writing + rename so the install-id is never briefly
        // world-readable at the final path (rename preserves the inode's mode).
        let _ = trimwire::fsperm::restrict_to_owner(&tmp);
        f.write_all(hex_id.as_bytes())
            .with_context(|| format!("write tmp {}", tmp.display()))?;
        std::fs::rename(&tmp, &path)
            .with_context(|| format!("rename {} → {}", tmp.display(), path.display()))?;
        Ok(())
    })();
    if write_result.is_err() {
        // Fallback: direct write (non-atomic but acceptable on first create).
        std::fs::write(&path, hex_id.as_bytes())
            .with_context(|| format!("write install id to {}", path.display()))?;
    }
    // Owner-only perms (0600 on Unix): the install-id is an HMAC key (never transmitted);
    // keep it off other local users' eyes. Best-effort, on the final path.
    let _ = trimwire::fsperm::restrict_to_owner(&path);
    Ok(hex_id)
}

/// Platform-portable random byte generator: 16 bytes from the OS CSPRNG via
/// `getrandom` (which uses `/dev/urandom`/`getrandom(2)` on Unix and
/// `BCryptGenRandom` on Windows — so NATIVE Windows gets a real CSPRNG, not the
/// old weak fallback). The only fallback is a time-seeded xorshift, reached just if
/// `getrandom` itself errors (no OS entropy source — essentially never on a real OS).
///
/// **Note:** the install id is never transmitted; only the daily-rotating HMAC
/// digest (`dedup_token`) leaves the machine. A weak id would only degrade community
/// dashboard de-duplication, never privacy. With the OS CSPRNG that risk is gone.
fn generate_random_bytes() -> [u8; 16] {
    let mut buf = [0u8; 16];
    if getrandom::fill(&mut buf).is_ok() {
        return buf;
    }
    // Last resort only if the OS CSPRNG is unavailable (effectively never): a
    // time-seeded xorshift64 — NOT a CSPRNG; just a unique-ish install id.
    let t1 = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0x_dead_beef_cafe_babe);
    let t2 = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0x_0123_4567_89ab_cdef);
    let mut state = t1 ^ t2.rotate_right(17) ^ 0x_6c62_272e_07bb_0142;
    for chunk in buf.chunks_mut(8) {
        // xorshift64
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        let bytes = state.to_le_bytes();
        let n = chunk.len();
        chunk.copy_from_slice(&bytes[..n]);
    }
    buf
}

/// Compute the day-scoped dedup token: `hex(HMAC-SHA256(key=install_id_bytes, msg=day))`.
///
/// The install id (hex string, 32 chars = 16 bytes) is decoded to bytes and used
/// as the HMAC key.  The message is the `sent_day` string (`YYYY-MM-DD`).
/// The result is the full 64-hex digest.  Different days → different tokens;
/// same install id + same day → same token (idempotent re-upload).
fn compute_dedup_token(install_id: &str, sent_day: &str) -> Result<String> {
    use hmac::{Hmac, KeyInit, Mac};
    use sha2::Sha256;

    type HmacSha256 = Hmac<Sha256>;

    let key_bytes =
        hex::decode(install_id).context("install id is not valid hex — file may be corrupt")?;
    let mut mac = HmacSha256::new_from_slice(&key_bytes).context("HMAC key length invalid")?;
    mac.update(sent_day.as_bytes());
    let result = mac.finalize();
    Ok(hex::encode(result.into_bytes()))
}

/// Resolve the trimwire data directory: the directory that holds `install-id`,
/// `share-state`, and other local state files.  Uses the same `resolve_path`
/// helper the ledger uses so the path follows the same `~/` expansion rules.
fn trimwire_data_dir() -> std::path::PathBuf {
    ledger::resolve_path("~/.trimwire")
}

/// Build the content-free payload. Pure: `version` and `sent_day` are injected
/// so tests are deterministic and don't depend on the wall clock or build mode.
fn build_payload(
    report: &Report,
    sessions: &[SessionRow],
    config: &Config,
    version: String,
    sent_day: String,
    dedup_token: String,
) -> SharePayload {
    let saved = report.bytes_saved();
    let reduction = report.reduction_pct();
    let rm = &report.response_metrics;
    let cache_hit = rm.cache_hit_pct();

    // summarizer_backend wire field: "off" | "local" | "api" — the engine (§3.4).
    // Resolve the primary engine string to a kind:
    //   "model-free" → "off", "local" → "local", any provider id → "api".
    let s = &config.summarizer;
    let summarizer_backend = match s.engine.as_str() {
        "model-free" => "off".to_owned(),
        "local" => "local".to_owned(),
        // Any other string is a provider id → "api" for the wire field.
        _ => "api".to_owned(),
    };
    let model_tag = s.local.model.as_str();
    // For the api case, find the primary provider's style for summarizer_family.
    let primary_provider_style = if summarizer_backend == "api" {
        s.providers
            .iter()
            .find(|p| p.id == s.engine)
            .map(|p| p.style.as_str())
            .unwrap_or("")
    } else {
        ""
    };
    let summarizer_family =
        summarizer_family_for_backend(&summarizer_backend, Some(model_tag), primary_provider_style);
    let summarizer_size_bucket = summarizer_size_bucket(&summarizer_backend, Some(model_tag));
    // Accumulator is meaningful only when the summarizer is actually active.
    let accumulator_enabled = summarizer_backend != "off" && s.accumulator;

    SharePayload {
        schema_version: SCHEMA_VERSION,
        sent_day,
        trimwire_version: version,
        harness: harness(),
        // Most-recent session's model (sessions are newest-first) — the single
        // best representative of "what this user currently runs"; coarsened to
        // family so it can't fingerprint.
        model_family: model_family(sessions.first().and_then(|s| s.model.as_deref())),
        profile: profile_bucket(config.profile.as_deref()),
        summarizer_backend,
        summarizer_family,
        conversation_length_bucket: length_bucket(median_requests(sessions)),
        reduction_pct_bucket: reduction_bucket(reduction),
        cache_hit_pct_bucket: cache_pct_bucket(cache_hit),
        cache_stability_bucket: stability_bucket(report.cache_stability.ratio),
        bytes_saved_bucket: bytes_saved_bucket(saved),
        strategy_share: strategy_share(&report.per_strategy_bytes),
        reprune_enabled: config.reprune.enabled,
        simhash_enabled: config.strategies.simhash_dedup.enabled,
        accumulator_enabled,
        os_family: os_family(),
        native_compaction_rate_bucket: native_compaction_rate_bucket(
            rm.requests_with_applied_edits,
            report.total_requests,
        ),
        strategies_fired: strategies_fired(&report.per_strategy),
        summarizer_size_bucket,
        strategy_any_fired_pct_bucket: cache_pct_bucket(if report.total_requests == 0 {
            0.0
        } else {
            report.requests_with_strategy as f64 / report.total_requests as f64 * 100.0
        }),
        summarizer_accept_rate_bucket: summarizer_accept_rate_bucket(
            report.summarizer_accepted,
            report.summarizer_rejected,
        ),
        summarizer_trigger_rate_bucket: summarizer_trigger_rate_bucket(
            report.summarizer_accepted + report.summarizer_rejected + report.summarizer_errored,
            report.total_requests,
        ),
        max_session_length_bucket: length_bucket(max_requests(sessions)),
        dedup_token,
        // §8C/Q4: the engine kind that won the most accepted summaries this window.
        // "off" if nothing was accepted. Uses the same "off"|"local"|"api" closed set
        // as summarizer_backend so no new vocabulary is introduced.
        summarizer_backend_won: summarizer_backend_won(
            report.summarizer_accepted_local,
            report.summarizer_accepted_api,
        ),
    }
}

/// Which coarse engine kind won the most accepted summaries in this window.
/// `"off"` = zero accepted (the fallback always stood);
/// `"local"` = the local ollama engine produced more accepted summaries;
/// `"api"` = a cloud-API engine produced more accepted summaries.
/// Tie: prefer `"local"` (deterministic, no privacy implication).
fn summarizer_backend_won(accepted_local: u64, accepted_api: u64) -> String {
    if accepted_local == 0 && accepted_api == 0 {
        "off".to_owned()
    } else if accepted_local >= accepted_api {
        "local".to_owned()
    } else {
        "api".to_owned()
    }
}

/// Defense-in-depth content-free guard: refuse if the serialized payload has any
/// top-level key outside [`ALLOWED_KEYS`], or any unknown strategy in
/// `strategy_share`. A future field added to [`SharePayload`] without updating
/// the allowlist then fails CLOSED (never printed, never sent) instead of
/// silently leaking. Mirrors the `payload_is_content_free` unit test at runtime.
fn guard_content_free(value: &serde_json::Value) -> Result<()> {
    let obj = value
        .as_object()
        .context("payload must serialize to a JSON object")?;
    for key in obj.keys() {
        if !ALLOWED_KEYS.contains(&key.as_str()) {
            anyhow::bail!("refusing to share: unexpected payload field {key:?}");
        }
    }
    // Validate the closed string enums by VALUE, not just by key, so a future
    // helper change can't smuggle a raw/high-cardinality string onto the wire.
    let check_enum = |field: &str, allowed: &[&str]| -> Result<()> {
        let v = obj
            .get(field)
            .and_then(|v| v.as_str())
            .with_context(|| format!("missing/non-string field {field:?}"))?;
        if !allowed.contains(&v) {
            anyhow::bail!("refusing to share: field {field:?} has unexpected value {v:?}");
        }
        Ok(())
    };
    // model_family uses a shape check (tier + major.minor) — not a closed list —
    // so future model versions aren't rejected without a code change.
    let mf = obj
        .get("model_family")
        .and_then(|v| v.as_str())
        .context("missing/non-string field \"model_family\"")?;
    if !is_valid_model_family(mf) {
        anyhow::bail!("refusing to share: field \"model_family\" has unexpected value {mf:?}");
    }
    check_enum("harness", HARNESSES)?;
    check_enum("profile", PROFILES)?;
    check_enum("summarizer_backend", SUMMARIZER_BACKENDS)?;
    check_enum("conversation_length_bucket", LENGTH_BUCKETS)?;
    check_enum("bytes_saved_bucket", BYTES_BUCKETS)?;
    check_enum("os_family", OS_FAMILIES)?;
    // The bucketed integer metrics: each an int in a closed range, stepped.
    // Mirrors the collector's intInRange checks so a future bucketing-helper bug
    // fails closed here (never sent) instead of skewing the public dashboard.
    let check_int_bucket = |field: &str, max: i64, step: i64| -> Result<()> {
        let n = obj
            .get(field)
            .and_then(|v| v.as_i64())
            .with_context(|| format!("missing {field}"))?;
        if !(0..=max).contains(&n) || n % step != 0 {
            anyhow::bail!("refusing to share: {field} out of range");
        }
        Ok(())
    };
    check_int_bucket("native_compaction_rate_bucket", 100, 10)?;
    check_int_bucket("reduction_pct_bucket", 100, 5)?;
    check_int_bucket("cache_hit_pct_bucket", 100, 10)?;
    check_int_bucket("cache_stability_bucket", 10, 1)?;
    check_int_bucket("strategy_any_fired_pct_bucket", 100, 10)?;
    check_int_bucket("summarizer_trigger_rate_bucket", 100, 10)?;
    // summarizer_accept_rate_bucket: one of the closed "none"|"0".."100" set.
    let sar = obj
        .get("summarizer_accept_rate_bucket")
        .and_then(|v| v.as_str())
        .context("missing summarizer_accept_rate_bucket")?;
    if !SUMMARIZER_ACCEPT_RATE_BUCKETS.contains(&sar) {
        anyhow::bail!(
            "refusing to share: summarizer_accept_rate_bucket has unexpected value {sar:?}"
        );
    }
    // summarizer_family: "none", "other", "anthropic", "openai", or one of the
    // recognized ollama families (for the local backend).
    let sf = obj
        .get("summarizer_family")
        .and_then(|v| v.as_str())
        .context("missing summarizer_family")?;
    let sf_ok = sf == "none"
        || sf == "other"
        || sf == "anthropic"
        || sf == "openai"
        || SUMMARIZER_FAMILIES.contains(&sf);
    if !sf_ok {
        anyhow::bail!("refusing to share: summarizer_family has unexpected value {sf:?}");
    }
    if obj.get("schema_version").and_then(|v| v.as_u64()) != Some(SCHEMA_VERSION as u64) {
        anyhow::bail!("refusing to share: schema_version mismatch");
    }
    if let Some(shares) = obj.get("strategy_share").and_then(|v| v.as_object()) {
        for (k, v) in shares {
            if !KNOWN_STRATEGIES.contains(&k.as_str()) {
                anyhow::bail!("refusing to share: unknown strategy name {k:?}");
            }
            // Value-check too (not just the key): each share is a 5pp bucket in
            // 5..=100. A future bucketing bug then fails closed instead of skewing
            // the public dashboard with an out-of-range share.
            let n = v
                .as_i64()
                .with_context(|| format!("strategy_share[{k:?}] is not an integer"))?;
            if !(5..=100).contains(&n) || n % 5 != 0 {
                anyhow::bail!("refusing to share: strategy_share[{k:?}] = {n} out of range");
            }
        }
    }
    // strategies_fired: an array of known strategy names only.
    let fired = obj
        .get("strategies_fired")
        .and_then(|v| v.as_array())
        .context("missing strategies_fired")?;
    for s in fired {
        let name = s.as_str().context("strategies_fired entry not a string")?;
        if !KNOWN_STRATEGIES.contains(&name) {
            anyhow::bail!("refusing to share: unknown strategy in strategies_fired {name:?}");
        }
    }
    // summarizer_size_bucket: one of the closed size-tier set.
    let ssb = obj
        .get("summarizer_size_bucket")
        .and_then(|v| v.as_str())
        .context("missing summarizer_size_bucket")?;
    if !SUMMARIZER_SIZE_BUCKETS.contains(&ssb) {
        anyhow::bail!("refusing to share: summarizer_size_bucket has unexpected value {ssb:?}");
    }
    // max_session_length_bucket: same closed set as conversation_length_bucket.
    check_enum("max_session_length_bucket", LENGTH_BUCKETS)?;
    // dedup_token: 64 lowercase hex chars (SHA256 output = 32 bytes = 64 hex).
    let dt = obj
        .get("dedup_token")
        .and_then(|v| v.as_str())
        .context("missing dedup_token")?;
    if dt.len() != 64 || !dt.chars().all(|c| matches!(c, '0'..='9' | 'a'..='f')) {
        anyhow::bail!("refusing to share: dedup_token must be 64 lowercase hex chars");
    }
    // summarizer_backend_won: same closed set as summarizer_backend.
    check_enum("summarizer_backend_won", SUMMARIZER_BACKENDS)?;
    Ok(())
}

// ---- `trimwire share benchmark` payload (separate wire shape, separate
// benchmark collector at api.trimwire.dev/ingest-benchmark) ------------------
//
// Lives here, beside the stats telemetry, so ALL content-free machinery (the
// closed enum sets, the guard discipline, the bucketing helpers) sits in one
// audited place rather than scattered into the benchmark command. `benchmark.rs`
// computes the scores; this layer turns them into a guarded, coarse, per-model row.
pub(super) use benchmark_share::{
    BenchmarkPayload, BenchmarkShareInput, build_benchmark_payload, guard_benchmark_content_free,
};

mod benchmark_share {
    use anyhow::{Context, Result};
    use serde::Serialize;

    use super::{
        OS_FAMILIES, SCHEMA_VERSION, SUMMARIZER_FAMILIES, cache_pct_bucket, model_family,
        os_family, summarizer_family, summarizer_size_bucket,
    };

    /// Top-level keys the benchmark payload may contain. MIRRORS
    /// `BENCHMARK_ALLOWED_KEYS` in `collector/src/validate.ts` (the deployed
    /// benchmark route) — keep byte-identical across the language boundary.
    const BENCHMARK_ALLOWED_KEYS: &[&str] = &[
        "schema_version",
        "sent_day",
        "trimwire_version",
        "corpus_version",
        "backend",
        "provider_style",
        "provider_route",
        "model_family",
        "model_bucket",
        "model_size_bucket",
        "retention_bucket",
        "compression_bucket",
        "false_done_count",
        "produced_usable_summary",
        "benchmark_scope",
        "slice_count_bucket",
        "failed_slice_count",
        "error_kind",
        "os_family",
    ];

    /// Closed value set for `false_done_count` / `failed_slice_count` — capped at
    /// "2+" so a high count can't fingerprint.
    const COUNT_BUCKETS: &[&str] = &["0", "1", "2+"];
    /// Local-row `model_size_bucket` set (param-count tiers). API rows use "api".
    const BENCHMARK_SIZE_BUCKETS: &[&str] = &["≤2b", "3-4b", "5-9b", "≥10b", "unknown"];
    const BACKENDS: &[&str] = &["local", "api"];
    const PROVIDER_STYLES: &[&str] = &["none", "anthropic", "openai"];
    const PROVIDER_ROUTES: &[&str] = &[
        "none",
        "anthropic",
        "openai",
        "openrouter",
        "azure",
        "other",
    ];
    const BENCHMARK_SCOPES: &[&str] = &["full_corpus", "partial_corpus"];
    const SLICE_COUNT_BUCKETS: &[&str] = &["1", "2-4", "full"];
    const ERROR_KINDS: &[&str] = &[
        "none",
        "timeout",
        "http_status",
        "malformed",
        "empty",
        "unreachable",
        "auth_or_config",
        "other",
    ];
    /// Broad API model families coarsened from the real model id (claude tiers +
    /// the openai families + "other"). OPEN families (qwen/llama/…) are an
    /// additional recognized set — see `OPEN_FAMILY_BASES` + `is_valid_api_family`.
    /// A provider NAME (anthropic/openai/openrouter) is never a valid family.
    const API_FAMILIES: &[&str] = &[
        "claude-opus",
        "claude-sonnet",
        "claude-haiku",
        "gpt",
        "o-series",
        "other",
    ];

    /// Normalized OPEN-model family bases (api `model_family` for non-claude/openai
    /// models, so OpenRouter rows for qwen/llama/gemma/mistral/… stay distinct, not
    /// collapsed to "other"). MIRRORS `OPEN_FAMILY_BASES` in collector/src/validate.ts.
    const OPEN_FAMILY_BASES: &[&str] = &[
        "qwen",
        "qwen2",
        "qwen2.5",
        "qwen3",
        "qwen3.5",
        "llama",
        "llama-2",
        "llama-3",
        "llama-3.1",
        "llama-3.2",
        "llama-3.3",
        "gemma",
        "gemma-2",
        "gemma-3",
        "phi",
        "phi-3",
        "phi-4",
        "granite",
        "granite-3",
        "granite-3.1",
        "mistral",
        "mistral-small",
        "mistral-medium",
        "mistral-large",
        "mistral-nemo",
        "mixtral",
        "deepseek",
        "deepseek-v2",
        "deepseek-v3",
        "deepseek-r1",
    ];

    /// Bases whose bucket NEVER carries a `-<N>b` size (they already name a variant).
    const NO_SIZE_BASES: &[&str] = &[
        "mistral-small",
        "mistral-medium",
        "mistral-large",
        "mistral-nemo",
        "mixtral",
        "deepseek-v2",
        "deepseek-v3",
        "deepseek-r1",
    ];

    /// (id prefix → normalized base), MOST-SPECIFIC FIRST so `qwen3.5` wins over
    /// `qwen3` over `qwen`. Both dashed + undashed spellings handled.
    const OPEN_FAMILY_PREFIXES: &[(&str, &str)] = &[
        ("qwen3.5", "qwen3.5"),
        ("qwen-3.5", "qwen3.5"),
        ("qwen2.5", "qwen2.5"),
        ("qwen-2.5", "qwen2.5"),
        ("qwen3", "qwen3"),
        ("qwen-3", "qwen3"),
        ("qwen2", "qwen2"),
        ("qwen-2", "qwen2"),
        ("qwen", "qwen"),
        ("llama-3.3", "llama-3.3"),
        ("llama3.3", "llama-3.3"),
        ("llama-3.2", "llama-3.2"),
        ("llama3.2", "llama-3.2"),
        ("llama-3.1", "llama-3.1"),
        ("llama3.1", "llama-3.1"),
        ("llama-3", "llama-3"),
        ("llama3", "llama-3"),
        ("llama-2", "llama-2"),
        ("llama2", "llama-2"),
        ("llama", "llama"),
        ("gemma-3", "gemma-3"),
        ("gemma3", "gemma-3"),
        ("gemma-2", "gemma-2"),
        ("gemma2", "gemma-2"),
        ("gemma", "gemma"),
        ("phi-4", "phi-4"),
        ("phi4", "phi-4"),
        ("phi-3", "phi-3"),
        ("phi3", "phi-3"),
        ("phi", "phi"),
        ("granite-3.1", "granite-3.1"),
        ("granite3.1", "granite-3.1"),
        ("granite-3", "granite-3"),
        ("granite3", "granite-3"),
        ("granite", "granite"),
        ("mixtral", "mixtral"),
        ("mistral-small", "mistral-small"),
        ("mistral-medium", "mistral-medium"),
        ("mistral-large", "mistral-large"),
        ("mistral-nemo", "mistral-nemo"),
        ("mistral", "mistral"),
        ("deepseek-r1", "deepseek-r1"),
        ("deepseek-v3", "deepseek-v3"),
        ("deepseek-v2", "deepseek-v2"),
        ("deepseek", "deepseek"),
    ];

    /// True iff `s` is a recognized api `model_family` (closed + open sets).
    fn is_valid_api_family(s: &str) -> bool {
        API_FAMILIES.contains(&s) || OPEN_FAMILY_BASES.contains(&s)
    }

    /// A coarse param-size token: digits (one optional dot) + `b`, e.g. `32b`, `3.8b`.
    fn is_size_token(seg: &str) -> bool {
        match seg.strip_suffix('b') {
            Some(num) => {
                !num.is_empty()
                    && num.len() <= 5
                    && num.chars().all(|c| c.is_ascii_digit() || c == '.')
                    && num.chars().any(|c| c.is_ascii_digit())
            }
            None => false,
        }
    }

    /// Extract the first param-size token from a model id (scans `-`/`:` segments).
    fn extract_size(m: &str) -> Option<String> {
        m.split(['-', ':'])
            .find(|s| is_size_token(s))
            .map(str::to_owned)
    }

    /// Aggregated, content-free inputs `benchmark.rs` hands to the share layer (one
    /// per benchmarked model). No prose, no per-slice detail, no raw URLs/ids — only
    /// the coarse values the dashboard rank table needs. Coarsening happens HERE.
    pub struct BenchmarkShareInput<'a> {
        /// "local" | "api".
        pub backend: &'a str,
        /// The model string to coarsen: the ollama tag (local) or `provider.model`
        /// (api). Coarsened to family/bucket here; NEVER sent raw.
        pub model: &'a str,
        /// API style ("none" for local).
        pub provider_style: &'a str,
        /// Coarse route bucket ("none" for local).
        pub provider_route: &'a str,
        /// `CORPUS_VERSION` the score was produced against.
        pub corpus_version: &'a str,
        /// Overall fact retention, 0..1.
        pub retention: f64,
        /// Overall reduction/compression (1 − out/in), 0..1.
        pub reduction: f64,
        /// Total unsupported completion claims across the scored slices.
        pub false_done_total: usize,
        /// True iff EVERY scored slice produced a usable summary.
        pub all_usable: bool,
        /// True when the full corpus was scored (false ⟹ partial_corpus).
        pub full_corpus: bool,
        /// How many slices were actually scored.
        pub scored_slices: usize,
        /// How many scored slices failed (call error).
        pub failed_slices: usize,
        /// Coarse error kind across failed slices ("none" when no failures).
        pub error_kind: &'a str,
    }

    #[derive(Debug, Serialize, PartialEq)]
    pub struct BenchmarkPayload {
        schema_version: u32,
        sent_day: String,
        trimwire_version: String,
        corpus_version: String,
        /// "local" | "api" — keeps API rows from being ranked as local models.
        backend: String,
        /// API style: "none" | "anthropic" | "openai".
        provider_style: String,
        /// Coarse route: "none" | anthropic | openai | openrouter | azure | other.
        provider_route: String,
        /// Broad family. local: ollama family; api: claude-{tier} | gpt | o-series | other.
        model_family: String,
        /// Public coarse model id. local: ollama family; api: claude-tier-N-N | gpt-… | o… | other.
        model_bucket: String,
        /// Size tier (local) or "api".
        model_size_bucket: String,
        retention_bucket: i64,
        compression_bucket: i64,
        false_done_count: String,
        produced_usable_summary: bool,
        /// "full_corpus" | "partial_corpus" — partial runs aren't ranked as full.
        benchmark_scope: String,
        /// "1" | "2-4" | "full".
        slice_count_bucket: String,
        /// Provider/model call failures, capped: "0" | "1" | "2+".
        failed_slice_count: String,
        /// Coarse error kind across failed slices (closed set).
        error_kind: String,
        os_family: String,
    }

    fn count_bucket(n: usize) -> String {
        match n {
            0 => "0",
            1 => "1",
            _ => "2+",
        }
        .to_owned()
    }

    fn slice_count_bucket(scored: usize, full_corpus: bool) -> String {
        if full_corpus {
            "full"
        } else if scored <= 1 {
            "1"
        } else {
            "2-4"
        }
        .to_owned()
    }

    /// Strip an OpenRouter-style `vendor/` prefix: `anthropic/claude-…` → `claude-…`.
    fn strip_vendor(m: &str) -> &str {
        m.rsplit('/').next().unwrap_or(m)
    }

    /// Public coarse model id for an API model. Content-free, low-cardinality;
    /// strips vendor prefixes + dated suffixes. Unknowns → "other".
    fn api_model_bucket(provider_model: &str) -> String {
        let lower = provider_model.to_ascii_lowercase();
        let m = strip_vendor(&lower);
        // claude-(opus|sonnet|haiku)-MAJ-MIN (dated suffix dropped by model_family()).
        let claude = model_family(Some(m));
        if claude != "other" {
            return claude;
        }
        // gpt-<ver>[-(mini|nano|turbo)] — keep one version token + optional variant.
        if let Some(rest) = m.strip_prefix("gpt-") {
            let mut parts = rest.split('-');
            if let Some(ver) = parts.next() {
                let ver_ok = !ver.is_empty()
                    && ver.len() <= 8
                    && ver.chars().all(|c| c.is_ascii_alphanumeric() || c == '.');
                if ver_ok {
                    return match parts.next() {
                        Some(v @ ("mini" | "nano" | "turbo")) => format!("gpt-{ver}-{v}"),
                        _ => format!("gpt-{ver}"),
                    };
                }
            }
            return "other".to_owned();
        }
        // o-series reasoning models: o1, o3-mini, o4-mini …
        if let Some(rest) = m.strip_prefix('o') {
            let mut parts = rest.split('-');
            if let Some(num) = parts.next() {
                if num.len() == 1 && num.chars().all(|c| c.is_ascii_digit()) && num != "0" {
                    return match parts.next() {
                        Some("mini") => format!("o{num}-mini"),
                        _ => format!("o{num}"),
                    };
                }
            }
        }
        // OPEN families (qwen/llama/gemma/mistral/phi/granite/deepseek/mixtral),
        // typically via OpenRouter (`vendor/` already stripped). Build the bucket
        // from a recognized base + the param size — dropping dates/finetune/instruct
        // suffixes so no arbitrary string leaks.
        for (prefix, base) in OPEN_FAMILY_PREFIXES {
            if m.starts_with(prefix) {
                if NO_SIZE_BASES.contains(base) {
                    return (*base).to_owned();
                }
                return match extract_size(m) {
                    Some(size) => format!("{base}-{size}"),
                    None => (*base).to_owned(),
                };
            }
        }
        "other".to_owned()
    }

    /// Broad family from a coarse api `model_bucket`.
    fn api_model_family(bucket: &str) -> String {
        if let Some(tier) = bucket
            .strip_prefix("claude-")
            .and_then(|r| r.split('-').next())
        {
            if matches!(tier, "opus" | "sonnet" | "haiku") {
                return format!("claude-{tier}");
            }
        }
        if bucket.starts_with("gpt-") {
            return "gpt".to_owned();
        }
        if bucket.len() >= 2 && bucket.starts_with('o') && bucket.as_bytes()[1].is_ascii_digit() {
            return "o-series".to_owned();
        }
        // OPEN: bucket is "{base}" or "{base}-{N}b" — strip a trailing size to the base.
        let base = match bucket.rsplit_once('-') {
            Some((head, tail)) if is_size_token(tail) => head,
            _ => bucket,
        };
        if OPEN_FAMILY_BASES.contains(&base) {
            return base.to_owned();
        }
        "other".to_owned()
    }

    /// Build one content-free benchmark row. Pure: `version`/`sent_day` injected for
    /// deterministic tests (mirrors [`build_payload`]).
    pub fn build_benchmark_payload(
        input: &BenchmarkShareInput,
        version: String,
        sent_day: String,
    ) -> BenchmarkPayload {
        let is_api = input.backend == "api";
        let model_bucket = if is_api {
            api_model_bucket(input.model)
        } else {
            // local: the ollama tag coarsens to its family; family == bucket.
            summarizer_family(Some(input.model))
        };
        let model_family = if is_api {
            api_model_family(&model_bucket)
        } else {
            model_bucket.clone()
        };
        let model_size_bucket = if is_api {
            "api".to_owned()
        } else {
            summarizer_size_bucket("default", Some(input.model))
        };
        BenchmarkPayload {
            schema_version: SCHEMA_VERSION,
            sent_day,
            trimwire_version: version,
            corpus_version: input.corpus_version.to_owned(),
            backend: input.backend.to_owned(),
            provider_style: input.provider_style.to_owned(),
            provider_route: input.provider_route.to_owned(),
            model_family,
            model_bucket,
            model_size_bucket,
            retention_bucket: cache_pct_bucket(input.retention * 100.0),
            compression_bucket: cache_pct_bucket(input.reduction * 100.0),
            false_done_count: count_bucket(input.false_done_total),
            produced_usable_summary: input.all_usable,
            benchmark_scope: if input.full_corpus {
                "full_corpus".to_owned()
            } else {
                "partial_corpus".to_owned()
            },
            slice_count_bucket: slice_count_bucket(input.scored_slices, input.full_corpus),
            failed_slice_count: count_bucket(input.failed_slices),
            error_kind: input.error_kind.to_owned(),
            os_family: os_family(),
        }
    }

    /// True iff `s` is a valid api `model_bucket` shape (claude-tier-N-N | gpt-… | o… | "other").
    fn is_valid_api_model_bucket(s: &str) -> bool {
        if s == "other" {
            return true;
        }
        // claude-(opus|sonnet|haiku)-D-D, each D = 1..=3 digits. The 1–3 digit bound
        // is the DOCUMENTED shared limit with the TS collector's API_BUCKET_CLAUDE_RE
        // (`\d{1,3}`) — keep both in sync so a future version can't pass one + 400 the
        // other. (Current Claude versions are single-digit; 3 digits is ample headroom.)
        if let Some(rest) = s.strip_prefix("claude-") {
            let parts: Vec<&str> = rest.split('-').collect();
            let digits_1_3 =
                |p: &str| (1..=3).contains(&p.len()) && p.chars().all(|c| c.is_ascii_digit());
            if parts.len() == 3
                && matches!(parts[0], "opus" | "sonnet" | "haiku")
                && digits_1_3(parts[1])
                && digits_1_3(parts[2])
            {
                return true;
            }
            return false;
        }
        if let Some(rest) = s.strip_prefix("gpt-") {
            let mut parts = rest.split('-');
            let ver = parts.next().unwrap_or("");
            let ver_ok = !ver.is_empty()
                && ver.len() <= 8
                && ver.chars().all(|c| c.is_ascii_alphanumeric() || c == '.');
            let variant_ok = match parts.next() {
                None => true,
                Some("mini" | "nano" | "turbo") => parts.next().is_none(),
                Some(_) => false,
            };
            return ver_ok && variant_ok;
        }
        if let Some(rest) = s.strip_prefix('o') {
            let mut parts = rest.split('-');
            let num = parts.next().unwrap_or("");
            let num_ok = num.len() == 1 && num.chars().all(|c| c.is_ascii_digit()) && num != "0";
            let variant_ok = match parts.next() {
                None => true,
                Some("mini") => parts.next().is_none(),
                Some(_) => false,
            };
            if num_ok && variant_ok {
                return true;
            }
        }
        // OPEN: "{base}" or "{base}-{N}b" for a recognized base (size only when allowed).
        let (base, has_size) = match s.rsplit_once('-') {
            Some((head, tail)) if is_size_token(tail) => (head, true),
            _ => (s, false),
        };
        if OPEN_FAMILY_BASES.contains(&base) {
            return !(has_size && NO_SIZE_BASES.contains(&base));
        }
        false
    }

    /// Content-free guard for the benchmark payload — fail-closed: unknown top-level
    /// key, or any closed-enum/bucket value out of its set/range, refuses (never
    /// printed, never sent). Backend-aware: provider/model fields are validated
    /// against the local vs api rules.
    pub fn guard_benchmark_content_free(value: &serde_json::Value) -> Result<()> {
        let obj = value
            .as_object()
            .context("benchmark payload must serialize to a JSON object")?;
        for key in obj.keys() {
            if !BENCHMARK_ALLOWED_KEYS.contains(&key.as_str()) {
                anyhow::bail!("refusing to share: unexpected benchmark field {key:?}");
            }
        }
        for key in BENCHMARK_ALLOWED_KEYS {
            if !obj.contains_key(*key) {
                anyhow::bail!("refusing to share: missing benchmark field {key:?}");
            }
        }
        let get_str = |field: &str| -> Result<&str> {
            obj.get(field)
                .and_then(|v| v.as_str())
                .with_context(|| format!("missing/non-string benchmark field {field:?}"))
        };
        if obj.get("schema_version").and_then(|v| v.as_u64()) != Some(SCHEMA_VERSION as u64) {
            anyhow::bail!("refusing to share: benchmark schema_version mismatch");
        }
        check_enum_in(get_str("os_family")?, OS_FAMILIES, "os_family")?;
        let backend = get_str("backend")?;
        check_enum_in(backend, BACKENDS, "backend")?;
        check_enum_in(
            get_str("provider_style")?,
            PROVIDER_STYLES,
            "provider_style",
        )?;
        check_enum_in(
            get_str("provider_route")?,
            PROVIDER_ROUTES,
            "provider_route",
        )?;
        check_enum_in(
            get_str("benchmark_scope")?,
            BENCHMARK_SCOPES,
            "benchmark_scope",
        )?;
        check_enum_in(
            get_str("slice_count_bucket")?,
            SLICE_COUNT_BUCKETS,
            "slice_count_bucket",
        )?;
        check_enum_in(
            get_str("false_done_count")?,
            COUNT_BUCKETS,
            "false_done_count",
        )?;
        check_enum_in(
            get_str("failed_slice_count")?,
            COUNT_BUCKETS,
            "failed_slice_count",
        )?;
        check_enum_in(get_str("error_kind")?, ERROR_KINDS, "error_kind")?;

        let fam = get_str("model_family")?;
        let bucket = get_str("model_bucket")?;
        let size = get_str("model_size_bucket")?;
        let provider_style = get_str("provider_style")?;
        let provider_route = get_str("provider_route")?;
        if backend == "api" {
            // family/bucket derived from the REAL model — a provider name is invalid.
            if !is_valid_api_family(fam) {
                anyhow::bail!("refusing to share: model_family has unexpected value {fam:?}");
            }
            if !is_valid_api_model_bucket(bucket) {
                anyhow::bail!("refusing to share: model_bucket has unexpected value {bucket:?}");
            }
            // family must be the one DERIVED from the (public, coarsened) bucket —
            // a valid-but-mismatched pair (e.g. family "gpt" + bucket "o3-mini") is
            // rejected. Uses only the bucket already in the payload, never raw input.
            if fam != api_model_family(bucket) {
                anyhow::bail!(
                    "refusing to share: model_family {fam:?} is inconsistent with model_bucket {bucket:?}"
                );
            }
            if size != "api" {
                anyhow::bail!("refusing to share: api model_size_bucket must be \"api\"");
            }
            // an api row carries a real style + route ("none" is local-only).
            if provider_style == "none" || provider_route == "none" {
                anyhow::bail!("refusing to share: api row must have a real provider_style + route");
            }
        } else {
            // local: family == bucket, an ollama family or "other".
            if fam != "other" && !SUMMARIZER_FAMILIES.contains(&fam) {
                anyhow::bail!("refusing to share: model_family has unexpected value {fam:?}");
            }
            if bucket != "other" && !SUMMARIZER_FAMILIES.contains(&bucket) {
                anyhow::bail!("refusing to share: model_bucket has unexpected value {bucket:?}");
            }
            // local family + bucket are the SAME coarsened ollama family — enforce it.
            if fam != bucket {
                anyhow::bail!(
                    "refusing to share: local model_family {fam:?} must equal model_bucket {bucket:?}"
                );
            }
            check_enum_in(size, BENCHMARK_SIZE_BUCKETS, "model_size_bucket")?;
            if provider_style != "none" || provider_route != "none" {
                anyhow::bail!(
                    "refusing to share: local row must have provider_style/route \"none\""
                );
            }
        }
        // Cross-field consistency (fail closed even for hand-crafted payloads):
        let failed = get_str("failed_slice_count")?;
        let error_kind = get_str("error_kind")?;
        if (failed == "0") != (error_kind == "none") {
            anyhow::bail!("refusing to share: failed_slice_count/error_kind are inconsistent");
        }
        let scope = get_str("benchmark_scope")?;
        let slice_count = get_str("slice_count_bucket")?;
        if (scope == "full_corpus") != (slice_count == "full") {
            anyhow::bail!("refusing to share: benchmark_scope/slice_count_bucket are inconsistent");
        }
        for field in ["retention_bucket", "compression_bucket"] {
            let n = obj
                .get(field)
                .and_then(|v| v.as_i64())
                .with_context(|| format!("missing benchmark {field}"))?;
            if !(0..=100).contains(&n) || n % 10 != 0 {
                anyhow::bail!("refusing to share: benchmark {field} out of range");
            }
        }
        obj.get("produced_usable_summary")
            .and_then(|v| v.as_bool())
            .context("missing produced_usable_summary")?;
        for field in ["sent_day", "trimwire_version", "corpus_version"] {
            get_str(field)?; // present + a string; values are coarse by construction
        }
        Ok(())
    }

    /// Shared closed-enum check (value must be in `allowed`).
    fn check_enum_in(value: &str, allowed: &[&str], field: &str) -> Result<()> {
        if !allowed.contains(&value) {
            anyhow::bail!("refusing to share: field {field:?} has unexpected value {value:?}");
        }
        Ok(())
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use serde_json::Value;

        fn input() -> BenchmarkShareInput<'static> {
            BenchmarkShareInput {
                backend: "local",
                model: "qwen3.5:4b",
                provider_style: "none",
                provider_route: "none",
                corpus_version: "1",
                retention: 1.0,
                reduction: 0.51,
                false_done_total: 0,
                all_usable: true,
                full_corpus: true,
                scored_slices: 5,
                failed_slices: 0,
                error_kind: "none",
            }
        }

        fn api_input() -> BenchmarkShareInput<'static> {
            BenchmarkShareInput {
                backend: "api",
                model: "claude-haiku-4-5-20251101",
                provider_style: "anthropic",
                provider_route: "anthropic",
                corpus_version: "1",
                retention: 0.9,
                reduction: 0.6,
                false_done_total: 0,
                all_usable: true,
                full_corpus: true,
                scored_slices: 5,
                failed_slices: 0,
                error_kind: "none",
            }
        }

        #[test]
        fn benchmark_payload_local_is_content_free_and_coarse() {
            let p = build_benchmark_payload(&input(), "0.1".to_owned(), "2026-06-08".to_owned());
            let v = serde_json::to_value(&p).unwrap();
            let obj = v.as_object().unwrap();
            for k in obj.keys() {
                assert!(
                    BENCHMARK_ALLOWED_KEYS.contains(&k.as_str()),
                    "unexpected key {k}"
                );
            }
            assert_eq!(obj.len(), BENCHMARK_ALLOWED_KEYS.len(), "no silent drops");
            assert_eq!(p.backend, "local");
            assert_eq!(p.provider_style, "none");
            assert_eq!(p.provider_route, "none");
            assert_eq!(p.model_family, "qwen3.5");
            assert_eq!(p.model_bucket, "qwen3.5");
            assert_eq!(p.model_size_bucket, "3-4b");
            assert_eq!(p.retention_bucket, 100);
            assert_eq!(p.compression_bucket, 50); // 51% → 50
            assert_eq!(p.benchmark_scope, "full_corpus");
            assert_eq!(p.slice_count_bucket, "full");
            assert_eq!(p.failed_slice_count, "0");
            assert_eq!(p.error_kind, "none");
            guard_benchmark_content_free(&v).expect("clean local payload passes the guard");
        }

        #[test]
        fn benchmark_payload_api_coarsens_from_real_model() {
            let p =
                build_benchmark_payload(&api_input(), "0.2".to_owned(), "2026-06-14".to_owned());
            // model_family/bucket come from the real model, NOT the provider id/style.
            assert_eq!(p.model_bucket, "claude-haiku-4-5"); // dated suffix dropped
            assert_eq!(p.model_family, "claude-haiku");
            assert_eq!(p.backend, "api");
            assert_eq!(p.provider_style, "anthropic");
            assert_eq!(p.provider_route, "anthropic");
            assert_eq!(p.model_size_bucket, "api"); // param count meaningless for cloud
            let v = serde_json::to_value(&p).unwrap();
            guard_benchmark_content_free(&v).expect("clean api payload passes the guard");
        }

        #[test]
        fn benchmark_payload_api_openai_and_openrouter_prefix() {
            let mut i = api_input();
            i.model = "openai/gpt-4.1-mini"; // openrouter-style vendor prefix
            i.provider_style = "openai";
            i.provider_route = "openrouter";
            let p = build_benchmark_payload(&i, "0.2".to_owned(), "2026-06-14".to_owned());
            assert_eq!(p.model_bucket, "gpt-4.1-mini"); // vendor/ stripped, variant kept
            assert_eq!(p.model_family, "gpt");
            assert_eq!(p.provider_route, "openrouter");
            let v = serde_json::to_value(&p).unwrap();
            guard_benchmark_content_free(&v).expect("openai/openrouter api payload passes");
        }

        #[test]
        fn false_done_count_caps_at_two_plus() {
            let mut i = input();
            i.false_done_total = 7;
            let p = build_benchmark_payload(&i, "0.1".to_owned(), "2026-06-08".to_owned());
            assert_eq!(p.false_done_count, "2+");
        }

        #[test]
        fn benchmark_guard_rejects_drift() {
            let p = build_benchmark_payload(&input(), "0.1".to_owned(), "2026-06-08".to_owned());

            // an unexpected top-level key (e.g. a raw tag leaking through)
            let mut extra = serde_json::to_value(&p).unwrap();
            extra.as_object_mut().unwrap().insert(
                "model_tag".to_owned(),
                Value::String("qwen3.5:4b".to_owned()),
            );
            assert!(guard_benchmark_content_free(&extra).is_err());

            // a missing key
            let mut missing = serde_json::to_value(&p).unwrap();
            missing.as_object_mut().unwrap().remove("backend");
            assert!(guard_benchmark_content_free(&missing).is_err());

            // a value out of a closed enum (backend)
            let mut bad_backend = serde_json::to_value(&p).unwrap();
            bad_backend
                .as_object_mut()
                .unwrap()
                .insert("backend".to_owned(), Value::String("cloud".to_owned()));
            assert!(guard_benchmark_content_free(&bad_backend).is_err());

            // an `api-dry-run` placeholder must never validate as a real payload:
            // dry-run rows (an API model requested without --yes) made no provider
            // calls, so they carry no real data and must be fail-closed rejected.
            let mut dry_run = serde_json::to_value(&p).unwrap();
            dry_run.as_object_mut().unwrap().insert(
                "backend".to_owned(),
                Value::String("api-dry-run".to_owned()),
            );
            assert!(
                guard_benchmark_content_free(&dry_run).is_err(),
                "api-dry-run is not a valid wire backend"
            );

            // a retention bucket that isn't a 10pp step
            let mut bad_ret = serde_json::to_value(&p).unwrap();
            bad_ret
                .as_object_mut()
                .unwrap()
                .insert("retention_bucket".to_owned(), Value::from(85));
            assert!(guard_benchmark_content_free(&bad_ret).is_err());

            // error_kind outside the closed set
            let mut bad_err = serde_json::to_value(&p).unwrap();
            bad_err
                .as_object_mut()
                .unwrap()
                .insert("error_kind".to_owned(), Value::String("explode".to_owned()));
            assert!(guard_benchmark_content_free(&bad_err).is_err());

            // local row must not carry a provider_style/route
            let mut local_with_provider = serde_json::to_value(&p).unwrap();
            local_with_provider.as_object_mut().unwrap().insert(
                "provider_style".to_owned(),
                Value::String("anthropic".to_owned()),
            );
            assert!(guard_benchmark_content_free(&local_with_provider).is_err());

            // local model_family/size that don't belong (none/api) are rejected.
            for bad in ["none", "api"] {
                let mut v = serde_json::to_value(&p).unwrap();
                v.as_object_mut().unwrap().insert(
                    "model_size_bucket".to_owned(),
                    Value::String(bad.to_owned()),
                );
                assert!(guard_benchmark_content_free(&v).is_err());
            }
            let mut bad_fam = serde_json::to_value(&p).unwrap();
            bad_fam
                .as_object_mut()
                .unwrap()
                .insert("model_family".to_owned(), Value::String("none".to_owned()));
            assert!(guard_benchmark_content_free(&bad_fam).is_err());
        }

        #[test]
        fn benchmark_guard_rejects_provider_as_family_on_api() {
            // provider name as model_family / model_bucket is invalid for api rows.
            let p =
                build_benchmark_payload(&api_input(), "0.2".to_owned(), "2026-06-14".to_owned());
            for provider in ["anthropic", "openai", "openrouter"] {
                let mut bad_fam = serde_json::to_value(&p).unwrap();
                bad_fam.as_object_mut().unwrap().insert(
                    "model_family".to_owned(),
                    Value::String(provider.to_owned()),
                );
                assert!(
                    guard_benchmark_content_free(&bad_fam).is_err(),
                    "model_family {provider:?} must be rejected on api rows"
                );

                let mut bad_bucket = serde_json::to_value(&p).unwrap();
                bad_bucket.as_object_mut().unwrap().insert(
                    "model_bucket".to_owned(),
                    Value::String(provider.to_owned()),
                );
                assert!(
                    guard_benchmark_content_free(&bad_bucket).is_err(),
                    "model_bucket {provider:?} must be rejected on api rows"
                );
            }
            // api rows must carry size "api" and a real provider_style.
            let mut bad_size = serde_json::to_value(&p).unwrap();
            bad_size.as_object_mut().unwrap().insert(
                "model_size_bucket".to_owned(),
                Value::String("3-4b".to_owned()),
            );
            assert!(guard_benchmark_content_free(&bad_size).is_err());
        }

        #[test]
        fn api_open_model_buckets_preserve_family_and_size() {
            // OpenRouter-style vendor prefixes stripped; family + size preserved;
            // instruct/it/date suffixes dropped. NOT collapsed to "other".
            let cases = [
                ("qwen/qwen3-32b", "qwen3-32b", "qwen3"),
                ("qwen/qwen3-4b", "qwen3-4b", "qwen3"),
                ("google/gemma-3-27b-it", "gemma-3-27b", "gemma-3"),
                (
                    "meta-llama/llama-3.1-8b-instruct",
                    "llama-3.1-8b",
                    "llama-3.1",
                ),
                (
                    "mistralai/mistral-small-2501",
                    "mistral-small",
                    "mistral-small",
                ),
                ("qwen/qwen2.5-7b-instruct", "qwen2.5-7b", "qwen2.5"),
            ];
            for (model, want_bucket, want_family) in cases {
                let mut i = api_input();
                i.model = model;
                i.provider_style = "openai";
                i.provider_route = "openrouter";
                let p = build_benchmark_payload(&i, "0.2".to_owned(), "2026-06-14".to_owned());
                assert_eq!(p.model_bucket, want_bucket, "bucket for {model}");
                assert_eq!(p.model_family, want_family, "family for {model}");
                let v = serde_json::to_value(&p).unwrap();
                guard_benchmark_content_free(&v)
                    .unwrap_or_else(|e| panic!("{model} should pass: {e}"));
            }
        }

        #[test]
        fn benchmark_guard_enforces_cross_field_consistency() {
            let p =
                build_benchmark_payload(&api_input(), "0.2".to_owned(), "2026-06-14".to_owned());
            let tweak = |k: &str, val: &str| {
                let mut v = serde_json::to_value(&p).unwrap();
                v.as_object_mut()
                    .unwrap()
                    .insert(k.to_owned(), Value::String(val.to_owned()));
                v
            };
            // failed_slice_count=0 but error_kind != none → reject
            assert!(guard_benchmark_content_free(&tweak("error_kind", "timeout")).is_err());
            // benchmark_scope=full_corpus but slice_count_bucket != full → reject
            assert!(guard_benchmark_content_free(&tweak("slice_count_bucket", "2-4")).is_err());
            // api row with provider_route=none → reject
            assert!(guard_benchmark_content_free(&tweak("provider_route", "none")).is_err());
            // local row carrying a provider_route → reject
            let lp = build_benchmark_payload(&input(), "0.1".to_owned(), "2026-06-08".to_owned());
            let mut bad_local = serde_json::to_value(&lp).unwrap();
            bad_local.as_object_mut().unwrap().insert(
                "provider_route".to_owned(),
                Value::String("openrouter".to_owned()),
            );
            assert!(guard_benchmark_content_free(&bad_local).is_err());
        }

        #[test]
        fn benchmark_guard_rejects_family_bucket_mismatch() {
            // LOCAL: family must equal bucket (both the same coarsened ollama family).
            let lp = build_benchmark_payload(&input(), "0.1".to_owned(), "2026-06-08".to_owned());
            let mut local_mismatch = serde_json::to_value(&lp).unwrap();
            local_mismatch.as_object_mut().unwrap().insert(
                "model_bucket".to_owned(),
                Value::String("llama3.1".to_owned()),
            );
            assert!(
                guard_benchmark_content_free(&local_mismatch).is_err(),
                "local family=qwen3.5 + bucket=llama3.1 must be rejected"
            );

            // API: family must be the one DERIVED from the bucket. Each pair is
            // individually valid but inconsistent → reject.
            let ap =
                build_benchmark_payload(&api_input(), "0.2".to_owned(), "2026-06-14".to_owned());
            let mismatches = [
                ("qwen3", "llama-3.1-8b"),
                ("claude-haiku", "gpt-4.1-mini"),
                ("gpt", "o3-mini"),
                ("gemma-3", "qwen3-32b"),
            ];
            for (fam, bucket) in mismatches {
                let mut v = serde_json::to_value(&ap).unwrap();
                {
                    let o = v.as_object_mut().unwrap();
                    o.insert("model_family".to_owned(), Value::String(fam.to_owned()));
                    o.insert("model_bucket".to_owned(), Value::String(bucket.to_owned()));
                }
                assert!(
                    guard_benchmark_content_free(&v).is_err(),
                    "api family={fam:?} + bucket={bucket:?} must be rejected"
                );
            }
        }
    }
} // mod benchmark_share

/// Today's UTC calendar date `YYYY-MM-DD` (shared no-chrono civil math). Also used
/// by the sibling `share benchmark` path.
pub(super) fn utc_today() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    super::civil::fmt_date(secs)
}

// ---- endpoint resolution ---------------------------------------------------

/// Resolve the stats collector endpoint.
///
/// Resolution order (first non-empty wins):
/// 1. Explicit `[share] endpoint` in the user's config (allows self-hosting/testing).
/// 2. Built-in `COMMUNITY_STATS_ENDPOINT` constant (the deployed `api.trimwire.dev`).
/// 3. Empty string → dry run; no network I/O.
fn resolve_stats_endpoint(config_endpoint: &str) -> &'static str {
    // The caller is responsible for checking `config_endpoint` first (non-empty
    // config wins and is returned directly by the caller). This function is only
    // invoked when the config endpoint is empty, so it returns the built-in const.
    // The `_` suppresses the "unused argument" lint while keeping the doc contract
    // visible in the signature.
    let _ = config_endpoint;
    COMMUNITY_STATS_ENDPOINT
}

/// Resolve the benchmark collector endpoint.
///
/// Resolution order (first non-empty wins):
/// 1. Explicit `[share] benchmark_endpoint` in the user's config.
/// 2. Built-in `COMMUNITY_BENCHMARK_ENDPOINT` constant (the deployed
///    `api.trimwire.dev/ingest-benchmark`).
/// 3. Empty string → dry run.
///
/// Called by `benchmark.rs` (`run_share`), symmetric with `resolve_stats_endpoint`.
pub(super) fn resolve_benchmark_endpoint(config_endpoint: &str) -> &str {
    if !config_endpoint.trim().is_empty() {
        config_endpoint
    } else {
        COMMUNITY_BENCHMARK_ENDPOINT
    }
}

// ---- consent (share enable / disable) -------------------------------------

/// `trimwire share enable` — opt in: persist `[share] enabled = true` to the
/// global config so `trimwire share stats` uploads without `--yes` each run.
pub fn share_enable() -> Result<()> {
    let path = set_share_enabled(true)?;
    println!(
        "{} telemetry enabled — `trimwire share stats` will now upload (anonymous, content-free).",
        super::render::ok()
    );
    println!("  wrote `[share] enabled = true` to {}", path.display());
    println!(
        "  (uploads go to the community collector at api.trimwire.dev; opt back out any time with \
         `trimwire share disable`.)"
    );
    Ok(())
}

/// `trimwire share disable` — opt out: persist `[share] enabled = false`.
pub fn share_disable() -> Result<()> {
    let path = set_share_enabled(false)?;
    println!(
        "{} telemetry disabled — nothing will be uploaded.",
        super::render::ok()
    );
    println!("  wrote `[share] enabled = false` to {}", path.display());
    Ok(())
}

/// Write `[share] enabled = <on>` into the global config, preserving the rest of
/// the file (comments + other sections). Creates the file/section if absent.
/// Returns the config path written.
fn set_share_enabled(on: bool) -> Result<std::path::PathBuf> {
    let path = global_config_path();
    let existing = if path.exists() {
        std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?
    } else {
        String::new()
    };
    let updated = upsert_share_enabled(&existing, on);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create config dir {}", parent.display()))?;
    }
    std::fs::write(&path, updated).with_context(|| format!("write {}", path.display()))?;
    Ok(path)
}

/// Pure: set/insert `enabled = <on>` inside the `[share]` table of `toml`,
/// preserving every other line. Appends a `[share]` section if none exists.
/// Line-based (not a toml round-trip) so user comments/formatting survive.
fn upsert_share_enabled(toml: &str, on: bool) -> String {
    let val = if on { "true" } else { "false" };
    let mut lines: Vec<String> = toml.lines().map(str::to_string).collect();
    let mut in_share = false;
    let mut share_hdr: Option<usize> = None;
    for i in 0..lines.len() {
        let trimmed = lines[i].trim();
        // Strip an inline comment before detection so `[share] # x` is recognized.
        let hdr = match trimmed.find('#') {
            Some(j) => trimmed[..j].trim_end(),
            None => trimmed,
        };
        if hdr.starts_with('[') && hdr.ends_with(']') {
            let name = hdr.trim_start_matches('[').trim_end_matches(']');
            in_share = name == "share";
            if in_share {
                share_hdr = Some(i);
            }
        } else if in_share && hdr.starts_with("enabled") && hdr[7..].trim_start().starts_with('=') {
            lines[i] = format!("enabled = {val}");
            return with_trailing_newline(lines.join("\n"));
        }
    }
    if let Some(idx) = share_hdr {
        lines.insert(idx + 1, format!("enabled = {val}"));
        return with_trailing_newline(lines.join("\n"));
    }
    let mut out = toml.trim_end().to_string();
    if !out.is_empty() {
        out.push_str("\n\n");
    }
    out.push_str(&format!("[share]\nenabled = {val}\n"));
    out
}

fn with_trailing_newline(mut s: String) -> String {
    if !s.ends_with('\n') {
        s.push('\n');
    }
    s
}

// ---- command --------------------------------------------------------------

/// `trimwire share stats [--yes] [--force]`.
///
/// Consent model:
/// - Without consent (`[share] enabled = false`, the default): always a dry run
///   that prints the payload + how to opt in. Nothing is ever sent.
/// - With consent (`[share] enabled = true`) AND a non-empty resolved endpoint:
///   uploads without requiring `--yes` on each run (once-per-day throttle applies).
/// - `--yes` is an explicit per-run override: it acts as consent for a single
///   upload even if `enabled` is not set, provided a non-empty endpoint exists.
///   It also lets first-time uploaders confirm before setting `enabled` permanently.
/// - Both community endpoints are live (`api.trimwire.dev`). Self-hosters who set
///   an empty `[share] endpoint`/`benchmark_endpoint` and have no built-in const
///   simply dry-run — with no destination there is nothing to send.
///
/// `--force` bypasses the once-per-day upload throttle (useful for testing).
pub fn share_stats(yes: bool, force: bool) -> Result<()> {
    let config = Config::load().context("load config")?;
    if !config.ledger.enabled {
        println!(
            "{} ledger is disabled ([ledger] enabled = false) — nothing to share.",
            super::render::bullet()
        );
        return Ok(());
    }
    if !ledger::resolve_path(&config.ledger.db_path).exists() {
        println!(
            "{} ledger not yet created — run {}/{} first.",
            super::render::bullet(),
            super::render::accent("trimwire on"),
            super::render::accent("trimwire run")
        );
        return Ok(());
    }

    let report = Ledger::report(&config.ledger.db_path).context("read ledger")?;
    let sessions =
        Ledger::list_sessions(&config.ledger.db_path, None, 50).context("list sessions")?;
    // Compute the day-scoped dedup token.  The install id never leaves the machine.
    let sent_day = utc_today();
    let data_dir = trimwire_data_dir();
    let dedup_token = match read_or_create_install_id(&data_dir) {
        Ok(install_id) => compute_dedup_token(&install_id, &sent_day).unwrap_or_else(|e| {
            eprintln!("  (warning: couldn't compute dedup token: {e}; using placeholder)");
            "0".repeat(64)
        }),
        Err(e) => {
            eprintln!("  (warning: couldn't read/create install id: {e}; using placeholder)");
            "0".repeat(64)
        }
    };
    let payload = build_payload(
        &report,
        &sessions,
        &config,
        version_bucket(),
        sent_day,
        dedup_token,
    );
    let value = serde_json::to_value(&payload).context("serialize payload")?;
    // Fail closed: never even print/send a payload with an unexpected field.
    guard_content_free(&value).context("payload content-free guard")?;
    let body = serde_json::to_string_pretty(&value).context("render payload")?;

    println!(
        "{} trimwire share stats — anonymous, content-free telemetry",
        super::render::header()
    );
    println!(
        "  {}\n",
        super::render::dim(
            "This is the *entire* payload (coarse buckets only; see docs/TELEMETRY.md):"
        )
    );
    println!("{body}\n");
    println!(
        "  No prompts, code, paths, ids, IPs, timestamps, or raw counts are in it,\n\
         \x20  and nothing else is collected."
    );

    // Resolve the effective endpoint: explicit config override wins over the
    // built-in constant. Both empty → dry run.
    let config_ep = config.share.endpoint.trim();
    let endpoint: &str = if !config_ep.is_empty() {
        config_ep
    } else {
        resolve_stats_endpoint(config_ep)
    };

    if endpoint.is_empty() {
        // No endpoint resolved — only reachable if a self-hoster set [share]
        // endpoint empty (the built-in const ships pointing at api.trimwire.dev).
        // Dry run regardless of consent or --yes — there is nowhere to send.
        println!(
            "\n  No collector endpoint is configured, so this was a DRY RUN.\n\
             \x20  To opt in to the community dashboard: `trimwire share enable`.\n\
             \x20  To self-host, set [share] endpoint in `trimwire config edit`."
        );
        return Ok(());
    }

    // An endpoint exists. Check consent: upload if explicitly enabled or --yes.
    let has_consent = config.share.enabled || yes;
    if !has_consent {
        println!(
            "\n  DRY RUN — you haven't opted in yet.\n\
             \x20  To opt in: `trimwire share enable`, then re-run `trimwire share stats`.\n\
             \x20  Or for a one-time upload: re-run `trimwire share stats --yes`.\n\
             \x20  Endpoint: {endpoint}"
        );
        return Ok(());
    }

    // Soft, identity-free throttle: at most one upload per UTC day. The date is
    // stored locally and NEVER transmitted (see docs/TELEMETRY.md "no identity").
    // --force bypasses the throttle (useful for testing or re-sending after a fix).
    let state_path = ledger::resolve_path("~/.trimwire/share-state");
    let today = payload.sent_day.clone();
    if !force {
        match std::fs::read_to_string(&state_path) {
            Ok(prev) if prev.trim() == today => {
                println!(
                    "\n  Already shared today ({today} UTC).\n\
                     \x20  Use --force to bypass the once-per-day throttle and send again."
                );
                return Ok(());
            }
            Ok(_) => {} // a different (older) day → proceed; will overwrite below.
            // No state file (fresh install, cleared /tmp, deleted): the daily throttle
            // can't engage. Say so plainly rather than silently skip it.
            Err(_) => eprintln!(
                "  (note: no prior share record found at {} — the once-a-day throttle isn't enforced for this run)",
                state_path.display()
            ),
        }
    }

    post(endpoint, &body).context("upload telemetry")?;
    println!(
        "\n  {} Shared. Thank you — your anonymous numbers help tune trimwire for everyone.",
        super::render::ok()
    );

    // Persist the throttle date (never transmitted). A literal "~" prefix means
    // $HOME was unset and resolve_path couldn't expand it — warn rather than
    // create a stray "~" directory, since the next read would miss it and the
    // daily throttle would silently no-op.
    if state_path.starts_with("~") {
        eprintln!(
            "  (warning: $HOME unset — couldn't record the daily-share throttle; \
             repeated `share` runs won't be rate-limited)"
        );
        return Ok(());
    }
    let write_result = state_path
        .parent()
        .map(std::fs::create_dir_all)
        .unwrap_or(Ok(()))
        .and_then(|()| std::fs::write(&state_path, &today));
    if let Err(e) = write_result {
        eprintln!(
            "  (warning: couldn't record the daily-share throttle at {}: {e}; \
             a re-run today will upload again)",
            state_path.display()
        );
    }
    Ok(())
}

/// POST the JSON body using the same hyper-rustls client the proxy already
/// depends on (no extra HTTP dependency). A one-shot current-thread tokio
/// runtime keeps the sync CLI path runtime-free elsewhere. `https_or_http`
/// means an `http://localhost` collector works for local testing too.
pub(super) fn post(endpoint: &str, body: &str) -> Result<()> {
    use http_body_util::{BodyExt, Full};
    use hyper::Request;
    use hyper::body::Bytes;
    use trimwire::proxy::upstream::build_client;

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("build runtime")?;
    rt.block_on(async {
        let client = build_client();
        let req = Request::builder()
            .method("POST")
            .uri(endpoint)
            .header("content-type", "application/json")
            .body(Full::new(Bytes::from(body.to_owned())))
            .context("build request")?;
        let resp = client.request(req).await.context("send request")?;
        let status = resp.status();
        if !status.is_success() {
            // Surface a short prefix of the collector's reason (e.g. which field
            // it rejected) so a failed upload is debuggable, not just "HTTP 400".
            let bytes = resp
                .into_body()
                .collect()
                .await
                .map(|b| b.to_bytes())
                .unwrap_or_default();
            let take = bytes.len().min(256);
            let snippet = String::from_utf8_lossy(&bytes[..take]);
            let snippet = snippet.trim();
            if snippet.is_empty() {
                anyhow::bail!("collector returned HTTP {status}");
            }
            anyhow::bail!("collector returned HTTP {status}: {snippet}");
        }
        Ok(())
    })
}

/// `trimwire share benchmark [--yes]` — score the configured summarizer model and
/// share the content-free per-model rows to the community benchmark endpoint.
/// Delegates to `benchmark::benchmark_share`.
pub fn share_benchmark(models: Vec<String>, all_installed: bool, yes: bool) -> Result<()> {
    super::benchmark::benchmark_share(models, all_installed, yes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;
    use trimwire::ledger::{CacheStability, ResponseMetrics};

    #[test]
    fn upsert_share_enabled_appends_inserts_and_replaces() {
        // No [share] section → append one.
        let a = upsert_share_enabled("[server]\nlisten = \"127.0.0.1:8765\"\n", true);
        assert!(a.contains("[server]"), "preserves other sections: {a}");
        assert!(
            a.contains("[share]\nenabled = true\n"),
            "appends section: {a}"
        );
        // Existing [share] header, no enabled → insert right under the header.
        let b = upsert_share_enabled("[share]\nendpoint = \"\"\n", true);
        assert!(
            b.contains("[share]\nenabled = true\nendpoint = \"\""),
            "inserts: {b}"
        );
        // Existing enabled → replace in place, exactly one enabled line.
        let c = upsert_share_enabled("[share]\nenabled = false  # note\n", true);
        assert_eq!(
            c.matches("enabled =").count(),
            1,
            "single enabled line: {c}"
        );
        assert!(c.contains("enabled = true"), "flips to true: {c}");
        // disable path.
        let d = upsert_share_enabled("[share]\nenabled = true\n", false);
        assert!(d.contains("enabled = false"), "flips to false: {d}");
        // A nested table like [summarizer.providers] must not be treated as [share].
        let e = upsert_share_enabled("[summarizer]\nengine = \"local\"\n", true);
        assert!(
            e.contains("[summarizer]") && e.contains("[share]\nenabled = true"),
            "{e}"
        );
    }

    fn empty_report() -> Report {
        Report {
            total_requests: 0,
            total_in_bytes: 0,
            total_out_bytes: 0,
            per_day: vec![],
            per_strategy: vec![],
            per_strategy_bytes: vec![],
            cache_stability: CacheStability {
                no_strategy_total: 0,
                no_strategy_stable: 0,
                ratio: 0.0,
            },
            response_metrics: ResponseMetrics::default(),
            requests_with_strategy: 0,
            summarizer_accepted: 0,
            summarizer_rejected: 0,
            summarizer_errored: 0,
            summarizer_accepted_local: 0,
            summarizer_accepted_api: 0,
            summarizer_collapses: 0,
            upstream_errors: 0,
            upstream_timeouts: 0,
            post_prune_errors: 0,
            upstream_http_errors: 0,
            invalid_prune_rollbacks: 0,
            db_path: std::path::PathBuf::from(":memory:"),
        }
    }

    #[test]
    fn buckets_are_coarse() {
        assert_eq!(reduction_bucket(42.1), 40);
        assert_eq!(reduction_bucket(99.9), 95);
        assert_eq!(reduction_bucket(-5.0), 0);
        assert_eq!(cache_pct_bucket(68.3), 60);
        assert_eq!(cache_pct_bucket(100.0), 100);
        assert_eq!(stability_bucket(0.73), 7);
        assert_eq!(stability_bucket(1.0), 10);
        assert_eq!(bytes_saved_bucket(50_000), "<100kb");
        assert_eq!(bytes_saved_bucket(5_000_000), "1mb-10mb");
        assert_eq!(bytes_saved_bucket(-9), "<100kb");
        assert_eq!(length_bucket(7), "<10");
        assert_eq!(length_bucket(200), "50-200");
        assert_eq!(length_bucket(201), ">200");
        // native compaction rate = applied/total, floored to 10pp
        assert_eq!(native_compaction_rate_bucket(0, 0), 0);
        assert_eq!(native_compaction_rate_bucket(0, 50), 0);
        assert_eq!(native_compaction_rate_bucket(1, 3), 30); // 33.3 → 30
        assert_eq!(native_compaction_rate_bucket(10, 10), 100);
        // os_family is always one of the closed set
        assert!(
            OS_FAMILIES.contains(&os_family().as_str()),
            "got {}",
            os_family()
        );
    }

    #[test]
    fn version_is_major_minor_or_dev() {
        let v = version_bucket();
        // debug test build ⇒ "dev"; a release build ⇒ "MAJOR.MINOR" (no patch).
        assert!(v == "dev" || (v.matches('.').count() == 1), "got {v}");
    }

    #[test]
    fn model_and_summarizer_families_coarsen() {
        // model_family now emits tier + major.minor (not just tier).
        assert_eq!(
            model_family(Some("claude-opus-4-5-20251101")),
            "claude-opus-4-5"
        );
        assert_eq!(model_family(Some("claude-sonnet-4-6")), "claude-sonnet-4-6");
        assert_eq!(model_family(Some("gpt-4o")), "other");
        assert_eq!(model_family(None), "other");
        assert_eq!(summarizer_family(Some("qwen3.5:4b")), "qwen3.5");
        assert_eq!(summarizer_family(Some("weird-model:13b")), "other");
        assert_eq!(summarizer_family(None), "none");
    }

    #[test]
    fn strategy_share_drops_unknown_and_small() {
        let bytes = vec![
            ("bloat_cap".to_owned(), 600_i64),
            ("sliding_window".to_owned(), 300),
            ("stale_reads".to_owned(), 100),
            ("not_a_strategy".to_owned(), 9_999), // unknown → dropped
        ];
        let share = strategy_share(&bytes);
        // total over KNOWN = 1000 → 60/30/10; the 10 (>=5) kept, unknown gone.
        assert_eq!(share.get("bloat_cap"), Some(&60));
        assert_eq!(share.get("sliding_window"), Some(&30));
        assert_eq!(share.get("stale_reads"), Some(&10));
        assert!(!share.contains_key("not_a_strategy"));
    }

    /// THE content-free guarantee: the serialized payload contains ONLY the
    /// allowed top-level keys, and strategy_share keys are all known strategies.
    #[test]
    fn payload_is_content_free() {
        let mut report = empty_report();
        report.total_in_bytes = 1000;
        report.total_out_bytes = 400;
        report.per_strategy_bytes = vec![("bloat_cap".to_owned(), 600)];
        report.per_strategy = vec![
            ("bloat_cap".to_owned(), 3),
            ("thinking_strip".to_owned(), 1),
        ];
        let config = Config::default();
        let payload = build_payload(
            &report,
            &[],
            &config,
            "0.1".to_owned(),
            "2026-06-06".to_owned(),
            test_dedup_token(),
        );
        let v: Value = serde_json::to_value(&payload).unwrap();
        let obj = v.as_object().unwrap();
        for key in obj.keys() {
            assert!(
                ALLOWED_KEYS.contains(&key.as_str()),
                "unexpected key: {key}"
            );
        }
        // every allowed key is present (no silent drops)
        assert_eq!(obj.len(), ALLOWED_KEYS.len());
        for s in payload.strategy_share.keys() {
            assert!(
                KNOWN_STRATEGIES.contains(&s.as_str()),
                "unknown strategy {s}"
            );
        }
        // spot-check a couple of derived buckets
        assert_eq!(payload.reduction_pct_bucket, 60); // 600/1000
        assert_eq!(payload.bytes_saved_bucket, "<100kb");
        assert_eq!(payload.summarizer_backend, "off");
        assert_eq!(payload.harness, "claude-code");
        // marginals: defaults are off; accumulator false when summarizer_backend=off
        assert_eq!(payload.schema_version, 1);
        assert!(!payload.accumulator_enabled);
        assert_eq!(payload.native_compaction_rate_bucket, 0);
        assert!(OS_FAMILIES.contains(&payload.os_family.as_str()));
        // every fired strategy is captured (sorted, known-only)
        assert_eq!(
            payload.strategies_fired,
            vec!["bloat_cap", "thinking_strip"]
        );
        // new fields: summarizer_size_bucket "none" (summarizer_backend=off), strategy_any_fired 0
        assert_eq!(payload.summarizer_size_bucket, "none");
        assert_eq!(payload.strategy_any_fired_pct_bucket, 0);
        assert!(SUMMARIZER_SIZE_BUCKETS.contains(&payload.summarizer_size_bucket.as_str()));
        // summarizer rate buckets: no attempts recorded → "none" install rate, 0 trigger
        assert_eq!(payload.summarizer_accept_rate_bucket, "none");
        assert_eq!(payload.summarizer_trigger_rate_bucket, 0);
        assert!(
            SUMMARIZER_ACCEPT_RATE_BUCKETS
                .contains(&payload.summarizer_accept_rate_bucket.as_str())
        );
        // §8C/Q4: no accepted summaries → backend_won = "off"
        assert_eq!(payload.summarizer_backend_won, "off");
        assert!(SUMMARIZER_BACKENDS.contains(&payload.summarizer_backend_won.as_str()));

        // a freshly built payload must pass the runtime guard
        guard_content_free(&v).expect("clean payload passes the guard");
    }

    #[test]
    fn summarizer_backend_won_compute() {
        // No accepted → "off"
        assert_eq!(summarizer_backend_won(0, 0), "off");
        // Only local accepted
        assert_eq!(summarizer_backend_won(3, 0), "local");
        // Only api accepted
        assert_eq!(summarizer_backend_won(0, 5), "api");
        // Local more
        assert_eq!(summarizer_backend_won(4, 2), "local");
        // Api more
        assert_eq!(summarizer_backend_won(1, 3), "api");
        // Tie: prefer local (deterministic)
        assert_eq!(summarizer_backend_won(2, 2), "local");
    }

    /// Tie-breaking and all-zero edge cases for `summarizer_backend_won`.
    #[test]
    fn summarizer_backend_won_tie_and_zero() {
        // All-zero → "off" (no summaries accepted at all)
        assert_eq!(summarizer_backend_won(0, 0), "off");
        // Tie (1, 1) → "local" (local preferred; deterministic, no privacy implication)
        assert_eq!(summarizer_backend_won(1, 1), "local");
        // Large equal tie → "local"
        assert_eq!(summarizer_backend_won(100, 100), "local");
        // Sanity: result is always in the closed SUMMARIZER_BACKENDS set
        for (l, a) in [(0, 0), (1, 1), (0, 5), (5, 0), (3, 7)] {
            let w = summarizer_backend_won(l, a);
            assert!(
                SUMMARIZER_BACKENDS.contains(&w.as_str()),
                "backend_won({l},{a}) = {w:?} not in SUMMARIZER_BACKENDS"
            );
        }
    }

    /// `build_payload` with `summarizer_backend = "api"`: verifies the api-engine
    /// path fills `summarizer_family` (api style) and `summarizer_size_bucket = "api"`,
    /// and that the payload passes the content-free guard end-to-end.
    #[test]
    fn build_payload_with_api_backend() {
        use figment::Figment;
        use figment::providers::{Format, Serialized, Toml};

        // Construct a Config with an API summarizer provider.
        let toml = r#"
[summarizer]
engine = "myprovider"

[[summarizer.providers]]
id          = "myprovider"
style       = "anthropic"
base_url    = "https://api.anthropic.com"
model       = "claude-haiku-4-5"
api_key_env = "ANTHROPIC_API_KEY"
"#;
        let config: trimwire::config::Config =
            Figment::from(Serialized::defaults(trimwire::config::Config::default()))
                .merge(Toml::string(toml))
                .extract()
                .expect("parse api provider config");

        let mut report = empty_report();
        report.total_in_bytes = 2000;
        report.total_out_bytes = 1000;
        report.summarizer_accepted = 5;
        report.summarizer_accepted_api = 5;

        let payload = build_payload(
            &report,
            &[],
            &config,
            "0.1".to_owned(),
            "2026-06-09".to_owned(),
            test_dedup_token(),
        );

        // summarizer_backend resolves to "api" for any provider id.
        assert_eq!(
            payload.summarizer_backend, "api",
            "api engine must map to summarizer_backend = \"api\""
        );
        // summarizer_family for api backend = the provider's style ("anthropic").
        assert_eq!(
            payload.summarizer_family, "anthropic",
            "api backend with anthropic style must produce summarizer_family = \"anthropic\""
        );
        // summarizer_size_bucket for api backend is always "api".
        assert_eq!(
            payload.summarizer_size_bucket, "api",
            "api backend must produce summarizer_size_bucket = \"api\""
        );
        // accumulator_enabled = true when backend != off.
        assert!(
            payload.accumulator_enabled,
            "accumulator_enabled must be true when summarizer_backend != off"
        );
        // summarizer_backend_won = "api" because only api summaries were accepted.
        assert_eq!(
            payload.summarizer_backend_won, "api",
            "with only api accepts, backend_won must be \"api\""
        );

        // The full payload must pass the content-free guard.
        let v = serde_json::to_value(&payload).unwrap();
        guard_content_free(&v).expect("api-backend payload passes the content-free guard");
    }

    #[test]
    fn summarizer_rate_buckets_compute() {
        // install rate = accepted/(accepted+rejected), floored to 10pp; errors excluded.
        assert_eq!(summarizer_accept_rate_bucket(0, 0), "none"); // no attempts
        assert_eq!(summarizer_accept_rate_bucket(3, 1), "70"); // 75% → 70
        assert_eq!(summarizer_accept_rate_bucket(1, 0), "100");
        assert_eq!(summarizer_accept_rate_bucket(0, 5), "0");
        // trigger rate = attempts/total_requests, floored to 10pp.
        assert_eq!(summarizer_trigger_rate_bucket(0, 0), 0);
        assert_eq!(summarizer_trigger_rate_bucket(0, 100), 0);
        assert_eq!(summarizer_trigger_rate_bucket(1, 4), 20); // 25% → 20
        assert_eq!(summarizer_trigger_rate_bucket(10, 10), 100);
    }

    #[test]
    fn guard_rejects_drift() {
        let config = Config::default();
        let payload = build_payload(
            &empty_report(),
            &[],
            &config,
            "0.1".to_owned(),
            "2026-06-06".to_owned(),
            test_dedup_token(),
        );
        let mut v = serde_json::to_value(&payload).unwrap();

        // an unexpected top-level key
        let mut extra = v.clone();
        extra.as_object_mut().unwrap().insert(
            "leaked_path".to_owned(),
            Value::String("/home/me/secret".to_owned()),
        );
        assert!(guard_content_free(&extra).is_err());

        // a raw/high-cardinality value smuggled into a closed enum field
        v.as_object_mut().unwrap().insert(
            "model_family".to_owned(),
            Value::String("claude-opus-4-5-20251101".to_owned()),
        );
        assert!(guard_content_free(&v).is_err());

        // a bad os_family value
        let mut bad_os = serde_json::to_value(&payload).unwrap();
        bad_os
            .as_object_mut()
            .unwrap()
            .insert("os_family".to_owned(), Value::String("plan9".to_owned()));
        assert!(guard_content_free(&bad_os).is_err());

        // a harness value outside the closed set
        let mut bad_harness = serde_json::to_value(&payload).unwrap();
        bad_harness.as_object_mut().unwrap().insert(
            "harness".to_owned(),
            Value::String("emacs-gptel".to_owned()),
        );
        assert!(guard_content_free(&bad_harness).is_err());

        // an out-of-range native compaction rate (not a multiple of 10)
        let mut bad_ncr = serde_json::to_value(&payload).unwrap();
        bad_ncr
            .as_object_mut()
            .unwrap()
            .insert("native_compaction_rate_bucket".to_owned(), Value::from(37));
        assert!(guard_content_free(&bad_ncr).is_err());

        // reduction_pct_bucket not a multiple of 5
        let mut bad_red = serde_json::to_value(&payload).unwrap();
        bad_red
            .as_object_mut()
            .unwrap()
            .insert("reduction_pct_bucket".to_owned(), Value::from(42));
        assert!(guard_content_free(&bad_red).is_err());

        // cache_hit_pct_bucket above range
        let mut bad_hit = serde_json::to_value(&payload).unwrap();
        bad_hit
            .as_object_mut()
            .unwrap()
            .insert("cache_hit_pct_bucket".to_owned(), Value::from(110));
        assert!(guard_content_free(&bad_hit).is_err());

        // cache_stability_bucket above range (0..=10)
        let mut bad_stab = serde_json::to_value(&payload).unwrap();
        bad_stab
            .as_object_mut()
            .unwrap()
            .insert("cache_stability_bucket".to_owned(), Value::from(11));
        assert!(guard_content_free(&bad_stab).is_err());

        // an unknown strategy name in strategy_share
        let mut bad_strat = serde_json::to_value(&payload).unwrap();
        bad_strat
            .get_mut("strategy_share")
            .unwrap()
            .as_object_mut()
            .unwrap()
            .insert("exfiltrate".to_owned(), Value::from(50));
        assert!(guard_content_free(&bad_strat).is_err());

        // a KNOWN strategy with an out-of-range share value (not a 5pp bucket)
        let mut bad_share_val = serde_json::to_value(&payload).unwrap();
        bad_share_val
            .get_mut("strategy_share")
            .unwrap()
            .as_object_mut()
            .unwrap()
            .insert("bloat_cap".to_owned(), Value::from(3));
        assert!(guard_content_free(&bad_share_val).is_err());

        // an unknown strategy name in strategies_fired
        let mut bad_fired = serde_json::to_value(&payload).unwrap();
        bad_fired
            .get_mut("strategies_fired")
            .unwrap()
            .as_array_mut()
            .unwrap()
            .push(Value::String("exfiltrate".to_owned()));
        assert!(guard_content_free(&bad_fired).is_err());
    }

    #[test]
    fn strategies_fired_keeps_known_sorted_drops_unknown() {
        let per_strategy = vec![
            ("thinking_strip".to_owned(), 2_u64),
            ("bloat_cap".to_owned(), 5),
            ("not_a_strategy".to_owned(), 9), // unknown → dropped
            ("stale_reads".to_owned(), 0),    // count 0 → dropped
        ];
        assert_eq!(
            strategies_fired(&per_strategy),
            vec!["bloat_cap", "thinking_strip"]
        );
    }

    #[test]
    fn summarizer_size_bucket_maps_tags() {
        // "none" when backend is "off"
        assert_eq!(summarizer_size_bucket("off", Some("qwen3.5:4b")), "none");
        assert_eq!(summarizer_size_bucket("off", None), "none");
        // "api" when backend is "api" (parameter count meaningless for cloud models)
        assert_eq!(
            summarizer_size_bucket("api", Some("claude-sonnet-4-6")),
            "api"
        );
        assert_eq!(summarizer_size_bucket("api", None), "api");
        // ≤2b
        assert_eq!(summarizer_size_bucket("default", Some("qwen3.5:2b")), "≤2b");
        assert_eq!(
            summarizer_size_bucket("default", Some("qwen3.5:0.8b")),
            "≤2b"
        );
        // 3-4b
        assert_eq!(
            summarizer_size_bucket("default", Some("qwen3.5:4b")),
            "3-4b"
        );
        // quantization suffix stripped: "4b-q8_0" → "4b" → 3-4b
        assert_eq!(
            summarizer_size_bucket("default", Some("qwen3.5:4b-q8_0")),
            "3-4b"
        );
        // 5-9b (5..=9)
        assert_eq!(
            summarizer_size_bucket("default", Some("qwen3.5:9b")),
            "5-9b"
        );
        assert_eq!(
            summarizer_size_bucket("default", Some("qwen3.5:5b")),
            "5-9b"
        );
        // ≥10b
        assert_eq!(
            summarizer_size_bucket("default", Some("qwen3.5:14b")),
            "≥10b"
        );
        // no parseable suffix → "unknown"
        assert_eq!(summarizer_size_bucket("default", Some("weird")), "unknown");
        assert_eq!(
            summarizer_size_bucket("default", Some("weird:nosize")),
            "unknown"
        );
        // None tag → "unknown"
        assert_eq!(summarizer_size_bucket("default", None), "unknown");
    }

    #[test]
    fn guard_rejects_bad_summarizer_size_bucket_and_strategy_fired_pct() {
        let config = Config::default();
        let payload = build_payload(
            &empty_report(),
            &[],
            &config,
            "0.1".to_owned(),
            "2026-06-06".to_owned(),
            test_dedup_token(),
        );

        // bad summarizer_size_bucket value
        let mut bad_ssb = serde_json::to_value(&payload).unwrap();
        bad_ssb.as_object_mut().unwrap().insert(
            "summarizer_size_bucket".to_owned(),
            Value::String("giant".to_owned()),
        );
        assert!(guard_content_free(&bad_ssb).is_err());

        // strategy_any_fired_pct_bucket not a multiple of 10
        let mut bad_fired = serde_json::to_value(&payload).unwrap();
        bad_fired
            .as_object_mut()
            .unwrap()
            .insert("strategy_any_fired_pct_bucket".to_owned(), Value::from(37));
        assert!(guard_content_free(&bad_fired).is_err());

        // strategy_any_fired_pct_bucket above 100
        let mut bad_range = serde_json::to_value(&payload).unwrap();
        bad_range
            .as_object_mut()
            .unwrap()
            .insert("strategy_any_fired_pct_bucket".to_owned(), Value::from(110));
        assert!(guard_content_free(&bad_range).is_err());

        // summarizer_accept_rate_bucket: a non-bucket value ("75" isn't a 10pp step)
        let mut bad_sar = serde_json::to_value(&payload).unwrap();
        bad_sar.as_object_mut().unwrap().insert(
            "summarizer_accept_rate_bucket".to_owned(),
            Value::String("75".to_owned()),
        );
        assert!(guard_content_free(&bad_sar).is_err());

        // summarizer_trigger_rate_bucket: not a multiple of 10
        let mut bad_trig = serde_json::to_value(&payload).unwrap();
        bad_trig
            .as_object_mut()
            .unwrap()
            .insert("summarizer_trigger_rate_bucket".to_owned(), Value::from(37));
        assert!(guard_content_free(&bad_trig).is_err());

        // dedup_token: too short / wrong chars
        let mut bad_dt = serde_json::to_value(&payload).unwrap();
        bad_dt
            .as_object_mut()
            .unwrap()
            .insert("dedup_token".to_owned(), Value::String("abc".to_owned()));
        assert!(guard_content_free(&bad_dt).is_err());

        // dedup_token: uppercase hex not accepted
        let mut bad_dt2 = serde_json::to_value(&payload).unwrap();
        bad_dt2
            .as_object_mut()
            .unwrap()
            .insert("dedup_token".to_owned(), Value::String("A".repeat(64)));
        assert!(guard_content_free(&bad_dt2).is_err());
    }

    /// A deterministic 64-hex dedup token for use in unit tests (avoids real
    /// file I/O while satisfying the guard's 64-lowercase-hex requirement).
    fn test_dedup_token() -> String {
        "a".repeat(64)
    }

    #[test]
    fn model_family_emits_major_minor() {
        // Full dated id → tier + major.minor only.
        assert_eq!(
            model_family(Some("claude-opus-4-5-20251101")),
            "claude-opus-4-5"
        );
        assert_eq!(model_family(Some("claude-sonnet-4-6")), "claude-sonnet-4-6");
        assert_eq!(
            model_family(Some("claude-haiku-3-5-20241022")),
            "claude-haiku-3-5"
        );
        // Claude Code's 1M-context runtime marker must be stripped, not break the
        // minor-version parse (regression: `[1m]` → miscoarsened to "other").
        assert_eq!(model_family(Some("claude-opus-4-8[1m]")), "claude-opus-4-8");
        assert_eq!(
            model_family(Some("claude-sonnet-4-6[1m]")),
            "claude-sonnet-4-6"
        );
        // Non-Claude model → other.
        assert_eq!(model_family(Some("gpt-4o")), "other");
        assert_eq!(model_family(None), "other");
        // Emitted values pass the shape validator.
        assert!(is_valid_model_family("claude-opus-4-5"));
        assert!(is_valid_model_family("claude-sonnet-4-6"));
        assert!(is_valid_model_family("other"));
        assert!(!is_valid_model_family("claude-opus")); // old format rejected
        assert!(!is_valid_model_family("claude-opus-4-5-20251101")); // dated suffix rejected
    }

    #[test]
    fn dedup_token_is_64_hex() {
        // compute_dedup_token with a known key and day → deterministic 64-hex output.
        let token = compute_dedup_token(&"a".repeat(32), "2026-06-09").unwrap();
        assert_eq!(token.len(), 64);
        assert!(token.chars().all(|c| matches!(c, '0'..='9' | 'a'..='f')));
        // different day → different token (not the same string)
        let token2 = compute_dedup_token(&"a".repeat(32), "2026-06-10").unwrap();
        assert_ne!(token, token2);
        // same key + same day → same token (idempotent)
        let token3 = compute_dedup_token(&"a".repeat(32), "2026-06-09").unwrap();
        assert_eq!(token, token3);
    }

    #[test]
    fn max_session_length_bucket_is_correct() {
        use trimwire::ledger::SessionRow;
        // Helper to make a minimal SessionRow with a given request count.
        let sr = |n: u64| SessionRow {
            session_id: String::new(),
            last_day: String::new(),
            model: None,
            requests: n,
            in_bytes: 0,
            out_bytes: 0,
            input_tokens: 0,
            cache_read_input_tokens: 0,
            cache_creation_input_tokens: 0,
        };
        assert_eq!(max_requests(&[sr(5), sr(200), sr(30)]), 200);
        assert_eq!(
            length_bucket(max_requests(&[sr(5), sr(200), sr(30)])),
            "50-200"
        );
        assert_eq!(max_requests(&[]), 0);

        let config = Config::default();
        let sessions = vec![sr(5), sr(200), sr(30)];
        let payload = build_payload(
            &empty_report(),
            &sessions,
            &config,
            "0.1".to_owned(),
            "2026-06-09".to_owned(),
            test_dedup_token(),
        );
        assert_eq!(payload.max_session_length_bucket, "50-200");
        // Guard must pass.
        let v = serde_json::to_value(&payload).unwrap();
        guard_content_free(&v).expect("payload with max_session_length_bucket passes guard");
    }

    /// `median_requests` uses the LOWER median on even-length arrays.
    #[test]
    fn median_requests_lower_median() {
        use trimwire::ledger::SessionRow;
        let sr = |n: u64| SessionRow {
            session_id: String::new(),
            last_day: String::new(),
            model: None,
            requests: n,
            in_bytes: 0,
            out_bytes: 0,
            input_tokens: 0,
            cache_read_input_tokens: 0,
            cache_creation_input_tokens: 0,
        };
        // Odd length: standard median.
        assert_eq!(median_requests(&[sr(1), sr(3), sr(5)]), 3);
        // Even length (2 elements): lower median = first after sort → 3, not 7.
        assert_eq!(median_requests(&[sr(3), sr(7)]), 3);
        // Even length (4 elements): lower median = element at index (4-1)/2 = 1.
        assert_eq!(median_requests(&[sr(1), sr(2), sr(3), sr(4)]), 2);
        // Single element: itself.
        assert_eq!(median_requests(&[sr(42)]), 42);
        // Empty: 0.
        assert_eq!(median_requests(&[]), 0);
        // Unsorted input: must sort before picking median.
        assert_eq!(median_requests(&[sr(9), sr(1), sr(5)]), 5);
    }
}
