//! `trimwire summarizer benchmark` — score a LOCAL ollama model OR a configured
//! API provider against a bundled, reasoning-dense corpus.
//!
//! **Scope:** scores local ollama models (via `call_model`) and configured API
//! providers (via `api::call_api`).  An API provider is detected when the value
//! passed to `--model` matches a `[[summarizer.providers]]` `id` in the user's
//! config.  Otherwise the value is treated as a local ollama tag.
//!
//! **Safety (API path):** each corpus slice is a REAL, PAID call on the user's own
//! provider key (never the Anthropic subscription token). Before any network I/O
//! the benchmark prints an explicit cost/scope warning.  Without `--yes` (i.e. from
//! the `trimwire summarizer benchmark` path) this is a **DRY RUN** — it prints what
//! it would send and exits.  Pass `--yes` via `trimwire share benchmark --yes` to
//! actually execute the API calls.
//!
//! **Non-goal:** benchmarking on the Anthropic subscription/OAuth token is
//! explicitly NOT supported.  The API path only uses the key from the user's own
//! `api_key_env` env var.  This is enforced by `api::call_api`.
//!
//! **Comparability disclaimer:** API-model scores are NOT directly comparable to
//! local-model scores.  The corpus is tuned for local summarizers (dense reasoning
//! excerpts, tight length budget, free-form FACTS-FIRST prompt).  Cloud models with
//! larger context windows, different temperature defaults, and no `num_ctx` cap may
//! score differently for structural reasons unrelated to summarisation quality.
//! Treat API scores as a DIRECTIONAL sanity-check within the same model family, not
//! a cross-backend ranking.
//!
//! It is a **sanity-check + a TRANSPARENT, directional rank** — NOT an
//! authoritative quality ranking. `APPROVED_MODELS` (the blind real-slice
//! gut-read in `src/summarizer/mod.rs`) stays the authority; this just lets
//! a user see how their own model behaves on the same kind of slice the
//! proxy summarizes, with every component shown so nothing hides behind one number.
//!
//! Composite `FCS = retention × compression × 100` (the council-vetted multiplicative
//! form already in `benchmark/model_bench.sh`: a verbatim copy OR a fact-dropper
//! both score ~0), behind a **false-done safety gate** — any unsupported completion
//! claim, or any slice that produced no usable summary, drops the model to the
//! bottom tier regardless of FCS (FCS alone can't see the harm-causing failure mode).
//!
//! Layering: this is a CLI-layer command. The model I/O is `call_model` (local) or
//! `api::call_api` (cloud); the scoring (`score_summary` / `aggregate`) is pure and
//! unit-tested with no live model or network.

use std::io::IsTerminal as _;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use trimwire::config::{Config, SummarizerLocalConfig, SummarizerProviderConfig};
use trimwire::summarizer::api::call_api;
use trimwire::summarizer::{
    APPROVED_MODELS, CompactorError, WARN_MODELS, build_prompt, call_model, fact_retention,
    harm_check::detect_false_done, is_disqualified,
};

/// Bump when ANY corpus slice or needle changes — shared results from different
/// corpus versions are NOT comparable. The collector/dashboard segregates by it.
/// Pinned to the embedded bytes by `corpus_bytes_match_pinned_sha`.
pub const CORPUS_VERSION: &str = "1";

/// SHA-256 (hex) of the embedded corpus files concatenated in `CORPUS_FILES`
/// order. A test pins this so editing a slice without bumping `CORPUS_VERSION`
/// fails CI (forces an intentional, reviewed version change). Also the runtime
/// HARD-gate for `--share`: a fork that edits the corpus without re-pinning this
/// (e.g. skipping the test) can't upload polluted rows under `corpus_version`.
const CORPUS_SHA: &str = "a19af1adb92b910f550a6617ce9ea89bdd30b38ee299e25394a2e4033ca9bbf8";

/// SHA-256 (hex) of the embedded corpus, in `CORPUS_FILES` order.
fn corpus_sha() -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    for f in CORPUS_FILES {
        h.update(f.as_bytes());
    }
    hex::encode(h.finalize())
}

/// Verbatim "summary is shorter than this fraction of the slice" floor: a summary
/// that barely shrinks the input isn't a summary (it's a copy) → not usable.
const MIN_USABLE_REDUCTION: f64 = 0.10;

/// One reasoning-dense evaluation slice + its end-state load-bearing needles.
/// (Extra JSON fields like `source` are ignored — serde skips unknown keys.)
#[derive(Debug, Deserialize)]
struct CorpusSlice {
    id: String,
    /// A faithful summary of this slice must make NO completion claim (the slice
    /// shows work announced/started but never finished). Documentation only —
    /// scoring measures `detect_false_done` on the model's output regardless.
    #[serde(default)]
    false_done_trap: bool,
    /// End-state facts a faithful summary MUST keep (matched case- and
    /// separator-insensitively via `fact_retention`).
    needles: Vec<String>,
    /// Pre-serialized excerpt text (the `### role` form `serialize_slice` emits).
    slice: String,
}

/// The embedded corpus. A glob can't be embedded, so each file is listed; keep
/// this order in sync with `CORPUS_SHA`.
const CORPUS_FILES: &[&str] = &[
    include_str!("../../benchmark/quality_corpus/s1_build_and_migrate.json"),
    include_str!("../../benchmark/quality_corpus/s2_false_done_trap.json"),
    include_str!("../../benchmark/quality_corpus/s3_decision_dense.json"),
    include_str!("../../benchmark/quality_corpus/s4_partial_progress.json"),
    include_str!("../../benchmark/quality_corpus/s5_noise_heavy.json"),
];

fn load_corpus() -> Result<Vec<CorpusSlice>> {
    CORPUS_FILES
        .iter()
        .enumerate()
        .map(|(i, raw)| {
            serde_json::from_str(raw).with_context(|| format!("parse embedded corpus slice #{i}"))
        })
        .collect()
}

/// One model's result on one slice. PURE given the summary text.
#[derive(Debug, Clone, Serialize)]
struct SliceScore {
    id: String,
    /// Whether this slice is a designated false-done trap (from the corpus).
    is_trap: bool,
    kept: usize,
    total: usize,
    in_chars: usize,
    out_chars: usize,
    /// Count of unsupported completion claims (`detect_false_done`).
    false_done: usize,
    usable: bool,
    /// `Some` when the model call failed (skip-compaction error); the slice then
    /// scores zero retention and is not usable.
    error: Option<String>,
    /// Coarse, content-free classification of `error` (none when `error` is None):
    /// `timeout | http_status | malformed | empty | unreachable | auth_or_config | other`.
    /// Never the raw error string.
    error_kind: Option<&'static str>,
}

/// Coarsen a summarizer call failure into a content-free `error_kind` enum value.
/// Never returns the raw message. Mirrors the closed set the collector validates.
fn classify_error(e: &CompactorError) -> &'static str {
    match e {
        CompactorError::Timeout => "timeout",
        CompactorError::HttpStatus(401 | 403) => "auth_or_config",
        CompactorError::HttpStatus(_) => "http_status",
        CompactorError::Malformed(_) => "malformed",
        CompactorError::EmptyResponse => "empty",
        CompactorError::Unreachable(_) => "unreachable",
    }
}

impl SliceScore {
    /// Fraction of bytes removed (`1 − out/in`); higher = tighter. 0 if it grew.
    fn reduction(in_chars: usize, out_chars: usize) -> f64 {
        if in_chars == 0 {
            0.0
        } else {
            (1.0 - out_chars as f64 / in_chars as f64).max(0.0)
        }
    }
}

