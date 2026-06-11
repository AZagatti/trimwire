//! Local-only profiler over REAL Claude Code sessions — answers two questions the
//! synthetic corpora can't:
//!   1. How often does `select_slice` have >1 eligible window? (i.e. is the
//!      density-aware `select_slice` idea ever a NO-OP on real traffic — the
//!      empirical gate from the code-options council.)
//!   2. What model-free reduction% do real sessions actually get (default vs gentle)?
//!
//! NOT a CI test: it reads real reconstructed bodies from a directory that is never
//! committed (the no-content invariant keeps real message bodies out of the repo).
//! Produce the bodies with `benchmark/reconstruct_session.py` run on COPIES of your
//! `~/.claude` transcripts (never the live originals), then:
//!
//!   cargo run --example session_profile -- [DIR]
//!
//! DIR defaults to `/tmp/trimwire_real_sessions/bodies`.

fn main() {
    use serde_json::Value;
    use trimwire::config::{Config, profile_baseline};
    use trimwire::summarizer::slice::eligible_window_count;

    fn reduction(msgs: &[Value], cfg: &Config) -> f64 {
        let before = serde_json::to_vec(msgs).map(|v| v.len()).unwrap_or(0);
        let mut m = msgs.to_vec();
        // On Err the array is partially mutated → the reduction would be wrong; bail.
        if trimwire::strategies::run(&mut m, cfg).is_err() {
            return f64::NAN;
        }
        let after = serde_json::to_vec(&m).map(|v| v.len()).unwrap_or(0);
        if before == 0 {
            0.0
        } else {
            100.0 * (before - after) as f64 / before as f64
        }
    }

    // Per-strategy elided bytes (nonzero only) — diagnoses WHICH strategies fire.
    fn per_strategy(msgs: &[Value], cfg: &Config) -> Vec<(&'static str, i64)> {
        let mut m = msgs.to_vec();
        trimwire::strategies::run(&mut m, cfg)
            .map(|v| {
                v.into_iter()
                    .map(|(n, s)| (n, s.elided_bytes()))
                    .filter(|(_, b)| *b != 0)
                    .collect()
            })
            .unwrap_or_default()
    }

    let dir = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "/tmp/trimwire_real_sessions/bodies".to_owned());
    let mut files: Vec<_> = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("read dir {dir}: {e}"))
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
        .collect();
    files.sort();

    let def = profile_baseline("default");
    let gentle = profile_baseline("gentle");
    // Honesty caveats (review): win@N use max_end=usize::MAX (the FULL reconstructed
    // history) — production passes the reprune checkpoint_len, so these are BEST-CASE
    // window counts, not per-request production counts. def%/gentle% are messages[]-only
    // (the ~12KB system+tools prefix is excluded → % runs higher than full-wire-body).
    // Absolute % may also be understated: CC's own microcompact can pre-clear large old
    // results before they reach the wire. Use these for SIGN/shape, not exact figures.
    println!(
        "(win@N: max_end=MAX best-case · %: messages[]-only · CC may pre-clear · sample = this dir)\n"
    );
    println!(
        "{:<14} {:>7} {:>9} {:>7} {:>7} {:>8} {:>8} {:>9}",
        "session", "msgs", "body_KB", "win@2", "win@4", "def%", "gentle%", "prune_ms"
    );
    let (mut sliceable, mut multi) = (0usize, 0usize);
    for p in &files {
        let Ok(bytes) = std::fs::read(p) else {
            continue;
        };
        let Ok(body) = serde_json::from_slice::<Value>(&bytes) else {
            continue;
        };
        let msgs = body["messages"].as_array().cloned().unwrap_or_default();
        let w2 = eligible_window_count(&msgs, 2, usize::MAX);
        let w4 = eligible_window_count(&msgs, 4, usize::MAX);
        if w2 >= 1 {
            sliceable += 1;
            if w2 > 1 {
                multi += 1;
            }
        }
        // The per-request CPU trimwire adds: parse messages[] + run strategies +
        // re-serialize. (reprune adds a per-segment SHA-256, ~0 here — measured separately.)
        let t0 = std::time::Instant::now();
        let parsed: Value = serde_json::from_slice(&bytes).unwrap();
        let mut pm = parsed["messages"].as_array().cloned().unwrap_or_default();
        let _ = trimwire::strategies::run(&mut pm, &def);
        let _ = serde_json::to_vec(&pm);
        let prune_ms = t0.elapsed().as_secs_f64() * 1000.0;
        let name = p.file_stem().unwrap().to_string_lossy();
        println!(
            "{:<14} {:>7} {:>9} {:>7} {:>7} {:>7.1} {:>7.1} {:>9.2}",
            &name[..name.len().min(13)],
            msgs.len(),
            bytes.len() / 1024,
            w2,
            w4,
            reduction(&msgs, &def),
            reduction(&msgs, &gentle),
            prune_ms,
        );
    }
    println!("\nsessions with a prunable slice (win@2 >= 1): {sliceable}");
    println!("  of those, with >1 eligible window (density-aware has a CHOICE): {multi}");
    if sliceable > 0 {
        let noop = 100.0 * (sliceable - multi) as f64 / sliceable as f64;
        println!(
            "  => density-aware select_slice would be a NO-OP on {noop:.0}% of sliceable real sessions"
        );
    }

    // Per-strategy firing (default vs gentle) on the non-trivial sessions — diagnoses
    // WHY gentle saves what it saves (is gentle ≈ 0% genuine or a bug?).
    println!("\n--- per-strategy elided bytes (sessions with >=12 msgs) ---");
    for p in &files {
        let Ok(bytes) = std::fs::read(p) else {
            continue;
        };
        let Ok(body) = serde_json::from_slice::<Value>(&bytes) else {
            continue;
        };
        let msgs = body["messages"].as_array().cloned().unwrap_or_default();
        if msgs.len() < 12 {
            continue;
        }
        let name = p.file_stem().unwrap().to_string_lossy();
        println!(
            "{:<14} default={:?}",
            &name[..name.len().min(13)],
            per_strategy(&msgs, &def)
        );
        println!("{:<14} gentle ={:?}", "", per_strategy(&msgs, &gentle));
    }
}
