//! After ALL current default strategies run, WHERE do the remaining bytes live?
//! This picks the highest-value next model-free lever by measuring the residual
//! mass per block category on real sessions — denoise (tool_result noise),
//! idle-path demand-paging (kept Read results), token-aware cutoff (recent-window
//! mass), or text_bloat_cap (old assistant prose). Whatever dominates the residual
//! is where a new lever can still win.
//!
//! Run: `cargo run --release --example residual_profile [dir]`
//! (default dir: /tmp/trimwire_real_sessions/bodies).

use serde_json::Value;
use trimwire::config::profile_baseline;

fn len_of(v: &Value) -> usize {
    v.as_str()
        .map(str::len)
        .unwrap_or_else(|| v.to_string().len())
}

/// Is this content a trimwire/CC elision marker (already pruned — don't count it)?
fn is_marker(v: &Value) -> bool {
    v.as_str()
        .is_some_and(|s| s.starts_with("[trimwire:") || s == "[Old tool result content cleared]")
}

fn main() {
    let dir = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "/tmp/trimwire_real_sessions/bodies".to_owned());
    let mut files: Vec<_> = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("read dir {dir}: {e}"))
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
        .collect();
    files.sort();
    let cfg = profile_baseline("default");

    println!(
        "{:<14} {:>9} {:>11} {:>12} {:>12} {:>10} {:>9}",
        "session", "resid_KB", "asst_text", "toolres_kept", "tooluse_in", "thinking", "user_text"
    );
    // Grand totals across the corpus (bytes).
    let (mut g_text, mut g_tres, mut g_tin, mut g_think, mut g_utext, mut g_total) =
        (0usize, 0usize, 0usize, 0usize, 0usize, 0usize);

    for p in &files {
        let Ok(bytes) = std::fs::read(p) else {
            continue;
        };
        let Ok(body) = serde_json::from_slice::<Value>(&bytes) else {
            continue;
        };
        let Some(msgs0) = body["messages"].as_array() else {
            continue;
        };
        let mut msgs = msgs0.clone();
        if trimwire::strategies::run(&mut msgs, &cfg).is_err() {
            continue;
        }
        let (mut text, mut tres, mut tin, mut think, mut utext) = (0, 0, 0, 0, 0);
        for m in &msgs {
            let role = m.get("role").and_then(Value::as_str).unwrap_or("");
            match m.get("content") {
                Some(Value::String(s)) if role == "user" => utext += s.len(),
                Some(Value::Array(blocks)) => {
                    for b in blocks {
                        let ty = b.get("type").and_then(Value::as_str).unwrap_or("");
                        match ty {
                            "text" => {
                                let n = b.get("text").map(len_of).unwrap_or(0);
                                if role == "user" {
                                    utext += n;
                                } else {
                                    text += n;
                                }
                            }
                            "tool_result" => {
                                if let Some(c) = b.get("content") {
                                    if !is_marker(c) {
                                        tres += len_of(c);
                                    }
                                }
                            }
                            "tool_use" => {
                                if let Some(i) = b.get("input") {
                                    if !is_marker(i) {
                                        tin += len_of(i);
                                    }
                                }
                            }
                            "thinking" | "redacted_thinking" => {
                                think += b
                                    .get("thinking")
                                    .or_else(|| b.get("data"))
                                    .map(len_of)
                                    .unwrap_or(0);
                            }
                            _ => {}
                        }
                    }
                }
                _ => {}
            }
        }
        let resid = serde_json::to_vec(&msgs).map(|v| v.len()).unwrap_or(0);
        if resid < 4096 {
            continue;
        } // skip trivial sessions
        let name = p.file_stem().and_then(|s| s.to_str()).unwrap_or("?");
        println!(
            "{:<14} {:>9} {:>11} {:>12} {:>12} {:>10} {:>9}",
            &name[..name.len().min(12)],
            resid / 1024,
            text,
            tres,
            tin,
            think,
            utext,
        );
        g_text += text;
        g_tres += tres;
        g_tin += tin;
        g_think += think;
        g_utext += utext;
        g_total += text + tres + tin + think + utext;
    }

    println!("\n── corpus residual mass by category (of categorized bytes) ──");
    let pct = |x: usize| {
        if g_total == 0 {
            0.0
        } else {
            100.0 * x as f64 / g_total as f64
        }
    };
    println!(
        "  assistant_text (prose)  : {:>10} B  {:>5.1}%   → I5 text_bloat_cap",
        g_text,
        pct(g_text)
    );
    println!(
        "  tool_result kept        : {:>10} B  {:>5.1}%   → denoise / idle-paging",
        g_tres,
        pct(g_tres)
    );
    println!(
        "  tool_use input kept     : {:>10} B  {:>5.1}%   → stale_input_cap window",
        g_tin,
        pct(g_tin)
    );
    println!(
        "  thinking kept (recent)  : {:>10} B  {:>5.1}%   → thinking_strip window",
        g_think,
        pct(g_think)
    );
    println!(
        "  user_text               : {:>10} B  {:>5.1}%   → (mostly untouchable)",
        g_utext,
        pct(g_utext)
    );
}