/// Score a model's `summary` against one corpus `slice`. Pure — no I/O, no model.
fn score_summary(slice: &CorpusSlice, summary: &str) -> SliceScore {
    let needle_refs: Vec<&str> = slice.needles.iter().map(String::as_str).collect();
    let (kept, total) = fact_retention(summary, &needle_refs);
    let in_chars = slice.slice.chars().count();
    let out_chars = summary.chars().count();
    let false_done = detect_false_done(summary, &slice.slice).len();
    // Usable = non-empty AND it actually shrank. A verbatim/near-verbatim copy is
    // not a summary — the safety gate treats "not usable" like a false-done.
    let usable = !summary.trim().is_empty()
        && SliceScore::reduction(in_chars, out_chars) > MIN_USABLE_REDUCTION;
    SliceScore {
        id: slice.id.clone(),
        is_trap: slice.false_done_trap,
        kept,
        total,
        in_chars,
        out_chars,
        false_done,
        usable,
        error: None,
        error_kind: None,
    }
}

/// Score for a slice whose model call failed: zero retention, not usable. Captures
/// a coarse, content-free `error_kind` (never the raw message text in the share path).
fn errored_score(slice: &CorpusSlice, err: &CompactorError) -> SliceScore {
    SliceScore {
        id: slice.id.clone(),
        is_trap: slice.false_done_trap,
        kept: 0,
        total: slice.needles.len(),
        in_chars: slice.slice.chars().count(),
        out_chars: 0,
        false_done: 0,
        usable: false,
        error: Some(err.to_string()),
        error_kind: Some(classify_error(err)),
    }
}

/// A model's aggregate across the whole corpus.
#[derive(Debug, Clone, Serialize)]
struct ModelScore {
    model: String,
    /// Backend kind: `"local"` (ollama) or `"api"` (cloud provider).
    backend: String,
    /// `total_kept / total_needles` across all slices (0..1).
    retention: f64,
    /// Summary **compression** = `1 − Σout/Σin` across all slices (0..1). (Named
    /// `reduction` here for historical reasons; the public/UI term is "compression",
    /// distinct from the request-byte *reduction* trimwire reports in `stats`.)
    reduction: f64,
    false_done_total: usize,
    /// % of slices that produced a usable summary.
    usable_pct: f64,
    /// `retention × compression × 100`, BEFORE the safety gate.
    fcs: f64,
    /// Safety gate: any false-done OR any unusable slice → bottom tier.
    gated: bool,
    /// `gated ? 0 : fcs` — the value the rank sorts on.
    composite: f64,
    /// For `backend = "api"`: the provider's real model id (e.g. `claude-haiku-4-5`,
    /// `gpt-5.4-mini`, or an OpenRouter `vendor/model`). `None` for local rows.
    /// Used by the share path to coarsen `model_family` from the actual model — NOT
    /// the provider id. Never uploaded raw (coarsened first).
    provider_model: Option<String>,
    /// For `backend = "api"`: the provider's API style (`"anthropic"` | `"openai"`).
    /// Becomes the content-free `provider_style` field on the shared row. `None` local.
    provider_style: Option<String>,
    /// For `backend = "api"`: a coarse public route bucket derived from the provider
    /// base URL (`anthropic` | `openai` | `openrouter` | `azure` | `other`). Never the
    /// raw URL/host. `None` for local rows.
    provider_route: Option<String>,
    /// True when every corpus slice was scored (a `--max-calls` cap below the corpus
    /// size makes this false → `benchmark_scope = partial_corpus`). Local runs are
    /// always full.
    full_corpus: bool,
    slices: Vec<SliceScore>,
}

/// The single authoritative predicate for whether a benchmark row may leave this
/// machine. A row is uploadable only when it represents **real, clean** data:
///   - not an `api-dry-run` placeholder (an API model requested without `--yes`,
///     for which NO provider calls were made — there is nothing real to share), and
///   - every scored slice succeeded (no failed slices; a failure conflates a weak
///     model with a broken provider/config/network and is never uploaded yet).
///
/// `run_share` routes non-uploadable rows to distinct human-readable notices and
/// `continue`s before any payload is built/printed/posted. This function exists so
/// that invariant is one line, asserted before the post, and unit-testable.
fn is_uploadable_row(r: &ModelScore) -> bool {
    r.backend != "api-dry-run" && r.slices.iter().all(|s| s.error.is_none())
}

fn aggregate(model: String, backend: &str, slices: Vec<SliceScore>) -> ModelScore {
    let total_kept: usize = slices.iter().map(|s| s.kept).sum();
    let total_needles: usize = slices.iter().map(|s| s.total).sum();
    // Reduction only over slices that produced a summary: an errored slice has
    // out=0, which would otherwise read as 100% reduction and inflate the number
    // for a model that's actually failing (it's already gated; this just keeps the
    // displayed figure honest).
    let sum_in: usize = slices
        .iter()
        .filter(|s| s.error.is_none())
        .map(|s| s.in_chars)
        .sum();
    let sum_out: usize = slices
        .iter()
        .filter(|s| s.error.is_none())
        .map(|s| s.out_chars)
        .sum();
    let false_done_total: usize = slices.iter().map(|s| s.false_done).sum();
    let usable_count = slices.iter().filter(|s| s.usable).count();

    let retention = if total_needles == 0 {
        1.0
    } else {
        total_kept as f64 / total_needles as f64
    };
    let reduction = SliceScore::reduction(sum_in, sum_out);
    let usable_pct = if slices.is_empty() {
        0.0
    } else {
        usable_count as f64 / slices.len() as f64 * 100.0
    };
    let fcs = retention * reduction * 100.0;
    // The gate dominates the rank: FCS can't see a confident false-completion, so a
    // model that hallucinates "tests passed" must rank below an honest one even if
    // its FCS is higher.
    let gated = false_done_total > 0 || usable_count < slices.len();
    let composite = if gated { 0.0 } else { fcs };

    ModelScore {
        model,
        backend: backend.to_owned(),
        retention,
        reduction,
        false_done_total,
        usable_pct,
        fcs,
        gated,
        composite,
        // Defaults for local rows; the API path overrides provider_* + full_corpus.
        provider_model: None,
        provider_style: None,
        provider_route: None,
        full_corpus: true,
        slices,
    }
}

/// Sanitize a model tag for a filename (`qwen3.5:4b` → `qwen3.5_4b`).
fn safe_tag(model: &str) -> String {
    model.replace([':', '/'], "_")
}

/// Run one model over the whole corpus (live ollama via `call_model`), saving each
/// summary to `out` when given. Any model error degrades that slice to a zero score.
async fn run_model(
    lm: &SummarizerLocalConfig,
    timeout_secs: u64,
    corpus: &[CorpusSlice],
    out: Option<&Path>,
) -> ModelScore {
    use super::render;
    let mut scores = Vec::with_capacity(corpus.len());
    for slice in corpus {
        let score = match call_model(lm, timeout_secs, build_prompt(&slice.slice)).await {
            Ok(summary) => {
                if let Some(dir) = out {
                    let path = dir.join(format!("{}__{}.txt", safe_tag(&lm.model), slice.id));
                    if let Err(e) = std::fs::write(&path, &summary) {
                        eprintln!("{} could not write {}: {e}", render::warn(), path.display());
                    }
                }
                score_summary(slice, &summary)
            }
            Err(e) => errored_score(slice, &e),
        };
        scores.push(score);
    }
    aggregate(lm.model.clone(), "local", scores)
}

/// Run one API provider over up to `max_calls` corpus slices, saving each summary
/// to `out` when given.  Any call error degrades that slice to a zero score.
///
/// The provider key is resolved from `provider.api_key_env` by `call_api` — trimwire
/// never touches the Anthropic subscription/OAuth token.  This function is called
/// ONLY after the safety gate in `benchmark()` has confirmed `yes=true`.
async fn run_api_provider(
    provider: &SummarizerProviderConfig,
    corpus: &[CorpusSlice],
    max_calls: usize,
    out: Option<&Path>,
) -> ModelScore {
    use super::render;
    let slices = &corpus[..max_calls.min(corpus.len())];
    let mut scores = Vec::with_capacity(slices.len());
    for slice in slices {
        let score = match call_api(provider, build_prompt(&slice.slice)).await {
            Ok(summary) => {
                if let Some(dir) = out {
                    let path = dir.join(format!("{}__{}.txt", safe_tag(&provider.id), slice.id));
                    if let Err(e) = std::fs::write(&path, &summary) {
                        eprintln!("{} could not write {}: {e}", render::warn(), path.display());
                    }
                }
                score_summary(slice, &summary)
            }
            Err(e) => errored_score(slice, &e),
        };
        scores.push(score);
    }
    // Label the table row with the provider id (for the local printout), and carry
    // the content-free provenance the share path coarsens (provider model/style/route)
    // — never the provider id — plus whether the full corpus ran.
    let full_corpus = slices.len() >= corpus.len();
    let mut ms = aggregate(provider.id.clone(), "api", scores);
    ms.provider_model = Some(provider.model.clone());
    ms.provider_style = Some(provider.style.clone());
    ms.provider_route = Some(provider_route(&provider.base_url, &provider.style));
    ms.full_corpus = full_corpus;
    ms
}

