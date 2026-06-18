//! Regression guards for the benchmark harness (`benchmark/corpora.rs`), so the
//! published numbers can't silently rot and the safety invariants hold on more
//! than the four hand-shapes. Runs in CI via `cargo test`.
//!
//! The corpus generators + metrics are shared with `examples/bench.rs` through
//! this `#[path]` include — one source of truth.

#[path = "../benchmark/corpora.rs"]
#[allow(dead_code)]
mod corpora;

use serde_json::{Value, json};
use trimwire::strategies::{BodyOutcome, apply_to_body};

use corpora::{
    Pricing, b64_blob, bash_session, common_prefix_len, context_quality, corpora as all_corpora,
    default_install_config, est_tokens, leading_common_msgs, lines, measure, prune, session_cost,
    tuned_config,
};

// ---- invariants on every corpus, both configs --------------------------------

#[test]
fn every_corpus_preserves_invariants_under_both_configs() {
    for c in all_corpora() {
        for (label, cfg) in [
            ("default", default_install_config()),
            ("tuned", tuned_config()),
        ] {
            let r = measure(&c.body, &cfg);
            assert!(
                r.orphan_free,
                "{} [{label}]: pruning orphaned a pair",
                c.name
            );
            assert!(
                r.never_grew,
                "{} [{label}]: output grew ({} > {})",
                c.name, r.out_bytes, r.in_bytes
            );
            assert!(
                r.system_preserved,
                "{} [{label}]: system field was modified",
                c.name
            );
        }
    }
}

// ---- the corpora must keep exercising what they claim to -----------------------

/// If a refactor makes a corpus stop firing its strategy, the benchmark would
/// quietly report 0% and nobody would notice. Pin the intent.
#[test]
fn each_corpus_fires_its_intended_strategy() {
    let def = default_install_config();
    let stubbed = |name: &str, strat: &str| -> usize {
        let c = all_corpora();
        let c = c.iter().find(|c| c.name == name).expect("corpus");
        measure(&c.body, &def)
            .per_strategy_stubbed
            .iter()
            .find(|(n, _)| *n == strat)
            .map(|(_, s)| *s)
            .unwrap_or(0)
    };

    assert!(
        stubbed("repeated_grep", "cross_turn_dedup") >= 7,
        "dedup should drop the repeats"
    );
    assert!(
        stubbed("coding", "bloat_cap") >= 1,
        "coding should bloat_cap the old log"
    );
    assert!(
        stubbed("coding", "cross_turn_dedup") >= 1,
        "coding should dedup re-reads"
    );
    assert!(
        stubbed("unique_bash_spam", "bloat_cap") >= 1,
        "old bash logs should cap"
    );
    assert!(
        stubbed("mcp_non_playwright", "bloat_cap") >= 1,
        "old query tables should cap"
    );
    assert!(
        stubbed("browser_heavy", "image_strip") >= 1,
        "old screenshots should strip"
    );
    assert!(
        stubbed("at_the_boundary", "bloat_cap") >= 1,
        "aged logs should cap"
    );
    // The flagship exercises several at once. (image_strip is absent here on
    // purpose: sliding_window stubs the old playwright pairs first, so by the
    // time image_strip runs the recent screenshots are within its keep window.)
    for s in [
        "cross_turn_dedup",
        "failed_input_purge",
        "bloat_cap",
        "sliding_window",
    ] {
        assert!(
            stubbed("mixed_realistic", s) >= 1,
            "mixed_realistic should fire {s}"
        );
    }
    // Coverage corpora added so attribution isn't blind to the three default-on
    // strategies the original fixtures never exercised.
    assert!(
        stubbed("stale_input_heavy", "stale_input_cap") >= 1,
        "stale_input_heavy should reduce old successful bulky inputs"
    );
    assert!(
        stubbed("thinking_heavy", "thinking_strip") >= 1,
        "thinking_heavy should strip old thinking blocks"
    );
    // "Read coverage gap" fix: old large Read results are now bloat_capped (Read is
    // age-gated, not exempt-at-every-age). The existing corpora's reads are all under
    // bloat_cap's threshold, so this corpus is the integration-level proof of fix #1.
    assert!(
        stubbed("read_heavy", "bloat_cap") >= 1,
        "read_heavy should bloat_cap its old large Read results"
    );
}

