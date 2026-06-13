//! Opt-in context compaction via a local or cloud summarizer.
//!
//! Always compiled; active only when `[summarizer] engine` is not `"model-free"` in config
//! (disabled by default). Summarizes the OLD prunable slice of a session via a
//! local ollama server or a cloud API provider of your choice.
//! See `docs/SUMMARIZER.md` for the full user guide and privacy posture.
//!
//! **Layer:** this is NOT a strategy. Strategies are pure, no-I/O functions over
//! `messages[]`; this does network I/O, so — like reprune — it is a gateway/
//! reprune-layer concern. The model is called from the async gateway; the
//! resulting summary is cached in `PruneState` and replayed synchronously by
//! reprune on subsequent turns (mirroring `apply_thinking_removals`).
//!
//! **Never load-bearing.** Every entry point returns a `Result` whose error path
//! the caller maps to "skip compaction, forward the model-free-pruned body".
//! The proxy never blocks, hangs, or corrupts a request because the local model
//! is down, slow, or produces garbage.

use std::time::Duration;

use http_body_util::{BodyExt, Full};
use hyper::body::Bytes;
use serde_json::Value;

#[cfg(test)]
use crate::config::SummarizerConfig;
use crate::config::SummarizerLocalConfig;

pub mod api;
pub mod harm_check;
pub mod probe;
pub mod slice;

/// Why a compaction attempt produced no usable summary. Every variant maps to
/// the same caller behavior: **skip compaction, keep the model-free output.**
/// The local model is best-effort; none of these is ever surfaced to Claude Code.
#[derive(Debug)]
pub enum CompactorError {
    /// The local server could not be reached (connection refused, DNS, I/O).
    Unreachable(String),
    /// The call exceeded `timeout_secs`.
    Timeout,
    /// The server answered with a non-2xx status.
    HttpStatus(u16),
    /// The response body was not the expected JSON shape.
    Malformed(String),
    /// The model returned an empty (or whitespace-only) summary.
    EmptyResponse,
}

impl std::fmt::Display for CompactorError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CompactorError::Unreachable(e) => write!(f, "summarizer backend unreachable: {e}"),
            CompactorError::Timeout => write!(f, "summarizer backend timed out"),
            CompactorError::HttpStatus(s) => write!(f, "summarizer backend HTTP {s}"),
            CompactorError::Malformed(e) => write!(f, "summarizer response malformed: {e}"),
            CompactorError::EmptyResponse => write!(f, "summarizer returned an empty summary"),
        }
    }
}

impl std::error::Error for CompactorError {}

/// The summarization SYSTEM instruction — the council-locked FACTS-FIRST free-form
/// harness (see `benchmark/model_bench.sh` `SYSTEM_FREEFORM`, the 5-slice blind
/// gut-read, and the P0b harm gate that validated it on qwen3.5:4b). Verbatim-copy
/// rule first, a hard ≤25% length budget, anti-preamble, and "NEXT = what was left
/// OPEN" — these replaced the old "a long summary is fine" wording that made small
/// models ramble + echo. Content-free so the system message is deterministic; only
/// the user excerpt varies.
pub const SUMMARY_SYSTEM_PROMPT: &str = "RULES (violations are FAILURES):\n\
1. Copy VERBATIM, character-for-character, never paraphrase: every file path, error code (error[E0277]-style), identifier, port, env var, version, command, number.\n\
2. Output MUST be shorter than the excerpt — aim for ≤25% of its length. Prefer shorter: the main model can re-read files for detail. Drop tool-call boilerplate, progress/log spam, repeated scaffolding, and exploration that led nowhere.\n\
3. Capture the state at the END of the excerpt: NEXT must be what was actually left open/in-progress, not work already completed. If the excerpt ends mid-assistant-turn (the last turn is incomplete), NEXT is what that turn was in the middle of doing.\n\
4. No preamble or closing remarks — start directly with GOAL. Do not reproduce raw tool output. Do not invent anything.\n\
5. Do NOT mark work finished unless the excerpt explicitly shows it completed. Never write 'fixed', 'done', 'resolved', 'implemented', or 'complete' for an item still open or in progress. For a multi-item task (a review queue, a finding list, a checklist), state progress as 'N of M complete' and list EVERY still-open item in NEXT. When unsure whether something finished, treat it as still open.\n\
6. Before writing NEXT, scan the last ~5 assistant turns for any task ANNOUNCED or STARTED with no completing edit/output/confirmation in the excerpt (e.g. 'now I'll…', 'next step…', a file only located/grepped but not yet edited, a 'Task N' just begun): list each in NEXT as still-open — an announced-or-only-located task is OPEN, never done. Do NOT put already-finished work (completed edits, commits, done task-status updates) in NEXT.\n\n\
You are a coding-session compactor. Summarize the excerpt into ONLY these sections (omit any that are empty), terse:\n\
GOAL: <one line — what the coding SESSION is trying to accomplish, NOT a description of this summarization task>\n\
FILES: <ONLY paths an edit/write/create/move actually targeted in this excerpt, each copied verbatim from the text, one per line; never a file merely read, grepped, or referenced; if you cannot copy a path exactly as written, omit it rather than guess a directory or extension>\n\
DECIDED: <decisions made, verbatim identifiers inline>\n\
ERRORS: <error codes/messages, verbatim, or omit>\n\
FACTS: <other exact identifiers, numbers, ports, env vars, versions, commands>\n\
NEXT: <what was about to happen / left open at the end>";

/// Approved summarization models (real-slice blind gut-read + Disagree-Seeking):
/// - `qwen3.5:4b` — DEFAULT; passes cost (P0a) + harm (P0b).
/// - `qwen3.5:9b` — PRO upgrade (2026-06-04): same family, 3/3 clean on diverse real
///   slices, no hallucination / correct NEXT / no false-done; ~6.1 GB resident (fits a
///   13 GB box one-at-a-time). Opt-in for users with the RAM; default stays 4b.
/// - `qwen3.5:4b-q8_0` — higher-fidelity MEDIUM upgrade (~4.8 GB resident): same model,
///   Q8 quant. On 3 new harder real slices it measurably beat the Q4 default — it avoided
///   a FALSE-DONE that Q4 committed (Q4 claimed a gate "reactivated" when the session only
///   located it); its only defect was a recoverable path-format slip. Default stays Q4
///   (lighter); promote to default is a maintainer call (RAM +~1.7 GB).
/// - `qwen3.5:2b` — lighter opt-down that FAILS the harm gate (drops load-bearing
///   facts); allowed but [`WARN_MODELS`] flags it. Also the one tier that mildly
///   REGRESSED under the 2026-06-04 FILES-field hardening (the longer FILES descriptor
///   over-triggers it → more inflation on b_s1/b_s3 + a false-done; the 3 approved tiers
///   improved). Consistent with its degraded status; see harm_gate/RESULTS.md.
///
/// Anything else is unvalidated (the guard warns); proven-harmful tags are in
/// [`DISQUALIFIED_MODELS`]. See `benchmark/results/model_tiers/RESULTS.md`.
pub const APPROVED_MODELS: &[&str] = &["qwen3.5:4b", "qwen3.5:4b-q8_0", "qwen3.5:9b", "qwen3.5:2b"];

/// Models proven harmful by the blind real-slice gut-read (+ sequential
/// Disagree-Seeking) — never summarize with these even if a stale config names one:
/// - `granite4.1:3b` / `granite4.1:8b` — the granite FAMILY false-done pattern
///   (claims "session complete / no further open items" mid-task; granite4.1:8b
///   also fabricated a `src/lib/server/drizzle/` path prefix → a resuming agent
///   stops early / chases a non-existent file). The 8b recurrence triggered the
///   family-disqualify rule.
/// - `qwen2.5-coder:3b` — hallucinates completed work.
/// - `ministral-3:3b` — fabricated an identifier (`era1_garage`) + collapsed a
///   complex slice to a lone GOAL line (content collapse).
/// - `gemma3:4b` — fabricated an action that never happened (`fake backend in
///   db:generate`) + false-done (claimed snapshots reconstructed) on one slice; a
///   misdirected NEXT (execution tasks vs. plan-editing) on another. Not a default
///   alternative.
/// - `qwen3.5:0.8b` — LIGHT-tier probe: too small (3/3 FAIL — fabricated a
///   `getLoreForEra` change, false-done'd "0 BLOCKER issues" when the raw had blockers,
///   dropped committed tasks). It COMMITS (fabricates/false-dones), unlike `qwen3.5:2b`
///   which only OMITS — so it's refused, not merely warned. No viable LIGHT tier exists;
///   `qwen3.5:2b` stays the only (warned) lighter option.
pub const DISQUALIFIED_MODELS: &[&str] = &[
    "granite4.1:3b",
    "granite4.1:8b",
    "qwen2.5-coder:3b",
    "ministral-3:3b",
    "gemma3:4b",
    "qwen3.5:0.8b",
];

/// APPROVED-but-WEAKER models: allowed (the user's explicit opt-in choice) but they
/// FAILED the planted-fact harm gate, so the gateway warns once per re-summarization
/// that fidelity is degraded. `qwen3.5:2b` dropped load-bearing facts (synthetic 75%
/// / real 83%) — it's a RAM opt-down, not an equal to the 4b default. Distinct from
/// the "unverified tag" warning (a model that was never gated at all).
pub const WARN_MODELS: &[&str] = &["qwen3.5:2b"];

/// Disqualified *families*: every tag with one of these prefixes is refused, not
/// just the sizes the gut-read happened to test. Only `granite4.1` is family-level
/// — the documented family-disqualify rule (the whole granite4.1 family false-dones;
/// the granite4.1:8b recurrence triggered it). The other [`DISQUALIFIED_MODELS`]
/// entries are size-specific (e.g. `qwen3.5:0.8b` is refused but `qwen3.5:4b` is
/// APPROVED), so they are NOT families.
pub const DISQUALIFIED_FAMILIES: &[&str] = &["granite4.1:"];

/// Is `tag` a model the blind gut-read proved harmful? True for an exact tag in
/// [`DISQUALIFIED_MODELS`], OR any tag in a [`DISQUALIFIED_FAMILIES`] family — so a
/// variant the list never enumerated (`granite4.1:latest`, `granite4.1:2b`) is still
/// refused, not silently summarized-with. Prefer this over a bare
/// `DISQUALIFIED_MODELS.contains`.
pub fn is_disqualified(tag: &str) -> bool {
    DISQUALIFIED_MODELS.contains(&tag) || DISQUALIFIED_FAMILIES.iter().any(|f| tag.starts_with(f))
}

/// Assemble the one-shot summarization USER message (the excerpt). The
/// FACTS-FIRST rules live in [`SUMMARY_SYSTEM_PROMPT`], sent as the chat system
/// message by [`call_model`]; only the excerpt varies here.
pub fn build_prompt(slice_text: &str) -> String {
    format!(
        "<excerpt>\n{slice_text}\n</excerpt>\n\nCompact the excerpt above into the sections below."
    )
}

/// Normalize a load-bearing fact for harm-gate matching: lowercase + treat hyphen
/// and underscore as equal (small models render `max-retries`/`max_retries`
/// interchangeably — the FACT is the identifier, not its separator style). Used by
/// `examples/compaction_harm.rs` and guarded by a deterministic CI test.
pub fn normalize_fact(s: &str) -> String {
    s.to_lowercase().replace('-', "_")
}

/// Count how many `needles` survive in `summary` under [`normalize_fact`] matching.
/// Returns `(kept, total)`. This is the harm-gate's retention counter, factored out
/// of the example so a no-live-ollama CI test can pin its logic.
pub fn fact_retention(summary: &str, needles: &[&str]) -> (usize, usize) {
    let hay = normalize_fact(summary);
    let kept = needles
        .iter()
        .filter(|n| hay.contains(&normalize_fact(n)))
        .count();
    (kept, needles.len())
}

/// Absolute hard ceiling on the configurable local `num_ctx` — bounds the KV-cache
/// allocation so a stray huge `summarizer.local.max_num_ctx` can't OOM the box. The
/// actual local num_ctx is `summarizer.local.max_num_ctx` (default 25600 ≈ 64 KB slice),
/// clamped to this.
const MAX_NUM_CTX_CEILING: u64 = 131_072;

/// Flat ceiling on `num_predict` (the local summary's max generated tokens). A
/// summary is meant to be a SMALL fraction of the slice; real ones run ~150-700
/// tokens. ~4K tokens (~16 KB) is generous headroom over that while capping the
/// `num_ctx/4` proportional formula so a large window can't license a runaway.
const MAX_NUM_PREDICT: u64 = 4_096;

/// Local slice budget (chars) for a given `num_ctx`: `≈ num_ctx × 2.5`, minus headroom
/// for the system prompt + wrapper. Above this, ollama truncates the prompt head and the
/// summary silently misrepresents the oldest turns; `cap_slice_start` narrows to fit.
/// Derived from `summarizer.local.max_num_ctx` so a bigger configured num_ctx → a bigger
/// local slice (qwen3.5:4b held 92% at ~117 KB / num_ctx 40000 in testing).
/// Max generated tokens for the local summary. Proportional to the window
/// (`num_ctx/4`) for small contexts, FLAT-capped at [`MAX_NUM_PREDICT`] so a large
/// `num_ctx` (e.g. 40000 → 10000) can't license a runaway summary that overshoots
/// the ≤25% rule, burns generation time, and gets rejected by `summary_is_smaller`
/// anyway. Real summaries run ~150-700 tokens, so the cap only ever bites a
/// pathological run — it can never truncate a healthy summary. Floor 200 keeps
/// tiny slices usable.
fn num_predict_for(num_ctx: u64) -> u64 {
    (num_ctx / 4).clamp(200, MAX_NUM_PREDICT)
}

fn local_char_budget(max_num_ctx: u64) -> usize {
    ((max_num_ctx.clamp(4096, MAX_NUM_CTX_CEILING) as usize) * 5 / 2).saturating_sub(2_000)
}

/// Per-segment slice budget for an API-ONLY chain (no local engine that could run).
/// Cloud models have 100K+ context windows and no ollama-style KV-cache OOM risk, so
/// the summary can cover far more OLD content per pass (~128 KB ≈ 32K tokens, well
/// within Haiku/GLM/GPT-4o-mini windows). This is what lets the summary — not lossy
/// model-free — own the old region (§15). Users can override via `slice_char_budget`.
const API_SLICE_CHAR_BUDGET: usize = 131_072;

/// Absolute cap on how much a relaxed-ratio summary may GROW a region beyond what
/// lossy model-free achieved (§15 S2). Even at a high `accept_ratio`, a summary that
/// would add more than this many bytes to a (possibly tiny) region is rejected — so a
/// verbose/runaway summary can never inflate the body unboundedly.
const MAX_SUMMARY_GROWTH_BYTES: usize = 16_384;

/// The serialized-slice budget to use for THIS config. An explicit
/// `slice_char_budget` wins. Otherwise: if the LOCAL engine can run for this request
/// (primary OR fallback), the slice must fit ollama's num_ctx → the small num_ctx-safe
/// cap; only an API-ONLY chain gets the large budget (the slice is serialized once and
/// fed to whichever engine actually runs, so we size for the smallest-window engine in
/// the chain).
pub fn effective_char_budget(s: &crate::config::SummarizerConfig) -> usize {
    if let Some(b) = s.slice_char_budget {
        return b;
    }
    let local_in_chain = s.engine == "local" || s.fallback.iter().any(|f| f == "local");
    if local_in_chain {
        local_char_budget(s.local.max_num_ctx)
    } else {
        API_SLICE_CHAR_BUDGET
    }
}