/// Coarse, content-free route bucket from a provider base URL (never the raw URL,
/// host, or user-defined id): `anthropic | openai | openrouter | azure | other`.
/// An empty base_url (provider default) falls back to the API style.
fn provider_route(base_url: &str, style: &str) -> String {
    let u = base_url.to_ascii_lowercase();
    if u.contains("openrouter") {
        "openrouter"
    } else if u.contains("azure") {
        "azure"
    } else if u.contains("anthropic.com") {
        "anthropic"
    } else if u.contains("openai.com") {
        "openai"
    } else if u.is_empty() {
        match style {
            "anthropic" => "anthropic",
            "openai" => "openai",
            _ => "other",
        }
    } else {
        "other"
    }
    .to_owned()
}

/// Print the pre-call safety warning for an API provider and return whether
/// the caller should proceed (i.e. `yes=true`) or treat this as a dry run.
///
/// Always returns `false` when `yes` is false — the caller must skip all
/// network I/O.  When `yes` is true the warning is still printed so the user
/// can see exactly what is about to be charged.
fn api_safety_warning(provider: &SummarizerProviderConfig, corpus_len: usize, yes: bool) -> bool {
    use super::render;
    eprintln!(
        "{}  API BENCHMARK — REAL MONEY WARNING\n\
         \x20  This makes {corpus_len} real API call(s) to {} using model {}.\n\
         \x20  Charged to your {} key (NOT your Anthropic subscription).\n\
         \x20  API scores are NOT directly comparable to local-model scores\n\
         \x20  (corpus tuned for local summarizers; temperature/context differ).\n\
         \x20  Treat them as a directional sanity-check within the same model family.",
        render::warn(),
        if provider.base_url.is_empty() {
            "(provider default URL)".to_owned()
        } else {
            provider.base_url.clone()
        },
        render::accent(&format!("{:?}", provider.model)),
        provider.api_key_env,
    );
    if !yes {
        eprintln!(
            "\n  {} DRY RUN — no API calls made.\n\
             \x20  To run locally (no upload): {}\n\
             \x20  To run AND share the score: {}",
            render::dim("→"),
            render::accent(&format!(
                "trimwire summarizer benchmark --model {} --yes",
                provider.id
            )),
            render::accent(&format!(
                "trimwire share benchmark --model {} --yes",
                provider.id
            )),
        );
    }
    yes
}

/// Top-level JSON shape for `--json`.
#[derive(Serialize)]
struct BenchmarkReport<'a> {
    corpus_version: &'a str,
    /// Always true — this rank is directional, not an authoritative quality score.
    directional: bool,
    models: &'a [ModelScore],
}

// ─── interactive model picker ────────────────────────────────────────────────

/// The outcome of parsing one line of user input in the benchmark model picker.
/// Extracted as a pure function so it can be unit-tested without I/O.
#[derive(Debug, PartialEq)]
pub enum PickerChoice {
    /// Benchmark this single model tag.
    Model(String),
    /// Benchmark all installed ollama models (same as `--all-installed`).
    AllInstalled,
    /// Use the default model (whatever was configured / the recommended default).
    Default,
    /// Cancel — do not run the benchmark.
    Cancel,
}

/// Pure parse of one line of user input against the installed model list.
///
/// Rules (applied in order, case-insensitive trim):
/// - empty string → `Default`
/// - `"q"` or `"quit"` → `Cancel`
/// - `"a"` or `"all"` → `AllInstalled`
/// - a decimal number in `1..=installed.len()` → `Model(installed[n-1])`
/// - anything else (out-of-range number, unrecognised text) → `Cancel`
///
/// This function is pure (no I/O) so it is cheap to unit-test exhaustively.
pub fn resolve_picker_choice(input: &str, installed: &[String], _default: &str) -> PickerChoice {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return PickerChoice::Default;
    }
    let lower = trimmed.to_lowercase();
    match lower.as_str() {
        "q" | "quit" => return PickerChoice::Cancel,
        "a" | "all" => return PickerChoice::AllInstalled,
        _ => {}
    }
    if let Ok(n) = trimmed.parse::<usize>() {
        if n >= 1 && n <= installed.len() {
            return PickerChoice::Model(installed[n - 1].clone());
        }
    }
    PickerChoice::Cancel
}

/// The default ollama endpoint (mirrors `SummarizerLocalConfig::default()`).
const OLLAMA_DEFAULT_ENDPOINT: &str = "http://localhost:11434";

/// Annotation suffix for a model tag in the picker list.
fn model_annotation(tag: &str) -> &'static str {
    use trimwire::summarizer::APPROVED_MODELS;
    if tag == APPROVED_MODELS.first().copied().unwrap_or("") {
        " ← recommended"
    } else if is_disqualified(tag) {
        " (DISQUALIFIED)"
    } else if WARN_MODELS.contains(&tag) {
        " (warn: failed harm gate)"
    } else if !APPROVED_MODELS.contains(&tag) {
        " (unvalidated)"
    } else {
        ""
    }
}

/// Show the numbered picker and read one line from stdin.
///
/// Returns the user's `PickerChoice` after parsing.  On EOF (stdin closed /
/// Ctrl-D) returns `PickerChoice::Cancel` — no infinite loop.
///
/// Caller MUST have already confirmed that stdin is a TTY before calling this.
fn prompt_model_picker(installed: &[String], default_model: &str) -> PickerChoice {
    use super::render;
    use std::io::Write as _;

    println!("Select a model to benchmark (installed ollama models):");
    println!();
    for (i, tag) in installed.iter().enumerate() {
        let ann = model_annotation(tag);
        println!(
            "  {:>2})  {}{}",
            i + 1,
            render::accent(tag),
            render::dim(ann)
        );
    }
    println!();
    println!("   {}  all installed", render::accent("a)"));
    println!("   {}  cancel (no benchmark)", render::accent("q)"));
    println!();
    print!("Choice [Enter = {default_model}]: ");
    let _ = std::io::stdout().flush();

    let mut buf = String::new();
    match std::io::stdin().read_line(&mut buf) {
        Ok(0) | Err(_) => {
            // EOF / error — cancel cleanly.
            println!(); // newline after the prompt
            PickerChoice::Cancel
        }
        Ok(_) => resolve_picker_choice(&buf, installed, default_model),
    }
}

// ─── public entry points ─────────────────────────────────────────────────────

/// `trimwire share benchmark [--yes]` entry point: score models then share.
///
/// Backs `trimwire share benchmark [--yes]`.
/// Dry-run by default; real upload only with `--yes` (and a configured endpoint).
pub fn benchmark_share(models: Vec<String>, all_installed: bool, yes: bool) -> Result<()> {
    benchmark(models, all_installed, None, false, false, true, yes, None)
}