#[test]
fn floor_corpora_are_genuine_no_ops() {
    let def = default_install_config();
    for name in ["pure_chat_floor", "exempt_heavy"] {
        let c = all_corpora();
        let c = c.iter().find(|c| c.name == name).unwrap();
        let r = measure(&c.body, &def);
        assert_eq!(r.saved(), 0, "{name} should prune nothing");
        // pure_chat (no tools) forwards verbatim; exempt_heavy fires no stub.
        assert!(r.out_bytes <= r.in_bytes);
    }
    // The no-tools floor must be a literal passthrough.
    let c = all_corpora();
    let chat = c.iter().find(|c| c.name == "pure_chat_floor").unwrap();
    let body = serde_json::to_vec(&chat.body).unwrap();
    assert!(matches!(apply_to_body(&body, &def), BodyOutcome::Unchanged));
}

// ---- determinism + monotonicity -----------------------------------------------

#[test]
fn savings_are_bit_for_bit_deterministic() {
    let def = default_install_config();
    for c in all_corpora() {
        let a = measure(&c.body, &def);
        let b = measure(&c.body, &def);
        assert_eq!(a.out_bytes, b.out_bytes, "{} not deterministic", c.name);
    }
}

/// The *derived* metrics — cost, leave-one-out attribution, and the stateful
/// reprune spike — must also be reproducible run-to-run. They're pure functions
/// of `(corpus, config)`, but the spike threads a mutable `PruneState` and the
/// attribution folds a `HashMap`, so a future change could sneak in an
/// iteration-order or state-carry dependency. Pin them.
#[test]
fn derived_metrics_are_deterministic() {
    let def = default_install_config();
    let pr = corpora::Pricing::default();
    for c in all_corpora() {
        let m = c.messages();
        assert_eq!(
            session_cost(m, &def, pr).dollars,
            session_cost(m, &def, pr).dollars,
            "{} cost not deterministic",
            c.name
        );
        assert_eq!(
            corpora::marginal_attribution(&c.body, &def),
            corpora::marginal_attribution(&c.body, &def),
            "{} attribution not deterministic",
            c.name
        );
        let a = corpora::spike_compare(m, &def, 8);
        let b = corpora::spike_compare(m, &def, 8);
        assert_eq!(
            a.map(|s| (s.stable_cost, s.stable_stability)),
            b.map(|s| (s.stable_cost, s.stable_stability)),
            "{} spike not deterministic",
            c.name
        );
    }
}

#[test]
fn gentle_never_saves_more_than_default() {
    // "default" is the aggressive profile; "gentle" is conservative — for every
    // corpus, default must produce output that is at most as large as gentle
    // (i.e. default saves at least as much). This is the two-profile ordering
    // guarantee: aggressive ≤ conservative in output bytes.
    let def = default_install_config();
    let gentle = tuned_config(); // tuned_config() == gentle profile
    for c in all_corpora() {
        let d = measure(&c.body, &def).out_bytes;
        let g = measure(&c.body, &gentle).out_bytes;
        assert!(
            d <= g,
            "{}: default ({d}) saved less than gentle ({g}) — aggressive must save at least as much",
            c.name
        );
    }
}

// ---- guard against silent drift of the published numbers ----------------------

#[test]
fn harness_assumes_the_shipped_default_thresholds() {
    // The "default" profile is now aggressive (mirrors old "high"). The corpora
    // exercise the boundary at these knobs — if config.rs changes them, the
    // corpora may silently stop exercising the right boundary.
    // Note: bloat_cap threshold is 4 KB (aggressive); results >4 KB get trimmed.
    let d = default_install_config();
    assert_eq!(d.strategies.bloat_cap.threshold_bytes, 4_096);
    assert_eq!(d.strategies.bloat_cap.keep_recent_turns, 2);
    assert_eq!(d.strategies.failed_input_purge.keep_recent_turns, 2);
    assert_eq!(d.strategies.image_strip.keep_recent_count, 1);
    assert_eq!(d.strategies.sliding_window.keep_recent_turns, 2);
}

#[test]
fn published_magnitudes_stay_in_band() {
    // Loose bands (not exact pins) catch a strategy silently losing half its
    // effect, without being brittle to small content tweaks.
    let def = default_install_config();
    let red = |name: &str| {
        let c = all_corpora();
        let c = c.iter().find(|c| c.name == name).unwrap();
        measure(&c.body, &def).reduction_pct()
    };
    assert!(
        (90.0..=99.9).contains(&red("mixed_realistic")),
        "mixed_realistic = {}",
        red("mixed_realistic")
    );
    assert!(
        (70.0..=88.0).contains(&red("coding")),
        "coding = {}",
        red("coding")
    );
    assert!(
        red("giant_paste") > 95.0,
        "giant_paste = {}",
        red("giant_paste")
    );
    assert!(
        red("browser_heavy") > 45.0,
        "browser_heavy = {}",
        red("browser_heavy")
    );
    assert_eq!(red("pure_chat_floor"), 0.0);
}

