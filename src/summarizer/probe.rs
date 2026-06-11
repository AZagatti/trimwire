//! Slice-ceiling probe: plant distinctive verbatim facts across a synthetic OLD
//! conversation slice of a target size, then measure how many survive a real
//! summary — reporting retention BY POSITION (start / middle / end) so an
//! "early-drop" degradation is visible, not just an aggregate.
//!
//! This is the shared core behind both `examples/api_harm.rs` (the dev gate) and
//! `trimwire summarizer probe` (the installed-user command), so a user can check
//! whether THEIR chosen model holds up at THEIR slice budget before trusting it.
//!
//! It does not call a model itself — the caller runs the summary (local or API)
//! and passes the result to [`ProbeReport::score`]. Building the slice and scoring
//! the facts are pure and deterministic; only the model call is not.

use serde_json::{Value, json};

use crate::summarizer::{normalize_fact, slice};

/// Where a fact landed in the slice, for the per-position breakdown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Bucket {
    Start,
    Mid,
    End,
}

/// The curated probe facts: `(label, verbatim needle, the realistic line it sits
/// in)`. A faithful summary must preserve the needle TOKEN; paraphrasing the
/// surrounding line is fine.
///
/// Kept deliberately SMALL (~12 distinctive needles). This probes SLICE-SIZE
/// recall — whether the model still surfaces the load-bearing facts as the slice
/// grows. Cramming it to ~24 needles instead measures SUMMARY BUDGET (a terse
/// summary holds only ~12-18 distinct facts), which fails at every slice size and
/// tells you nothing about the ceiling. Vary the byte budget, NOT the fact count.
pub const PROBE_FACTS: &[(&str, &str, &str)] = &[
    (
        "auth file",
        "session_7421.rs",
        "Edited src/auth/session_7421.rs to fix the blocking call.",
    ),
    (
        "migration",
        "migrate_v9.sql",
        "Applied db/migrate_v9.sql to add the ledger table.",
    ),
    (
        "error code",
        "error[E0277]",
        "Hit error[E0277]: the trait bound `Job: Send` is not satisfied.",
    ),
    (
        "network errno",
        "ECONNREFUSED",
        "Probe failed with ECONNREFUSED against the upstream.",
    ),
    (
        "db decision",
        "SQLite",
        "DECIDED: use SQLite (rejected Postgres) for the local ledger.",
    ),
    (
        "retry const",
        "max_retries",
        "Set max_retries = 5 in the reconnect loop.",
    ),
    (
        "function",
        "reconcile_balances",
        "fn reconcile_balances() must stay synchronous.",
    ),
    (
        "env var",
        "TRIMWIRE_AUDIT",
        "Reads the TRIMWIRE_AUDIT env var to pick the audit path.",
    ),
    (
        "port",
        "8765",
        "The gateway listens on port 8765 by default.",
    ),
    (
        "test count",
        "37 tests",
        "Suite is 37 tests; 3 were failing before the fix.",
    ),
    (
        "todo",
        "leap-second",
        "TODO: handle the leap-second edge case in the scheduler.",
    ),
    (
        "symbol",
        "PruneState",
        "The summary is cached in PruneState and replayed by reprune.",
    ),
];

/// The default slice budget (bytes) for a probe run — matches the API-engine
/// `slice_char_budget` default so the probe exercises the real ceiling.
pub const DEFAULT_PROBE_BYTES: usize = 131_072;

/// A built probe slice: the serialized text to summarize plus the turn count it
/// was spread across (needed to bucket facts by position when scoring).
pub struct ProbeSlice {
    /// The serialized slice text (already run through [`slice::serialize_slice`]),
    /// ready to wrap in a summary prompt via `summarizer::build_prompt`.
    pub slice_text: String,
    /// Number of `[assistant, user]` turns the facts were spread across.
    pub n_turns: usize,
}