/// Phase 2: skip the model entirely when the candidate slice is mostly bulky
/// tool_result OUTPUT — the deterministic strategies already prune that, so a prose
/// summary can't beat model-free pruning (`summary_is_smaller` would reject it). The
/// local model's value is reasoning-dense slices, not log dumps. Cheap pre-spawn gate.
const MAX_TOOL_FRACTION: f64 = 0.6;

/// Density-aware `select_slice` FALLBACK threshold: when the widest slice exceeds
/// `MAX_TOOL_FRACTION` (would be skipped), only rescue a denser sub-window that is
/// MAJORITY reasoning (≤ this). Stricter than the skip gate on purpose — a safety
/// margin. The harm gate found that marginal sub-windows (~0.55-0.6 tool) tend to
/// straddle a prose→tool task boundary and the summarizer drops the early prose;
/// requiring a clearly-reasoning window keeps the rescue faithful + additive.
const RESCUE_TOOL_FRACTION: f64 = 0.5;

/// Process-wide cap on concurrently-running local-model summaries. Production = 1
/// (the local box runs ONE model at a time; ollama serializes execution anyway, this
/// just bounds spawned tasks). Tests use a high count so the process-global permit
/// doesn't make parallel `#[tokio::test]`s contend (a strict-N exhaustion test would
/// need single-threaded/serial execution).
#[cfg(not(test))]
const MAX_CONCURRENT_SUMMARIES: usize = 1;
#[cfg(test)]
const MAX_CONCURRENT_SUMMARIES: usize = 1024;

static SUMMARY_SEMAPHORE: std::sync::OnceLock<tokio::sync::Semaphore> = std::sync::OnceLock::new();

fn summary_semaphore() -> &'static tokio::sync::Semaphore {
    SUMMARY_SEMAPHORE.get_or_init(|| tokio::sync::Semaphore::new(MAX_CONCURRENT_SUMMARIES))
}

/// Fraction of a slice's content bytes that are tool_result OUTPUT (0.0–1.0).
/// Fraction (0–1) of a slice's measured bytes that live in `tool_result` blocks.
/// Pub so the harm/measurement example (`examples/density_harm`) scores against
/// the EXACT production function instead of a drifting copy.
pub fn tool_result_fraction(slice: &[Value]) -> f64 {
    let (mut total, mut tool) = (0usize, 0usize);
    for m in slice {
        match m.get("content") {
            Some(Value::String(s)) => total += s.len(),
            Some(Value::Array(blocks)) => {
                for b in blocks {
                    // Measure raw string length where the value is a string (so a
                    // string tool_result isn't inflated by JSON quotes vs a text
                    // block); fall back to serialized length for array/object values.
                    let val_len = |v: &Value| {
                        v.as_str()
                            .map(str::len)
                            .unwrap_or_else(|| v.to_string().len())
                    };
                    let len = b
                        .get("text")
                        .and_then(Value::as_str)
                        .map(str::len)
                        .or_else(|| b.get("content").map(&val_len))
                        .or_else(|| b.get("input").map(&val_len))
                        .unwrap_or(0);
                    total += len;
                    if b.get("type").and_then(Value::as_str) == Some("tool_result") {
                        tool += len;
                    }
                }
            }
            _ => {}
        }
    }
    if total == 0 {
        0.0
    } else {
        tool as f64 / total as f64
    }
}

/// Conservative ollama `num_ctx` for a prompt of `prompt_len` BYTES: `len/2.5`
/// (over-estimates tokens for English/code so we don't under-allocate and silently
/// truncate — the original bug), floored at 4096, capped at `max_num_ctx` (the
/// configured `summarizer.local.max_num_ctx`, itself clamped to `MAX_NUM_CTX_CEILING`,
/// the KV-cache OOM guard). Above the cap, ollama truncates the prompt head (see the
/// debug log in `maybe_spawn_summarization`); the slice cap is the real fix for that.
fn num_ctx_for(prompt_len: usize, max_num_ctx: u64) -> u64 {
    ((prompt_len as f64 / 2.5).ceil() as u64)
        .clamp(4096, max_num_ctx.clamp(4096, MAX_NUM_CTX_CEILING))
}

/// Is the summary worth keeping? The local model is for OLD content the
/// deterministic strategies CAN'T compress — it must never make the body bigger
/// than what model-free pruning already achieves on the same region (the draft's
/// "complementary, not replacement" thesis; on tool-output-heavy sessions
/// model-free routinely beats a verbose prose summary). Returns true iff the
/// summary message-pair is strictly smaller than the slice AFTER model-free
/// pruning IN FULL CONTEXT — pruning the whole array (not the slice in isolation,
/// which under-prunes because the old turns look "recent" within the slice) and
/// measuring `messages[start..end]` there. No strategy adds/removes whole
/// messages, so the indices still line up after pruning.
///
/// NOTE: this is a SIZE gate, NOT a quality gate. A shorter summary always passes,
/// so a too-aggressive serialization (e.g. a tiny tool_result cap) can make a
/// lower-context summary pass more easily — summary FIDELITY is the harm gate's job
/// (`examples/compaction_harm.rs`), not this function's.
///
/// §15 S2 — FIDELITY PRIORITY: `summarizer.accept_ratio` (default 1.0) relaxes the
/// strict "must be smaller" rule. At 1.0 this is the original gate (summary <
/// model-free). Above 1.0 (e.g. 1.5, recommended for strong API engines) it accepts a
/// higher-fidelity summary up to `accept_ratio ×` the LOSSY model-free size — because
/// a clean prose summary is far more useful to the model than elision markers, even at
/// a modest byte premium — bounded by an absolute growth cap so a verbose summary can
/// never inflate a small region unboundedly.
pub fn summary_is_smaller(
    summary_msgs: &[Value],
    messages: &[Value],
    start: usize,
    end: usize,
    cfg: &crate::config::Config,
) -> bool {
    if start >= end || end > messages.len() {
        return false;
    }
    let mut full = messages.to_vec();
    let _ = crate::strategies::run(&mut full, cfg);
    let mf_slice = crate::strategies::serialized_len(&full[start..end]);
    let summ = crate::strategies::serialized_len(summary_msgs);
    let ratio = cfg.summarizer.accept_ratio;
    if ratio <= 1.0 {
        // Default / strict: the summary must beat lossy model-free outright.
        return summ < mf_slice;
    }
    // Relaxed: accept up to ratio× model-free, capped by an absolute growth bound.
    let ratio_cap = (mf_slice as f64 * ratio) as usize;
    let abs_cap = mf_slice.saturating_add(MAX_SUMMARY_GROWTH_BYTES);
    summ <= ratio_cap.min(abs_cap)
}

/// Decide whether this request warrants a (re)summarization and, if so, spawn the
/// model call in the BACKGROUND. The current request is NEVER blocked: the
/// resulting summary is cached in `PruneState` and replayed by reprune starting
/// on the next turn. Best-effort end-to-end — any failure leaves the model-free
/// pruning untouched.
///
/// Gating (all under the per-session lock, held only for the sync decision —
/// never across the await): the feature is enabled, the body exceeds
/// `trigger_bytes`, a checkpoint exists (so the stable region is known), a fresh
/// slice is worth summarizing, the un-summarized delta has grown by
/// `resummarize_after_bytes` since the cached summary (batching), and no task is
/// already in flight.
/// The summarizer engages ONLY when BOTH the engine is not `"model-free"` AND
/// `reprune.enabled` is true: reprune carries the cached summary across turns, so
/// without it the feature is a SILENT NO-OP. Encodes that coupling in one testable
/// place — the gateway gates on it and `trimwire doctor` consults it to warn a user
/// who configured a summarizer but left reprune off.
pub fn engages(config: &crate::config::Config) -> bool {
    config.summarizer.engine != "model-free" && config.reprune.enabled
}

/// Run the engine cascade for one summarization call.
///
/// Resolves the ordered token chain from `summarizer.engine` + `summarizer.fallback`,
/// de-duplicated, with `"model-free"` as the IMPLICIT terminal (always last;
/// appearing earlier truncates the chain because `"model-free"` returns `None`
/// immediately).
///
/// Token resolution (string-keyed):
/// - `"model-free"` → terminal, return `None` (no summary, model-free stands).
/// - `"local"`      → [`call_model`] with the local ollama config.
/// - any other str  → find the [`SummarizerProviderConfig`] with matching `id`
///   and call [`api::call_api`]; if the id is not found, log a
///   warning and fall through to the next token.
///
/// Fall-through to the NEXT engine ONLY on a real `Err` (network/timeout/
/// non-2xx/empty/malformed) or an unresolved id.  On `Ok(text)` stop immediately
/// and return `Some(text)` — even if that text later fails the `summary_is_smaller`
/// size-gate (that's the caller's concern; a too-big summary is a clean
/// "no install"; it does NOT trigger the next, possibly paid, engine).
///
/// Each fall-through is logged at `warn!` / `debug!` — content-free (no
/// slice or summary text in logs).
///
/// Returns `Some((summary_text, winning_engine))` where `winning_engine` is
/// `"local"` or `"api"` (the coarse backend kind of the engine that succeeded),
/// or `None` if the cascade reached the model-free terminal.
pub(crate) async fn run_cascade(
    summarizer: &crate::config::SummarizerConfig,
    prompt: String,
) -> Option<(String, &'static str)> {
    // Build the de-duplicated ordered chain with "model-free" as implicit terminal.
    let mut chain: Vec<String> = std::iter::once(summarizer.engine.clone())
        .chain(summarizer.fallback.iter().cloned())
        .collect();

    // Truncate at the first "model-free" that appears: it's the terminal and
    // any tokens after it would never be reached.
    if let Some(pos) = chain.iter().position(|e| e == "model-free") {
        chain.truncate(pos + 1);
    }

    // De-duplicate (preserve first-occurrence order).
    let mut seen: Vec<String> = Vec::with_capacity(chain.len());
    chain.retain(|e| {
        if seen.iter().any(|s| s == e) {
            false
        } else {
            seen.push(e.clone());
            true
        }
    });

    // Ensure "model-free" is always the last entry (implicit terminal).
    if chain.last().map(|s| s.as_str()) != Some("model-free") {
        chain.push("model-free".to_owned());
    }

    for token in &chain {
        match token.as_str() {
            "model-free" => {
                // Terminal: model-free pruning already applied; no summary.
                return None;
            }
            "local" => {
                // Skip a disqualified local model (the blind gut-read proved it
                // drops/hallucinates load-bearing facts) — fall through to the next
                // engine rather than summarize harmfully. This is why a disqualified
                // local is NOT a pre-spawn abort: an API primary/fallback still runs.
                if is_disqualified(&summarizer.local.model) {
                    tracing::warn!(
                        model = %summarizer.local.model,
                        "trimwire: local engine SKIPPED (disqualified model); trying next in cascade"
                    );
                    continue;
                }
                match call_model(&summarizer.local, summarizer.timeout_secs, prompt.clone()).await {
                    Ok(text) => return Some((text, "local")),
                    Err(e) => {
                        tracing::warn!(
                            error = %e,
                            "trimwire: local-model engine failed; trying next in cascade"
                        );
                    }
                }
            }
            provider_id => {
                // Resolve to a named provider by id.
                let Some(provider) = summarizer.providers.iter().find(|p| p.id == provider_id)
                else {
                    tracing::warn!(
                        id = provider_id,
                        "trimwire: provider id not found in config; skipping in cascade"
                    );
                    continue;
                };
                match api::call_api(provider, prompt.clone()).await {
                    Ok(text) => return Some((text, "api")),
                    Err(e) => {
                        tracing::warn!(
                            provider = provider_id,
                            error = %e,
                            "trimwire: API provider failed; trying next in cascade"
                        );
                    }
                }
            }
        }
    }

    // All non-terminal engines exhausted (should only reach here if chain was
    // somehow empty or all fell through — "model-free" terminal above normally fires).
    None
}

/// Result of a one-shot summarizer preview (returned by [`preview_summary`]).
///
/// Sizes are the SERIALIZED byte lengths of the messages JSON:
/// `slice_before` is the model-free-pruned `messages[start..end]` region;
/// `slice_after` is the accepted summary pair. Both are measured under the
/// same model-free pruning the production gateway applies so the comparison
/// is apples-to-apples.
#[derive(Debug)]
pub struct PreviewSummary {
    /// Start index (inclusive) of the slice that was summarized.
    pub start: usize,
    /// End index (exclusive) of the slice.
    pub end: usize,
    /// Serialized byte length of the slice AFTER model-free pruning (the baseline).
    pub slice_before: usize,
    /// Serialized byte length of the accepted summary pair (the summarizer output).
    pub slice_after: usize,
    /// Coarse backend kind that produced the summary (`"local"` or `"api"`).
    pub engine_kind: &'static str,
}

/// One-shot summarizer preview: given a reconstructed `messages[]` array and config,
/// mirror the production gate sequence
/// (`select_slice` → `cap_slice_start` → density gate (`tool_result_fraction` /
/// `densify_start`) → `serialize_slice` → `build_prompt` → `run_cascade` →
/// `SummaryDecision::new` → `summary_is_smaller`) and return the
/// accepted reduction as a [`PreviewSummary`] when a summary passes the size gate,
/// or `None` when:
/// - the engine is `"model-free"` (nothing to preview; caller prints a note),
/// - no eligible slice exists,
/// - the slice is too tool-result-dominated and no denser sub-window qualifies
///   (skipped BEFORE any model call — avoids a wasted paid round-trip),
/// - the cascade produced no summary (all engines errored / skipped),
/// - the summary failed the `summary_is_smaller` size gate.
///
/// This is called from `cli::preview` when `--with-summarizer` is passed.
/// It is a **single-slice, one-shot** preview — not the full accumulator loop the
/// live gateway runs. The caller must label the output accordingly ("directional").
///
/// # Cost
/// A `"local"` engine calls ollama with no paid cost. An API engine makes a
/// REAL PAID call on the user's own key — the caller is responsible for gating
/// this behind `--yes` before invoking.
pub async fn preview_summary(
    messages: &[serde_json::Value],
    cfg: &crate::config::Config,
) -> anyhow::Result<Option<PreviewSummary>> {
    let s = &cfg.summarizer;
    // model-free engine: nothing for the summarizer to contribute.
    if s.engine == "model-free" {
        return Ok(None);
    }
    // Select the oldest eligible slice. For a fresh preview there is no reprune
    // checkpoint — pass `messages.len()` as max_end (unconstrained upper bound),
    // which mirrors a "checkpoint at the full array" and lets select_slice choose
    // the widest eligible region. (max_end=0 would block all indices.)
    let Some((start, end)) = slice::select_slice(messages, s.keep_recent_turns, messages.len())
    else {
        return Ok(None);
    };
    // Apply the same slice-size cap the production gateway uses (avoids silent
    // prompt-head truncation by narrowing the start forward if needed).
    let mut start = slice::cap_slice_start(
        messages,
        start,
        end,
        slice::REASONING_BLOCK_CAP,
        slice::TOOL_RESULT_BLOCK_CAP,
        effective_char_budget(s),
    );
    // Density gate (mirrors the gateway, REPLACE path): a tool-result-dominated
    // slice summarizes poorly and `summary_is_smaller` would reject it anyway — so
    // skip it here BEFORE the model call to avoid a wasted (paid) round-trip. Try a
    // denser reasoning sub-window first; if none qualifies, report no contribution.
    if tool_result_fraction(&messages[start..end]) > MAX_TOOL_FRACTION {
        match slice::densify_start(
            messages,
            start,
            end,
            RESCUE_TOOL_FRACTION,
            tool_result_fraction,
        ) {
            Some(s2) => start = s2,
            None => return Ok(None),
        }
    }
    // Serialize and build the prompt exactly as the production gateway does.
    let slice_text = slice::serialize_slice(
        &messages[start..end],
        slice::REASONING_BLOCK_CAP,
        slice::TOOL_RESULT_BLOCK_CAP,
    );
    let prompt = build_prompt(&slice_text);
    // Run the engine cascade (may make a paid API call — caller gates behind --yes).
    let Some((summary_text, engine_kind)) = run_cascade(s, prompt).await else {
        return Ok(None);
    };
    // Build a SummaryDecision (the shape that summary_is_smaller expects).
    let Some(decision) = slice::SummaryDecision::new(messages, start, end, &summary_text) else {
        return Ok(None);
    };
    // Size gate: only accept when the summary beats model-free pruning on this slice.
    if !summary_is_smaller(&decision.messages, messages, start, end, cfg) {
        return Ok(None);
    }
    let slice_before = crate::strategies::serialized_len(&{
        let mut full = messages.to_vec();
        let _ = crate::strategies::run(&mut full, cfg);
        full[start..end].to_vec()
    });
    let slice_after = crate::strategies::serialized_len(&decision.messages);
    Ok(Some(PreviewSummary {
        start,
        end,
        slice_before,
        slice_after,
        engine_kind,
    }))
}

