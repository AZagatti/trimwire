//! Longitudinal degradation harness (internal notes §18 / T4).
//!
//! The question this answers: on a long ("infinite") session, does the OPT-IN
//! summarizer make the upstream model progressively "dumber" — losing
//! load-bearing facts — and is that loss the summarizer's fault?
//!
//! THE KEY FAITHFULNESS POINT (verified against the code, council-reconciled):
//! what the model actually sees at turn N is NOT just the summary splice. It is
//! the summary splice COMBINED WITH model-free pruning on the old turns the
//! summary does NOT cover. A naive harness that measures the splice in isolation
//! gives FALSE confidence: a fact in the un-summarized old tail can be erased by
//! `bloat_cap` / `stale_reads` while the splice-only view still shows it. So this
//! harness drives the REAL pipeline (`reprune::stable_apply_to_body`) and reads
//! what it emits — never a splice alone.
//!
//! What it proves, deterministically (no model call):
//!   1. FAITHFULNESS — a splice-only view shows a fact the full pipeline drops
//!      (model-free elision of the uncovered tail). This is why splice-only lies.
//!   2. FROZEN REPLAY (no telephone amplification) — a fact captured in an
//!      installed summary segment stays visible verbatim across every subsequent
//!      turn and across a re-checkpoint; the summarizer never re-compresses its
//!      own output.
//!   3. COLLAPSE — at `max_summary_segments` the chain REPLACEs into one segment.
//!      This re-reads the ORIGINAL bytes (NOT a summary-of-summaries / telephone
//!      game). Facts inside the new window + the recent window still survive; the
//!      old region that loses coverage reverts to deterministic model-free stubs
//!      (recoverable — files are always re-readable), never hallucination.
//!
//! The one thing this CANNOT test offline is the summarizing MODEL's own fidelity
//! (does the model drop a fact it was shown?). Because collapse re-reads originals,
//! that question is exactly "does a large slice summarize faithfully?" — which
//! `examples/api_harm.rs` measures (vary `TRIMWIRE_API_HARM_BYTES`). An opt-in
//! model arm here would be redundant with it.
//!
//! Run (no key, no network):
//!   cargo run --release --example longitudinal_harm
//! Exit 0 = all invariants hold; 1 = a degradation invariant was violated.

use serde_json::{Value, json};
use trimwire::config::{Config, profile_baseline};
use trimwire::pairing::PairingIndex;
use trimwire::reprune::{PruneState, stable_apply_to_body};
use trimwire::strategies::BodyOutcome;
use trimwire::summarizer::{normalize_fact, slice};

const THRESHOLD: usize = 8; // reprune stable-vs-recheckpoint message-growth budget

fn main() {
    std::process::exit(run());
}

fn run() -> i32 {
    println!("── longitudinal degradation harness (offline, deterministic) ──\n");
    let mut failures = 0;
    failures += phase1_faithfulness();
    failures += phase2_frozen_replay();
    failures += phase3_collapse();

    println!();
    if failures == 0 {
        println!("PASS — all degradation invariants hold");
        0
    } else {
        eprintln!("FAIL — {failures} invariant(s) violated");
        1
    }
}

// ── helpers ───────────────────────────────────────────────────────────────

/// Wrap a `messages[]` array in a minimal request body.
fn body_of(messages: &[Value]) -> Vec<u8> {
    serde_json::to_vec(&json!({
        "model": "claude",
        "system": [{"type": "text", "text": "sys"}],
        "messages": messages,
    }))
    .unwrap()
}

/// Parse `messages[]` back out of a request body.
fn messages_of(body: &[u8]) -> Vec<Value> {
    let v: Value = serde_json::from_slice(body).unwrap();
    v["messages"].as_array().cloned().unwrap_or_default()
}

/// What the upstream model actually sees: drive the REAL pipeline and return its
/// emitted `messages[]` (the whole point — never a splice in isolation).
fn model_sees(body: &[u8], cfg: &Config, state: &mut PruneState) -> Vec<Value> {
    match stable_apply_to_body(body, cfg, state, THRESHOLD) {
        BodyOutcome::Mutated { bytes, .. } => messages_of(&bytes),
        BodyOutcome::Unchanged | BodyOutcome::RolledBack => messages_of(body),
    }
}

