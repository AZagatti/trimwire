//! Harm gate for the density-aware `select_slice` FALLBACK. For each real session
//! whose WIDEST slice is too tool-dominated to summarize (the 0.6 gate would skip
//! it today), this finds the density-aware rescued sub-window, summarizes it with
//! the configured local model (qwen3.5:4b by default), and checks the NEW risk the
//! fallback introduces — that a previously-skipped window now gets summarized:
//!   - summary_is_smaller (the size gate still holds for the rescued window),
//!   - pairing valid after splice (no orphans),
//!   - detect_false_done (no injected "tests passed"/"committed" with no evidence),
//!   - prints the summary + slice head for a blind gut-read of fidelity.
//!
//! Run: `cargo run --release --example density_harm`
//!      (reads /tmp/trimwire_real_sessions/bodies; TRIMWIRE_HARM_MODEL to A/B).
//! Exit 0 if every rescued window passes the automated checks (or SKIP if no model
//! / no rescuable windows); 1 if any rescued window fails size/pairing.

#[tokio::main]
async fn main() {
    std::process::exit(run().await);
}

// Score against the EXACT production function (not a drifting local copy).
use trimwire::summarizer::tool_result_fraction as tool_frac;

async fn run() -> i32 {
    use serde_json::Value;
    use trimwire::config::{Config, SummarizerLocalConfig};
    use trimwire::pairing::PairingIndex;
    use trimwire::summarizer::slice::{self, SummaryDecision, densify_start, select_slice};
    use trimwire::summarizer::{build_prompt, call_model, summary_is_smaller};

    const MAX_TOOL_FRACTION: f64 = 0.6; // skip gate (widest)
    const RESCUE_TOOL_FRACTION: f64 = 0.5; // density-aware rescue safety margin (prod)
    const KEEP: usize = 6; // local_model default keep_recent_turns

    // `#[non_exhaustive]` — build from Default and set fields (no struct literal).
    let mut lm = SummarizerLocalConfig::default();
    if let Ok(model) = std::env::var("TRIMWIRE_HARM_MODEL") {
        lm.model = model;
    }
    if let Ok(endpoint) = std::env::var("TRIMWIRE_HARM_ENDPOINT") {
        lm.endpoint = endpoint;
    }
    let timeout_secs: u64 = 180;
    let full_cfg = Config::default(); // for summary_is_smaller's model-free baseline

    let dir = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "/tmp/trimwire_real_sessions/bodies".to_owned());
    let mut files: Vec<_> = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("read dir {dir}: {e}"))
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
        .collect();
    files.sort();

    println!(
        "── density-aware select_slice harm gate ──\nmodel={}  keep={KEEP}\n",
        lm.model
    );

    let (mut rescued, mut passed, mut false_done_flags) = (0usize, 0usize, 0usize);
    for p in &files {
        let Ok(bytes) = std::fs::read(p) else {
            continue;
        };
        let Ok(body) = serde_json::from_slice::<Value>(&bytes) else {
            continue;
        };
        let msgs = body["messages"].as_array().cloned().unwrap_or_default();
        let Some((wstart, end)) = select_slice(&msgs, KEEP, msgs.len()) else {
            continue;
        };
        if tool_frac(&msgs[wstart..end]) <= MAX_TOOL_FRACTION {
            continue; // widest already summarizable — not a rescue case
        }
        let Some(s) = densify_start(&msgs, wstart, end, RESCUE_TOOL_FRACTION, tool_frac) else {
            continue; // nothing to rescue (would skip, as before)
        };
        rescued += 1;
        let name = p.file_stem().and_then(|x| x.to_str()).unwrap_or("?");
        let slice_text = slice::serialize_slice(
            &msgs[s..end],
            slice::REASONING_BLOCK_CAP,
            slice::TOOL_RESULT_BLOCK_CAP,
        );
        println!(
            "═══ {name}: rescued window [{s}..{end}] ({} msgs, widest tool_frac {:.0}% → dense {:.0}%, {} slice chars) ═══",
            end - s,
            tool_frac(&msgs[wstart..end]) * 100.0,
            tool_frac(&msgs[s..end]) * 100.0,
            slice_text.len(),
        );
        let summary = match call_model(&lm, timeout_secs, build_prompt(&slice_text)).await {
            Ok(s) => s,
            Err(e) => {
                eprintln!("SKIP: model unavailable ({e})");
                return 0;
            }
        };
        let Some(d) = SummaryDecision::new(&msgs, s, end, &summary) else {
            eprintln!("  ✗ SummaryDecision::new failed (empty/invalid)");
            continue;
        };
        let smaller = summary_is_smaller(&d.messages, &msgs, s, end, &full_cfg);
        let mut out = msgs.clone();
        let spliced = slice::apply_summary(&mut out, &msgs, &d);
        let pairing_ok = spliced && PairingIndex::build(&out).validate().is_ok();
        let fd = trimwire::summarizer::harm_check::detect_false_done(&summary, &slice_text);
        if !fd.is_empty() {
            false_done_flags += fd.len();
        }
        println!(
            "  size_smaller={smaller}  pairing_ok={pairing_ok}  false_done_flags={}",
            fd.len()
        );
        for f in &fd {
            println!("    ⚠ {}\n        ↳ {}", f.claim, f.reason);
        }
        println!("  ── summary ({} chars) ──", summary.len());
        for line in summary.lines().take(24) {
            println!("    {line}");
        }
        println!("  ── slice head (gut-read source) ──");
        for line in slice_text.lines().take(12) {
            println!("    {}", &line[..line.len().min(160)]);
        }
        println!();
        if smaller && pairing_ok {
            passed += 1;
        }
    }

    println!(
        "\n══ RESULT ══ rescued windows: {rescued}  passed(size+pairing): {passed}  false_done_flags: {false_done_flags}"
    );
    if rescued == 0 {
        println!("(no rescuable windows in this corpus — fallback is a no-op here)");
        return 0;
    }
    if passed == rescued {
        println!(
            "AUTOMATED PASS (all rescued windows shrink + keep pairing). Gut-read the summaries above for fidelity."
        );
        0
    } else {
        eprintln!(
            "FAIL: {} rescued window(s) failed size/pairing",
            rescued - passed
        );
        1
    }
}