/// `trimwire summarizer benchmark` (feature `local_model`).
#[allow(clippy::too_many_arguments)]
pub fn benchmark(
    models: Vec<String>,
    all_installed: bool,
    out: Option<PathBuf>,
    json: bool,
    quiet: bool,
    share: bool,
    yes: bool,
    max_calls: Option<usize>,
) -> Result<()> {
    use super::render;
    let cfg = Config::load().unwrap_or_else(|_| trimwire::config::profile_baseline("default"));

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("build tokio runtime")?;

    // Classify each requested tag as LOCAL or API.
    // A tag is "API" when it matches a configured [[summarizer.providers]] id.
    // Everything else is treated as a local ollama tag (may not exist — ollama
    // will error, which is scored as errored_score per slice; the user gets a
    // clear error message).
    let is_provider = |tag: &str| -> bool { cfg.summarizer.providers.iter().any(|p| p.id == tag) };

    // ── Interactive model picker ──────────────────────────────────────────────
    // Trigger ONLY when: no --model given AND NOT --all-installed AND NOT --json
    // AND NOT --quiet AND stdin is a TTY. Scripts (piped stdin, --json, --quiet)
    // keep the existing silent-default behaviour.
    let (mut models, mut all_installed) = (models, all_installed);
    if models.is_empty() && !all_installed && !json && !quiet && std::io::stdin().is_terminal() {
        let endpoint = if cfg.summarizer.local.endpoint.is_empty() {
            OLLAMA_DEFAULT_ENDPOINT
        } else {
            cfg.summarizer.local.endpoint.as_str()
        };
        match rt.block_on(super::fetch_ollama_tags(endpoint)) {
            Err(e) => {
                eprintln!(
                    "{} no ollama models found at {endpoint} ({e})\n  \
                     {} pull one: {}, or pass --model <tag>",
                    render::warn(),
                    render::dim("→"),
                    render::accent("ollama pull qwen3.5:4b")
                );
                // Fall through: the default resolution below will pick up the
                // configured/default model, or produce a clear ollama error per-slice.
            }
            Ok(installed) if installed.is_empty() => {
                eprintln!(
                    "{} no ollama models found at {endpoint} — pull one: \
                     {}, or pass --model <tag>",
                    render::warn(),
                    render::accent("ollama pull qwen3.5:4b")
                );
                // Fall through to default behaviour.
            }
            Ok(installed) => {
                // Determine the default (configured model > APPROVED_MODELS[0]).
                let configured = cfg.summarizer.local.model.trim().to_owned();
                let default_model = if configured.is_empty() {
                    APPROVED_MODELS[0].to_owned()
                } else {
                    configured
                };
                let choice = prompt_model_picker(&installed, &default_model);
                match choice {
                    PickerChoice::Model(tag) => models = vec![tag],
                    PickerChoice::AllInstalled => all_installed = true,
                    PickerChoice::Default => models = vec![default_model],
                    PickerChoice::Cancel => {
                        println!("{} benchmark cancelled.", render::bullet());
                        return Ok(());
                    }
                }
            }
        }
    }

    // Resolve which models to score, in order, deduped:
    //   --all-installed → every installed LOCAL ollama tag (DISQUALIFIED ones skipped
    //   with a warn; provider ids never come from this path),
    //   then any explicit --model (benchmarked even if disqualified — that's the
    //   point; provider ids are also accepted here),
    //   else the configured summarizer model, else the default approved tag.
    let mut resolved: Vec<String> = Vec::new();
    let push = |tag: String, list: &mut Vec<String>| {
        if !tag.is_empty() && !list.contains(&tag) {
            list.push(tag);
        }
    };
    if all_installed {
        match rt.block_on(super::fetch_ollama_tags(&cfg.summarizer.local.endpoint)) {
            Ok(installed) => {
                for tag in installed {
                    if is_disqualified(&tag) {
                        eprintln!(
                            "{} skipping {} (DISQUALIFIED for summarization)",
                            render::warn(),
                            render::accent(&tag)
                        );
                        continue;
                    }
                    push(tag, &mut resolved);
                }
            }
            Err(e) => eprintln!("{} --all-installed: {e}", render::warn()),
        }
    }
    for m in models {
        push(m, &mut resolved);
    }
    if resolved.is_empty() {
        let configured = cfg.summarizer.local.model.trim().to_owned();
        push(
            if configured.is_empty() {
                APPROVED_MODELS[0].to_owned()
            } else {
                configured
            },
            &mut resolved,
        );
    }
    let models = resolved;

    // Warn (never refuse) on a local model the gut-read flagged — benchmarking a
    // weak or disqualified model is a legitimate reason to run this.
    // Provider ids are not checked against the local approved/disqualified lists.
    for m in &models {
        if !is_provider(m) {
            if is_disqualified(m) {
                eprintln!(
                    "{} {} is DISQUALIFIED for production summarization (hallucinates / overstates completed work) — benchmarking it anyway",
                    render::warn(),
                    render::accent(m)
                );
            } else if WARN_MODELS.contains(&m.as_str()) {
                eprintln!(
                    "{} {} FAILED the harm gate (drops load-bearing facts) — a RAM opt-down, not an equal to qwen3.5:4b",
                    render::warn(),
                    render::accent(m)
                );
            } else if !APPROVED_MODELS.contains(&m.as_str()) {
                eprintln!(
                    "{} {} is unvalidated — its summary fidelity has not been gut-read",
                    render::warn(),
                    render::accent(m)
                );
            }
        }
    }

    if let Some(dir) = &out {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("create --out dir {}", dir.display()))?;
    }

    let corpus = load_corpus()?;

    // Score each model/provider in order.  Provider ids are routed through the API
    // path (behind the safety gate); local tags go through ollama.
    let mut results: Vec<ModelScore> = models
        .iter()
        .map(|model| {
            if let Some(provider) = cfg.summarizer.providers.iter().find(|p| &p.id == model) {
                // API path: cap the paid calls at --max-calls (default = full corpus).
                let cap = max_calls.unwrap_or(corpus.len()).min(corpus.len());
                // Show the safety warning for the actual call count; proceed only if yes=true.
                let proceed = api_safety_warning(provider, cap, yes);
                if !proceed {
                    // Dry run: return an empty ModelScore placeholder (all zeros,
                    // backend="api-dry-run") so the table still renders a row.
                    return aggregate(format!("{} (DRY RUN)", provider.id), "api-dry-run", vec![]);
                }
                rt.block_on(run_api_provider(provider, &corpus, cap, out.as_deref()))
            } else {
                // Local ollama path (unchanged behaviour).
                let mut lm = cfg.summarizer.local.clone();
                lm.model = model.clone();
                let timeout = cfg.summarizer.timeout_secs;
                rt.block_on(run_model(&lm, timeout, &corpus, out.as_deref()))
            }
        })
        .collect();

    // Rank: not-gated before gated, then composite descending, then model name for
    // a deterministic order within the gated block (where every composite is 0).
    results.sort_by(|a, b| {
        a.gated
            .cmp(&b.gated)
            .then(
                b.composite
                    .partial_cmp(&a.composite)
                    .unwrap_or(std::cmp::Ordering::Equal),
            )
            .then_with(|| a.model.cmp(&b.model))
    });

    if share {
        // Symmetric with `share stats`: an explicit config endpoint wins, else
        // the built-in const (the deployed api.trimwire.dev/ingest-benchmark).
        let endpoint =
            super::share::resolve_benchmark_endpoint(cfg.share.benchmark_endpoint.trim());
        return run_share(&results, yes, endpoint);
    }

    if json {
        let report = BenchmarkReport {
            corpus_version: CORPUS_VERSION,
            directional: true,
            models: &results,
        };
        println!("{}", serde_json::to_string_pretty(&report)?);
        return Ok(());
    }

    if quiet {
        for r in &results {
            let fcs = if r.backend == "api-dry-run" {
                "DRY-RUN".to_owned()
            } else if r.gated {
                "FAIL".to_owned()
            } else {
                format!("{:.0}", r.fcs)
            };
            println!("{:<22} [{}] FCS {fcs}", r.model, r.backend);
        }
        return Ok(());
    }

    print_table(&results, &corpus, out.as_deref());
    Ok(())
}

