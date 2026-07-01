//! P0a — COST-REPLAY: does opt-in local-model compaction actually save quota?
//!
//! Replays a REAL session turn-by-turn under Anthropic prompt-cache pricing and
//! compares two arms on the SAME transcript:
//! - BASELINE: production model-free pruning (reprune + strategies).
//! - COMPACTION: the same, plus the local-model summary of the OLD slice,
//!   installed at a realistic re-summarization cadence.
//!
//! The cost model bills, per turn, the prompt-cache buckets the council formula
//! uses: `cost = P·[input + 0.1·cache_read + 1.25·cache_creation]`. The reused
//! prefix is `cache_read` (0.1×); the freshly-appended (and re-summarized) region
//! is `cache_creation` (1.25×) — so a re-summarization that rewrites the prefix
//! is billed at the FULL 1.25× cache-write rate that turn (the "bust"), exactly
//! the cost the feature must out-earn. Billing fresh at 1.25× is CONSERVATIVE
//! against compaction (it makes every bust maximally expensive).
//!
//! The summarizer delegates to the SHIPPED production `call_model` (facts-first
//! system prompt incl. rule 6, /api/chat, conservative num_ctx, num_predict,
//! near-greedy, think:false) — since Phase 1a that IS the harness, so this measures
//! the feature exactly as it ships.
//!
//! Run (needs ollama serving + the model pulled):
//!   cargo run --release --example cost_replay -- BODY.json [BODY2.json ...]
//!   TRIMWIRE_COST_MODEL=qwen3.5:2b cargo run --release --example cost_replay -- BODY.json
//! Env: TRIMWIRE_COST_MODEL (default qwen3.5:4b), TRIMWIRE_COST_ENDPOINT,
//!      TRIMWIRE_COST_PROFILE (default), TRIMWIRE_COST_RESUMMARIZE_AFTER (msgs, default 24),
//!      TRIMWIRE_COST_SLICE_CHARS (model-input cap, default 40000),
//!      TRIMWIRE_COST_INPUT_PER_MTOK (default 3.0).

#[path = "../benchmark/corpora.rs"]
#[allow(dead_code)]
mod corpora;

#[tokio::main]
async fn main() {
    std::process::exit(run().await);
}

const CACHE_READ_MULT: f64 = 0.10;
const CACHE_CREATE_MULT: f64 = 1.25;

/// Prompt-cache token buckets accumulated over a whole session replay.
#[derive(Default, Clone, Copy)]
struct Buckets {
    /// Truly-uncached input tokens billed at 1.0× (here: ~0; CC caches the prefix).
    input: f64,
    /// Cache-read tokens (reused prefix) billed at 0.1×.
    cache_read: f64,
    /// Cache-creation tokens (freshly written prefix, incl. re-summarization busts) at 1.25×.
    cache_create: f64,
}

impl Buckets {
    fn dollars(&self, input_per_mtok: f64) -> f64 {
        self.dollars_with(CACHE_CREATE_MULT, input_per_mtok)
    }
    /// Cost under an arbitrary cache-creation multiplier — used to bound the result
    /// between the council formula (1.25×, conservative against compaction) and the
    /// `session_cost`-style simplification (1.0×, ignores the cache-write surcharge).
    fn dollars_with(&self, create_mult: f64, input_per_mtok: f64) -> f64 {
        (self.input + CACHE_READ_MULT * self.cache_read + create_mult * self.cache_create)
            / 1_000_000.0
            * input_per_mtok
    }
    fn total_tokens(&self) -> f64 {
        self.input + self.cache_read + self.cache_create
    }
}

