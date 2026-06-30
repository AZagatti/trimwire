//! API-engine harm gate for the §15 LARGE slice budget (internal notes §16D).
//!
//! The local harm gate (`compaction_harm.rs`) validates a SMALL slice on a local model.
//! The §15 per-engine budget feeds an API engine a much LARGER slice (~128 KB+), where
//! the risk is the model dropping the OLDEST content. This gate plants distinctive
//! verbatim facts spread across a large slice (start / middle / end) and measures how
//! many survive a real API summary — reporting retention BY POSITION so an "early-drop"
//! degradation is visible, not just an aggregate.
//!
//! The slice-build + scoring live in `trimwire::summarizer::probe` (shared with the
//! installed-user command `trimwire summarizer probe`); this example wires it to a
//! provider configured purely from env vars (no config file needed).
//!
//! Run (one PAID call on your own key):
//!   ZAI_API_KEY=... cargo run --release --example api_harm
//!   TRIMWIRE_API_HARM_BYTES=262144 ZAI_API_KEY=... cargo run --release --example api_harm
//!
//! Env: TRIMWIRE_API_HARM_BYTES (default 131072), _BASE_URL (default Z.ai anthropic),
//! _MODEL (default GLM-4.5-Air), _STYLE (anthropic|openai), _KEY_ENV (default ZAI_API_KEY),
//! _FULL_URL (override the POST URL verbatim).
//!
//! Exit 0 = retention >= threshold (or SKIP if no key); 1 = below threshold. The API
//! model is non-deterministic, so this is a measurement gate, not a unit test.

use trimwire::config::SummarizerProviderConfig;
use trimwire::summarizer::{self, probe};

#[tokio::main]
async fn main() {
    std::process::exit(run().await);
}

async fn run() -> i32 {
    const THRESHOLD: f64 = 0.90;

    let target_bytes: usize = std::env::var("TRIMWIRE_API_HARM_BYTES")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(probe::DEFAULT_PROBE_BYTES);

    let slice = probe::build_probe_slice(target_bytes);

    let provider = SummarizerProviderConfig {
        id: "harm".to_owned(),
        style: std::env::var("TRIMWIRE_API_HARM_STYLE").unwrap_or_else(|_| "anthropic".to_owned()),
        base_url: std::env::var("TRIMWIRE_API_HARM_BASE_URL")
            .unwrap_or_else(|_| "https://api.z.ai/api/anthropic".to_owned()),
        full_url: std::env::var("TRIMWIRE_API_HARM_FULL_URL")
            .ok()
            .filter(|u| !u.trim().is_empty()),
        model: std::env::var("TRIMWIRE_API_HARM_MODEL")
            .unwrap_or_else(|_| "GLM-4.5-Air".to_owned()),
        api_key_env: std::env::var("TRIMWIRE_API_HARM_KEY_ENV")
            .unwrap_or_else(|_| "ZAI_API_KEY".to_owned()),
        api_key_file: None,
        timeout_secs: 300,
    };
    if std::env::var(&provider.api_key_env)
        .map(|v| v.is_empty())
        .unwrap_or(true)
    {
        eprintln!(
            "SKIP: ${} is not set — export your API key to run the API harm gate.",
            provider.api_key_env
        );
        return 0;
    }

    println!(
        "── API harm gate ──\nprovider={} model={} style={}\nturns={}  slice={} chars (~{} KB)  facts={}",
        provider.base_url,
        provider.model,
        provider.style,
        slice.n_turns,
        slice.slice_text.len(),
        slice.slice_text.len() / 1024,
        probe::PROBE_FACTS.len(),
    );

    let summary =
        match summarizer::api::call_api(&provider, summarizer::build_prompt(&slice.slice_text))
            .await
        {
            Ok(s) => s,
            Err(e) => {
                eprintln!("SKIP: API call failed ({e}).");
                return 0;
            }
        };
    println!("\n── summary ({} chars) ──\n{summary}\n", summary.len());

    let false_done = summarizer::harm_check::detect_false_done(&summary, &slice.slice_text);
    if false_done.is_empty() {
        println!("── false-done check ── none");
    } else {
        println!("── false-done check ── {} FLAG(S):", false_done.len());
        for f in &false_done {
            println!("  ⚠ {}\n      ↳ {}", f.claim, f.reason);
        }
    }

    let report = probe::ProbeReport::score(&summary, slice.n_turns);
    report.print();
    let retention = report.retention();
    println!(
        "retention: {}/{} = {:.1}%  (threshold {:.0}%)",
        report.kept(),
        report.total(),
        retention * 100.0,
        THRESHOLD * 100.0,
    );

    if retention + 1e-9 >= THRESHOLD {
        println!("PASS");
        0
    } else {
        eprintln!("FAIL: retention below threshold (note: check the start bucket for early-drop)");
        1
    }
}