// ---- context quality (rot resistance) — the point of the tool ------------------

#[test]
fn pruning_never_worsens_focus_and_cuts_redundancy() {
    let def = default_install_config();
    // Pruning must never bury the recent ask deeper — focus only goes up (or
    // stays put on the clean floors). This is the rot-resistance guarantee.
    for c in all_corpora() {
        let raw = context_quality(c.messages()).focus;
        let pruned = context_quality(&prune(c.messages(), &def)).focus;
        assert!(
            pruned >= raw - 1e-9,
            "{}: pruning dropped focus {raw:.3} -> {pruned:.3}",
            c.name
        );
    }
    // On a repetitive session, pruning slashes redundant tool output.
    let rg = all_corpora()
        .into_iter()
        .find(|c| c.name == "repeated_grep")
        .unwrap();
    let raw = context_quality(rg.messages()).redundancy;
    let pruned = context_quality(&prune(rg.messages(), &def)).redundancy;
    assert!(
        pruned < raw * 0.5,
        "repeated_grep redundancy should plummet: {raw:.3} -> {pruned:.3}"
    );
}

// ---- stable-prefix re-pruning (production `reprune`, exercised offline) --------

#[test]
fn stable_reprune_lifts_cache_stability_without_breaking_anything() {
    let def = default_install_config();
    // On EVERY corpus, stable-prefix re-pruning must never *lower* turn-to-turn
    // cache stability vs stateless (the whole point is to raise it). Tolerance
    // 1e-4 pp: under the aggressive `default` profile, `giant_paste` (one huge
    // pasted blob) shows a measured 5.7e-6 pp dip from a checkpoint byte-diff —
    // genuinely negligible. 1e-4 absorbs that while staying ~5 orders of
    // magnitude below any real regression (which would neutralise tens of pp).
    for c in all_corpora() {
        if let Some(s) = corpora::spike_compare(c.messages(), &def, 8) {
            assert!(
                s.stable_stability >= s.stateless_stability - 1e-4,
                "{}: stable stability {:.6} < stateless {:.6}",
                c.name,
                s.stable_stability,
                s.stateless_stability
            );
        }
    }
    // On churn-heavy corpora reprune lifts stability a lot — guard against a
    // regression silently neutralising it. (Stability, not cost: see below.)
    for name in ["browser_heavy", "mixed_realistic", "unique_bash_spam"] {
        let c = all_corpora().into_iter().find(|c| c.name == name).unwrap();
        let s = corpora::spike_compare(c.messages(), &def, 8).unwrap();
        assert!(
            s.stable_stability > s.stateless_stability + 20.0,
            "{name}: expected a big stability lift, got {:.1} → {:.1}",
            s.stateless_stability,
            s.stable_stability
        );
    }
    // Cost is the SUBTLER claim and only holds on long repeated sessions.
    // Measured under the aggressive `default` profile (keep_recent_turns=2):
    // stable reprune lowers cost on unique_bash_spam (0.48→0.32) but NOT on
    // browser_heavy (0.34→0.38) or mixed_realistic (0.42→0.44) — there the
    // aggressively-stubbed stateless snapshot is smaller than the deferred
    // stable prefix on these shortish corpora, so reprune trades a little cost
    // for the big stability lift. So we assert the cost win only where it's
    // real; the README/benchmark already document cost as non-monotonic.
    {
        let name = "unique_bash_spam";
        let c = all_corpora().into_iter().find(|c| c.name == name).unwrap();
        let s = corpora::spike_compare(c.messages(), &def, 8).unwrap();
        assert!(
            s.stable_cost < s.stateless_cost,
            "{name}: stable should cost less"
        );
    }
}

// ---- the cost crossover claim --------------------------------------------------

#[test]
fn pruning_wins_on_cost_for_long_sessions() {
    let pr = Pricing::default();
    let off = trimwire::config::Config::default();
    let def = default_install_config();
    let long = bash_session(200);
    let m = long["messages"].as_array().unwrap();
    let base = session_cost(m, &off, pr).dollars;
    let pruned = session_cost(m, &def, pr).dollars;
    assert!(
        pruned < base,
        "long session should be a cost win: pruned ${pruned} vs base ${base}"
    );
}

// ---- helper unit tests --------------------------------------------------------

