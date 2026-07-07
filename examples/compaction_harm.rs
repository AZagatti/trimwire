//! Harm gate for the opt-in summarizer (`docs/SUMMARIZER.md`).
//! Plants distinctive, verbatim-preservable facts (file paths, exact
//! error text, decisions, constants) across an OLD slice, summarizes it with the
//! configured local model, and measures how many facts survive the summary.
//!
//! Run:  `cargo run --release --example compaction_harm`
//!
//! Exit codes: 0 = retention ≥ threshold (or SKIP when no local model is
//! reachable); 1 = retention below threshold. The local model is
//! non-deterministic, so this is a measurement gate, not a unit test.

#[tokio::main]
async fn main() {
    std::process::exit(run().await);
}

async fn run() -> i32 {
    use serde_json::{Value, json};
    use trimwire::config::SummarizerLocalConfig;
    use trimwire::summarizer::slice;

    // Retention threshold the feature must clear to be considered usable
    // (§ADJUDICATED FINAL PLAN P0b: ≥90–95%). We gate at 0.90 and report the exact %.
    const THRESHOLD: f64 = 0.90;

    // (label, the irreducible fact token a faithful summary must preserve). We
    // match the distinctive token, not the surrounding phrasing — paraphrase is
    // fine, dropping the fact is not.
    let synthetic_facts: &[(&str, &str)] = &[
        ("auth file path", "session_7421.rs"),
        ("migration file path", "migrate_v9.sql"),
        ("compile error code", "e0277"),
        ("trait in error", "job: send"),
        ("network errno", "econnrefused"),
        // The decision's load-bearing outcome is the CHOSEN backend (SQLite);
        // a faithful summary keeps that even if it drops the rejected alternative.
        ("db decision (SQLite)", "sqlite"),
        ("retry constant", "max_retries"),
        ("function name", "reconcile_balances"),
        ("env var", "trimwire_audit"),
        ("listen port", "8765"),
        ("test count", "37 test"),
        ("todo note", "leap-second"),
    ];

    // Build a realistic OLD slice: each turn carries a load-bearing result line
    // (the facts) followed by trailing bulk noise (the kind summarization should
    // drop). The signal sits at the START of the result so it survives the
    // head+tail cap — mirroring real tool output where the key line isn't buried
    // in the exact middle of a megabyte of logs. Shape: whole
    // [assistant(tool_use), user(tool_result)] pairs.
    let mut slice_msgs: Vec<Value> = Vec::new();
    let bulk = "    debug: irrelevant trace line that should be summarized away\n".repeat(20);
    let turns: &[(&str, &str)] = &[
        (
            "Read the auth module to plan the change.",
            "Opened src/auth/session_7421.rs (210 lines); it defines fn reconcile_balances() \
             and currently uses a blocking call.",
        ),
        (
            "Run the build to see the failure.",
            "error[E0277]: the trait bound `Job: Send` is not satisfied — the spawned \
             task captures a non-Send handle.",
        ),
        (
            "Decide on the storage backend for the ledger.",
            "Decision: chose SQLite over Postgres for the ledger (single-file, no server). \
             Set MAX_RETRIES = 5 for the writer.",
        ),
        (
            "Try connecting to the dev DB.",
            "Connection failed: ECONNREFUSED on 127.0.0.1. The daemon listens on port 8765; \
             the audit sink is gated behind the TRIMWIRE_AUDIT env var.",
        ),
        (
            "Apply the migration and run the suite.",
            "Applied src/db/migrate_v9.sql; 37 tests passed. \
             TODO: handle the leap-second edge case in reconcile_balances.",
        ),
    ];
    for (i, (ask, result)) in turns.iter().enumerate() {
        let id = format!("h{i}");
        slice_msgs.push(json!({"role":"assistant","content":[
            {"type":"text","text": ask},
            {"type":"tool_use","id": id,"name":"Bash","input":{"command": format!("step {i}")}}
        ]}));
        // Load-bearing line first, then trailing bulk noise.
        slice_msgs.push(json!({"role":"user","content":[
            {"type":"tool_result","tool_use_id": id,"content": format!("{result}\n{bulk}")}
        ]}));
    }

    // `#[non_exhaustive]` — build from Default and set fields (no struct literal).
    // Override the model/endpoint from the env to A/B different local models,
    // e.g. TRIMWIRE_HARM_MODEL=qwen2.5-coder:3b.
    let mut cfg = SummarizerLocalConfig::default();
    if let Ok(model) = std::env::var("TRIMWIRE_HARM_MODEL") {
        cfg.model = model;
    }
    if let Ok(endpoint) = std::env::var("TRIMWIRE_HARM_ENDPOINT") {
        cfg.endpoint = endpoint;
    }
    let timeout_secs: u64 = 180;

    // Default: the hand-curated synthetic planted-fact slice. Override with a REAL
    // staged slice + curated load-bearing facts via env (TRIMWIRE_HARM_SLICE_FILE =
    // slice text, TRIMWIRE_HARM_FACTS_FILE = `label|needle` per line, `#` comments).
    let (facts, slice_text, source): (Vec<(String, String)>, String, String) = match (
        std::env::var("TRIMWIRE_HARM_SLICE_FILE"),
        std::env::var("TRIMWIRE_HARM_FACTS_FILE"),
    ) {
        (Ok(sf), Ok(ff)) => {
            let text = std::fs::read_to_string(&sf).unwrap_or_default();
            let facts = std::fs::read_to_string(&ff)
                .unwrap_or_default()
                .lines()
                .map(str::trim)
                .filter(|l| !l.is_empty() && !l.starts_with('#'))
                .map(|l| match l.split_once('|') {
                    Some((label, needle)) => (label.trim().to_owned(), needle.trim().to_owned()),
                    None => (l.to_owned(), l.to_owned()),
                })
                .collect();
            (facts, text, format!("real slice {sf}"))
        }
        _ => {
            let facts = synthetic_facts
                .iter()
                .map(|(l, n)| ((*l).to_owned(), (*n).to_owned()))
                .collect();
            (
                facts,
                slice::serialize_slice(
                    &slice_msgs,
                    slice::REASONING_BLOCK_CAP,
                    slice::TOOL_RESULT_BLOCK_CAP,
                ),
                "synthetic planted-fact slice".to_owned(),
            )
        }
    };
    println!(
        "── compaction harm gate ──\nmodel={}  endpoint={}\nsource={source}  facts={}  slice={} chars",
        cfg.model,
        cfg.endpoint,
        facts.len(),
        slice_text.len(),
    );

    // Use the free-form harness, which now MIRRORS the (Phase-1-fixed) production
    // `call_model` (/api/chat + num_ctx + facts-first prompt). Kept as a local copy
    // so the gate can A/B models via env without touching production config.
    let summary = match summarize_shipintent(&cfg, timeout_secs, &slice_text).await {
        Ok(s) => s,
        Err(e) => {
            // No reachable model → SKIP (don't fail a machine without ollama).
            eprintln!(
                "SKIP: local model unavailable ({e}). Pull the model and start ollama to run the gate."
            );
            return 0;
        }
    };

    println!("\n── summary ({} chars) ──\n{summary}\n", summary.len());

    // Deterministic false-done check (the retention gate below only catches DROPPED
    // facts; this catches INJECTED false completions — "tests passed"/"committed"
    // with no supporting evidence in the slice). Advisory: it does not change the
    // PASS/FAIL, but a flag here is a strong false-done signal worth a human look.
    let false_done = trimwire::summarizer::harm_check::detect_false_done(&summary, &slice_text);
    if false_done.is_empty() {
        println!("── false-done check ── none");
    } else {
        println!("── false-done check ── {} FLAG(S):", false_done.len());
        for f in &false_done {
            println!("  ⚠ {}\n      ↳ {}", f.claim, f.reason);
        }
    }
    println!();

    let hay = trimwire::summarizer::normalize_fact(&summary);
    let mut kept = 0usize;
    println!("── fact retention (normalized: case-insensitive, hyphen≡underscore) ──");
    for (label, needle) in &facts {
        let present = hay.contains(&trimwire::summarizer::normalize_fact(needle));
        if present {
            kept += 1;
        }
        println!("  [{}] {label}: {needle}", if present { "✓" } else { "·" });
    }
    let retention = kept as f64 / facts.len() as f64;
    println!(
        "\nretention: {kept}/{} = {:.1}%  (threshold {:.0}%)",
        facts.len(),
        retention * 100.0,
        THRESHOLD * 100.0,
    );

    if retention + 1e-9 >= THRESHOLD {
        println!("PASS");
        0
    } else {
        eprintln!("FAIL: retention below threshold");
        1
    }
}

/// The council-locked FREE-FORM facts-first harness — now equivalent to the
/// Summarize via the SHIPPED production path. Since Phase 1a, `call_model` IS the
/// ship-intent harness (/api/chat, facts-first SUMMARY_SYSTEM_PROMPT, conservative
/// num_ctx, num_predict, near-greedy, per-model top_p, `think:false` for qwen3,
/// `<think>` strip). Delegating here means the harm gate exercises EXACTLY the code
/// the proxy runs — no drifting copy of the prompt or sampling options. The gate
/// A/Bs models purely via the `model` field on `cfg` (set from TRIMWIRE_HARM_MODEL).
async fn summarize_shipintent(
    cfg: &trimwire::config::SummarizerLocalConfig,
    timeout_secs: u64,
    slice_text: &str,
) -> Result<String, String> {
    trimwire::summarizer::call_model(
        cfg,
        timeout_secs,
        trimwire::summarizer::build_prompt(slice_text),
    )
    .await
    .map_err(|e| e.to_string())
}