/// Is `needle` present anywhere in the serialized array (normalized match — the
/// same comparison the harm gates use)?
fn has_fact(messages: &[Value], needle: &str) -> bool {
    let hay = normalize_fact(&serde_json::to_string(messages).unwrap());
    hay.contains(&normalize_fact(needle))
}

/// A growing tool session: a leading user turn (required by `select_slice`), then
/// `pairs` `[assistant(tool_use), user(tool_result)]` turns. `big_at` turns get an
/// oversized result with `needle` planted in its MIDDLE (so `bloat_cap`'s head+tail
/// trim erases it once the turn ages); other listed `(turn, needle)` plant the
/// needle at the HEAD (survives trimming / serialization).
fn session(
    pairs: usize,
    head_facts: &[(usize, &str)],
    big_mid_facts: &[(usize, &str)],
) -> Vec<Value> {
    let mut m =
        vec![json!({"role": "user", "content": [{"type": "text", "text": "start the work"}]})];
    for t in 0..pairs {
        let id = format!("t{t}");
        m.push(json!({"role": "assistant", "content": [
            {"type": "tool_use", "id": id, "name": "Bash", "input": {"command": format!("step {t}")}}
        ]}));
        let content = if let Some((_, needle)) = big_mid_facts.iter().find(|(p, _)| *p == t) {
            // Fact buried in the MIDDLE of a >threshold result → bloat_cap trims it.
            let pad = "x".repeat(2_500);
            format!("{pad}\nLOAD_BEARING: {needle} must be remembered\n{pad}")
        } else if let Some((_, needle)) = head_facts.iter().find(|(p, _)| *p == t) {
            format!("RESULT: {needle} — load-bearing fact\nstep {t} ok")
        } else {
            format!("step {t} completed ok")
        };
        m.push(json!({"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": id, "content": content}
        ]}));
    }
    m
}

/// Config with the summarizer "on" (so the replay honors an installed chain) and
/// deterministic, tight model-free knobs so the offline behavior is reproducible.
fn harness_cfg(max_segments: usize) -> Config {
    let mut cfg = profile_baseline("default");
    cfg.reprune.enabled = true;
    // Engine is irrelevant to the sync REPLAY (we inject summaries directly) but set
    // it non-"model-free" to mirror a real summarizer-enabled deployment.
    cfg.summarizer.engine = "local".to_owned();
    cfg.summarizer.accumulator = true;
    cfg.summarizer.keep_recent_turns = 2;
    cfg.summarizer.max_summary_segments = max_segments;
    // Deterministic bloat_cap: trim string results > ~2 KB older than 2 assistant
    // turns, keeping only a small head+tail — so a mid-result fact is erased.
    cfg.strategies.bloat_cap.enabled = true;
    cfg.strategies.bloat_cap.threshold_bytes = 2_000;
    cfg.strategies.bloat_cap.head_bytes = 200;
    cfg.strategies.bloat_cap.tail_bytes = 200;
    cfg.strategies.bloat_cap.keep_recent_turns = 2;
    cfg
}

fn check(label: &str, pass: bool) -> usize {
    println!("  [{}] {label}", if pass { "✓" } else { "✗" });
    usize::from(!pass)
}

// ── Phase 1: faithfulness (splice-only would lie) ───────────────────────────

