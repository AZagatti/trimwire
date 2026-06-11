//! RUNWAY — how much longer a real session lasts, and how much token cost/quota it
//! saves, with trimwire's deterministic pruning vs without (§17 T1).
//!
//! Replays a reconstructed session body turn-by-turn (growing message prefixes) and,
//! for an assumed context window, reports WHERE each path crosses the "rot wall"
//! (a fill fraction of the window) — WITHOUT trimwire vs WITH — plus the final
//! reduction. The ratio of the two crossing points is the **runway multiplier**.
//!
//! HONEST SCOPE: this models the DEFAULT model-free path only (the opt-in summarizer
//! extends long sessions FURTHER — see `cost_replay` for the cache-weighted cost win).
//! Token counts are an estimate (`chars / chars_per_token`); the wire body also carries
//! a system prompt + tool schemas outside `messages[]` (a constant prefix), so absolute
//! turns are approximate — the *ratio* (runway multiplier) is the robust number. Mid-pair
//! prefixes that would orphan a tool pair are counted UNPRUNED (conservative: it can only
//! understate trimwire's win).
//!
//! Usage (reconstruct first, on a COPY — never the live transcript):
//!   python3 benchmark/reconstruct_session.py SESSION.jsonl /tmp/body.json
//!   cargo run --release --example runway -- /tmp/body.json [more.json ...]
//! Env: TRIMWIRE_RUNWAY_WINDOW (ctx tokens, default 200000), TRIMWIRE_RUNWAY_FILL
//!   (0..1 wall, default 0.8), TRIMWIRE_RUNWAY_CHARS_PER_TOKEN (default 3.5).

use serde_json::Value;
use trimwire::config::profile_baseline;
use trimwire::strategies;

fn env_f(k: &str, d: f64) -> f64 {
    std::env::var(k)
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(d)
}
fn json_tokens(m: &[Value], cpt: f64) -> f64 {
    serde_json::to_vec(&m).map(|v| v.len()).unwrap_or(0) as f64 / cpt
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        eprintln!(
            "usage: runway BODY.json [BODY2.json ...]\n  \
             (reconstruct a session first: python3 benchmark/reconstruct_session.py SESSION.jsonl BODY.json)"
        );
        std::process::exit(2);
    }
    let window = env_f("TRIMWIRE_RUNWAY_WINDOW", 200_000.0);
    let fill = env_f("TRIMWIRE_RUNWAY_FILL", 0.8);
    let cpt = env_f("TRIMWIRE_RUNWAY_CHARS_PER_TOKEN", 3.5);
    let wall = window * fill;
    let cfg = profile_baseline("default");

    println!(
        "# trimwire runway — window {:.0}K tok, rot-wall at {:.0}% = {:.0}K tok (~{cpt} chars/tok)",
        window / 1000.0,
        fill * 100.0,
        wall / 1000.0,
    );
    println!(
        "# default model-free path only; the opt-in summarizer extends long sessions further."
    );

    for path in &args {
        let Ok(raw) = std::fs::read_to_string(path) else {
            eprintln!("{path}: cannot read, skipping");
            continue;
        };
        let body: Value = serde_json::from_str(&raw).unwrap_or(Value::Null);
        let msgs = body["messages"].as_array().cloned().unwrap_or_default();
        let n = msgs.len();
        if n < 4 {
            eprintln!("{path}: too short ({n} messages), skipping");
            continue;
        }
        let step = (n / 80).max(1);
        let (mut base_wall, mut tw_wall): (Option<usize>, Option<usize>) = (None, None);
        let (mut last_base, mut last_tw) = (0.0, 0.0);

        let mut k = 2;
        while k <= n {
            let base_tok = json_tokens(&msgs[..k], cpt);
            let mut pruned = msgs[..k].to_vec();
            // An orphaned (mid-pair) prefix → run() errors; count it unpruned (conservative).
            let tw_tok = match strategies::run(&mut pruned, &cfg) {
                Ok(_) => json_tokens(&pruned, cpt),
                Err(_) => base_tok,
            };
            last_base = base_tok;
            last_tw = tw_tok;
            if base_wall.is_none() && base_tok > wall {
                base_wall = Some(k);
            }
            if tw_wall.is_none() && tw_tok > wall {
                tw_wall = Some(k);
            }
            k += step;
        }

        let red = if last_base > 0.0 {
            (last_base - last_tw) / last_base * 100.0
        } else {
            0.0
        };
        println!(
            "\n## {path} — {n} msgs; final {:.0}K → {:.0}K tok ({red:.0}% lighter)",
            last_base / 1000.0,
            last_tw / 1000.0,
        );
        match (base_wall, tw_wall) {
            (Some(b), Some(t)) => println!(
                "   rot-wall hit: WITHOUT trimwire at msg {b}; WITH at msg {t}  →  {:.1}× runway",
                t as f64 / b.max(1) as f64
            ),
            (Some(b), None) => println!(
                "   WITHOUT trimwire hits the wall at msg {b}; WITH trimwire it NEVER does in this \
                 session (≥ {:.1}× headroom)",
                n as f64 / b.max(1) as f64
            ),
            (None, _) => println!(
                "   never reaches the {:.0}% wall even unpruned (window too large or session too short)",
                fill * 100.0
            ),
        }
    }
}