async fn run() -> i32 {
    use trimwire::config::{SummarizerConfig, SummarizerLocalConfig, profile_baseline};

    // Offline-telemetry subscriber: surfaces the production tracing logs
    // (`trimwire: summarizer compaction installed` with ratio_pct/segments, and
    // `reprune re-checkpoint` with saved_bytes) when replaying a real session with
    // RUST_LOG=trimwire=debug — the instrument that validates accumulator behavior
    // and re-checkpoint economics at scale without a live session.
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("trimwire=info")),
        )
        .with_writer(std::io::stderr)
        .try_init();

    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        eprintln!("usage: cost_replay BODY.json [BODY2.json ...]");
        return 1;
    }
    let env = |k: &str, d: &str| std::env::var(k).unwrap_or_else(|_| d.to_owned());
    let profile = env("TRIMWIRE_COST_PROFILE", "default");
    let input_per_mtok: f64 = env("TRIMWIRE_COST_INPUT_PER_MTOK", "3.0")
        .parse()
        .unwrap_or(3.0);
    let resummarize_after: usize = env("TRIMWIRE_COST_RESUMMARIZE_AFTER", "24")
        .parse()
        .unwrap_or(24);
    let slice_chars: usize = env("TRIMWIRE_COST_SLICE_CHARS", "40000")
        .parse()
        .unwrap_or(40_000);

    let lm = SummarizerConfig {
        engine: "local".to_owned(),
        timeout_secs: 240,
        local: SummarizerLocalConfig {
            keep_alive_secs: 30,
            model: env("TRIMWIRE_COST_MODEL", "qwen3.5:4b"),
            endpoint: env("TRIMWIRE_COST_ENDPOINT", "http://localhost:11434"),
            ..Default::default()
        },
        ..Default::default()
    };
    let cfg = profile_baseline(&profile);

    println!("# P0a cost-replay");
    println!(
        "model={}  profile={}  resummarize_after={}msgs  slice_cap={}chars  ${}/Mtok  \
         (cache_read {}× / cache_create {}×)\n",
        lm.local.model,
        profile,
        resummarize_after,
        slice_chars,
        input_per_mtok,
        CACHE_READ_MULT,
        CACHE_CREATE_MULT
    );

    let mut any_fail = false;
    for path in &args {
        match replay_one(
            path,
            &cfg,
            &lm,
            input_per_mtok,
            resummarize_after,
            slice_chars,
        )
        .await
        {
            Ok(()) => {}
            Err(e) => {
                eprintln!("{path}: ERROR {e}");
                any_fail = true;
            }
        }
    }
    if any_fail { 1 } else { 0 }
}