/// Build a synthetic OLD slice of roughly `target_bytes` serialized chars, with
/// the [`PROBE_FACTS`] planted at evenly-spread turn positions so some land in the
/// OLDEST turns (the early-drop risk this probe exists to catch). The load-bearing
/// line is placed FIRST in each planted tool_result (so it survives the head/tail
/// cap), followed by filler noise that should be summarized away.
pub fn build_probe_slice(target_bytes: usize) -> ProbeSlice {
    // Each serialized pair ≈ ~520 B after the per-block caps; keep at least a few
    // turns per fact so positions stay distinct.
    let approx_per_turn = 520usize;
    let n_turns = (target_bytes / approx_per_turn).max(PROBE_FACTS.len() * 3);
    let bulk = "    debug: irrelevant trace line that should be summarized away\n".repeat(6);

    let mut fact_at: std::collections::HashMap<usize, usize> = std::collections::HashMap::new();
    for (i, _) in PROBE_FACTS.iter().enumerate() {
        let pos = (i * (n_turns.saturating_sub(1))) / (PROBE_FACTS.len().saturating_sub(1).max(1));
        fact_at.insert(pos, i);
    }

    let mut slice_msgs: Vec<Value> = Vec::new();
    for t in 0..n_turns {
        let id = format!("t{t}");
        slice_msgs.push(json!({"role":"assistant","content":[
            {"type":"tool_use","id": id, "name":"Bash","input":{"command": format!("step {t}")}}
        ]}));
        let result = if let Some(&fi) = fact_at.get(&t) {
            format!("{}\n{bulk}", PROBE_FACTS[fi].2)
        } else {
            format!("step {t} completed ok\n{bulk}")
        };
        slice_msgs.push(json!({"role":"user","content":[
            {"type":"tool_result","tool_use_id": id, "content": result}
        ]}));
    }

    let slice_text = slice::serialize_slice(
        &slice_msgs,
        slice::REASONING_BLOCK_CAP,
        slice::TOOL_RESULT_BLOCK_CAP,
    );
    ProbeSlice {
        slice_text,
        n_turns,
    }
}

/// Aggregate the per-run retention fractions of a multi-run (`--runs N`) probe into
/// `(passes, p50, min)`. Model summaries are non-deterministic, so a single run is a
/// coin flip near the gate — this is what makes the distribution legible. `p50` is the
/// lower-median of the sorted retentions; `passes` counts runs at/above `threshold`.
pub fn summarize_runs(retentions: &[f64], threshold: f64) -> (usize, f64, f64) {
    if retentions.is_empty() {
        return (0, 0.0, 0.0);
    }
    let passes = retentions
        .iter()
        .filter(|&&r| r + 1e-9 >= threshold)
        .count();
    let mut sorted = retentions.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let p50 = sorted[sorted.len() / 2];
    let min = sorted[0];
    (passes, p50, min)
}

/// The bucket a fact at index `i` (of `n` total) lands in, given `n_turns`.
fn bucket_of(i: usize, n: usize, n_turns: usize) -> Bucket {
    let pos = (i * (n_turns.saturating_sub(1))) / (n.saturating_sub(1).max(1));
    let third = (n_turns / 3).max(1);
    if pos < third {
        Bucket::Start
    } else if pos < 2 * third {
        Bucket::Mid
    } else {
        Bucket::End
    }
}

/// The outcome of scoring a returned summary against the planted facts.
pub struct ProbeReport {
    /// `(present, label, needle, bucket)` per fact, in declaration order.
    pub facts: Vec<(bool, &'static str, &'static str, Bucket)>,
}

impl ProbeReport {
    /// Score `summary` against the planted facts for a slice built over `n_turns`
    /// (use [`ProbeSlice::n_turns`]). A fact is "kept" when its normalized needle
    /// appears anywhere in the normalized summary.
    pub fn score(summary: &str, n_turns: usize) -> Self {
        let hay = normalize_fact(summary);
        let n = PROBE_FACTS.len();
        let facts = PROBE_FACTS
            .iter()
            .enumerate()
            .map(|(i, (label, needle, _))| {
                let present = hay.contains(&normalize_fact(needle));
                (present, *label, *needle, bucket_of(i, n, n_turns))
            })
            .collect();
        Self { facts }
    }

    /// Total facts retained.
    pub fn kept(&self) -> usize {
        self.facts.iter().filter(|f| f.0).count()
    }