/// RAII release of a session's in-flight summarization slot. Dropping it (on the
/// spawned task's normal completion, early return, or **panic**) clears the
/// `summary_inflight` flag — but only if the task's `epoch` is still the active one,
/// so an evicted+recreated entry or a newer in-flight summary is never disturbed.
/// Without this, a panic in the background task would leave the flag stuck and that
/// session would never summarize again until TTL eviction.
struct InFlightGuard {
    cache: std::sync::Arc<dashmap::DashMap<String, crate::reprune::PruneState>>,
    key: String,
    epoch: u64,
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        if let Some(mut st) = self.cache.get_mut(&self.key) {
            st.end_summary_if(self.epoch);
        }
    }
}

pub fn maybe_spawn_summarization(
    cache: std::sync::Arc<dashmap::DashMap<String, crate::reprune::PruneState>>,
    key: String,
    cfg: std::sync::Arc<crate::config::Config>,
    original_body: hyper::body::Bytes,
    ledger: crate::ledger::Ledger,
) {
    let s = &cfg.summarizer;
    // Self-contained gate: a summary is only replayable when reprune is on, so
    // refuse to spend a model call without it (production already gates via
    // `engages()`; this protects future callers / tests from a silent no-op).
    if s.engine == "model-free" || !cfg.reprune.enabled || original_body.len() <= s.trigger_bytes {
        return;
    }
    let Ok(root) = serde_json::from_slice::<Value>(&original_body) else {
        return;
    };
    let Some(messages) = root.get("messages").and_then(Value::as_array) else {
        return;
    };

    // Sync decision under the lock; capture an owned snapshot for the task.
    let (start, end, is_append, snapshot, permit, epoch) = {
        let Some(mut st) = cache.get_mut(&key) else {
            return;
        };
        if !st.is_initialized() || st.summary_inflight() {
            return;
        }
        let Some((start, end)) =
            slice::select_slice(messages, s.keep_recent_turns, st.checkpoint_len())
        else {
            return;
        };
        // Phase 2.5a: narrow the start forward so the serialized slice fits the
        // num_ctx budget (summarize the most-recent old turns; the oldest stay
        // model-free-pruned) — prevents silent prompt-head truncation.
        let start = slice::cap_slice_start(
            messages,
            start,
            end,
            slice::REASONING_BLOCK_CAP,
            slice::TOOL_RESULT_BLOCK_CAP,
            effective_char_budget(s),
        );
        // Adaptive batch gate (Phase 2.5c): only re-summarize once the UN-summarized
        // prunable delta (serialized bytes added since the cached summary) reaches
        // `resummarize_after_bytes` — a byte threshold auto-adapts to content density
        // where a fixed message count would bust the cache constantly on a 1M session.
        // Honored ONLY while the cached summary still anchors; if it went stale (CC
        // rewrote history) fall through and re-summarize regardless of size.
        if let Some(prev_end) = st.summary_slice_end() {
            if st.summary_anchor_matches(messages) {
                let delta_bytes = if end > prev_end {
                    crate::strategies::serialized_len(&messages[prev_end..end])
                } else {
                    0
                };
                if delta_bytes < s.resummarize_after_bytes {
                    return;
                }
            }
        }
        // Phase 2.5b ACCUMULATOR (opt-in): when enabled and the cached chain still
        // anchors (and is under the segment cap), summarize the delta forward of the
        // chain in a budget-sized CONTIGUOUS chunk `[prev_end..capped_end]` and APPEND a
        // frozen segment — older segments stay byte-frozen (the cache busts only on the
        // bounded delta; the oldest facts never drift out as the slice cap migrates
        // `start` forward). `prev_end` is an assistant-turn boundary (a prior
        // select_slice `end`), so the delta is whole pairs and contiguous with the
        // chain; `cap_slice_end` bounds the segment so we APPEND rather than fall back to
        // a full REPLACE (which would discard the whole chain). Falls back to REPLACE
        // only when off, when the chain is stale/empty, at the segment cap, or when no
        // ≥2-pair delta fits. Computed here (before the tool-fraction gate) so that gate
        // sees the ACTUAL region to be summarized.
        let (mut start, end, is_append) = if s.accumulator
            && st.summary_anchor_matches(messages)
            && st.summary_segment_count() < s.max_summary_segments
        {
            match st.summary_slice_end() {
                Some(prev_end) if end > prev_end + 4 => {
                    let capped_end = slice::cap_slice_end(
                        messages,
                        prev_end,
                        end,
                        slice::REASONING_BLOCK_CAP,
                        slice::TOOL_RESULT_BLOCK_CAP,
                        effective_char_budget(s),
                    );
                    if capped_end >= prev_end + 4 {
                        (prev_end, capped_end, true)
                    } else {
                        (start, end, false)
                    }
                }
                _ => (start, end, false),
            }
        } else {
            (start, end, false)
        };
        // Phase 2 early-skip: a tool_result-dominated slice is model-free's job; skip
        // BEFORE begin_summary (else the in-flight slot would leak). summary_is_smaller
        // would reject it anyway — this just avoids the wasted model call. Uses the
        // ACTUAL [start..end] region decided above (the delta chunk in accumulator mode).
        if tool_result_fraction(&messages[start..end]) > MAX_TOOL_FRACTION {
            // Density-aware fallback (REPLACE path only): the WIDEST slice is too
            // tool-result-dominated to summarize, but a denser sub-window (start
            // advanced forward, dropping the old tool-heavy turns) may qualify —
            // summarize THAT instead, recovering reduction that is otherwise SKIPPED.
            // Purely additive: only a skip becomes a (narrower) summary; an
            // already-passing slice is never changed. Not for the accumulator delta —
            // advancing `start` would gap the frozen chain — so only when !is_append.
            // (Measured on real sessions: the widest slice is tool-dominated and
            // skipped on 5/7 sliceable sessions; a denser sub-window rescues 2-4.)
            let densified = if is_append {
                None
            } else {
                // Rescue with a SAFETY MARGIN stricter than the 0.6 skip gate: only
                // summarize a sub-window that is MAJORITY reasoning (≤ 0.5 tool
                // fraction). The harm gate showed marginal windows (~0.55-0.6 tool)
                // mix early prose with late tool output, and the summarizer then
                // drops the early prose; a clearly-reasoning window (well under 0.5)
                // is where the model is faithful and the win is genuinely additive.
                slice::densify_start(
                    messages,
                    start,
                    end,
                    RESCUE_TOOL_FRACTION,
                    tool_result_fraction,
                )
            };
            match densified {
                Some(s2) => {
                    tracing::debug!(
                        widest_start = start,
                        dense_start = s2,
                        end,
                        "trimwire: density-aware select_slice rescued a tool-heavy slice"
                    );
                    start = s2;
                }
                None => return,
            }
        }
        // Runtime model-guard for the LOCAL engine — evaluated HERE (just before a
        // spawn is committed) so the warnings fire at most once per re-summarization,
        // not on every request over trigger_bytes. Only applies when the Local engine
        // is actually in the cascade chain.
        // NEVER summarize with a model the blind gut-read proved harmful (drops /
        // hallucinates load-bearing facts); warn-but-proceed on a non-validated tag
        // since it's the user's explicit opt-in choice.
        let local_in_chain = s.engine == "local" || s.fallback.iter().any(|f| f == "local");
        // API-in-chain: true when at least one named provider appears in the engine or
        // fallback chain. Using a direct providers-vec check avoids the old fragile
        // heuristic ("anything that isn't 'model-free' or 'local'") that broke when an
        // unknown token crept in or was later added.
        let api_in_chain = s
            .providers
            .iter()
            .any(|p| s.engine == p.id || s.fallback.iter().any(|f| f == &p.id));
        if local_in_chain {
            let local_model = &s.local.model;
            if is_disqualified(local_model) {
                // Warn only when local is the PRIMARY engine — the user configured it
                // as the main engine and it will be skipped.  When local is only a
                // fallback (s.engine != "local") the model-guard fires at most once on a
                // restart and is typically never reached; log at debug to avoid spamming
                // on every spawn for a fallback path that likely never runs.
                if s.engine == "local" {
                    tracing::warn!(
                        model = %local_model,
                        "trimwire: local engine will be SKIPPED — disqualified model \
                         (drops/hallucinates load-bearing facts); set summarizer.local.model to \
                         an approved tag (qwen3.5:4b)"
                    );
                } else {
                    tracing::debug!(
                        model = %local_model,
                        "trimwire: local fallback will be SKIPPED if reached — disqualified model"
                    );
                }
                // If Local is the ONLY real engine, there is nothing to summarize
                // with — skip the spawn entirely (as before). But if an Api engine
                // is also in the chain, proceed: run_cascade skips the disqualified
                // Local and tries Api (the bug fix — a disqualified local fallback
                // must not kill an Api primary).
                if !api_in_chain {
                    return;
                }
            } else if WARN_MODELS.contains(&local_model.as_str()) {
                tracing::warn!(
                    model = %local_model,
                    "trimwire: summarizer model FAILED the harm gate (drops load-bearing \
                     facts: synthetic 75% / real 83%) — it is a RAM opt-down, NOT an equal \
                     to qwen3.5:4b; prefer qwen3.5:4b unless RAM-pinched"
                );
            } else if !APPROVED_MODELS.contains(&local_model.as_str()) {
                tracing::warn!(
                    model = %local_model,
                    approved = %APPROVED_MODELS.join(", "),
                    "trimwire: summarizer model is not a validated tag; summary fidelity \
                     is unverified for it (approved tags in the `approved` field)"
                );
            }
        }
        // Phase 2 concurrency cap: acquire a permit BEFORE begin_summary (never await
        // — skip if the process is already at MAX_CONCURRENT_SUMMARIES, so we don't
        // queue or leak the in-flight slot). The owned permit moves into the task and
        // releases on its drop/panic.
        let Ok(permit) = summary_semaphore().try_acquire() else {
            return;
        };
        let epoch = st.begin_summary();
        (start, end, is_append, messages.to_vec(), permit, epoch)
    };

    tokio::spawn(async move {
        let _permit = permit; // held for the task's lifetime; releases the slot on drop
        // RAII: clear THIS task's in-flight slot on drop — normal completion, early
        // return, OR panic — but only if our epoch is still active (so an evicted +
        // recreated entry, or a newer summary, is never disturbed). Fixes the
        // flag-leak-on-panic that would otherwise wedge this session's summarizer
        // until TTL eviction.
        let _inflight = InFlightGuard {
            cache: cache.clone(),
            key: key.clone(),
            epoch,
        };
        let slice_text = slice::serialize_slice(
            &snapshot[start..end],
            slice::REASONING_BLOCK_CAP,
            slice::TOOL_RESULT_BLOCK_CAP,
        );
        // Visibility for the known num_ctx truncation tension: above the local budget (~max_num_ctx·2.5)
        // bytes the prompt exceeds the capped num_ctx and ollama truncates it from
        // the START → the summary would cover only the recent tail of the slice. This
        // is a LOCAL-engine concern only (cloud models have large windows + no ollama
        // num_ctx cap), so only warn when the local engine can run for this request.
        let local_in_chain = cfg.summarizer.engine == "local"
            || cfg.summarizer.fallback.iter().any(|f| f == "local");
        if local_in_chain
            && slice_text.len() > local_char_budget(cfg.summarizer.local.max_num_ctx) + 2_000
        {
            tracing::debug!(
                slice_bytes = slice_text.len(),
                "trimwire: slice exceeds the num_ctx cap; summary may cover only the recent portion"
            );
        }
        // Run the cascade: engine + fallback chain, de-duplicated, with ModelFree
        // as the implicit terminal.  Never blocks the request hot path — this whole
        // block is inside the spawned background task.
        // Returns Some((summary_text, winning_engine)) where winning_engine is
        // "local" or "api" (the coarse backend kind that produced the text).
        let result = run_cascade(&cfg.summarizer, build_prompt(&slice_text)).await;
        // Re-acquire the lock to record the outcome (never held across an await).
        // If the session was evicted mid-flight, get_mut returns None and we drop
        // the result; the next request starts a fresh PruneState. The in-flight slot
        // is released by `_inflight` (the RAII guard) on this task's exit, under our
        // epoch — so a recycled entry / newer summary is never disturbed.
        if let Some(mut st) = cache.get_mut(&key) {
            // If this entry was evicted+recreated (or a newer summary superseded us)
            // while we ran, our epoch is no longer active — drop the stale result
            // rather than splicing it onto a different generation's state.
            if !st.summary_active(epoch) {
                return;
            }
            // Outcome code recorded (content-free) for the `share stats` install-rate:
            // 'a' accepted (installed), 'r' rejected / empty decision, 'e' model error.
            // winning_engine: "local" | "api" when outcome='a'; "model-free" otherwise.
            // `collapsed` is set true only on a genuine accumulator chain COLLAPSE.
            let mut collapsed = false;
            let (outcome, winning_engine): (char, &str) = match result {
                Some((summary, engine_kind)) => {
                    match slice::SummaryDecision::new(&snapshot, start, end, &summary) {
                        Some(d) => {
                            // Only keep it if it beats model-free pruning on this
                            // slice — never make the body larger than deterministic
                            // pruning already does.
                            if summary_is_smaller(&d.messages, &snapshot, start, end, &cfg) {
                                // Compression ratio (observability; the SIZE gate above only
                                // checks summary < model-free, never logs how much was won).
                                // raw = the verbatim slice the summary replaces; ratio is the
                                // summary as a percentage of it — lower is tighter.
                                let raw_bytes =
                                    crate::strategies::serialized_len(&snapshot[start..end]);
                                let summary_bytes = crate::strategies::serialized_len(&d.messages);
                                let ratio_pct =
                                    (summary_bytes * 100).checked_div(raw_bytes).unwrap_or(0);
                                // Accumulator: APPEND a frozen delta segment (older segments
                                // stay byte-frozen); else REPLACE (default single-summary).
                                // append_summary falls back to replace if non-contiguous.
                                let segments_before = st.summary_segment_count();
                                let appended = is_append && st.append_summary(d.clone());
                                if !appended {
                                    st.set_summary(d);
                                    // §17 T4: a genuine chain COLLAPSE — the accumulator hit
                                    // max_summary_segments, so the whole frozen chain was
                                    // re-summarized into one (a summary-of-summaries; the oldest
                                    // detail may now be lost). Rare (needs ~max×resummarize bytes).
                                    // One content-free heads-up to the operator's own terminal —
                                    // never injected into the model's context (ToS).
                                    if cfg.summarizer.accumulator
                                        && segments_before >= cfg.summarizer.max_summary_segments
                                    {
                                        collapsed = true;
                                        tracing::warn!(
                                            prior_segments = segments_before,
                                            "trimwire: summarizer chain collapsed — very old context was \
                                             replaced by a single fresh summary of the ORIGINAL bytes (not a \
                                             summary-of-summaries), so the oldest fine detail may now fall \
                                             outside the summary window and revert to model-free stubs. For \
                                             long sessions, checkpoint with Claude Code /compact or start a \
                                             fresh session + handoff (files are always re-readable)."
                                        );
                                    }
                                }
                                // Chain length: how many frozen summary segments exist now.
                                // The accumulator's behavior at scale (does it fire enough on
                                // a 1M session before MAX_SUMMARY_SEGMENTS forces a collapse?)
                                // is the open question that gates any context-pressure
                                // escalation work — this is the field that answers it.
                                let segments = st.summary_segment_count();
                                // §15 S5: coverage = this segment's message span as a
                                // % of the whole conversation — the maintainer's signal
                                // for "is the summary owning a big part of old content?"
                                let coverage_pct = ((end - start) * 100)
                                    .checked_div(snapshot.len())
                                    .unwrap_or(0);
                                tracing::info!(
                                    turns = end - start,
                                    appended,
                                    raw_bytes,
                                    summary_bytes,
                                    ratio_pct,
                                    segments,
                                    coverage_pct,
                                    "trimwire: summarizer compaction installed"
                                );
                                ('a', engine_kind)
                            } else {
                                tracing::info!(
                                    turns = end - start,
                                    "trimwire: summary rejected (model-free pruning is smaller)"
                                );
                                ('r', "model-free")
                            }
                        }
                        None => {
                            tracing::debug!("summarizer skipped (empty slice)");
                            ('r', "model-free")
                        }
                    }
                }
                None => {
                    // The cascade reached its ModelFree terminal without any engine
                    // producing a summary. This block only runs when the configured
                    // engine is NOT model-free (gated above), so reaching here means
                    // every non-terminal engine ERRORED (network/timeout/HTTP/empty/
                    // malformed) or was skipped — an error outcome, distinct from 'r'
                    // (a summary was produced but lost to model-free pruning).
                    // warn (not debug): when every configured engine errors (ollama
                    // down, API 429/timeout, …) the summarizer is effectively broken,
                    // and the default log level is `warn` — an operator must see this.
                    tracing::warn!(
                        "trimwire: summarizer cascade exhausted — every configured engine \
                         errored (ollama down / API error / timeout?); model-free pruning \
                         stands. Check `trimwire stats` and your summarizer config."
                    );
                    ('e', "model-free")
                }
            };
            // Content-free outcome record (timestamp + code + winning engine) →
            // `share stats` summarizer install-rate / trigger-rate / won-backend.
            // Fire-and-forget.
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
            ledger.record_summarizer_event(now, outcome, winning_engine, collapsed);
        }
    });
}