#[test]
fn helpers_behave() {
    assert_eq!(lines(100, "x").len(), 100);
    assert_eq!(lines(0, "x").len(), 0);
    assert_eq!(b64_blob(4096, "p").len(), 4096);
    assert!(
        b64_blob(8000, "p")
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '+' || c == '/')
    );
    assert_eq!(est_tokens(4000), 1000);
    assert_eq!(common_prefix_len(b"abcXYZ", b"abcQRS"), 3);
    assert_eq!(common_prefix_len(b"abc", b"abc"), 3);
    let a = vec![json!({"a":1}), json!({"b":2})];
    let b = vec![json!({"a":1}), json!({"b":9})];
    assert_eq!(leading_common_msgs(&a, &b), 1);
    assert_eq!(leading_common_msgs(&a, &a), 2);
}

// ---- safety fuzz: invariants hold on randomized bodies, not just 11 shapes ----

/// A tiny deterministic PRNG (no dev-dep) for reproducible fuzzing.
struct Lcg(u64);
impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        self.0 >> 16
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

/// Build a random-but-structured request body: alternating assistant tool_use /
/// user tool_result, with occasional text turns, mismatched pairs, big/small
/// payloads, screenshots, and errors — to exercise the pairing + strategy paths.
fn random_body(rng: &mut Lcg) -> Value {
    let turns = 1 + rng.below(12) as usize;
    let mut messages = Vec::new();
    for i in 0..turns {
        match rng.below(5) {
            0 => messages
                .push(json!({"role": "user", "content": lines(rng.below(2000) as usize, "u")})),
            1 => messages.push(json!({"role": "assistant", "content": [
                {"type": "text", "text": lines(rng.below(500) as usize, "a")}
            ]})),
            _ => {
                let id = format!("id_{i}_{}", rng.next());
                let name = [
                    "Bash",
                    "Read",
                    "Grep",
                    "mcp__playwright__browser_take_screenshot",
                ][rng.below(4) as usize];
                messages.push(json!({"role": "assistant", "content": [
                    {"type": "tool_use", "id": id, "name": name,
                     "input": {"x": lines(rng.below(3000) as usize, "in")}}
                ]}));
                // Usually pair the result; sometimes orphan it or skip it.
                let pair = rng.below(10);
                if pair < 8 {
                    let is_err = rng.below(4) == 0;
                    let content = if name.contains("screenshot") {
                        json!(b64_blob(4096 + rng.below(40000) as usize, "img"))
                    } else if rng.below(2) == 0 {
                        json!(lines(rng.below(40000) as usize, "res"))
                    } else {
                        json!("small")
                    };
                    let use_id = if pair == 7 {
                        "DANGLING".to_owned()
                    } else {
                        id_of(&messages)
                    };
                    let mut block =
                        json!({"type": "tool_result", "tool_use_id": use_id, "content": content});
                    if is_err {
                        block["is_error"] = Value::Bool(true);
                    }
                    messages.push(json!({"role": "user", "content": [block]}));
                }
            }
        }
    }
    json!({
        "model": "claude-sonnet-4-5",
        "system": [{"type": "text", "text": "sys", "cache_control": {"type": "ephemeral"}}],
        "messages": messages,
    })
}

/// The id of the last tool_use we pushed (to pair its result), or a placeholder.
fn id_of(messages: &[Value]) -> String {
    messages
        .last()
        .and_then(|m| m["content"].as_array())
        .and_then(|c| c.iter().find(|b| b["type"] == "tool_use"))
        .and_then(|b| b["id"].as_str())
        .unwrap_or("MISSING")
        .to_owned()
}

#[test]
fn fuzz_apply_to_body_never_breaks_invariants() {
    let cfgs = [default_install_config(), tuned_config()];
    let mut rng = Lcg(0x9E3779B97F4A7C15);
    for _ in 0..1500 {
        let body = random_body(&mut rng);
        let original = serde_json::to_vec(&body).expect("serialize");
        for cfg in &cfgs {
            match apply_to_body(&original, cfg) {
                BodyOutcome::Unchanged => {}
                BodyOutcome::Mutated { bytes, .. } => {
                    // Safety invariants: stays valid JSON, orphan-free, system intact.
                    // (Body size: strategies only shrink real content, but on a
                    // degenerate result *smaller* than the ~40-byte stub marker the
                    // replacement can nudge the body up a few bytes — documented on
                    // `Stats::elided_bytes`. We bound that, not forbid it.)
                    assert!(
                        bytes.len() <= original.len() + 4096,
                        "fuzz: mutated body grew implausibly ({} → {})",
                        original.len(),
                        bytes.len(),
                    );
                    let after: Value =
                        serde_json::from_slice(&bytes).expect("fuzz: invalid JSON out");
                    assert_eq!(
                        after.get("system"),
                        body.get("system"),
                        "fuzz: system mutated"
                    );
                    let msgs = after["messages"].as_array().expect("fuzz: messages[]");
                    trimwire::pairing::PairingIndex::build(msgs)
                        .validate()
                        .expect("fuzz: produced an orphan");
                }
            }
        }
    }
}