fn phase1_faithfulness() -> usize {
    println!("Phase 1 — faithfulness: splice-only shows a fact the real pipeline drops");
    let mut fails = 0;

    // turn 2 → a COVERED fact (will be inside the summary window).
    // turn 8 → a TAIL fact buried in a big old result OUTSIDE the window.
    let covered = "COVERED_session_7421";
    let tail = "TAIL_ECONNREFUSED";
    let msgs = session(20, &[(2, covered)], &[(8, tail)]);
    let cfg = harness_cfg(8);
    let mut state = PruneState::default();

    // Cold checkpoint: model-free trims the big old turn-8 result and records it.
    let _ = model_sees(&body_of(&msgs), &cfg, &mut state);

    // Install a faithful mock summary over an EARLY window [1, 11) that covers the
    // turn-2 fact but NOT the turn-8 tail fact. The mock text preserves the covered
    // needle (a perfect summarizer) so any covered-fact loss would be a replay bug.
    let summary_text = format!("## Earlier work\nUsed {covered}; ran setup steps.");
    let d = slice::SummaryDecision::new(&msgs, 1, 11, &summary_text).expect("valid early window");
    state.set_summary(d.clone());

    // A stable turn: the model sees splice + replayed model-free decisions.
    let grown = session(21, &[(2, covered)], &[(8, tail)]);
    let seen = model_sees(&body_of(&grown), &cfg, &mut state);

    // Splice-only view: apply the summary but NOT model-free pruning.
    let mut splice_only = grown.clone();
    let _ = slice::apply_summaries(&mut splice_only, &grown, std::slice::from_ref(&d));

    fails += check(
        "covered fact survives (preserved in the frozen summary text)",
        has_fact(&seen, covered),
    );
    fails += check(
        "splice-only view STILL CONTAINS the tail fact (the false-confidence trap)",
        has_fact(&splice_only, tail),
    );
    fails += check(
        "real pipeline DROPS the tail fact (model-free trimmed the uncovered old result)",
        !has_fact(&seen, tail),
    );
    fails += check(
        "pairing valid",
        PairingIndex::build(&seen).validate().is_ok(),
    );
    println!(
        "    → splice-only would have reported {}% retention; the faithful pipeline reports the real loss.\n",
        if has_fact(&splice_only, tail) { 100 } else { 0 }
    );
    fails
}

// ── Phase 2: frozen replay (no telephone amplification) ─────────────────────

fn phase2_frozen_replay() -> usize {
    println!(
        "Phase 2 — frozen replay: summarized facts stay verbatim across turns + a re-checkpoint"
    );
    let mut fails = 0;

    // Three covered facts at turns 2, 6, 10; a recent fact at turn 28 (protected).
    let f0 = "SEG0_migrate_v9";
    let f1 = "SEG1_max_retries";
    let f2 = "SEG2_PruneState";
    let recent = "RECENT_port_8765";
    let cfg = harness_cfg(8);
    let mut state = PruneState::default();

    let base = session(30, &[(2, f0), (6, f1), (10, f2), (28, recent)], &[]);
    let _ = model_sees(&body_of(&base), &cfg, &mut state);

    // Append a contiguous 3-segment chain covering [1,7) [7,13) [13,19). Each mock
    // text preserves the fact in its span (a faithful summarizer).
    for (start, end, text) in [
        (1usize, 7usize, format!("## seg0\n{f0} applied")),
        (7, 13, format!("## seg1\nset {f1}")),
        (13, 19, format!("## seg2\ncached in {f2}")),
    ] {
        let d = slice::SummaryDecision::new(&base, start, end, &text).expect("valid segment");
        assert!(state.append_summary(d), "contiguous append must succeed");
    }
    fails += check(
        "chain has 3 frozen segments",
        state.summary_segment_count() == 3,
    );

    // Drive stable turns AND cross the re-checkpoint threshold; the facts must
    // persist verbatim the whole way (frozen replay — no re-compression).
    let mut all_present_every_turn = true;
    for n in [31usize, 33, 45, 60] {
        let grown = session(
            n,
            &[(2, f0), (6, f1), (10, f2), (28 + (n - 30), recent)],
            &[],
        );
        let seen = model_sees(&body_of(&grown), &cfg, &mut state);
        let ok = has_fact(&seen, f0)
            && has_fact(&seen, f1)
            && has_fact(&seen, f2)
            && has_fact(&seen, recent)
            && PairingIndex::build(&seen).validate().is_ok();
        if !ok {
            all_present_every_turn = false;
            eprintln!(
                "      turn {n}: f0={} f1={} f2={} recent={}",
                has_fact(&seen, f0),
                has_fact(&seen, f1),
                has_fact(&seen, f2),
                has_fact(&seen, recent),
            );
        }
    }
    fails += check(
        "all summarized facts + the recent fact survive every turn (incl. across re-checkpoint)",
        all_present_every_turn,
    );
    println!();
    fails
}