/// Call the local model once (one-shot, non-streaming) and return the summary
/// text. Best-effort: any failure is a `CompactorError` the caller treats as
/// "skip compaction". Sends ollama's `keep_alive` so the model unloads after the
/// batch (RAM-friendly). The WHOLE call — connect, headers, AND body read — is
/// hard-bounded by `timeout_secs` (a header-then-hang must not wedge the task and
/// leak the in-flight slot).
///
/// Builds a fresh hyper/hyper-rustls client per call (its connector is
/// `https_or_http`, so the plain-HTTP localhost endpoint works with no extra
/// dependency). This is a rare batch call (once per `resummarize_after_bytes` of
/// new prunable delta), so a one-off client + connection is fine; we deliberately don't share the
/// gateway's pooled client to keep this module self-contained.
///
/// Posts the slice to ollama `/api/chat` with the facts-first system prompt and a
/// conservative `num_ctx` (`num_ctx_for`, floored 4096 / capped at the configured local max_num_ctx) so the
/// prompt is never silently head-truncated; `num_predict` and a 180s default timeout
/// are set in the payload/config. (Phase 1a fix — the earlier `/api/generate` +
/// no-num_ctx version is gone.)
/// Strip ALL `<think>…</think>` blocks a thinking model may emit despite
/// `think:false` (`str::find` returns only the first match, so loop until none
/// remain). Malformed/unpaired tags are left intact (the `</think>`-before-or-
/// without-`<think>` case breaks the loop).
fn strip_think_blocks(mut text: String) -> String {
    while let (Some(a), Some(b)) = (text.find("<think>"), text.find("</think>")) {
        if b > a {
            text.replace_range(a..b + "</think>".len(), "");
        } else {
            break;
        }
    }
    text
}