    /// Total facts probed.
    pub fn total(&self) -> usize {
        self.facts.len()
    }

    /// Overall retention fraction in `[0, 1]`.
    pub fn retention(&self) -> f64 {
        if self.facts.is_empty() {
            return 1.0;
        }
        self.kept() as f64 / self.total() as f64
    }

    /// `(kept, total)` for a position bucket.
    pub fn bucket(&self, b: Bucket) -> (usize, usize) {
        let in_b: Vec<_> = self.facts.iter().filter(|f| f.3 == b).collect();
        (in_b.iter().filter(|f| f.0).count(), in_b.len())
    }

    /// Render the per-fact table + by-position summary to stdout.
    pub fn print(&self) {
        println!("\n── fact retention (normalized) ──");
        for (present, label, needle, bucket) in &self.facts {
            let b = match bucket {
                Bucket::Start => "start",
                Bucket::Mid => "mid",
                Bucket::End => "end",
            };
            println!(
                "  [{}] ({b}) {label}: {needle}",
                if *present { "✓" } else { "·" }
            );
        }
        let pct = |(k, n): (usize, usize)| {
            if n == 0 {
                100.0
            } else {
                k as f64 / n as f64 * 100.0
            }
        };
        let (s, m, e) = (
            self.bucket(Bucket::Start),
            self.bucket(Bucket::Mid),
            self.bucket(Bucket::End),
        );
        println!(
            "\nby position: start {}/{} ({:.0}%)  mid {}/{} ({:.0}%)  end {}/{} ({:.0}%)",
            s.0,
            s.1,
            pct(s),
            m.0,
            m.1,
            pct(m),
            e.0,
            e.1,
            pct(e),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slice_grows_with_budget_and_plants_all_facts() {
        let small = build_probe_slice(20_000);
        let big = build_probe_slice(200_000);
        assert!(big.n_turns > small.n_turns, "bigger budget → more turns");
        // Every needle's line text must appear in the slice (planted, pre-summary).
        for (_, _, line) in PROBE_FACTS {
            assert!(
                big.slice_text.contains(line) || big.slice_text.contains(&normalize_fact(line)),
                "planted line must be present in the built slice: {line}"
            );
        }
    }

    #[test]
    fn perfect_summary_scores_full_retention_and_buckets_cover_all() {
        let s = build_probe_slice(60_000);
        // A "summary" that echoes every needle → 100% retention.
        let echoed = PROBE_FACTS
            .iter()
            .map(|(_, needle, _)| *needle)
            .collect::<Vec<_>>()
            .join(" ");
        let r = ProbeReport::score(&echoed, s.n_turns);
        assert_eq!(r.kept(), r.total());
        assert!((r.retention() - 1.0).abs() < 1e-9);
        // Buckets partition all facts.
        let (a, b, c) = (
            r.bucket(Bucket::Start).1,
            r.bucket(Bucket::Mid).1,
            r.bucket(Bucket::End).1,
        );
        assert_eq!(a + b + c, PROBE_FACTS.len());
    }

    #[test]
    fn summarize_runs_reports_passes_p50_min() {
        // 4/5 at/above 0.90; p50 = lower-median (index 2 of sorted), min = 0.75.
        let (passes, p50, min) = summarize_runs(&[0.917, 0.75, 1.0, 0.917, 0.917], 0.90);
        assert_eq!(passes, 4);
        assert!((min - 0.75).abs() < 1e-9);
        assert!((p50 - 0.917).abs() < 1e-9); // sorted: .75 .917 .917 .917 1.0 → idx2 = .917
        // All-pass + empty edge cases.
        assert_eq!(summarize_runs(&[1.0, 0.95], 0.90).0, 2);
        assert_eq!(summarize_runs(&[], 0.90), (0, 0.0, 0.0));
    }

    #[test]
    fn empty_summary_scores_zero_retention() {
        let s = build_probe_slice(60_000);
        let r = ProbeReport::score("", s.n_turns);
        assert_eq!(r.kept(), 0);
        assert!(r.retention() < 1e-9);
    }
}