// ── Phase 3: collapse (REPLACE re-reads originals; bounded loss) ─────────────

fn phase3_collapse() -> usize {
    println!(
        "Phase 3 — collapse: chain REPLACEs into one segment (re-reads originals, not summaries)"
    );
    let mut fails = 0;

    let old0 = "OLD0_leapsecond"; // turn 2 — covered by an early segment, dropped on collapse
    let old1 = "OLD1_reconcile"; // turn 6 — covered by an early segment, dropped on collapse
    let newin = "NEW_TRIMWIRE_AUDIT"; // turn 18 — inside the post-collapse window
    let recent = "RECENT_E0277"; // protected
    let cfg = harness_cfg(2); // tiny cap → collapse after 2 segments
    let mut state = PruneState::default();

    let base = session(26, &[(2, old0), (6, old1), (18, newin), (24, recent)], &[]);
    let _ = model_sees(&body_of(&base), &cfg, &mut state);

    // Fill the chain to the cap (2 segments over the OLD region).
    for (start, end, text) in [
        (1usize, 7usize, format!("## seg0\n{old0}")),
        (7, 13, format!("## seg1\n{old1}")),
    ] {
        let d = slice::SummaryDecision::new(&base, start, end, &text).expect("seg");
        assert!(state.append_summary(d));
    }
    let covered_before = 13 - 1; // messages spanned by the chain before collapse

    // COLLAPSE: the gateway, at the segment cap, REPLACEs with a fresh summary of a
    // newly-selected window over the ORIGINAL messages. Mirror that: select the
    // current window and install a single segment that re-reads originals (here the
    // post-collapse window starts later, covering `newin` but not the oldest facts).
    let (cstart, cend) = slice::select_slice(&base, cfg.summarizer.keep_recent_turns, base.len())
        .expect("a window exists");
    let cstart = slice::cap_slice_start(
        &base,
        cstart,
        cend,
        slice::REASONING_BLOCK_CAP,
        slice::TOOL_RESULT_BLOCK_CAP,
        // Force a budget that excludes the oldest turns (simulate a long session
        // where the collapse window cannot cover everything).
        2_000,
    );
    let collapse_text = format!("## collapsed summary\nrecent old work incl {newin}");
    let cd =
        slice::SummaryDecision::new(&base, cstart, cend, &collapse_text).expect("collapse window");
    state.set_summary(cd); // REPLACE = collapse
    fails += check(
        "chain collapsed to a single segment",
        state.summary_segment_count() == 1,
    );

    let seen = model_sees(&body_of(&base), &cfg, &mut state);

    // Hard invariants (P1/P2/P5): recent + newly-covered facts survive; pairing holds.
    fails += check(
        "recent (protected) fact survives the collapse",
        has_fact(&seen, recent),
    );
    fails += check(
        "fact inside the post-collapse window survives",
        has_fact(&seen, newin),
    );
    fails += check(
        "pairing valid after collapse",
        PairingIndex::build(&seen).validate().is_ok(),
    );

    // Diagnostic (P4): which old facts LOST summary coverage at the collapse. Their
    // message index is `2 + 2*turn` (the tool_result). A fact whose turn fell out of
    // the new `[cstart, cend)` window is no longer summary-covered → it reverts to
    // model-free pruning (here it stays verbatim because the content is small; on a
    // real session large/stale old content ages to deterministic model-free STUBS —
    // recoverable, files re-readable — never hallucination). This is the "ages out"
    // signal, not a failure.
    let in_window = |turn: usize| {
        let idx = 2 + 2 * turn;
        (cstart..cend).contains(&idx)
    };
    println!(
        "    → chain span before collapse: {covered_before} msgs (2 segments). \
         Post-collapse window [{cstart}, {cend}). old0(turn2) summary-covered: {}; \
         old1(turn6) summary-covered: {} — uncovered facts fall back to model-free.",
        in_window(2),
        in_window(6),
    );
    fails
}