/// `--share`: build the content-free per-model rows, run each through the same
/// fail-closed guard the stats telemetry uses, PRINT them, and — only with a
/// configured `[share] benchmark_endpoint` AND `--yes` — upload them. Inert
/// otherwise (dry run, no network I/O), mirroring `share stats`.
fn run_share(results: &[ModelScore], yes: bool, endpoint: &str) -> Result<()> {
    use super::render;
    use super::share::{
        BenchmarkPayload, BenchmarkShareInput, build_benchmark_payload,
        guard_benchmark_content_free, post,
    };

    // HARD gate: never upload rows produced against a corpus that doesn't match the
    // maintainer-reviewed, pinned hash — shared results across corpus content are
    // not comparable, and a fork that edits the corpus must not pollute the dataset
    // under this `corpus_version`.
    if corpus_sha() != CORPUS_SHA {
        anyhow::bail!(
            "refusing to share: the embedded corpus does not match the pinned hash for \
             corpus v{CORPUS_VERSION} — shared rows must use the unmodified corpus"
        );
    }

    println!(
        "{}",
        render::strong("trimwire share benchmark — anonymous, content-free per-model rows")
    );
    println!("  This is the ENTIRE payload (coarse buckets only; one row per model):\n");
    let mut bodies: Vec<String> = Vec::with_capacity(results.len());
    for r in results {
        // Skip placeholder rows from an API dry-run (no real data to share).
        if r.backend == "api-dry-run" {
            eprintln!(
                "{} skipping dry-run row for {} (no API calls were made)",
                render::warn(),
                render::accent(&r.model)
            );
            continue;
        }
        let is_api = r.backend == "api";
        let failed_slices = r.slices.iter().filter(|s| s.error.is_some()).count();
        let error_kind = r.slices.iter().find_map(|s| s.error_kind).unwrap_or("none");
        // Don't upload rows with provider/model call failures — they conflate a
        // weak model with a broken provider/config/network. Print a report-issue
        // hint instead (no error auto-upload yet). Never paste secrets/content.
        if failed_slices > 0 || error_kind != "none" {
            let scope = if r.full_corpus { "full" } else { "partial" };
            eprintln!(
                "\n{} {} had {failed_slices} failed slice(s) (error_kind: {error_kind}).\n\
                 \x20  This looks like a provider/config issue, not a clean model-quality result —\n\
                 \x20  not uploading this row. Please open an issue with: {}, the\n\
                 \x20  command used, backend={}, provider_style={}, provider_route={}, error_kind={error_kind},\n\
                 \x20  and whether the run was {scope} corpus. Do NOT paste API keys, prompts,\n\
                 \x20  summaries, session content, raw URLs, or stack traces.",
                render::warn(),
                render::accent(&r.model),
                render::accent("trimwire --version"),
                r.backend,
                r.provider_style.as_deref().unwrap_or("none"),
                r.provider_route.as_deref().unwrap_or("none"),
            );
            continue;
        }
        // Both skip branches above are exactly `!is_uploadable_row`: a dry-run
        // placeholder, or any failed slice. Assert it so the two stay in lockstep
        // with the predicate — nothing past this point may be a placeholder/failure.
        debug_assert!(
            is_uploadable_row(r),
            "non-uploadable row reached the payload builder"
        );
        let model = if is_api {
            r.provider_model.as_deref().unwrap_or("")
        } else {
            r.model.as_str()
        };
        let input = BenchmarkShareInput {
            backend: &r.backend,
            model,
            provider_style: r.provider_style.as_deref().unwrap_or("none"),
            provider_route: r.provider_route.as_deref().unwrap_or("none"),
            corpus_version: CORPUS_VERSION,
            retention: r.retention,
            reduction: r.reduction,
            false_done_total: r.false_done_total,
            // Exact (not a float compare on usable_pct): every slice produced a
            // usable summary. Robust as the corpus grows.
            all_usable: r.slices.iter().all(|s| s.usable),
            full_corpus: r.full_corpus,
            scored_slices: r.slices.len(),
            failed_slices,
            error_kind,
        };
        let payload: BenchmarkPayload = build_benchmark_payload(
            &input,
            super::share::version_bucket(),
            super::share::utc_today(),
        );
        let value = serde_json::to_value(&payload).context("serialize benchmark payload")?;
        // Fail closed: never even PRINT a row with an unexpected field/value.
        guard_benchmark_content_free(&value).context("benchmark payload content-free guard")?;
        println!("{}\n", serde_json::to_string_pretty(&value)?);
        bodies.push(serde_json::to_string(&value)?);
    }
    println!(
        "{}",
        render::dim(
            "  No raw model names/tags (only coarse family + bucket), no provider ids/URLs/keys,\n\
             \x20  no summaries, paths, or raw counts — just the rank-table columns + backend/\n\
             \x20  provider route. API and local rows are tagged `backend` and ranked separately.\n\
             \x20  See docs/TELEMETRY.md."
        )
    );

    if bodies.is_empty() {
        println!(
            "\n  {} Nothing to share — no real benchmark rows were produced (every requested\n\
             \x20  model was an API dry-run or unmatched). Re-run with a local model, or an\n\
             \x20  API provider plus --yes, to generate scores.",
            render::bullet()
        );
        return Ok(());
    }

    if endpoint.is_empty() {
        // Only reachable if a self-hoster blanks both [share] benchmark_endpoint
        // and the built-in const (which ships pointing at api.trimwire.dev).
        println!(
            "\n  {} No benchmark collector endpoint configured ([share] benchmark_endpoint = \"\"),\n\
             \x20  so this was a DRY RUN — nothing was sent. Set [share] benchmark_endpoint to\n\
             \x20  a collector URL to enable uploads.",
            render::warn()
        );
        return Ok(());
    }
    if !yes {
        println!(
            "\n  {} DRY RUN — re-run {} to upload the above to:\n  {endpoint}",
            render::warn(),
            render::accent("trimwire share benchmark --yes")
        );
        return Ok(());
    }
    for body in &bodies {
        post(endpoint, body).context("upload benchmark row")?;
    }
    println!(
        "\n  {} Shared {} row(s). Thank you — your anonymous numbers help everyone pick a model.",
        render::ok(),
        bodies.len()
    );
    Ok(())
}

