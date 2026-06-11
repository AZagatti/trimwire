//! End-to-end local-model compaction benchmark on a REAL `/v1/messages` body.
//!
//! Drives the actual gateway/reprune path: cold checkpoint (model-free) →
//! select + summarize the OLD slice with the LIVE local model → install the
//! summary → stable replay (model-free + summary substitution). Reports the
//! messages[] byte reduction at each stage, the summarized-slice compression,
//! pairing validity, and the model latency. This is the integration test the
//! deterministic unit tests can't be (it needs a live model).
//!
//! Run:
//!   cargo run --release --example compaction_bench
//!   cargo run --release --example compaction_bench -- benchmark/fixtures/long_running.json

#[tokio::main]
async fn main() {
    std::process::exit(run().await);
}

async fn run() -> i32 {
    use std::time::Instant;

    use serde_json::Value;
    use trimwire::config::{SummarizerConfig, SummarizerLocalConfig, profile_baseline};
    use trimwire::pairing::PairingIndex;
    use trimwire::reprune::{PruneState, stable_apply_to_body};
    use trimwire::summarizer::{build_prompt, call_model, slice};

    let path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "benchmark/fixtures/mixed_realistic.json".to_owned());
    let body = match std::fs::read(&path) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("cannot read {path}: {e}");
            return 1;
        }
    };
    let root: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("{path} is not JSON: {e}");
            return 1;
        }
    };
    let messages = root["messages"].as_array().cloned().unwrap_or_default();
    let msg_bytes = |m: &[Value]| serde_json::to_vec(m).map(|v| v.len()).unwrap_or(0);
    let orig = msg_bytes(&messages);

    // The aggressive default profile (reprune + all model-free strategies on).
    let cfg = profile_baseline("default");
    let lm = SummarizerConfig {
        engine: "local".to_owned(),
        timeout_secs: 240,
        local: SummarizerLocalConfig {
            model: std::env::var("TRIMWIRE_HARM_MODEL")
                .unwrap_or_else(|_| SummarizerLocalConfig::default().model),
            endpoint: std::env::var("TRIMWIRE_HARM_ENDPOINT")
                .unwrap_or_else(|_| SummarizerLocalConfig::default().endpoint),
            ..Default::default()
        },
        ..Default::default()
    };

    println!("── compaction bench: {path} ──");
    println!(
        "messages: {} turns, {orig} bytes (model={})",
        messages.len(),
        lm.local.model
    );

    // 1. Cold checkpoint — model-free only.
    let mut state = PruneState::default();
    let cold = stable_apply_to_body(&body, &cfg, &mut state, cfg.reprune.threshold);
    let mf_bytes = outcome_msg_bytes(&cold, &body);

    // 2. Select + summarize the OLD slice with the LIVE model.
    let Some((start, end)) =
        slice::select_slice(&messages, lm.keep_recent_turns, state.checkpoint_len())
    else {
        println!(
            "no eligible slice (transcript too short for keep_recent={})",
            lm.keep_recent_turns
        );
        return 0;
    };
    let slice_orig = msg_bytes(&messages[start..end]);
    let slice_text = slice::serialize_slice(
        &messages[start..end],
        slice::REASONING_BLOCK_CAP,
        slice::TOOL_RESULT_BLOCK_CAP,
    );
    println!(
        "slice: msgs[{start}..{end}] = {} turns, {slice_orig} bytes (prompt {} chars)",
        end - start,
        build_prompt(&slice_text).len()
    );

    let t = Instant::now();
    let summary = match call_model(&lm.local, lm.timeout_secs, build_prompt(&slice_text)).await {
        Ok(s) => s,
        Err(e) => {
            eprintln!("SKIP: local model unavailable ({e})");
            return 0;
        }
    };
    let latency = t.elapsed();
    let d = match slice::SummaryDecision::new(&messages, start, end, &summary) {
        Some(d) => d,
        None => {
            eprintln!("summary produced an empty decision");
            return 1;
        }
    };
    let summary_bytes = msg_bytes(&d.messages);
    // The production install gate: would the summary actually beat model-free
    // pruning on this slice? (We still install it below to MEASURE the replay.)
    let gate_keeps =
        trimwire::summarizer::summary_is_smaller(&d.messages, &messages, start, end, &cfg);
    state.set_summary(d);

    // 3. Stable replay — model-free + summary substitution (same body, append-only).
    let warm = stable_apply_to_body(&body, &cfg, &mut state, cfg.reprune.threshold);
    let warm_bytes = outcome_msg_bytes(&warm, &body);

    // Validate the forwarded body is orphan-free.
    let warm_root: Value = match &warm {
        trimwire::strategies::BodyOutcome::Mutated { bytes, .. } => {
            serde_json::from_slice(bytes).unwrap()
        }
        trimwire::strategies::BodyOutcome::Unchanged => root.clone(),
    };
    let pairing_ok = PairingIndex::build(warm_root["messages"].as_array().unwrap())
        .validate()
        .is_ok();

    let pct = |n: usize| 100.0 * (orig.saturating_sub(n)) as f64 / orig as f64;
    println!("\n── results ──");
    println!("original messages[]            : {orig} bytes");
    println!(
        "model-free prune (cold)        : {mf_bytes} bytes  ({:.1}% smaller)",
        pct(mf_bytes)
    );
    println!(
        "model-free + local summary     : {warm_bytes} bytes  ({:.1}% smaller)",
        pct(warm_bytes)
    );
    println!(
        "summarized slice               : {slice_orig} → {summary_bytes} bytes  ({:.1}% slice compression)",
        100.0 * (slice_orig.saturating_sub(summary_bytes)) as f64 / slice_orig.max(1) as f64
    );
    println!(
        "local-model latency            : {:.1}s",
        latency.as_secs_f64()
    );
    println!(
        "install gate (summary<model-free): {}  → {}",
        gate_keeps,
        if gate_keeps {
            "KEEP summary"
        } else {
            "REJECT (model-free wins; production keeps model-free)"
        }
    );
    println!("pairing valid after replay     : {pairing_ok}");
    println!(
        "summary present in forwarded   : {}",
        serde_json::to_string(&warm_root["messages"])
            .unwrap()
            .contains("local-model compaction")
    );

    if !pairing_ok {
        eprintln!("FAIL: orphaned pair after summary replay");
        return 1;
    }
    println!("\nPASS (end-to-end replay valid)");
    0
}

fn outcome_msg_bytes(o: &trimwire::strategies::BodyOutcome, original: &[u8]) -> usize {
    use serde_json::Value;
    let bytes = match o {
        trimwire::strategies::BodyOutcome::Mutated { bytes, .. } => bytes.clone(),
        trimwire::strategies::BodyOutcome::Unchanged => original.to_vec(),
    };
    let v: Value = serde_json::from_slice(&bytes).unwrap();
    serde_json::to_vec(&v["messages"])
        .map(|b| b.len())
        .unwrap_or(0)
}
