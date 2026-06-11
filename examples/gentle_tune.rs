//! Local-only experiment: measure candidate GENTLE retunes on real sessions, so the
//! choice is empirical, not a guess. The gentle-tuning council unanimously proposed
//! (a) lower bloat_cap 32KB→8KB and (b) add thinking_strip@keep6 — but flagged the
//! 8KB threshold as least-confident. This prints reduction% per real session under:
//! current gentle · +thinking_strip · +bloat@8KB · +bloat@16KB · +both · default.
//!
//! NOT a CI test (reads real reconstructed bodies; nothing committed). Run:
//!   cargo run --example gentle_tune -- [DIR]
//! DIR defaults to /tmp/trimwire_real_sessions/bodies.

fn main() {
    use serde_json::Value;
    use trimwire::config::{Config, profile_baseline};

    fn red(msgs: &[Value], cfg: &Config) -> f64 {
        let before = serde_json::to_vec(msgs).map(|v| v.len()).unwrap_or(0);
        let mut m = msgs.to_vec();
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
    fn with_think(mut c: Config, keep: usize) -> Config {
        c.strategies.thinking_strip.enabled = true;
        c.strategies.thinking_strip.keep_recent_turns = keep;
        c
    }

    let gentle = profile_baseline("gentle");
    let g_t6 = with_think(profile_baseline("gentle"), 6);
    let g_t8 = with_think(profile_baseline("gentle"), 8);
    let g_t10 = with_think(profile_baseline("gentle"), 10);
    let def = profile_baseline("default");

    let dir = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "/tmp/trimwire_real_sessions/bodies".to_owned());
    let mut files: Vec<_> = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("read dir {dir}: {e}"))
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
        .collect();
    files.sort();

    println!("(gentle+thinking_strip @ keep_recent 6/8/10; reduction% on messages[]; >=12 msgs)\n");
    println!(
        "{:<14}{:>6}{:>9}{:>9}{:>9}{:>9}{:>9}",
        "session", "msgs", "gentle", "t@6", "t@8", "t@10", "default"
    );
    for p in &files {
        let Ok(bytes) = std::fs::read(p) else {
            continue;
        };
        let Ok(body) = serde_json::from_slice::<Value>(&bytes) else {
            continue;
        };
        let m = body["messages"].as_array().cloned().unwrap_or_default();
        if m.len() < 12 {
            continue;
        }
        let name = p.file_stem().unwrap().to_string_lossy();
        println!(
            "{:<14}{:>6}{:>8.1}{:>8.1}{:>8.1}{:>8.1}{:>8.1}",
            &name[..name.len().min(13)],
            m.len(),
            red(&m, &gentle),
            red(&m, &g_t6),
            red(&m, &g_t8),
            red(&m, &g_t10),
            red(&m, &def),
        );
    }
}