fn print_table(results: &[ModelScore], corpus: &[CorpusSlice], out: Option<&Path>) {
    use super::render;
    let has_api = results
        .iter()
        .any(|r| r.backend == "api" || r.backend == "api-dry-run");
    println!(
        "{}\n",
        render::strong(&format!(
            "trimwire summarizer benchmark — corpus v{CORPUS_VERSION}, {} slices",
            corpus.len()
        ))
    );
    // Header + separator are secondary chrome — dim, and padded to their final
    // widths BEFORE colouring (an ANSI-wrapped string would otherwise count
    // escape bytes toward the column width and break alignment).
    println!(
        "{}",
        render::dim(&format!(
            "{:<22} {:>7} {:>10} {:>11} {:>11} {:>8} {:>6}",
            "model", "backend", "retention", "compression", "false-done", "usable", "FCS"
        ))
    );
    println!("{}", render::dim(&"─".repeat(77)));
    for r in results {
        let total_needles: usize = r.slices.iter().map(|s| s.total).sum();
        let total_kept: usize = r.slices.iter().map(|s| s.kept).sum();
        let retention = if r.backend == "api-dry-run" {
            "—".to_owned()
        } else {
            format!("{total_kept}/{total_needles} {:.0}%", r.retention * 100.0)
        };
        let reduction = if r.backend == "api-dry-run" {
            "—".to_owned()
        } else {
            format!("{:.0}%", r.reduction * 100.0)
        };
        let usable = if r.backend == "api-dry-run" {
            "—".to_owned()
        } else {
            format!("{:.0}%", r.usable_pct)
        };
        let fcs = if r.backend == "api-dry-run" {
            "—".to_owned()
        } else if r.gated {
            "FAIL".to_owned()
        } else {
            format!("{:.0}", r.fcs)
        };
        let flag = if r.gated && r.backend != "api-dry-run" {
            // Keep a leading glyph (not colour-only): restores the always-visible
            // ✗ this line had before, matching the gate-detail lines below.
            format!("  {} {}", render::bad(), render::error_text("gated"))
        } else {
            String::new()
        };
        // Model name is padded to its column width PLAIN first, then coloured —
        // same reasoning as the header above.
        println!(
            "{} {:>7} {:>10} {:>11} {:>11} {:>8} {:>6}{flag}",
            render::accent(&format!("{:<22}", r.model)),
            r.backend,
            retention,
            reduction,
            if r.backend == "api-dry-run" {
                "—".to_owned()
            } else {
                r.false_done_total.to_string()
            },
            usable,
            fcs
        );
    }

    // Per-model gate detail: which slice tripped, so the user knows WHY. Every
    // line here is by definition a failure reason, so it leads with `bad()`.
    for r in results {
        if !r.gated || r.backend == "api-dry-run" {
            continue;
        }
        println!("\n{}: gated —", render::accent(&r.model));
        for s in &r.slices {
            if let Some(e) = &s.error {
                println!("  {} {} — model error: {e}", render::bad(), s.id);
            } else if s.false_done > 0 {
                let trap = if s.is_trap { " (designated trap)" } else { "" };
                println!(
                    "  {} {} — {} unsupported completion claim(s){trap}",
                    render::bad(),
                    s.id,
                    s.false_done
                );
            } else if !s.usable {
                println!(
                    "  {} {} — no usable summary (empty or near-verbatim)",
                    render::bad(),
                    s.id
                );
            }
        }
    }

    println!(
        "{}",
        render::dim(
            "\nFCS (faithful-compression score) = retention × compression (0–100), behind a false-done safety gate."
        )
    );
    println!(
        "{}",
        render::dim("This is a DIRECTIONAL sanity-check, not an authoritative quality ranking —")
    );
    println!(
        "{}",
        render::dim("the APPROVED_MODELS list (a blind human gut-read) stays the authority.")
    );
    if has_api {
        println!(
            "{}",
            render::dim(
                "\nNOTE: API-backend scores are NOT directly comparable to local-backend scores.\n\
                 The corpus is tuned for local summarizers (dense reasoning, tight length budget,\n\
                 free-form FACTS-FIRST prompt). Cloud models with larger context windows and\n\
                 different defaults may score differently for structural reasons unrelated to\n\
                 summarisation quality. Use API scores as a directional sanity-check within\n\
                 the same model family, not as a cross-backend ranking."
            )
        );
    }
    match out {
        Some(dir) => println!(
            "Summaries saved to {} — skim them; the scores can't judge prose.",
            render::accent(&dir.display().to_string())
        ),
        None => println!(
            "Pass {} to save the summaries and skim them yourself.",
            render::accent("--out <DIR>")
        ),
    }
}

#[cfg(test)]
// set_var/remove_var are unsafe in Rust 2024; test-only, unique env var names per test.
#[allow(unsafe_code)]
mod tests {
    use super::*;

    // ── resolve_picker_choice — pure parsing logic ────────────────────────────

    fn models() -> Vec<String> {
        vec![
            "qwen3.5:4b".to_owned(),
            "qwen3.5:2b".to_owned(),
            "llama3.1:8b".to_owned(),
        ]
    }

    #[test]
    fn picker_empty_input_returns_default() {
        assert_eq!(
            resolve_picker_choice("", &models(), "qwen3.5:4b"),
            PickerChoice::Default
        );
        // Whitespace-only is also empty.
        assert_eq!(
            resolve_picker_choice("   ", &models(), "qwen3.5:4b"),
            PickerChoice::Default
        );
    }

    #[test]
    fn picker_valid_number_returns_model() {
        let ms = models();
        assert_eq!(
            resolve_picker_choice("1", &ms, "qwen3.5:4b"),
            PickerChoice::Model("qwen3.5:4b".to_owned())
        );
        assert_eq!(
            resolve_picker_choice("2", &ms, "qwen3.5:4b"),
            PickerChoice::Model("qwen3.5:2b".to_owned())
        );
        assert_eq!(
            resolve_picker_choice("3", &ms, "qwen3.5:4b"),
            PickerChoice::Model("llama3.1:8b".to_owned())
        );
    }

    #[test]
    fn picker_all_returns_all_installed() {
        let ms = models();
        assert_eq!(
            resolve_picker_choice("a", &ms, "qwen3.5:4b"),
            PickerChoice::AllInstalled
        );
        assert_eq!(
            resolve_picker_choice("A", &ms, "qwen3.5:4b"),
            PickerChoice::AllInstalled
        );
        assert_eq!(
            resolve_picker_choice("all", &ms, "qwen3.5:4b"),
            PickerChoice::AllInstalled
        );
        assert_eq!(
            resolve_picker_choice("ALL", &ms, "qwen3.5:4b"),
            PickerChoice::AllInstalled
        );
    }

    #[test]
    fn picker_q_returns_cancel() {
        let ms = models();
        assert_eq!(
            resolve_picker_choice("q", &ms, "qwen3.5:4b"),
            PickerChoice::Cancel
        );
        assert_eq!(
            resolve_picker_choice("Q", &ms, "qwen3.5:4b"),
            PickerChoice::Cancel
        );
        assert_eq!(
            resolve_picker_choice("quit", &ms, "qwen3.5:4b"),
            PickerChoice::Cancel
        );
    }

    #[test]
    fn picker_out_of_range_number_returns_cancel() {
        let ms = models();
        // 0 is out of range (1-based).
        assert_eq!(
            resolve_picker_choice("0", &ms, "qwen3.5:4b"),
            PickerChoice::Cancel
        );
        // len+1 is out of range.
        assert_eq!(
            resolve_picker_choice("4", &ms, "qwen3.5:4b"),
            PickerChoice::Cancel
        );
        // Very large number.
        assert_eq!(
            resolve_picker_choice("99", &ms, "qwen3.5:4b"),
            PickerChoice::Cancel
        );
    }

    #[test]
    fn picker_unrecognised_text_returns_cancel() {
        let ms = models();
        assert_eq!(
            resolve_picker_choice("foo", &ms, "qwen3.5:4b"),
            PickerChoice::Cancel
        );
        assert_eq!(
            resolve_picker_choice("qwen3.5:4b", &ms, "qwen3.5:4b"),
            PickerChoice::Cancel,
            "typing the model name directly is not a valid choice — only number/a/q/empty"
        );
    }

    #[test]
    fn picker_eof_equivalent_empty_list_is_cancel_via_range() {
        // With an empty installed list, every number is out of range.
        let empty: Vec<String> = vec![];
        assert_eq!(
            resolve_picker_choice("1", &empty, "qwen3.5:4b"),
            PickerChoice::Cancel
        );
        // Empty input still maps to Default even with an empty list.
        assert_eq!(
            resolve_picker_choice("", &empty, "qwen3.5:4b"),
            PickerChoice::Default
        );
    }

    fn slice(id: &str, trap: bool, needles: &[&str], text: &str) -> CorpusSlice {
        CorpusSlice {
            id: id.to_owned(),
            false_done_trap: trap,
            needles: needles.iter().map(|s| s.to_string()).collect(),
            slice: text.to_owned(),
        }
    }

    #[test]
    fn corpus_bytes_match_pinned_sha() {
        assert_eq!(
            corpus_sha(),
            CORPUS_SHA,
            "corpus changed — bump CORPUS_VERSION and update CORPUS_SHA \
             (results across corpus versions are not comparable)"
        );
    }

    #[test]
    fn corpus_loads_and_is_well_formed() {
        let corpus = load_corpus().expect("embedded corpus parses");
        assert_eq!(corpus.len(), 5, "5 bundled slices");
        assert!(
            corpus.iter().any(|s| s.false_done_trap),
            "at least one false-done trap"
        );
        for s in &corpus {
            assert!(!s.needles.is_empty(), "{} has needles", s.id);
            assert!(s.slice.len() > 100, "{} has a non-trivial slice", s.id);
        }
    }

    #[test]
    fn score_summary_counts_retention_reduction_and_false_done() {
        // A long input, a tight faithful summary keeping 2 of 3 needles, no claim.
        let s = slice(
            "t",
            false,
            &["session_7421.rs", "MAX_RETRIES", "missing_id"],
            &"### user\n[tool_result] ".to_owned().repeat(40),
        );
        let summary = "GOAL: harden writer\nFACTS: cap MAX-RETRIES in session_7421.rs";
        let sc = score_summary(&s, summary);
        assert_eq!(
            (sc.kept, sc.total),
            (2, 3),
            "separator-insensitive: keeps 2 of 3"
        );
        assert_eq!(sc.false_done, 0);
        assert!(sc.usable, "a short summary of a long slice is usable");
        assert!(SliceScore::reduction(sc.in_chars, sc.out_chars) > 0.5);
    }