pub async fn call_model(
    cfg: &SummarizerLocalConfig,
    timeout_secs: u64,
    prompt: String,
) -> Result<String, CompactorError> {
    let url = format!("{}/api/chat", cfg.endpoint.trim_end_matches('/'));
    // num_ctx from the excerpt size: ollama's 4096 default truncates the prompt
    // from the START (the silent-truncation bug). Conservative len/2.5, floored at
    // 4096, capped at cfg.max_num_ctx (≤ ceiling) so the KV-cache allocation can't OOM a small box.
    let num_ctx = num_ctx_for(prompt.len(), cfg.max_num_ctx);
    let num_predict = num_predict_for(num_ctx);
    let mut options = serde_json::json!({
        // Near-greedy, fixed seed — as deterministic as a local model gets (matches
        // the council-locked bench harness).
        "temperature": 0.1, "top_k": 20, "min_p": 0,
        "repeat_penalty": 1.1, "repeat_last_n": 64, "seed": 42,
        "num_ctx": num_ctx, "num_predict": num_predict,
        "stop": ["<|im_end|>", "\n\n---", "\nNote:", "\nSummary:", "\nConclusion:"],
    });
    options["top_p"] = serde_json::json!(if cfg.model.starts_with("qwen3") {
        0.8
    } else {
        0.9
    });
    let mut payload = serde_json::json!({
        "model": cfg.model,
        "stream": false,
        // ollama accepts an integer (seconds); 0 = unload immediately after.
        "keep_alive": cfg.keep_alive_secs,
        "messages": [
            {"role": "system", "content": SUMMARY_SYSTEM_PROMPT},
            {"role": "user", "content": prompt},
        ],
        "options": options,
    });
    // Qwen3 thinking models: disable the <think> pass (cheaper; no stray tags to strip).
    if cfg.model.starts_with("qwen3") {
        payload["think"] = serde_json::json!(false);
    }
    let body =
        serde_json::to_vec(&payload).map_err(|e| CompactorError::Malformed(e.to_string()))?;

    let req = hyper::Request::builder()
        .method(hyper::Method::POST)
        .uri(&url)
        .header(hyper::header::CONTENT_TYPE, "application/json")
        .body(Full::new(Bytes::from(body)))
        .map_err(|e| CompactorError::Unreachable(e.to_string()))?;

    // NOTE (#78 Disagree-Seeking): building the client here (inside the spawned
    // summarization task) was flagged as a possible leak-on-panic; it is NOT a bug.
    // There is no panic path between begin_summary and end_summary, and if the task
    // is cancelled/the session evicted, the in-flight flag dies with the PruneState
    // entry (the next request starts fresh). Kept inline; do not "fix".
    let client = crate::proxy::upstream::build_client();
    // Bound the ENTIRE exchange (request + body collection) in one timeout, so a
    // server that flushes headers then stalls on the body can't hang the task.
    let exchange = async {
        let resp = client
            .request(req)
            .await
            .map_err(|e| CompactorError::Unreachable(e.to_string()))?;
        let status = resp.status();
        if !status.is_success() {
            return Err(CompactorError::HttpStatus(status.as_u16()));
        }
        resp.into_body()
            .collect()
            .await
            .map_err(|e| CompactorError::Unreachable(e.to_string()))
            .map(|c| c.to_bytes())
    };
    let collected = match tokio::time::timeout(Duration::from_secs(timeout_secs), exchange).await {
        Err(_) => return Err(CompactorError::Timeout),
        Ok(r) => r?,
    };

    let v: Value =
        serde_json::from_slice(&collected).map_err(|e| CompactorError::Malformed(e.to_string()))?;
    // /api/chat returns the assistant turn under `.message.content`.
    let text = v
        .get("message")
        .and_then(|m| m.get("content"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    let text = strip_think_blocks(text);
    let text = text.trim().to_owned();
    if text.is_empty() {
        return Err(CompactorError::EmptyResponse);
    }
    Ok(text)
}

#[cfg(test)]
// set_var/remove_var are unsafe in Rust 2024; test-only, unique env var names per test.
#[allow(unsafe_code)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[test]
    fn strip_think_blocks_removes_all_and_preserves_unpaired() {
        // single block
        assert_eq!(
            strip_think_blocks("<think>x</think>summary".into()),
            "summary"
        );
        // MULTIPLE blocks (the bug: only the first used to be stripped)
        assert_eq!(
            strip_think_blocks("<think>a</think>keep1<think>b</think>keep2".into()),
            "keep1keep2"
        );
        // no blocks → untouched
        assert_eq!(strip_think_blocks("plain summary".into()), "plain summary");
        // unpaired/malformed (close before open) → left intact, no infinite loop
        assert_eq!(
            strip_think_blocks("</think>oops<think>".into()),
            "</think>oops<think>"
        );
    }

    fn cfg_for(endpoint: String) -> SummarizerLocalConfig {
        SummarizerLocalConfig {
            endpoint,
            ..Default::default()
        }
    }

    /// Build a full conversation: `old` bulky tool pairs followed by `recent`
    /// pairs, so the model-free strategies treat the leading slice as OLD (as in
    /// production) rather than "recent within an isolated slice".
    #[cfg(test)]
    fn bash_convo(old: usize, recent: usize, big: &str) -> Vec<serde_json::Value> {
        use serde_json::json;
        let mut m = Vec::new();
        for i in 0..(old + recent) {
            let id = format!("a{i}");
            m.push(json!({"role":"assistant","content":[
                {"type":"tool_use","id":id,"name":"Bash","input":{"command": format!("cmd {i}")}}
            ]}));
            m.push(json!({"role":"user","content":[
                {"type":"tool_result","tool_use_id":id,"content": big}
            ]}));
        }
        m
    }

    #[test]
    fn summary_gate_rejects_when_model_free_is_smaller() {
        let cfg = crate::config::profile_baseline("default");
        // 4 OLD bulky pairs + 3 recent: in FULL context the default strategies
        // (dedup + bloat_cap) crush the old pairs hard.
        let big = "X".repeat(20_000);
        let messages = bash_convo(4, 3, &big);
        let (start, end) = (0, 8); // the 4 old pairs
        let d =
            slice::SummaryDecision::new(&messages, start, end, &"summary ".repeat(2_000)).unwrap();
        assert!(
            !summary_is_smaller(&d.messages, &messages, start, end, &cfg),
            "a verbose summary must lose to aggressive model-free pruning in full context"
        );
    }

    #[test]
    fn summary_gate_keeps_when_summary_is_smaller() {
        use serde_json::json;
        let cfg = crate::config::profile_baseline("default");
        // Reasoning-dense TEXT turns model-free barely touches, made old by recent
        // turns after them; a tiny summary beats the un-prunable prose.
        let prose = "We analysed the failure and decided on approach B. ".repeat(60);
        let mut messages = vec![
            json!({"role":"assistant","content":[{"type":"text","text": prose.clone()}]}),
            json!({"role":"user","content":[{"type":"text","text": prose.clone()}]}),
            json!({"role":"assistant","content":[{"type":"text","text": prose.clone()}]}),
            json!({"role":"user","content":[{"type":"text","text": prose}]}),
        ];
        messages.extend(bash_convo(0, 3, "ok"));
        let (start, end) = (0, 4);
        let d = slice::SummaryDecision::new(&messages, start, end, "approach B chosen").unwrap();
        assert!(
            summary_is_smaller(&d.messages, &messages, start, end, &cfg),
            "a tiny summary must beat un-prunable reasoning text"
        );
    }

    #[test]
    fn effective_char_budget_small_for_local_large_for_api_only() {
        // Explicit override always wins.
        let s = SummarizerConfig {
            slice_char_budget: Some(12_345),
            ..Default::default()
        };
        assert_eq!(effective_char_budget(&s), 12_345);
        // Local primary → small num_ctx-safe budget.
        let s = SummarizerConfig {
            engine: "local".to_owned(),
            ..Default::default()
        };
        assert_eq!(
            effective_char_budget(&s),
            local_char_budget(s.local.max_num_ctx)
        );
        // API primary but local in FALLBACK → still small (slice must fit local).
        let s = SummarizerConfig {
            engine: "myapi".to_owned(),
            fallback: vec!["local".to_owned()],
            ..Default::default()
        };
        assert_eq!(
            effective_char_budget(&s),
            local_char_budget(s.local.max_num_ctx)
        );
        // API-ONLY chain (no local anywhere) → large budget.
        let s = SummarizerConfig {
            engine: "myapi".to_owned(),
            fallback: vec!["model-free".to_owned()],
            ..Default::default()
        };
        assert_eq!(effective_char_budget(&s), API_SLICE_CHAR_BUDGET);
    }

    #[test]
    fn num_predict_is_proportional_then_flat_capped() {
        // Small/mid window → proportional (num_ctx/4), above the 200 floor, below the cap.
        assert_eq!(num_predict_for(12_288), 3_072);
        assert_eq!(num_predict_for(8_192), 2_048);
        // Tiny window → floored at 200, never below.
        assert_eq!(num_predict_for(400), 200);
        // Large window → FLAT-capped, NOT num_ctx/4 (the runaway this guards).
        assert_eq!(num_predict_for(40_000), MAX_NUM_PREDICT);
        assert_eq!(num_predict_for(131_072), MAX_NUM_PREDICT);
        assert!(
            num_predict_for(131_072) < 131_072 / 4,
            "cap must bite the big case"
        );
    }

    #[test]
    fn accept_ratio_keeps_slightly_larger_summary_when_relaxed() {
        use serde_json::json;
        let base = crate::config::profile_baseline("default");
        // Two reasoning-text turns no strategy touches → model-free leaves them as-is,
        // so the model-free slice ≈ the original (~4 KB).
        let big = "A".repeat(2000);
        let mut messages = vec![
            json!({"role":"assistant","content":[{"type":"text","text": big.clone()}]}),
            json!({"role":"user","content":[{"type":"text","text": big}]}),
        ];
        messages.extend(bash_convo(0, 3, "ok"));
        let (start, end) = (0, 2);
        // A summary slightly LARGER than the model-free slice.
        let summary = "B".repeat(4700);
        let d = slice::SummaryDecision::new(&messages, start, end, &summary).unwrap();

        // Strict gate (default 1.0): reject (summary not smaller than model-free).
        let mut strict = base.clone();
        strict.summarizer.accept_ratio = 1.0;
        assert!(
            !summary_is_smaller(&d.messages, &messages, start, end, &strict),
            "strict gate must reject a summary larger than model-free"
        );
        // Relaxed gate (1.5): keep the higher-fidelity summary (within ratio + abs cap).
        let mut relaxed = base;
        relaxed.summarizer.accept_ratio = 1.5;
        assert!(
            summary_is_smaller(&d.messages, &messages, start, end, &relaxed),
            "relaxed accept_ratio must keep a slightly-larger higher-fidelity summary"
        );
    }

    #[test]
    fn prompt_embeds_excerpt_in_delimiters() {
        let p = build_prompt("role=user: hello");
        assert!(p.contains("<excerpt>\nrole=user: hello\n</excerpt>"));
        assert!(p.contains("Compact the excerpt above"));
    }

    #[test]
    fn system_prompt_is_facts_first() {
        // The FACTS-FIRST sections live in the SYSTEM message, not build_prompt.
        assert!(SUMMARY_SYSTEM_PROMPT.contains("GOAL:"));
        assert!(SUMMARY_SYSTEM_PROMPT.contains("NEXT:"));
        assert!(SUMMARY_SYSTEM_PROMPT.contains("VERBATIM"));
        // The old rambling-inducing wording must be gone.
        assert!(!SUMMARY_SYSTEM_PROMPT.contains("a long summary is fine"));
        // Anti-overstatement rule (harm-gate finding: the summarizer can claim
        // findings "fixed" when only some were done at the cut). FACTS/DECIDED must
        // not mark still-open work complete; partial progress is "N of M complete".
        assert!(
            SUMMARY_SYSTEM_PROMPT.contains("Do NOT mark work finished"),
            "prompt must forbid claiming unfinished work as done"
        );
        assert!(
            SUMMARY_SYSTEM_PROMPT.contains("N of M complete"),
            "prompt must prescribe the N-of-M partial-progress form"
        );
        // Rule 6 (active anti-false-done): enumerate announced-but-unconfirmed tasks
        // before NEXT. Validated to fix the truncated-session false-done that the
        // passive rule 5 missed (qwen3.5:4b on the b_s2 slice).
        assert!(
            SUMMARY_SYSTEM_PROMPT.contains("ANNOUNCED or STARTED"),
            "prompt must actively enumerate announced-but-unconfirmed tasks (rule 6)"
        );
        assert!(
            SUMMARY_SYSTEM_PROMPT.contains("only located"),
            "rule 6 must treat a located-but-unedited task as OPEN"
        );
        // FILES discipline (peripheral-identifier hardening): the approved qwen3.5
        // family's residual defect was FILES corruption — garbled paths
        // (drizzle/meta/_journal.json -> drizzle.meta._journal.json), guessed
        // extensions (+layout.svelte -> +layout.svelte.ts), and FILES-inflation
        // (listing a referenced-only file as touched). The FILES descriptor must
        // forbid inflation and bias toward omission over a guessed path.
        assert!(
            SUMMARY_SYSTEM_PROMPT.contains("actually targeted"),
            "FILES must list only edited/written files, not merely referenced ones"
        );
        assert!(
            SUMMARY_SYSTEM_PROMPT.contains("omit it rather than guess"),
            "FILES garble guard: omit a path you can't copy verbatim, never guess it"
        );
    }

    #[test]
    fn tier_search_disqualifications_are_pinned() {
        // The 2026-06-04 tier gut-read disqualified the false-done/fabricating candidates
        // (granite4.1:8b = granite-family false-done + fabricated path; ministral-3:3b =
        // fabricated identifier + content collapse). The runtime guard must refuse them.
        for bad in [
            "granite4.1:3b",
            "granite4.1:8b",
            "qwen2.5-coder:3b",
            "ministral-3:3b",
            "gemma3:4b",
            "qwen3.5:0.8b",
        ] {
            assert!(
                DISQUALIFIED_MODELS.contains(&bad),
                "{bad} must be in DISQUALIFIED_MODELS"
            );
            assert!(
                !APPROVED_MODELS.contains(&bad),
                "{bad} must never be APPROVED"
            );
        }
        // Approved: 4b default, q8_0 higher-fidelity MEDIUM upgrade, 9b PRO, 2b warned
        // opt-down. gemma3:4b disqualified (fabrication/false-done). qwen3:8b NOT approved
        // (misdirected-NEXT) but not refused — unlisted (warn-on-use).
        assert_eq!(
            APPROVED_MODELS,
            &["qwen3.5:4b", "qwen3.5:4b-q8_0", "qwen3.5:9b", "qwen3.5:2b"]
        );
        assert!(
            !APPROVED_MODELS.contains(&"qwen3:8b"),
            "qwen3:8b is not approved"
        );
        assert!(
            !DISQUALIFIED_MODELS.contains(&"qwen3:8b"),
            "but qwen3:8b is not refused"
        );
    }

    #[test]
    fn is_disqualified_matches_family_and_exact() {
        // exact tags the gut-read enumerated stay refused
        assert!(is_disqualified("granite4.1:8b"));
        assert!(is_disqualified("qwen2.5-coder:3b"));
        assert!(is_disqualified("qwen3.5:0.8b")); // size-specific exact entry
        // FAMILY: any granite4.1 variant the list never enumerated (the fixed nit)
        assert!(is_disqualified("granite4.1:latest"));
        assert!(is_disqualified("granite4.1:2b"));
        // qwen3.5 is NOT a family — the approved 4b (and warned 2b) must stay allowed
        assert!(!is_disqualified("qwen3.5:4b"));
        assert!(!is_disqualified("qwen3.5:2b"));
        assert!(!is_disqualified("llama3.1:8b"));
    }

    #[test]
    fn model_bench_freeform_prompt_matches_summary_system_prompt() {
        // The benchmark harness embeds a SHELL copy of the prompt (bash can't import the
        // Rust const). Pin it byte-identical to SUMMARY_SYSTEM_PROMPT so a future prompt
        // edit that forgets model_bench.sh can't silently benchmark a different prompt
        // than production. (The two Rust examples now import the const directly.)
        let sh = include_str!("../../benchmark/model_bench.sh");
        let start = "read -r -d '' SYSTEM_FREEFORM <<'EOF'\n";
        let from = sh.find(start).expect("SYSTEM_FREEFORM heredoc start") + start.len();
        let rest = &sh[from..];
        let to = rest.find("\nEOF").expect("SYSTEM_FREEFORM heredoc end");
        assert_eq!(
            &rest[..to],
            SUMMARY_SYSTEM_PROMPT,
            "benchmark/model_bench.sh SYSTEM_FREEFORM has drifted from SUMMARY_SYSTEM_PROMPT — keep them byte-identical"
        );
    }

    #[tokio::test]
    async fn ok_response_returns_trimmed_summary() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "message": {"role": "assistant", "content": "  ## Goal\nDo the thing\n  "},
                "done": true
            })))
            .mount(&server)
            .await;
        let out = call_model(&cfg_for(server.uri()), 5, build_prompt("x"))
            .await
            .expect("ok");
        assert_eq!(out, "## Goal\nDo the thing");
    }

    /// Deterministic harm-gate guard (no live ollama): a wiremock CANNED summary
    /// drives the real `call_model`, then the same `normalize_fact`/`fact_retention`
    /// the harm gate (`examples/compaction_harm.rs`) uses is asserted — separator-
    /// insensitive (hyphen≡underscore), case-insensitive, and a dropped load-bearing
    /// fact must pull retention below the 0.90 gate. Guards the gate LOGIC in CI.
    #[tokio::test]
    async fn harm_gate_retention_logic_is_deterministic() {
        // Canned model output: contains 4 of 5 planted facts. MAX-RETRIES exercises
        // the hyphen form vs the MAX_RETRIES needle; Reconcile_Balances exercises
        // case-insensitivity; game-engine.md is deliberately omitted.
        let canned = "GOAL: harden auth\n\
            FILES: src/auth/session_7421.rs\n\
            DECIDED: cap MAX-RETRIES = 5 in Reconcile_Balances\n\
            ERRORS: error[E0277] the trait bound `Job: Send` is not satisfied\n\
            NEXT: wire the writer";
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(
                    serde_json::json!({"message": {"content": canned}, "done": true}),
                ),
            )
            .mount(&server)
            .await;
        let summary = call_model(&cfg_for(server.uri()), 5, build_prompt("irrelevant"))
            .await
            .expect("canned summary");

        let needles = [
            "src/auth/session_7421.rs",
            "error[E0277]",
            "MAX_RETRIES",
            "reconcile_balances",
            "game-engine.md", // load-bearing fact the summary dropped
        ];
        let (kept, total) = fact_retention(&summary, &needles);
        assert_eq!(
            (kept, total),
            (4, 5),
            "4 of 5 planted facts survive (the dropped one is game-engine.md)"
        );
        // The single dropped load-bearing fact must fail the 0.90 harm gate.
        assert!((kept as f64 / total as f64) < 0.90);

        // Normalization edge cases, isolated from the model path.
        assert_eq!(normalize_fact("Max-Retries"), "max_retries");
        assert_eq!(
            fact_retention("uses max_retries here", &["MAX-RETRIES"]).0,
            1
        );
        assert_eq!(fact_retention("nothing relevant", &["missing_id"]).0, 0);
    }

    #[tokio::test]
    async fn non_200_is_http_status_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;
        let err = call_model(&cfg_for(server.uri()), 5, build_prompt("x"))
            .await
            .unwrap_err();
        assert!(matches!(err, CompactorError::HttpStatus(500)));
    }

    #[tokio::test]
    async fn empty_response_field_is_empty_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(
                    serde_json::json!({"message": {"content": "   "}, "done": true}),
                ),
            )
            .mount(&server)
            .await;
        let err = call_model(&cfg_for(server.uri()), 5, build_prompt("x"))
            .await
            .unwrap_err();
        assert!(matches!(err, CompactorError::EmptyResponse));
    }

    #[tokio::test]
    async fn malformed_body_is_malformed_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .respond_with(ResponseTemplate::new(200).set_body_string("not json"))
            .mount(&server)
            .await;
        let err = call_model(&cfg_for(server.uri()), 5, build_prompt("x"))
            .await
            .unwrap_err();
        assert!(matches!(err, CompactorError::Malformed(_)));
    }

    #[tokio::test]
    async fn unreachable_endpoint_fails_fast_to_fallback() {
        // Nothing listening here. Whether the OS refuses (Unreachable) or the
        // connect stalls until the deadline (Timeout) is platform-dependent —
        // both map to the same caller behavior (skip compaction), so accept
        // either, with a short timeout so the test stays quick.
        let cfg = cfg_for("http://127.0.0.1:1".to_owned());
        let err = call_model(&cfg, 2, build_prompt("x")).await.unwrap_err();
        assert!(
            matches!(
                err,
                CompactorError::Unreachable(_) | CompactorError::Timeout
            ),
            "unreachable endpoint must fail to a skip-compaction error, got {err:?}"
        );
    }

    #[tokio::test]
    async fn slow_response_times_out() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_delay(Duration::from_secs(3))
                    .set_body_json(
                        serde_json::json!({"message": {"content": "late"}, "done": true}),
                    ),
            )
            .mount(&server)
            .await;
        let cfg = cfg_for(server.uri());
        let err = call_model(&cfg, 1, build_prompt("x")).await.unwrap_err();
        assert!(matches!(err, CompactorError::Timeout));
    }

    // ---- maybe_spawn_summarization: gate + model-guard + spawn lifecycle ----

    /// Leading user turn + `pairs` [assistant(text), user(text)] pairs of DISTINCT
    /// reasoning prose (per-turn varied so cross_turn_dedup can't collapse it and
    /// model-free pruning can't compress it → a tiny summary beats it and the
    /// install gate keeps it). Shape select_slice accepts (leading user; ≥keep+2
    /// assistant turns).
    fn prose_convo(pairs: usize) -> Vec<Value> {
        use serde_json::json;
        let mut m =
            vec![json!({"role":"user","content":[{"type":"text","text":"start the task"}]})];
        for i in 0..pairs {
            let prose =
                format!("We analysed failure {i} and chose approach B for case {i}. ").repeat(40);
            m.push(json!({"role":"assistant","content":[{"type":"text","text": prose.clone()}]}));
            m.push(json!({"role":"user","content":[{"type":"text","text": prose}]}));
        }
        m
    }

    fn spawn_cfg(endpoint: String, model: &str) -> std::sync::Arc<crate::config::Config> {
        let mut cfg = crate::config::profile_baseline("default");
        cfg.summarizer = SummarizerConfig {
            engine: "local".to_owned(),
            trigger_bytes: 10,
            keep_recent_turns: 2,
            timeout_secs: 5,
            local: SummarizerLocalConfig {
                endpoint,
                model: model.to_owned(),
                ..Default::default()
            },
            ..Default::default()
        };
        std::sync::Arc::new(cfg)
    }

    /// A warmed (initialized) PruneState cache for `body` — run the model-free
    /// reprune once so checkpoint_len + init are set, as on a real later turn.
    fn warmed_cache(
        key: &str,
        body: &[u8],
        cfg: &crate::config::Config,
    ) -> std::sync::Arc<dashmap::DashMap<String, crate::reprune::PruneState>> {
        let mut st = crate::reprune::PruneState::default();
        let _ = crate::reprune::stable_apply_to_body(body, cfg, &mut st, cfg.reprune.threshold);
        let cache = std::sync::Arc::new(dashmap::DashMap::new());
        cache.insert(key.to_owned(), st);
        cache
    }

    #[test]
    fn inflight_guard_clears_flag_on_drop() {
        // The RAII guard releases the in-flight slot when the task ends — this is what
        // fixes the flag-leak-on-panic (Drop runs during unwind too, not just on the
        // normal path).
        let cache = std::sync::Arc::new(dashmap::DashMap::new());
        let mut st = crate::reprune::PruneState::default();
        let epoch = st.begin_summary();
        assert!(st.summary_inflight());
        cache.insert("k".to_owned(), st);
        {
            let _g = InFlightGuard {
                cache: cache.clone(),
                key: "k".to_owned(),
                epoch,
            };
        } // guard drops here
        assert!(
            !cache.get("k").unwrap().summary_inflight(),
            "guard must clear the in-flight flag on drop"
        );
    }

    #[test]
    fn inflight_guard_does_not_disturb_a_recycled_entry() {
        // If the entry was evicted + recreated (and a NEW summary started) while the
        // old task ran, the stale task's guard must NOT clear the new summary's flag,
        // and the stale task must NOT see its epoch as active.
        let cache = std::sync::Arc::new(dashmap::DashMap::new());
        let mut st = crate::reprune::PruneState::default();
        let stale_epoch = st.begin_summary();
        cache.insert("k".to_owned(), st);
        // Simulate eviction + recreation + a fresh in-flight summary on the same key:
        let mut fresh = crate::reprune::PruneState::default();
        let new_epoch = fresh.begin_summary();
        assert_ne!(stale_epoch, new_epoch, "epochs are process-globally unique");
        assert!(
            !fresh.summary_active(stale_epoch),
            "stale epoch is not active"
        );
        cache.insert("k".to_owned(), fresh);
        // Stale task's guard drops with the OLD epoch:
        {
            let _g = InFlightGuard {
                cache: cache.clone(),
                key: "k".to_owned(),
                epoch: stale_epoch,
            };
        }
        assert!(
            cache.get("k").unwrap().summary_inflight(),
            "a stale guard must not clear a recycled entry's newer in-flight flag"
        );
    }

    /// Poll until the background summary task settles (drops the in-flight flag).
    ///
    /// This is a **bounded busy-wait**: up to 200 × 25 ms = 5 s maximum. The flag is
    /// released by the task's `InFlightGuard` on ANY exit — normal completion, early
    /// return, or panic (Drop runs during unwind) — so this settles promptly even if
    /// the spawned task panics.
    async fn await_settled(
        cache: &dashmap::DashMap<String, crate::reprune::PruneState>,
        key: &str,
    ) {
        for _ in 0..200 {
            let inflight = cache
                .get(key)
                .map(|s| s.summary_inflight())
                .unwrap_or(false);
            if !inflight {
                tokio::task::yield_now().await;
                return;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    fn body_of(pairs: usize) -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({"messages": prose_convo(pairs)})).unwrap()
    }

    #[tokio::test]
    async fn maybe_spawn_skips_disqualified_model_without_calling_it() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"message": {"content": "done"}})),
            )
            .expect(0) // the guard must skip BEFORE any HTTP call
            .mount(&server)
            .await;
        let cfg = spawn_cfg(server.uri(), "qwen2.5-coder:3b"); // DISQUALIFIED
        let body = body_of(6);
        let cache = warmed_cache("s", &body, &cfg);
        maybe_spawn_summarization(
            cache.clone(),
            "s".into(),
            cfg,
            Bytes::from(body),
            crate::ledger::Ledger::disabled(),
        );
        await_settled(&cache, "s").await;
        assert!(
            cache.get("s").unwrap().summary_slice_end().is_none(),
            "disqualified model must not install a summary"
        );
    }

    #[tokio::test]
    async fn maybe_spawn_skips_under_trigger_bytes() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"message": {"content": "done"}})),
            )
            .expect(0)
            .mount(&server)
            .await;
        let mut cfg = (*spawn_cfg(server.uri(), "qwen3.5:4b")).clone();
        cfg.summarizer.trigger_bytes = usize::MAX; // body can never exceed it
        let cfg = std::sync::Arc::new(cfg);
        let body = body_of(6);
        let cache = warmed_cache("s", &body, &cfg);
        maybe_spawn_summarization(
            cache.clone(),
            "s".into(),
            cfg,
            Bytes::from(body),
            crate::ledger::Ledger::disabled(),
        );
        await_settled(&cache, "s").await;
        assert!(cache.get("s").unwrap().summary_slice_end().is_none());
    }

    #[tokio::test]
    async fn maybe_spawn_installs_summary_on_happy_path() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                serde_json::json!({"message": {"content": "GOAL: approach B chosen for the ledger."}}),
            ))
            .mount(&server)
            .await;
        let cfg = spawn_cfg(server.uri(), "qwen3.5:4b");
        let body = body_of(6);
        let cache = warmed_cache("s", &body, &cfg);
        maybe_spawn_summarization(
            cache.clone(),
            "s".into(),
            cfg,
            Bytes::from(body),
            crate::ledger::Ledger::disabled(),
        );
        await_settled(&cache, "s").await;
        let st = cache.get("s").unwrap();
        assert!(!st.summary_inflight(), "in-flight flag must clear");
        assert!(
            st.summary_slice_end().is_some(),
            "a tiny summary must install over un-prunable reasoning prose"
        );
    }

    #[test]
    fn warn_models_is_approved_subset_disjoint_from_disqualified() {
        // The warned-but-weaker set must be allowed (a subset of APPROVED) and never
        // overlap the refused set — otherwise the guard ordering would be incoherent.
        for m in WARN_MODELS {
            assert!(
                APPROVED_MODELS.contains(m),
                "{m} must be APPROVED to be merely warned"
            );
            assert!(
                !DISQUALIFIED_MODELS.contains(m),
                "{m} cannot be both warned and disqualified"
            );
        }
        assert!(
            WARN_MODELS.contains(&"qwen3.5:2b"),
            "the harm-failing opt-down must be warned"
        );
    }

    #[test]
    fn engages_requires_both_non_model_free_engine_and_reprune() {
        let mut c = crate::config::profile_baseline("default");
        c.summarizer.engine = "model-free".to_owned();
        c.reprune.enabled = true;
        assert!(!engages(&c), "model-free engine never engages");
        c.summarizer.engine = "local".to_owned();
        c.reprune.enabled = false;
        assert!(
            !engages(&c),
            "local engine without reprune is a SILENT NO-OP — must not engage"
        );
        c.reprune.enabled = true;
        assert!(engages(&c), "local engine + reprune → engages");
        // A provider id (not "model-free"): engages() is true (the summarizer IS
        // configured) — the fallback is handled at maybe_spawn, not at this gate.
        c.summarizer.engine = "my-api-provider".to_owned();
        assert!(
            engages(&c),
            "provider id (not model-free) + reprune → engages"
        );
    }

    #[tokio::test]
    async fn maybe_spawn_proceeds_for_unverified_nonapproved_model() {
        // A model in NEITHER approved/warn/disqualified (e.g. llama3.2:3b) is the
        // user's explicit opt-in: warn "unverified fidelity" but still PROCEED (unlike
        // a DISQUALIFIED model, which is refused).
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                serde_json::json!({"message": {"content": "GOAL: approach B chosen."}}),
            ))
            .mount(&server)
            .await;
        let cfg = spawn_cfg(server.uri(), "llama3.2:3b"); // unverified, not in any list
        assert!(!APPROVED_MODELS.contains(&"llama3.2:3b"));
        assert!(!DISQUALIFIED_MODELS.contains(&"llama3.2:3b"));
        assert!(!WARN_MODELS.contains(&"llama3.2:3b"));
        let body = body_of(6);
        let cache = warmed_cache("s", &body, &cfg);
        maybe_spawn_summarization(
            cache.clone(),
            "s".into(),
            cfg,
            Bytes::from(body),
            crate::ledger::Ledger::disabled(),
        );
        await_settled(&cache, "s").await;
        assert!(
            cache.get("s").unwrap().summary_slice_end().is_some(),
            "an unverified (non-disqualified) model must warn-but-proceed"
        );
    }

    #[tokio::test]
    async fn maybe_spawn_warns_but_proceeds_for_harm_failing_opt_down() {
        // qwen3.5:2b is APPROVED (allowed) but in WARN_MODELS — unlike a DISQUALIFIED
        // model it must still PROCEED and install a summary (the warning is advisory).
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                serde_json::json!({"message": {"content": "GOAL: approach B chosen."}}),
            ))
            .mount(&server)
            .await;
        let cfg = spawn_cfg(server.uri(), "qwen3.5:2b"); // WARN, not DISQUALIFIED
        let body = body_of(6);
        let cache = warmed_cache("s", &body, &cfg);
        maybe_spawn_summarization(
            cache.clone(),
            "s".into(),
            cfg,
            Bytes::from(body),
            crate::ledger::Ledger::disabled(),
        );
        await_settled(&cache, "s").await;
        assert!(
            cache.get("s").unwrap().summary_slice_end().is_some(),
            "a warned-but-approved model must still proceed and install a summary"
        );
    }

    /// Drive two summarization rounds on a growing prose session and return the
    /// resulting state (segment count + the replayed body of the larger turn). Round
    /// 1's model call returns `resp1`, round 2's returns `resp2` (distinct so a test
    /// can prove which segments survive). Round 1 installs the first summary; round 2
    /// re-summarizes the grown delta (append in accumulator mode, replace otherwise).
    async fn two_round_summary(accumulator: bool, resp1: &str, resp2: &str) -> (usize, Vec<Value>) {
        use serde_json::json;
        let server = MockServer::start().await;
        // wiremock evaluates mocks in MOUNT order, first match wins. Mount the
        // round-1 response FIRST limited to a single call (up_to_n_times(1)); once it
        // is exhausted the round-2 fallback (mounted second) answers every later call.
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"message": {"content": resp1}})),
            )
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"message": {"content": resp2}})),
            )
            .mount(&server)
            .await;
        let cfg = {
            let mut c = crate::config::profile_baseline("default");
            c.summarizer = SummarizerConfig {
                engine: "local".to_owned(),
                trigger_bytes: 10,
                keep_recent_turns: 2,
                timeout_secs: 5,
                resummarize_after_bytes: 200, // small so the grown delta triggers
                accumulator,
                local: SummarizerLocalConfig {
                    endpoint: server.uri(),
                    model: "qwen3.5:4b".to_owned(),
                    ..Default::default()
                },
                ..Default::default()
            };
            std::sync::Arc::new(c)
        };

        // Round 1: warm the cache (first checkpoint) then summarize.
        let body1 = serde_json::to_vec(&json!({"messages": prose_convo(8)})).unwrap();
        let cache = warmed_cache("s", &body1, &cfg);
        maybe_spawn_summarization(
            cache.clone(),
            "s".into(),
            cfg.clone(),
            Bytes::from(body1),
            crate::ledger::Ledger::disabled(),
        );
        await_settled(&cache, "s").await;
        assert_eq!(
            cache.get("s").unwrap().summary_segment_count(),
            1,
            "round 1 installs exactly one segment"
        );

        // Round 2: the session grew (append-only); re-checkpoint then re-summarize.
        let msgs2 = prose_convo(16);
        let body2 = serde_json::to_vec(&json!({"messages": msgs2})).unwrap();
        {
            let mut st = cache.get_mut("s").unwrap();
            let _ =
                crate::reprune::stable_apply_to_body(&body2, &cfg, &mut st, cfg.reprune.threshold);
        }
        maybe_spawn_summarization(
            cache.clone(),
            "s".into(),
            cfg.clone(),
            Bytes::from(body2.clone()),
            crate::ledger::Ledger::disabled(),
        );
        await_settled(&cache, "s").await;
        let segs = cache.get("s").unwrap().summary_segment_count();

        // Replay the larger turn and return it so the caller can assert validity.
        let replayed = match crate::reprune::stable_apply_to_body(
            &body2,
            &cfg,
            &mut cache.get_mut("s").unwrap(),
            cfg.reprune.threshold,
        ) {
            crate::strategies::BodyOutcome::Mutated { bytes, .. } => {
                serde_json::from_slice::<Value>(&bytes).unwrap()["messages"]
                    .as_array()
                    .unwrap()
                    .clone()
            }
            crate::strategies::BodyOutcome::Unchanged => msgs2,
        };
        (segs, replayed)
    }

    #[tokio::test]
    async fn accumulator_appends_and_preserves_the_oldest_segments_fact() {
        // round 1 captures GOAL_SEG0_FACT; round 2 captures GOAL_SEG1_FACT.
        let (segs, replayed) = two_round_summary(
            true,
            "GOAL: GOAL_SEG0_FACT ledger",
            "GOAL: GOAL_SEG1_FACT writer",
        )
        .await;
        assert_eq!(segs, 2, "accumulator mode APPENDS the delta → two segments");
        crate::pairing::PairingIndex::build(&replayed)
            .validate()
            .expect("accumulator replay must not orphan pairs");
        let roles: Vec<&str> = replayed
            .iter()
            .map(|m| m["role"].as_str().unwrap())
            .collect();
        for w in roles.windows(2) {
            assert_ne!(
                w[0], w[1],
                "roles must alternate after multi-segment splice"
            );
        }
        let s = serde_json::to_string(&replayed).unwrap();
        // FIDELITY: the oldest segment's fact SURVIVES re-summarization (frozen),
        // alongside the new delta's fact — this is the whole point of the accumulator.
        assert!(
            s.contains("GOAL_SEG0_FACT"),
            "oldest segment's fact must persist"
        );
        assert!(
            s.contains("GOAL_SEG1_FACT"),
            "the appended delta's fact must be present"
        );
        assert_eq!(
            s.matches("local-model compaction of original turns")
                .count(),
            2,
            "both frozen segments appear in the replay"
        );
    }

    #[tokio::test]
    async fn default_replace_drops_the_oldest_fact_demonstrating_the_drift() {
        let (segs, replayed) = two_round_summary(
            false,
            "GOAL: GOAL_SEG0_FACT ledger",
            "GOAL: GOAL_SEG1_FACT writer",
        )
        .await;
        assert_eq!(segs, 1, "default (replace) mode keeps a single segment");
        crate::pairing::PairingIndex::build(&replayed)
            .validate()
            .expect("replace replay must not orphan pairs");
        let s = serde_json::to_string(&replayed).unwrap();
        // The control: replace mode DROPS the round-1 fact (the drift the accumulator
        // fixes) and keeps only the most-recent summary.
        assert!(s.contains("GOAL_SEG1_FACT"), "latest summary present");
        assert!(
            !s.contains("GOAL_SEG0_FACT"),
            "replace mode drops the oldest fact — the drift the accumulator prevents"
        );
    }

    /// A session whose WIDEST summarizable slice is tool-dominated (the 0.6 gate
    /// would SKIP it) but whose tail is reasoning-dense — the density-aware rescue
    /// case. `tool_pairs` big tool_results up front, then `prose_pairs` prose turns.
    fn tool_then_prose_convo(tool_pairs: usize, prose_pairs: usize) -> Vec<Value> {
        use serde_json::json;
        let mut m = vec![json!({"role":"user","content":[{"type":"text","text":"go"}]})];
        for i in 0..tool_pairs {
            let id = format!("t{i}");
            m.push(json!({"role":"assistant","content":[
                {"type":"tool_use","id":id,"name":"Bash","input":{"command":format!("run {i}")}}
            ]}));
            m.push(json!({"role":"user","content":[
                {"type":"tool_result","tool_use_id":id,"content":"X".repeat(3000)}
            ]}));
        }
        for i in 0..prose_pairs {
            let prose = format!("Reasoning {i}: chose approach B for constraint {i}. ").repeat(40);
            m.push(json!({"role":"assistant","content":[{"type":"text","text": prose}]}));
            m.push(json!({"role":"user","content":[{"type":"text","text":"ok"}]}));
        }
        m
    }

    /// Density-aware select_slice (gateway path): when the WIDEST slice is too
    /// tool-dominated to summarize but a reasoning-dense sub-window exists, the
    /// fallback advances `start` and a summary IS installed — recovering reduction
    /// that the 0.6 gate would otherwise skip. The control (uniformly tool-heavy,
    /// no dense window) installs NOTHING, proving the gate still skips correctly.
    #[tokio::test]
    async fn density_aware_rescues_tool_heavy_widest_with_a_dense_tail() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                serde_json::json!({"message": {"content": "GOAL: rescued reasoning summary"}}),
            ))
            .mount(&server)
            .await;
        let cfg = spawn_cfg(server.uri(), "qwen3.5:4b");

        // RESCUE case: widest slice is genuinely tool-heavy (assert via the private
        // scorer), yet a dense tail exists → the fallback installs one summary.
        let rescue = tool_then_prose_convo(4, 4);
        let (ws, end) =
            slice::select_slice(&rescue, cfg.summarizer.keep_recent_turns, rescue.len())
                .expect("rescue convo must be sliceable");
        assert!(
            tool_result_fraction(&rescue[ws..end]) > MAX_TOOL_FRACTION,
            "the widest slice must be tool-dominated for this to be a real rescue"
        );
        let body = serde_json::to_vec(&serde_json::json!({"messages": rescue})).unwrap();
        let cache = warmed_cache("r", &body, &cfg);
        maybe_spawn_summarization(
            cache.clone(),
            "r".into(),
            cfg.clone(),
            Bytes::from(body),
            crate::ledger::Ledger::disabled(),
        );
        await_settled(&cache, "r").await;
        assert_eq!(
            cache.get("r").unwrap().summary_segment_count(),
            1,
            "density-aware fallback must rescue the tool-heavy widest slice (1 segment)"
        );

        // CONTROL: uniformly tool-heavy, no reasoning-dense sub-window → still skipped.
        let control = tool_convo(8, &"X".repeat(3000));
        let cbody = serde_json::to_vec(&serde_json::json!({"messages": control})).unwrap();
        let ccache = warmed_cache("c", &cbody, &cfg);
        maybe_spawn_summarization(
            ccache.clone(),
            "c".into(),
            cfg.clone(),
            Bytes::from(cbody),
            crate::ledger::Ledger::disabled(),
        );
        await_settled(&ccache, "c").await;
        assert_eq!(
            ccache.get("c").unwrap().summary_segment_count(),
            0,
            "no dense sub-window → the gate still skips (no false rescue)"
        );
    }

    #[test]
    fn tool_result_fraction_distinguishes_tool_vs_reasoning() {
        use serde_json::json;
        let tool_heavy = vec![
            json!({"role":"assistant","content":[{"type":"text","text":"short reason"}]}),
            json!({"role":"user","content":[{"type":"tool_result","tool_use_id":"t","content":"X".repeat(2000)}]}),
        ];
        assert!(
            tool_result_fraction(&tool_heavy) > 0.9,
            "bulky tool_result dominates"
        );
        let reasoning_heavy = vec![
            json!({"role":"assistant","content":[{"type":"text","text":"Y".repeat(2000)}]}),
            json!({"role":"user","content":[{"type":"tool_result","tool_use_id":"t","content":"ok"}]}),
        ];
        assert!(
            tool_result_fraction(&reasoning_heavy) < 0.05,
            "reasoning dominates"
        );
    }

    #[test]
    fn num_ctx_for_floors_and_clamps() {
        let cap = 25_600; // the default local.max_num_ctx
        assert_eq!(num_ctx_for(100, cap), 4096, "tiny prompt floors at 4096");
        assert_eq!(num_ctx_for(20_000, cap), 8000, "len/2.5 in the mid-range");
        assert_eq!(
            num_ctx_for(1_000_000, cap),
            cap,
            "huge prompt clamps to the configured cap"
        );
        assert_eq!(
            num_ctx_for(10_000_000, 1_000_000),
            MAX_NUM_CTX_CEILING,
            "a stray huge cap is itself clamped to the hard ceiling"
        );
    }

    fn tool_convo(pairs: usize, big: &str) -> Vec<Value> {
        use serde_json::json;
        let mut m = vec![json!({"role":"user","content":[{"type":"text","text":"go"}]})];
        for i in 0..pairs {
            let id = format!("t{i}");
            m.push(json!({"role":"assistant","content":[
                {"type":"tool_use","id":id,"name":"Bash","input":{"command":format!("run {i}")}}
            ]}));
            m.push(json!({"role":"user","content":[
                {"type":"tool_result","tool_use_id":id,"content": big}
            ]}));
        }
        m
    }

    #[tokio::test]
    async fn maybe_spawn_skips_when_delta_below_byte_threshold() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"message": {"content": "done"}})),
            )
            .expect(0) // adaptive gate: delta under the byte threshold → no model call
            .mount(&server)
            .await;
        let mut cfg = (*spawn_cfg(server.uri(), "qwen3.5:4b")).clone();
        cfg.summarizer.resummarize_after_bytes = usize::MAX; // any delta is "too small"
        let cfg = std::sync::Arc::new(cfg);
        let msgs = prose_convo(6);
        let body = serde_json::to_vec(&serde_json::json!({"messages": &msgs})).unwrap();
        let cache = warmed_cache("s", &body, &cfg);
        // Pre-install a summary over an early sub-range so summary_slice_end is Some
        // and still anchors to the (unchanged) body.
        {
            let mut st = cache.get_mut("s").unwrap();
            st.set_summary(slice::SummaryDecision::new(&msgs, 1, 5, "prior summary").unwrap());
        }
        let before = cache.get("s").unwrap().summary_slice_end();
        assert_eq!(before, Some(5));
        maybe_spawn_summarization(
            cache.clone(),
            "s".into(),
            cfg,
            Bytes::from(body),
            crate::ledger::Ledger::disabled(),
        );
        await_settled(&cache, "s").await;
        assert_eq!(
            cache.get("s").unwrap().summary_slice_end(),
            before,
            "a prunable delta below resummarize_after_bytes must NOT re-summarize"
        );
    }

    #[tokio::test]
    async fn maybe_spawn_skips_tool_heavy_slice_without_calling_model() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"message": {"content": "done"}})),
            )
            .expect(0) // tool-fraction early-skip → no model call
            .mount(&server)
            .await;
        let cfg = spawn_cfg(server.uri(), "qwen3.5:4b");
        let big = "X".repeat(3000);
        let body =
            serde_json::to_vec(&serde_json::json!({"messages": tool_convo(6, &big)})).unwrap();
        let cache = warmed_cache("s", &body, &cfg);
        maybe_spawn_summarization(
            cache.clone(),
            "s".into(),
            cfg,
            Bytes::from(body),
            crate::ledger::Ledger::disabled(),
        );
        await_settled(&cache, "s").await;
        assert!(
            cache.get("s").unwrap().summary_slice_end().is_none(),
            "a tool_result-dominated slice must skip the model"
        );
    }

    #[tokio::test]
    async fn maybe_spawn_rejects_summary_larger_than_model_free() {
        let server = MockServer::start().await;
        let huge = "x ".repeat(50_000); // loses the summary_is_smaller size gate
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"message": {"content": huge}})),
            )
            .mount(&server)
            .await;
        let cfg = spawn_cfg(server.uri(), "qwen3.5:4b");
        let body = body_of(6);
        let cache = warmed_cache("s", &body, &cfg);
        maybe_spawn_summarization(
            cache.clone(),
            "s".into(),
            cfg,
            Bytes::from(body),
            crate::ledger::Ledger::disabled(),
        );
        await_settled(&cache, "s").await;
        let st = cache.get("s").unwrap();
        assert!(
            !st.summary_inflight(),
            "in-flight flag must clear even on reject"
        );
        assert!(
            st.summary_slice_end().is_none(),
            "an oversized summary must be rejected by the size gate"
        );
    }

    // ── run_cascade tests ─────────────────────────────────────────────────────

    /// Build a `SummarizerConfig` with a single named provider (id=`"test-api"`)
    /// pointing at `api_base` (wiremock), local pointing at `local_base`.
    fn cascade_cfg(
        api_base: String,
        local_base: String,
        engine: &str,
        fallback: Vec<&str>,
        api_key_env: &str,
    ) -> SummarizerConfig {
        SummarizerConfig {
            engine: engine.to_owned(),
            fallback: fallback.iter().map(|s| (*s).to_owned()).collect(),
            trigger_bytes: 10,
            keep_recent_turns: 2,
            timeout_secs: 5,
            local: crate::config::SummarizerLocalConfig {
                endpoint: local_base,
                model: "qwen3.5:4b".to_owned(),
                ..Default::default()
            },
            providers: vec![crate::config::SummarizerProviderConfig {
                id: "test-api".to_owned(),
                style: "anthropic".to_owned(),
                base_url: api_base,
                full_url: None,
                model: "claude-haiku-4-20250514".to_owned(),
                api_key_env: api_key_env.to_owned(),
                timeout_secs: 5,
            }],
            ..Default::default()
        }
    }

    fn anthropic_ok_response() -> ResponseTemplate {
        ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "content": [{"type": "text", "text": "GOAL: approach B chosen for the ledger."}]
        }))
    }

    fn local_ok_response() -> ResponseTemplate {
        ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "message": {"content": "GOAL: approach B chosen for the ledger."}
        }))
    }

    /// Helper: set the env var for `api_key_env`.
    fn set_api_key(api_key_env: &str, value: &str) {
        // SAFETY: tests are single-threaded per-test; each uses a unique env var name.
        unsafe { std::env::set_var(api_key_env, value) };
    }

    #[tokio::test]
    async fn cascade_api_success_returns_some_and_does_not_call_local() {
        let api_server = MockServer::start().await;
        let local_server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(anthropic_ok_response())
            .expect(1) // api called exactly once
            .mount(&api_server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .respond_with(local_ok_response())
            .expect(0) // local must NOT be called
            .mount(&local_server)
            .await;

        let key_env = "CASCADE_TEST_API_SUCCESS";
        set_api_key(key_env, "test-key");
        let cfg = cascade_cfg(
            api_server.uri(),
            local_server.uri(),
            "test-api",
            vec!["local"],
            key_env,
        );
        let result = run_cascade(&cfg, "prompt".to_owned()).await;
        assert!(result.is_some(), "api success must return Some");

        api_server.verify().await;
        local_server.verify().await;
    }

    #[tokio::test]
    async fn cascade_skips_disqualified_local_and_tries_next_engine() {
        // Regression (review H1): a disqualified Local engine must be SKIPPED (never
        // called) and the cascade must fall through to the next engine — a disqualified
        // local fallback must NOT kill an Api primary, and a disqualified local primary
        // must hand off to its fallback rather than abort the whole cascade.
        let api_server = MockServer::start().await;
        let local_server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .respond_with(local_ok_response())
            .expect(0) // disqualified → must NOT be called
            .mount(&local_server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(anthropic_ok_response())
            .expect(1) // the fallback Api is tried instead
            .mount(&api_server)
            .await;

        let key_env = "CASCADE_TEST_DISQUALIFIED_LOCAL";
        set_api_key(key_env, "test-key");
        let mut cfg = cascade_cfg(
            api_server.uri(),
            local_server.uri(),
            "local",
            vec!["test-api"],
            key_env,
        );
        cfg.local.model = "granite4.1:8b".to_owned(); // a DISQUALIFIED family
        let result = run_cascade(&cfg, "prompt".to_owned()).await;
        assert!(
            result.is_some(),
            "disqualified local must be skipped and the Api fallback used"
        );

        api_server.verify().await;
        local_server.verify().await;
    }

    #[tokio::test]
    async fn cascade_api_error_falls_to_local() {
        let api_server = MockServer::start().await;
        let local_server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(ResponseTemplate::new(500)) // api fails
            .expect(1)
            .mount(&api_server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .respond_with(local_ok_response()) // local succeeds
            .expect(1)
            .mount(&local_server)
            .await;

        let key_env = "CASCADE_TEST_API_TO_LOCAL";
        set_api_key(key_env, "test-key");
        let cfg = cascade_cfg(
            api_server.uri(),
            local_server.uri(),
            "test-api",
            vec!["local"],
            key_env,
        );
        let result = run_cascade(&cfg, "prompt".to_owned()).await;
        assert!(
            result.is_some(),
            "local fallback must return Some after api error"
        );

        api_server.verify().await;
        local_server.verify().await;
    }

    #[tokio::test]
    async fn cascade_model_free_terminal_returns_none() {
        // "model-free" as primary engine: cascade terminates immediately with None.
        // No mock needed — the cascade must not make any HTTP call.
        let cfg = cascade_cfg(
            "http://127.0.0.1:1".to_owned(),
            "http://127.0.0.1:1".to_owned(),
            "model-free",
            vec![],
            "",
        );
        let result = run_cascade(&cfg, "prompt".to_owned()).await;
        assert!(
            result.is_none(),
            "ModelFree primary must return None immediately"
        );
    }

    #[tokio::test]
    async fn cascade_model_free_in_fallback_truncates_chain_and_returns_none() {
        // engine=Api, fallback=[ModelFree]: api errors → ModelFree terminal → None.
        // Local (if it were after ModelFree in fallback) must not be called.
        let api_server = MockServer::start().await;
        let local_server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(ResponseTemplate::new(503))
            .expect(1)
            .mount(&api_server)
            .await;
        // Verify local is never called: use .expect(0).
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .respond_with(local_ok_response())
            .expect(0) // local must NOT be called — ModelFree terminal fires first
            .mount(&local_server)
            .await;

        let key_env = "CASCADE_MF_TERMINAL";
        set_api_key(key_env, "key");
        let cfg = cascade_cfg(
            api_server.uri(),
            local_server.uri(),
            "test-api",
            vec!["model-free", "local"],
            key_env,
        );
        let result = run_cascade(&cfg, "prompt".to_owned()).await;
        assert!(
            result.is_none(),
            "ModelFree in fallback truncates chain → None"
        );

        api_server.verify().await;
        local_server.verify().await;
    }

    #[tokio::test]
    async fn cascade_deduplicates_repeated_engines() {
        // engine="local", fallback=["local"]: Local must only be called once.
        let local_server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .respond_with(local_ok_response())
            .expect(1) // dedup: only called once despite appearing twice
            .mount(&local_server)
            .await;

        let cfg = cascade_cfg(
            "http://127.0.0.1:1".to_owned(),
            local_server.uri(),
            "local",
            vec!["local"],
            "",
        );
        let result = run_cascade(&cfg, "prompt".to_owned()).await;
        assert!(result.is_some());
        local_server.verify().await;
    }

    #[tokio::test]
    async fn cascade_missing_api_key_falls_to_local() {
        // API provider with unset env var → Unreachable → falls through to Local.
        let local_server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .respond_with(local_ok_response())
            .expect(1)
            .mount(&local_server)
            .await;

        // SAFETY: test-only, unique env var name, single-threaded test runtime.
        unsafe { std::env::remove_var("CASCADE_MISSING_KEY_ENV") };
        let cfg = cascade_cfg(
            "http://127.0.0.1:1".to_owned(),
            local_server.uri(),
            "test-api",
            vec!["local"],
            "CASCADE_MISSING_KEY_ENV",
        );
        let result = run_cascade(&cfg, "prompt".to_owned()).await;
        assert!(result.is_some(), "local fallback after missing api key");
        local_server.verify().await;
    }

    // ── Multi-provider cascade tests ──────────────────────────────────────────

    /// Build a `SummarizerConfig` with multiple named providers.
    fn multi_provider_cfg(
        providers: Vec<crate::config::SummarizerProviderConfig>,
        local_base: String,
        engine: &str,
        fallback: Vec<&str>,
    ) -> SummarizerConfig {
        SummarizerConfig {
            engine: engine.to_owned(),
            fallback: fallback.iter().map(|s| (*s).to_owned()).collect(),
            trigger_bytes: 10,
            keep_recent_turns: 2,
            timeout_secs: 5,
            local: crate::config::SummarizerLocalConfig {
                endpoint: local_base,
                model: "qwen3.5:4b".to_owned(),
                ..Default::default()
            },
            providers,
            ..Default::default()
        }
    }

    fn anthropic_provider(
        base_url: String,
        api_key_env: &str,
    ) -> crate::config::SummarizerProviderConfig {
        crate::config::SummarizerProviderConfig {
            id: "anthropic".to_owned(),
            style: "anthropic".to_owned(),
            base_url,
            full_url: None,
            model: "claude-haiku-4-20250514".to_owned(),
            api_key_env: api_key_env.to_owned(),
            timeout_secs: 5,
        }
    }

    fn openai_provider(
        base_url: String,
        api_key_env: &str,
    ) -> crate::config::SummarizerProviderConfig {
        crate::config::SummarizerProviderConfig {
            id: "openai".to_owned(),
            style: "openai".to_owned(),
            base_url,
            full_url: None,
            model: "gpt-4o-mini".to_owned(),
            api_key_env: api_key_env.to_owned(),
            timeout_secs: 5,
        }
    }

    fn third_provider(
        base_url: String,
        api_key_env: &str,
    ) -> crate::config::SummarizerProviderConfig {
        crate::config::SummarizerProviderConfig {
            id: "third".to_owned(),
            style: "anthropic".to_owned(),
            base_url,
            full_url: None,
            model: "claude-haiku-4-20250514".to_owned(),
            api_key_env: api_key_env.to_owned(),
            timeout_secs: 5,
        }
    }

    #[tokio::test]
    async fn multi_provider_primary_ok_skips_fallbacks() {
        // Primary API provider answers → cascade returns immediately; second
        // provider's mock receives 0 calls.
        let provider_a = MockServer::start().await;
        let provider_b = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(anthropic_ok_response())
            .expect(1) // primary called exactly once
            .mount(&provider_a)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(anthropic_ok_response())
            .expect(0) // fallback must NOT be called
            .mount(&provider_b)
            .await;

        let key_a = "MULTI_PRIMARY_OK_A";
        let key_b = "MULTI_PRIMARY_OK_B";
        set_api_key(key_a, "key-a");
        set_api_key(key_b, "key-b");

        let cfg = multi_provider_cfg(
            vec![
                anthropic_provider(provider_a.uri(), key_a),
                openai_provider(provider_b.uri(), key_b),
            ],
            "http://127.0.0.1:1".to_owned(),
            "anthropic",
            vec!["openai"],
        );
        let result = run_cascade(&cfg, "prompt".to_owned()).await;
        assert!(result.is_some(), "primary success must return Some");
        provider_a.verify().await;
        provider_b.verify().await;
    }

    #[tokio::test]
    async fn multi_provider_primary_500_falls_to_second() {
        // Provider-A mock returns 500 → Provider-B mock returns 200 with a valid summary.
        let provider_a = MockServer::start().await;
        let provider_b = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(ResponseTemplate::new(500))
            .expect(1)
            .mount(&provider_a)
            .await;
        // Provider B uses OpenAI style (different path)
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "choices": [{"message": {"content": "GOAL: fallback summary"}}]
            })))
            .expect(1)
            .mount(&provider_b)
            .await;

        let key_a = "MULTI_500_A";
        let key_b = "MULTI_500_B";
        set_api_key(key_a, "key-a");
        set_api_key(key_b, "key-b");

        let cfg = multi_provider_cfg(
            vec![
                anthropic_provider(provider_a.uri(), key_a),
                openai_provider(provider_b.uri(), key_b),
            ],
            "http://127.0.0.1:1".to_owned(),
            "anthropic",
            vec!["openai"],
        );
        let result = run_cascade(&cfg, "prompt".to_owned()).await;
        assert!(
            result.is_some(),
            "fallback provider must return Some after primary 500"
        );
        provider_a.verify().await;
        provider_b.verify().await;
    }

    #[tokio::test]
    async fn multi_provider_both_fail_falls_to_local() {
        // Both API providers return 500 → local engine (mocked) returns a valid summary.
        let provider_a = MockServer::start().await;
        let provider_b = MockServer::start().await;
        let local_server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(ResponseTemplate::new(500))
            .expect(1)
            .mount(&provider_a)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(500))
            .expect(1)
            .mount(&provider_b)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .respond_with(local_ok_response())
            .expect(1)
            .mount(&local_server)
            .await;

        let key_a = "MULTI_BOTH_FAIL_A";
        let key_b = "MULTI_BOTH_FAIL_B";
        set_api_key(key_a, "key-a");
        set_api_key(key_b, "key-b");

        let cfg = multi_provider_cfg(
            vec![
                anthropic_provider(provider_a.uri(), key_a),
                openai_provider(provider_b.uri(), key_b),
            ],
            local_server.uri(),
            "anthropic",
            vec!["openai", "local"],
        );
        let result = run_cascade(&cfg, "prompt".to_owned()).await;
        assert!(
            result.is_some(),
            "local fallback must return Some after both providers fail"
        );
        provider_a.verify().await;
        provider_b.verify().await;
        local_server.verify().await;
    }

    #[tokio::test]
    async fn multi_provider_all_fail_returns_none() {
        // All providers + local return errors → cascade reaches "model-free" terminal
        // → returns None.
        let provider_a = MockServer::start().await;
        let local_server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(ResponseTemplate::new(500))
            .expect(1)
            .mount(&provider_a)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .respond_with(ResponseTemplate::new(500))
            .expect(1)
            .mount(&local_server)
            .await;

        let key_a = "MULTI_ALL_FAIL_A";
        set_api_key(key_a, "key-a");

        let cfg = multi_provider_cfg(
            vec![anthropic_provider(provider_a.uri(), key_a)],
            local_server.uri(),
            "anthropic",
            vec!["local"],
        );
        let result = run_cascade(&cfg, "prompt".to_owned()).await;
        assert!(result.is_none(), "all engines errored → must return None");
        provider_a.verify().await;
        local_server.verify().await;
    }

    #[tokio::test]
    async fn multi_provider_key_missing_skips_no_call() {
        // api_key_env unset for provider A → call_api returns Unreachable immediately
        // (no network call) → falls through to provider B which succeeds.
        let provider_a = MockServer::start().await;
        let provider_b = MockServer::start().await;

        // Provider A has NO mock registered — if it were called it would get a connection
        // refused / HTTP error, but wiremock's verify(expect(0)) confirms it wasn't.
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(anthropic_ok_response())
            .expect(0) // key missing → no HTTP call
            .mount(&provider_a)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "choices": [{"message": {"content": "GOAL: second provider ok"}}]
            })))
            .expect(1)
            .mount(&provider_b)
            .await;

        // Ensure key_a is NOT set.
        // SAFETY: test-only unique var name.
        unsafe { std::env::remove_var("MULTI_KEY_MISSING_A") };
        let key_b = "MULTI_KEY_MISSING_B";
        set_api_key(key_b, "key-b");

        let cfg = multi_provider_cfg(
            vec![
                anthropic_provider(provider_a.uri(), "MULTI_KEY_MISSING_A"),
                openai_provider(provider_b.uri(), key_b),
            ],
            "http://127.0.0.1:1".to_owned(),
            "anthropic",
            vec!["openai"],
        );
        let result = run_cascade(&cfg, "prompt".to_owned()).await;
        assert!(
            result.is_some(),
            "provider B must succeed when provider A key is missing"
        );
        provider_a.verify().await;
        provider_b.verify().await;
    }

    #[tokio::test]
    async fn multi_provider_cascade_order_preserved() {
        // Configure [A, B, C]; A and B fail; assert C is called exactly once.
        let provider_a = MockServer::start().await;
        let provider_b = MockServer::start().await;
        let provider_c = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(ResponseTemplate::new(500))
            .expect(1) // A fails
            .mount(&provider_a)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(500))
            .expect(1) // B fails
            .mount(&provider_b)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(anthropic_ok_response())
            .expect(1) // C succeeds
            .mount(&provider_c)
            .await;

        let key_a = "MULTI_ORDER_A";
        let key_b = "MULTI_ORDER_B";
        let key_c = "MULTI_ORDER_C";
        set_api_key(key_a, "key-a");
        set_api_key(key_b, "key-b");
        set_api_key(key_c, "key-c");

        let cfg = multi_provider_cfg(
            vec![
                anthropic_provider(provider_a.uri(), key_a),
                openai_provider(provider_b.uri(), key_b),
                third_provider(provider_c.uri(), key_c),
            ],
            "http://127.0.0.1:1".to_owned(),
            "anthropic",
            vec!["openai", "third"],
        );
        let result = run_cascade(&cfg, "prompt".to_owned()).await;
        assert!(result.is_some(), "third provider must succeed");
        provider_a.verify().await;
        provider_b.verify().await;
        provider_c.verify().await;
    }

    // ── maybe_spawn_summarization integration smoke test ──────────────────────

    /// Build a `SummarizerConfig` with two named API providers for the gateway
    /// integration path. Mirrors `spawn_cfg` but uses API providers instead of local.
    fn spawn_cfg_two_providers(
        provider_a: crate::config::SummarizerProviderConfig,
        provider_b: crate::config::SummarizerProviderConfig,
    ) -> std::sync::Arc<crate::config::Config> {
        let mut cfg = crate::config::profile_baseline("default");
        cfg.summarizer = SummarizerConfig {
            engine: provider_a.id.clone(),
            fallback: vec![provider_b.id.clone()],
            trigger_bytes: 10,
            keep_recent_turns: 2,
            timeout_secs: 5,
            resummarize_after_bytes: 1, // always re-summarize
            providers: vec![provider_a, provider_b],
            // local is not in the chain — point it nowhere reachable.
            local: crate::config::SummarizerLocalConfig {
                endpoint: "http://127.0.0.1:1".to_owned(),
                model: "qwen3.5:4b".to_owned(),
                ..Default::default()
            },
            ..Default::default()
        };
        std::sync::Arc::new(cfg)
    }

    /// Integration smoke test: `maybe_spawn_summarization` with two mocked API
    /// providers — provider A returns 500, provider B returns a valid summary.
    /// Config: engine="prov_a", fallback=["prov_b"], reprune=on, body over trigger_bytes.
    ///
    /// Asserts:
    /// - The ledger records outcome 'a' (accepted), inferred via `summary_slice_end().is_some()`.
    /// - Provider B's mock received exactly one call (fallback actually fired).
    /// - Provider A's mock received exactly one call (500 was attempted first).
    #[tokio::test]
    async fn multi_provider_gateway_cascade_installs_on_fallback_success() {
        let provider_a_mock = MockServer::start().await;
        let provider_b_mock = MockServer::start().await;

        // Provider A (anthropic style) — returns 500 to force fallback.
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(ResponseTemplate::new(500))
            .expect(1)
            .mount(&provider_a_mock)
            .await;

        // Provider B (openai style) — returns a valid summary.
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "choices": [{"message": {
                    "role": "assistant",
                    "content": "GOAL: the gateway fallback smoke test confirmed."
                }}]
            })))
            .expect(1)
            .mount(&provider_b_mock)
            .await;

        let key_a = "SMOKE_GATEWAY_A";
        let key_b = "SMOKE_GATEWAY_B";
        set_api_key(key_a, "key-a");
        set_api_key(key_b, "key-b");

        let prov_a = crate::config::SummarizerProviderConfig {
            id: "prov_a".to_owned(),
            style: "anthropic".to_owned(),
            base_url: provider_a_mock.uri(),
            full_url: None,
            model: "claude-haiku-4-20250514".to_owned(),
            api_key_env: key_a.to_owned(),
            timeout_secs: 5,
        };
        let prov_b = crate::config::SummarizerProviderConfig {
            id: "prov_b".to_owned(),
            style: "openai".to_owned(),
            base_url: provider_b_mock.uri(),
            full_url: None,
            model: "gpt-4o-mini".to_owned(),
            api_key_env: key_b.to_owned(),
            timeout_secs: 5,
        };

        let cfg = spawn_cfg_two_providers(prov_a, prov_b);

        // Build a prose body large enough to pass the trigger/size gates.
        // prose_convo(8) gives enough reasoning prose that a short summary beats
        // model-free pruning (identical to the local happy-path test).
        let body = body_of(8);
        let cache = warmed_cache("gw", &body, &cfg);

        maybe_spawn_summarization(
            cache.clone(),
            "gw".into(),
            cfg,
            Bytes::from(body),
            crate::ledger::Ledger::disabled(),
        );
        await_settled(&cache, "gw").await;

        // Outcome 'a' (accepted): summary was produced by provider B and installed.
        assert!(
            cache.get("gw").unwrap().summary_slice_end().is_some(),
            "provider B's fallback summary must be installed (ledger outcome = 'a')"
        );
        // Verify the mock call counts (provider A got 1 call → 500; provider B got 1 call → 200).
        provider_a_mock.verify().await;
        provider_b_mock.verify().await;
    }

    // ── preview_summary tests ─────────────────────────────────────────────────

    /// `preview_summary` with a local (ollama) engine: a wiremock server returns a
    /// short summary over a reasoning-dense slice-eligible body → `Some(PreviewSummary)`
    /// with `slice_after < slice_before`.
    #[tokio::test]
    async fn preview_summary_local_engine_returns_accepted_reduction() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "message": {"content": "GOAL: approach B chosen for the ledger."},
                "done": true
            })))
            .mount(&server)
            .await;

        let mut cfg = crate::config::profile_baseline("default");
        cfg.summarizer = SummarizerConfig {
            engine: "local".to_owned(),
            trigger_bytes: 10,
            keep_recent_turns: 2,
            timeout_secs: 5,
            local: crate::config::SummarizerLocalConfig {
                endpoint: server.uri(),
                model: "qwen3.5:4b".to_owned(),
                ..Default::default()
            },
            ..Default::default()
        };

        // prose_convo(6) produces a slice-eligible reasoning-dense body whose
        // model-free strategies can't compress significantly — a short summary beats it.
        let messages = prose_convo(6);
        let result = preview_summary(&messages, &cfg)
            .await
            .expect("preview_summary must not error");

        let ps = result.expect("a short summary over un-prunable prose must be accepted");
        assert!(
            ps.slice_after < ps.slice_before,
            "accepted summary must be smaller than the model-free baseline: \
             before={}, after={}",
            ps.slice_before,
            ps.slice_after
        );
        assert_eq!(ps.engine_kind, "local");
        assert!(ps.end > ps.start, "slice must cover at least one turn");
    }

    /// `preview_summary` returns `None` when `engine = "model-free"` (no model
    /// configured — nothing for the summarizer to contribute).
    #[tokio::test]
    async fn preview_summary_model_free_returns_none() {
        let mut cfg = crate::config::profile_baseline("default");
        cfg.summarizer.engine = "model-free".to_owned();

        let messages = prose_convo(6);
        let result = preview_summary(&messages, &cfg)
            .await
            .expect("preview_summary must not error");

        assert!(
            result.is_none(),
            "model-free engine must return None (caller prints the setup note)"
        );
    }

    /// `preview_summary` returns `None` when the session is too short to produce
    /// an eligible slice (fewer than keep_recent_turns + 2 assistant turns).
    #[tokio::test]
    async fn preview_summary_no_eligible_slice_returns_none() {
        let server = MockServer::start().await;
        // Register a mock but assert it is NEVER called (no network I/O).
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "message": {"content": "should not be called"},
                "done": true
            })))
            .expect(0)
            .mount(&server)
            .await;

        let mut cfg = crate::config::profile_baseline("default");
        cfg.summarizer = SummarizerConfig {
            engine: "local".to_owned(),
            trigger_bytes: 10,
            keep_recent_turns: 6, // protect 6 recent turns
            timeout_secs: 5,
            local: crate::config::SummarizerLocalConfig {
                endpoint: server.uri(),
                model: "qwen3.5:4b".to_owned(),
                ..Default::default()
            },
            ..Default::default()
        };

        // prose_convo(2) produces only ~5 messages — below keep_recent_turns+2 so
        // select_slice returns None.
        let messages = prose_convo(2);
        let result = preview_summary(&messages, &cfg)
            .await
            .expect("preview_summary must not error");

        assert!(
            result.is_none(),
            "a session too short for a slice must return None without calling the model"
        );
        server.verify().await; // confirm no HTTP call was made
    }
}