async fn replay_one(
    path: &str,
    cfg: &trimwire::config::Config,
    lm: &trimwire::config::SummarizerConfig,
    input_per_mtok: f64,
    resummarize_after: usize,
    slice_chars: usize,
) -> Result<(), String> {
    use serde_json::Value;

    let body = std::fs::read(path).map_err(|e| e.to_string())?;
    let root: Value = serde_json::from_slice(&body).map_err(|e| e.to_string())?;
    let messages = root["messages"]
        .as_array()
        .cloned()
        .ok_or("no messages[]")?;
    let bounds = corpora::turn_bounds(&messages);
    if bounds.len() < 8 {
        return Err(format!(
            "only {} turns — too short to be a long session",
            bounds.len()
        ));
    }

    // BASELINE: model-free reprune, no model. COMPACTION: single-summary (replace).
    // ACCUMULATOR: same, but re-summarization APPENDS frozen delta segments.
    let (base, base_curve, _n, _s0) = replay_arm(
        &messages,
        &bounds,
        cfg,
        None,
        input_per_mtok,
        resummarize_after,
        slice_chars,
    )
    .await;
    let single_lm = {
        let mut l = lm.clone();
        l.accumulator = false;
        l
    };
    let (comp, comp_curve, n_summ, _s1) = replay_arm(
        &messages,
        &bounds,
        cfg,
        Some(&single_lm),
        input_per_mtok,
        resummarize_after,
        slice_chars,
    )
    .await;
    let acc_lm = {
        let mut l = lm.clone();
        l.accumulator = true;
        l
    };
    let (acc, acc_curve, n_acc, acc_segments) = replay_arm(
        &messages,
        &bounds,
        cfg,
        Some(&acc_lm),
        input_per_mtok,
        resummarize_after,
        slice_chars,
    )
    .await;

    let bd = base.dollars(input_per_mtok);
    let cd = comp.dollars(input_per_mtok);
    let delta = cd - bd;
    let rel = if bd > 0.0 { 100.0 * delta / bd } else { 0.0 };

    // Break-even turn: first turn from which the cumulative compaction cost stays
    // ≤ baseline for the rest of the session.
    let mut breakeven: Option<usize> = None;
    for i in 0..base_curve.len() {
        if (i..base_curve.len()).all(|j| comp_curve[j] <= base_curve[j]) {
            breakeven = Some(i + 1);
            break;
        }
    }

    println!("## {path}");
    println!(
        "turns={}  messages={}  re-summarizations={}",
        bounds.len(),
        messages.len(),
        n_summ
    );
    println!(
        "baseline   : ${:.5}  ({:.0} Mtok-eq: read {:.1}M / create {:.1}M)",
        bd,
        base.total_tokens() / 1e6,
        base.cache_read / 1e6,
        base.cache_create / 1e6
    );
    println!(
        "compaction : ${:.5}  ({:.0} Mtok-eq: read {:.1}M / create {:.1}M)",
        cd,
        comp.total_tokens() / 1e6,
        comp.cache_read / 1e6,
        comp.cache_create / 1e6
    );
    let verdict = if delta < 0.0 { "SAVES" } else { "COSTS MORE" };
    println!(
        "ΔC/C_baseline = {:+.2}%   ({verdict}; Δ=${:+.5})   [council 1.25× cache-create]",
        rel, delta
    );
    // Sensitivity bound: same buckets billed with cache-create at 1.0× (the
    // session_cost simplification — ignores the cache-write surcharge). This is the
    // MOST favorable-to-compaction billing; if it still costs more here, the busts
    // dominate regardless of the surcharge.
    let b1 = base.dollars_with(1.0, input_per_mtok);
    let c1 = comp.dollars_with(1.0, input_per_mtok);
    let rel1 = if b1 > 0.0 {
        100.0 * (c1 - b1) / b1
    } else {
        0.0
    };
    println!(
        "ΔC/C_baseline = {:+.2}%   ({}; Δ=${:+.5})   [1.0× cache-create sensitivity]",
        rel1,
        if c1 < b1 { "SAVES" } else { "COSTS MORE" },
        c1 - b1
    );
    match breakeven {
        Some(t) => println!(
            "break-even: turn {t}/{} (compaction ≤ baseline from here on)",
            bounds.len()
        ),
        None => println!("break-even: NEVER reached within this session"),
    }

    // ACCUMULATOR arm: cost vs baseline, and the MARGINAL benefit over single-summary.
    let ad = acc.dollars(input_per_mtok);
    let rel_acc = if bd > 0.0 {
        100.0 * (ad - bd) / bd
    } else {
        0.0
    };
    let marginal = if cd > 0.0 {
        100.0 * (ad - cd) / cd
    } else {
        0.0
    };
    // How often did append actually fire? segments-1 appends out of n_acc re-summaries
    // (segment 1 is the seed); the rest fell back to replace (delta didn't fit budget).
    let appends = acc_segments.saturating_sub(1);
    println!(
        "accumulator: ${:.5}  (read {:.1}M / create {:.1}M)  segments={acc_segments} \
         (appends={appends}/{n_acc} re-summaries; rest fell back to replace)",
        ad,
        acc.cache_read / 1e6,
        acc.cache_create / 1e6,
    );
    println!(
        "ΔC/C_baseline = {:+.2}%  ({}) vs baseline;  marginal vs single-summary = {:+.2}% \
         ({})\n",
        rel_acc,
        if ad < bd { "SAVES" } else { "COSTS MORE" },
        marginal,
        if ad < cd {
            "accumulator cheaper"
        } else if ad > cd {
            "accumulator dearer"
        } else {
            "equal"
        },
    );
    let _ = acc_curve;
    Ok(())
}