    #[test]
    fn false_done_claim_gates_the_model_to_bottom() {
        // High retention but an unsupported "tests passed" claim on a slice with no
        // test-run evidence → gated, composite 0.
        let s = slice(
            "trap",
            true,
            &["payment_gateway.rs"],
            "### assistant\nEditing payment_gateway.rs\n[tool_use Bash] {\"command\":\"cargo test\"}\n### user\n[tool_result] Compiling...",
        );
        let summary =
            "GOAL: retry cap in payment_gateway.rs\nFACTS: all 37 tests passed, committed";
        let sc = score_summary(&s, summary);
        assert!(
            sc.false_done >= 1,
            "must flag the unsupported completion claim"
        );
        let m = aggregate("bad".into(), "local", vec![sc]);
        assert!(m.gated, "a false-done gates the model");
        assert_eq!(m.composite, 0.0, "gated composite is bottom-tier");
        assert!(
            m.fcs > 0.0,
            "but the raw FCS is still recorded (retention was high)"
        );
    }

    #[test]
    fn verbatim_copy_is_not_usable_and_gates() {
        // Output ≈ input (no shrink) → not usable → gated, even with full retention.
        let text = "### user\nkeep token_abc and token_def verbatim here please";
        let s = slice("verbatim", false, &["token_abc", "token_def"], text);
        let sc = score_summary(&s, text); // echo the slice back
        assert_eq!((sc.kept, sc.total), (2, 2), "retention is full");
        assert!(!sc.usable, "a verbatim copy is not a usable summary");
        let m = aggregate("copier".into(), "local", vec![sc]);
        assert!(m.gated, "an unusable slice gates the model");
        assert_eq!(m.composite, 0.0);
    }

    #[test]
    fn clean_summary_scores_fcs_and_is_not_gated() {
        let s = slice(
            "clean",
            false,
            &["alpha", "beta"],
            &"### user\n[tool_result] noise ".to_owned().repeat(60),
        );
        let summary = "GOAL: x\nFACTS: alpha and beta decided"; // tight, faithful, no claim
        let sc = score_summary(&s, summary);
        let m = aggregate("good".into(), "local", vec![sc]);
        assert!(!m.gated, "a clean tight faithful summary is not gated");
        assert_eq!(m.retention, 1.0);
        assert!(m.reduction > 0.5);
        assert!(
            (m.composite - m.fcs).abs() < 1e-9,
            "ungated composite == fcs"
        );
        assert!(
            m.composite > 40.0,
            "retention 1.0 × big reduction → solid FCS"
        );
    }

    #[test]
    fn errored_slice_is_zero_and_gates() {
        let s = slice(
            "e",
            false,
            &["a", "b"],
            "### user\nsome text here for length",
        );
        let sc = errored_score(
            &s,
            &CompactorError::Unreachable("local model unreachable".into()),
        );
        assert_eq!(sc.kept, 0);
        assert!(!sc.usable);
        assert_eq!(sc.error_kind, Some("unreachable"));
        let m = aggregate("down".into(), "local", vec![sc]);
        assert!(m.gated, "a model error gates the run");
        assert_eq!(m.composite, 0.0);
    }

    // ---- API provider path tests --------------------------------------------

    #[test]
    fn aggregate_records_backend_kind() {
        // local backend
        let m = aggregate("qwen3.5:4b".into(), "local", vec![]);
        assert_eq!(m.backend, "local");
        // api backend
        let m = aggregate("my-provider".into(), "api", vec![]);
        assert_eq!(m.backend, "api");
        // api-dry-run placeholder
        let m = aggregate("my-provider (DRY RUN)".into(), "api-dry-run", vec![]);
        assert_eq!(m.backend, "api-dry-run");
    }

    #[test]
    fn is_uploadable_row_rejects_dry_run_and_failures() {
        let s = slice("s1", false, &["alpha"], "alpha beta gamma delta epsilon");

        // Clean local row: real data, no failed slices → uploadable.
        let clean_local = aggregate(
            "qwen3.5:4b".into(),
            "local",
            vec![score_summary(&s, "alpha")],
        );
        assert!(
            is_uploadable_row(&clean_local),
            "a clean local row with no failed slices is uploadable"
        );

        // Clean api row → uploadable (backend "api", not the placeholder).
        let clean_api = aggregate(
            "my-provider".into(),
            "api",
            vec![score_summary(&s, "alpha")],
        );
        assert!(
            is_uploadable_row(&clean_api),
            "a clean api row is uploadable"
        );

        // api-dry-run placeholder (no calls made) → NEVER uploadable, even with an
        // empty slice set that would otherwise pass the all-clean check.
        let dry = aggregate("my-provider (DRY RUN)".into(), "api-dry-run", vec![]);
        assert!(
            !is_uploadable_row(&dry),
            "an api-dry-run placeholder is never uploadable"
        );

        // A row with any failed slice → not uploadable (provider/config failure).
        let failed = aggregate(
            "down".into(),
            "api",
            vec![errored_score(&s, &CompactorError::Timeout)],
        );
        assert!(
            !is_uploadable_row(&failed),
            "a row with a failed slice is not uploadable"
        );
    }

    #[test]
    fn api_safety_warning_returns_false_without_yes() {
        use trimwire::config::SummarizerProviderConfig;
        let provider = SummarizerProviderConfig {
            id: "my-api".to_owned(),
            style: "anthropic".to_owned(),
            base_url: "https://api.example.com".to_owned(),
            full_url: None,
            model: "fast-model".to_owned(),
            api_key_env: "MY_API_KEY".to_owned(),
            api_key_file: None,
            timeout_secs: 30,
        };
        // Without yes=true the gate must prevent API calls.
        let proceed = api_safety_warning(&provider, 5, false);
        assert!(
            !proceed,
            "dry-run: safety warning must return false without --yes"
        );
    }

    #[test]
    fn api_safety_warning_returns_true_with_yes() {
        use trimwire::config::SummarizerProviderConfig;
        let provider = SummarizerProviderConfig {
            id: "my-api".to_owned(),
            style: "anthropic".to_owned(),
            base_url: "https://api.example.com".to_owned(),
            full_url: None,
            model: "fast-model".to_owned(),
            api_key_env: "MY_API_KEY".to_owned(),
            api_key_file: None,
            timeout_secs: 30,
        };
        // With yes=true the warning prints but execution proceeds.
        let proceed = api_safety_warning(&provider, 5, true);
        assert!(proceed, "with --yes the safety gate must permit API calls");
    }