// ---- per-strategy byte-attribution regression (the "where are savings lost" gate) ----

/// Per-corpus reduction% + per-strategy *marginal byte* attribution for one config.
fn attribution_report(cfg: &trimwire::config::Config) -> Value {
    let mut report = serde_json::Map::new();
    for c in all_corpora() {
        let r = measure(&c.body, cfg);
        let attribution: Vec<Value> = corpora::marginal_attribution(&c.body, cfg)
            .into_iter()
            .filter(|(_, bytes)| *bytes != 0)
            .map(|(name, bytes)| json!({ "strategy": name, "bytes": bytes }))
            .collect();
        report.insert(
            c.name.to_string(),
            json!({
                "in_bytes": r.in_bytes,
                // round to 0.1% so float formatting never churns the snapshot
                "reduction_pct": (r.reduction_pct() * 10.0).round() / 10.0,
                "attribution": attribution,
            }),
        );
    }
    Value::Object(report)
}

/// Lock per-corpus reduction% + per-strategy marginal attribution under BOTH shipped
/// profiles as `insta` snapshots — any savings-profile change becomes a REVIEWED diff
/// instead of silent drift. Covers ALL nine strategies (stale_input_cap / stale_reads
/// / thinking_strip were missing from STRATEGY_NAMES, so default attribution was blind
/// to its three biggest contributors until this harness increment).
#[test]
fn per_strategy_attribution_snapshot_default() {
    insta::assert_json_snapshot!(
        "per_strategy_attribution_default",
        attribution_report(&default_install_config())
    );
}

#[test]
fn per_strategy_attribution_snapshot_gentle() {
    // tuned_config() == the gentle profile.
    insta::assert_json_snapshot!(
        "per_strategy_attribution_gentle",
        attribution_report(&tuned_config())
    );
}

/// Guard against STRATEGY_NAMES drifting out of sync with `strategies::run`: with
/// every strategy enabled, `run()` must return exactly the STRATEGY_NAMES set. If a
/// future strategy is added to `run()` but not STRATEGY_NAMES, `marginal_attribution`
/// would silently drop it again (the 5/9-coverage bug this increment fixed).
#[test]
fn strategy_names_match_run() {
    use std::collections::BTreeSet;
    let mut cfg = default_install_config();
    for name in corpora::STRATEGY_NAMES {
        corpora::set_enabled(&mut cfg, name, true);
    }
    let mut msgs = all_corpora()[0].messages().to_vec();
    let fired: BTreeSet<&str> = trimwire::strategies::run(&mut msgs, &cfg)
        .expect("run must not orphan")
        .into_iter()
        .map(|(n, _)| n)
        .collect();
    let names: BTreeSet<&str> = corpora::STRATEGY_NAMES.iter().copied().collect();
    assert_eq!(
        fired, names,
        "STRATEGY_NAMES is out of sync with strategies::run() — attribution would silently \
         drop a strategy; add the missing name to STRATEGY_NAMES/set_enabled/strategy_enabled"
    );
}

// ---- cache-stability regression gate ----------------------------------------

/// Lock per-corpus turn-to-turn prompt-cache stability under `default` as an `insta`
/// snapshot. `cache_stability` replays each corpus as a growing request sequence,
/// prunes each snapshot, and averages the share of the earlier pruned request that
/// survives as a byte-prefix of the next. NOTE: this is the STATELESS prune-each-
/// snapshot path (no reprune) — it gates regressions in turn-to-turn prefix stability
/// of the strategies themselves; it is NOT the production reprune path that yields the
/// live "92% cache-hit" figure (that's covered by the spike_compare / stable_reprune
/// tests). Any strategy change that erodes stateless prefix stability becomes a
/// REVIEWED diff instead of a silent regression. `null` = a corpus with fewer than
/// two turns (no stability defined).
#[test]
fn cache_stability_snapshot() {
    let def = default_install_config();
    let mut report = serde_json::Map::new();
    for c in all_corpora() {
        let stab = match corpora::cache_stability(c.messages(), &def) {
            // round to 0.1% so float formatting never churns the snapshot
            Some(v) => json!((v * 1000.0).round() / 1000.0),
            None => Value::Null,
        };
        report.insert(c.name.to_string(), stab);
    }
    insta::assert_json_snapshot!("cache_stability_default", Value::Object(report));
}