/// Replay one arm; returns (final buckets, cumulative-$ per turn, #re-summarizations).
async fn replay_arm(
    messages: &[serde_json::Value],
    bounds: &[usize],
    cfg: &trimwire::config::Config,
    lm: Option<&trimwire::config::SummarizerConfig>,
    input_per_mtok: f64,
    resummarize_after: usize,
    slice_chars: usize,
) -> (Buckets, Vec<f64>, usize, usize) {
    use serde_json::{Value, json};
    use trimwire::reprune::{PruneState, stable_apply_to_body};
    use trimwire::strategies::BodyOutcome;

    let mut state = PruneState::default();
    let mut acc = Buckets::default();
    let mut curve = Vec::with_capacity(bounds.len());
    let mut prev: Option<Vec<Value>> = None;
    let mut n_summ = 0usize;

    for &end in bounds {
        let upto = &messages[..=end];
        let body_bytes = match serde_json::to_vec(&json!({"messages": upto})) {
            Ok(b) => hyper::body::Bytes::from(b),
            Err(_) => continue,
        };
        let outcome = stable_apply_to_body(&body_bytes, cfg, &mut state, cfg.reprune.threshold);
        let snap: Vec<Value> = match &outcome {
            BodyOutcome::Mutated { bytes, .. } => serde_json::from_slice::<Value>(bytes)
                .ok()
                .and_then(|v| v["messages"].as_array().cloned())
                .unwrap_or_else(|| upto.to_vec()),
            BodyOutcome::Unchanged => upto.to_vec(),
        };

        bill_turn(&mut acc, prev.as_deref(), &snap);
        curve.push(acc.dollars(input_per_mtok));
        prev = Some(snap);

        // COMPACTION: decide a (re)summarization for the NEXT turn's replay.
        if let Some(lm) = lm {
            if maybe_resummarize(&mut state, upto, cfg, lm, resummarize_after, slice_chars).await {
                n_summ += 1;
            }
        }
    }
    let segments = state.summary_segment_count();
    (acc, curve, n_summ, segments)
}

/// Bill one turn into the cache buckets: the leading byte-identical message run
/// is cache_read (0.1×); everything after it is cache_create (1.25×, the cached
/// prefix being (re)written). A constant system+tools prefix is written once then
/// cache-read every later turn (pure denominator — cancels in the ratio).
fn bill_turn(acc: &mut Buckets, prev: Option<&[serde_json::Value]>, snap: &[serde_json::Value]) {
    let total = corpora::est_tokens(corpora::serialized_len(snap)) as f64;
    let cached = match prev {
        None => 0.0,
        Some(p) => {
            let common = corpora::leading_common_msgs(p, snap);
            corpora::est_tokens(corpora::serialized_len(&snap[..common])) as f64
        }
    };
    let fresh = (total - cached).max(0.0);
    acc.cache_read += cached;
    acc.cache_create += fresh;
    // Constant prefix: created on turn 1, cache-read after.
    if prev.is_none() {
        acc.cache_create += corpora::PREFIX_TOKENS as f64;
    } else {
        acc.cache_read += corpora::PREFIX_TOKENS as f64;
    }
}