    #[tokio::test]
    async fn run_api_provider_scores_happy_path() {
        // Verify the API scoring path end-to-end with a wiremock server:
        // one slice, the canned summary keeps both needles, no false-done.
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        // Canned tight summary — keeps both needles, no completion claim.
        let canned = "GOAL: harden retries\nFACTS: MAX_RETRIES=5 in session_7421.rs";
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "content": [{"type": "text", "text": canned}]
            })))
            .mount(&server)
            .await;

        // SAFETY: test-only, unique env var name.
        #[allow(unsafe_code)]
        unsafe {
            std::env::set_var("BENCH_TEST_API_KEY", "sk-test");
        }

        let provider = trimwire::config::SummarizerProviderConfig {
            id: "test-api".to_owned(),
            style: "anthropic".to_owned(),
            base_url: server.uri(),
            full_url: None,
            model: "test-model".to_owned(),
            api_key_env: "BENCH_TEST_API_KEY".to_owned(),
            api_key_file: None,
            timeout_secs: 5,
        };
        let big_slice = "### user\n".to_owned() + &"[tool_result] noise ".repeat(60);
        let corpus = vec![CorpusSlice {
            id: "s1".to_owned(),
            false_done_trap: false,
            needles: vec!["MAX_RETRIES".to_owned(), "session_7421.rs".to_owned()],
            slice: big_slice,
        }];
        let score = run_api_provider(&provider, &corpus, 1, None).await;
        assert_eq!(score.backend, "api");
        assert_eq!(score.model, "test-api");
        assert!(!score.gated, "a clean API summary must not be gated");
        assert_eq!(score.slices.len(), 1);
        assert_eq!(
            (score.slices[0].kept, score.slices[0].total),
            (2, 2),
            "both needles must be retained"
        );
        assert_eq!(score.slices[0].false_done, 0);

        #[allow(unsafe_code)]
        unsafe {
            std::env::remove_var("BENCH_TEST_API_KEY");
        }
    }

    #[tokio::test]
    async fn run_api_provider_max_calls_caps_slices() {
        // max_calls=1 on a 3-slice corpus must only score 1 slice.
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "content": [{"type": "text", "text": "GOAL: ok\nFACTS: alpha beta"}]
            })))
            .expect(1) // exactly ONE call (the cap)
            .mount(&server)
            .await;

        #[allow(unsafe_code)]
        unsafe {
            std::env::set_var("BENCH_MAX_CALLS_KEY", "sk-test");
        }

        let provider = trimwire::config::SummarizerProviderConfig {
            id: "test-api".to_owned(),
            style: "anthropic".to_owned(),
            base_url: server.uri(),
            full_url: None,
            model: "test-model".to_owned(),
            api_key_env: "BENCH_MAX_CALLS_KEY".to_owned(),
            api_key_file: None,
            timeout_secs: 5,
        };
        let big = "### user\n[tool_result] ".to_owned() + &"x".repeat(400);
        let corpus = vec![
            CorpusSlice {
                id: "s1".to_owned(),
                false_done_trap: false,
                needles: vec!["alpha".to_owned()],
                slice: big.clone(),
            },
            CorpusSlice {
                id: "s2".to_owned(),
                false_done_trap: false,
                needles: vec!["beta".to_owned()],
                slice: big.clone(),
            },
            CorpusSlice {
                id: "s3".to_owned(),
                false_done_trap: false,
                needles: vec!["gamma".to_owned()],
                slice: big,
            },
        ];
        let score = run_api_provider(&provider, &corpus, 1, None).await;
        assert_eq!(score.slices.len(), 1, "max_calls=1 must cap at 1 slice");
        server.verify().await; // exactly 1 HTTP call

        #[allow(unsafe_code)]
        unsafe {
            std::env::remove_var("BENCH_MAX_CALLS_KEY");
        }
    }

    #[tokio::test]
    async fn run_api_provider_errors_produce_gated_score() {
        // When the API call fails (e.g. server returns 500), the slice is scored as
        // errored → gated aggregate.
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;

        #[allow(unsafe_code)]
        unsafe {
            std::env::set_var("BENCH_ERROR_KEY", "sk-test");
        }

        let provider = trimwire::config::SummarizerProviderConfig {
            id: "test-api".to_owned(),
            style: "anthropic".to_owned(),
            base_url: server.uri(),
            full_url: None,
            model: "test-model".to_owned(),
            api_key_env: "BENCH_ERROR_KEY".to_owned(),
            api_key_file: None,
            timeout_secs: 5,
        };
        let corpus = vec![CorpusSlice {
            id: "s1".to_owned(),
            false_done_trap: false,
            needles: vec!["alpha".to_owned()],
            slice: "### user\nsome text here".to_owned(),
        }];
        let score = run_api_provider(&provider, &corpus, 5, None).await;
        assert_eq!(score.backend, "api");
        assert!(score.gated, "an API error must gate the aggregate");
        assert_eq!(score.composite, 0.0);
        assert!(
            score.slices[0].error.is_some(),
            "the errored slice must carry the error message"
        );

        #[allow(unsafe_code)]
        unsafe {
            std::env::remove_var("BENCH_ERROR_KEY");
        }
    }

    // ── dry-run upload gate: `share benchmark` without --yes is upload-free ───────
    //
    // `run_share` is the upload side of `trimwire share benchmark`. The "dry-run is
    // model-free" claim has two halves:
    //   (1) no model/API call without `--yes` — the `api_safety_warning` gate
    //       (tested above). NOTE the precise scope: that gate only covers an API
    //       PROVIDER. A LOCAL ollama tag is ALWAYS scored live via `run_model`
    //       (there is no dry-run on the local branch), so the model-free guarantee
    //       holds specifically for `--model <provider-id>` without `--yes`.
    //   (2) no network UPLOAD without `--yes` — covered by the tests below, which
    //       hold regardless of backend.
    // We prove (2) with a differential against a closed endpoint: identical inputs,
    // only `yes` flips, and only `yes=true` ever touches the socket. (`post()` is the
    // sole network call in run_share, reached only after the `!yes` early-return.)

    /// A 127.0.0.1 port with nothing listening: bind `:0` to claim a free port, then
    /// drop the listener so the port is closed again. A connect attempt is then
    /// refused immediately (fast — unlike port 0, which stalls). Standard
    /// recently-freed-port pattern; the reuse window is a negligible flake risk.
    fn closed_endpoint() -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
        let port = listener.local_addr().expect("local_addr").port();
        drop(listener);
        format!("http://127.0.0.1:{port}")
    }

    /// A clean LOCAL benchmark row that `is_uploadable_row` accepts — i.e. one that
    /// WOULD be uploaded with `--yes`. Built exactly like the uploadable fixture in
    /// `is_uploadable_row_rejects_dry_run_and_failures`.
    fn clean_local_row() -> ModelScore {
        let s = slice("s1", false, &["alpha"], "alpha beta gamma delta epsilon");
        aggregate(
            "qwen3.5:4b".into(),
            "local",
            vec![score_summary(&s, "alpha")],
        )
    }

    #[test]
    fn run_share_without_yes_does_not_upload() {
        let row = clean_local_row();
        // Non-vacuous pre-condition: with `--yes` this row WOULD be uploaded, so the
        // only thing preventing the POST below is the missing `--yes`.
        assert!(is_uploadable_row(&row), "fixture must be an uploadable row");
        // Dry run against a closed endpoint: had it tried to POST, post() would get
        // connection-refused and return Err. Ok ⇒ no socket was ever touched.
        let r = run_share(&[row], /* yes = */ false, &closed_endpoint());
        assert!(
            r.is_ok(),
            "dry-run `share benchmark` must be inert (no upload) and return Ok: {r:?}"
        );
    }

    #[test]
    fn run_share_with_yes_attempts_upload() {
        // Positive control / differential vs the test above: SAME uploadable row, SAME
        // closed endpoint, only `yes` flips to true. Now post() IS reached, so the
        // connect to the closed port fails ⇒ Err. This proves the Ok in the dry-run
        // test is the `!yes` gate firing — not a no-op that would never upload anyway.
        let row = clean_local_row();
        let r = run_share(&[row], /* yes = */ true, &closed_endpoint());
        assert!(
            r.is_err(),
            "with --yes, run_share must reach post() and fail against a closed endpoint"
        );
    }

    #[test]
    fn run_share_with_only_dry_run_rows_uploads_nothing() {
        // An API provider scored WITHOUT --yes yields an `api-dry-run` placeholder (no
        // API call was made). When that placeholder is the only result, run_share must
        // produce ZERO upload bodies — so even with `yes=true` it never reaches post():
        // it returns Ok ("nothing to share") instead of connecting. (This is the
        // `share benchmark` upload path; `summarizer benchmark` never calls run_share
        // at all — it only renders the dry-run row — so it has no upload to gate.)
        let dry = aggregate("my-provider (DRY RUN)".into(), "api-dry-run", vec![]);
        assert!(
            !is_uploadable_row(&dry),
            "an api-dry-run placeholder is never uploadable"
        );
        let r = run_share(&[dry], /* yes = */ true, &closed_endpoint());
        assert!(
            r.is_ok(),
            "a dry-run-only result has no bodies to upload ⇒ no network ⇒ Ok: {r:?}"
        );
    }
}