/// Mirror production gating: if a checkpoint exists and a fresh, sufficiently-grown
/// OLD slice is worth a call, summarize it live and install it (subject to the
/// `summary_is_smaller` gate). Returns true iff a new summary was installed. Never
/// load-bearing — any model failure just leaves model-free pruning in place.
async fn maybe_resummarize(
    state: &mut trimwire::reprune::PruneState,
    upto: &[serde_json::Value],
    cfg: &trimwire::config::Config,
    lm: &trimwire::config::SummarizerConfig,
    resummarize_after: usize,
    slice_chars: usize,
) -> bool {
    use trimwire::summarizer::{slice, summary_is_smaller};

    // Production gate (maybe_spawn_summarization): never engage until the body
    // exceeds trigger_bytes — short/early sessions never call the model. Modeled
    // here for fidelity (the messages[] bytes dominate the real body size).
    if corpora::serialized_len(upto) <= lm.trigger_bytes {
        return false;
    }
    let max_end = state.checkpoint_len();
    let Some((mut start, end)) = slice::select_slice(upto, lm.keep_recent_turns, max_end) else {
        return false;
    };
    // Batch: only re-summarize once the slice end has advanced enough AND the
    // cached summary still anchors (mirrors production maybe_spawn_summarization —
    // a stale anchor after a history rewrite re-summarizes immediately). In an
    // append-only replay the anchor always matches, so this is for faithfulness.
    if let Some(prev_end) = state.summary_slice_end() {
        if state.summary_anchor_matches(upto) && end < prev_end + resummarize_after {
            return false;
        }
    }
    // Phase-2.5 slice-size cap (applied simply here): narrow `start` forward to an
    // assistant-turn boundary so the serialized slice fits the model-input budget
    // (avoids feeding a >context slice → silent truncation). Summarize the most
    // recent old turns; earlier old content stays model-free-pruned.
    start = cap_slice_start(upto, start, end, slice_chars);

    // Phase-2.5b ACCUMULATOR arm (mirrors maybe_spawn_summarization): when enabled and
    // the cached chain still anchors (under the segment cap), summarize the delta in a
    // budget-sized CONTIGUOUS chunk [prev_end..capped_end] via cap_slice_end and APPEND
    // a frozen segment; else REPLACE the capped full slice.
    let (seg_start, seg_end, is_append) = if lm.accumulator
        && state.summary_anchor_matches(upto)
        && state.summary_segment_count() < 64
    {
        match state.summary_slice_end() {
            Some(prev_end) if end > prev_end + 4 => {
                let capped_end = slice::cap_slice_end(
                    upto,
                    prev_end,
                    end,
                    slice::REASONING_BLOCK_CAP,
                    slice::TOOL_RESULT_BLOCK_CAP,
                    slice_chars,
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
    if seg_end < seg_start + 4 {
        return false;
    }

    let slice_text = slice::serialize_slice(
        &upto[seg_start..seg_end],
        slice::REASONING_BLOCK_CAP,
        slice::TOOL_RESULT_BLOCK_CAP,
    );
    let summary = match summarize_live(lm, &slice_text).await {
        Ok(s) => s,
        Err(_) => return false, // never load-bearing
    };
    let Some(d) = slice::SummaryDecision::new(upto, seg_start, seg_end, &summary) else {
        return false;
    };
    if !summary_is_smaller(&d.messages, upto, seg_start, seg_end, cfg) {
        return false; // model-free wins this region → keep it
    }
    if !(is_append && state.append_summary(d.clone())) {
        state.set_summary(d);
    }
    true
}

/// Earliest assistant-turn start in `[start, end)` whose serialized slice
/// `[s..end]` is ≤ `budget` chars — i.e. the LARGEST ≤budget slice ending at the
/// protected boundary (first-ship intent: compact as much old content as the
/// model context fits; the oldest turns that don't fit stay model-free-pruned).
/// Falls back to `start` if even the smallest 2-pair slice exceeds the budget.
fn cap_slice_start(
    messages: &[serde_json::Value],
    start: usize,
    end: usize,
    budget: usize,
) -> usize {
    use serde_json::Value;
    let asst: Vec<usize> = (start..end)
        .filter(|&i| messages[i].get("role").and_then(Value::as_str) == Some("assistant"))
        .collect();
    let mut best = start;
    for &s in &asst {
        if s + 4 > end {
            break;
        }
        if corpora::serialized_len(&messages[s..end]) <= budget {
            best = s;
            break;
        }
    }
    best
}

/// The council-locked FREE-FORM facts-first harness (mirrors benchmark/model_bench.sh
/// and the Phase-1 ship intent): /api/chat, conservative num_ctx, num_predict,
/// near-greedy sampling, stop sequences, `think:false` for qwen3.
/// Summarize via the SHIPPED production path. `call_model` IS the ship-intent harness
/// (since Phase 1a): /api/chat, facts-first SUMMARY_SYSTEM_PROMPT, conservative
/// num_ctx, num_predict, per-model top_p, `think:false` for qwen3, `<think>` strip.
/// Delegating keeps the cost replay's summaries byte-for-byte the production output —
/// no drifting copy of the prompt/options.
async fn summarize_live(
    lm: &trimwire::config::SummarizerConfig,
    slice_text: &str,
) -> Result<String, String> {
    trimwire::summarizer::call_model(
        &lm.local,
        lm.timeout_secs,
        trimwire::summarizer::build_prompt(slice_text),
    )
    .await
    .map_err(|e| e.to_string())
}
