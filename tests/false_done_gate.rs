//! Harm gate — false-completion detection on realistic summaries.
//!
//! The planted-fact harm gate (`tests/harm.rs`) catches DROPPED load-bearing
//! facts. This gate catches the opposite, more dangerous failure: an INJECTED
//! false completion — a summary that claims "tests passed" / "committed" when the
//! source slice shows no such thing (the b_s2-style false-done this project has
//! been catching by hand). `detect_false_done` is deterministic (no model), so
//! this is a real CI assertion, not a flaky measurement.
//!
//! False-completions are a failure mode of the local-model SUMMARY path (the
//! only thing that produces these prose claims), so this gate lives with it.
//! Runs under `cargo test`.
//!
//! Where `tests/harm.rs` plants needles in a `messages[]` array, this exercises
//! `detect_false_done` on full GOAL/FILES/DECIDED/ERRORS/FACTS/NEXT summaries vs
//! the serialized slice they were (notionally) produced from — the realistic
//! shape, not the minimal strings the in-module unit tests use.

use trimwire::summarizer::harm_check::detect_false_done;

/// A realistic resume-summary in the shipped facts-first section format.
fn summary(
    goal: &str,
    files: &str,
    decided: &str,
    errors: &str,
    facts: &str,
    next: &str,
) -> String {
    format!(
        "GOAL: {goal}\nFILES:\n{files}\nDECIDED:\n{decided}\nERRORS:\n{errors}\nFACTS:\n{facts}\nNEXT:\n{next}"
    )
}

/// A serialized slice the way `serialize_slice` shapes it: role headers + tagged
/// tool_use / tool_result blocks. This is the evidence `detect_false_done` scans.
fn slice(turns: &[&str]) -> String {
    turns.join("\n")
}

// --- TRUE false-dones: the summary claims completion the slice can't support ---

#[test]
fn flags_tests_passed_with_no_test_run_in_slice() {
    // The slice only LOCATED the gate (Grep) — no test was ever run. The summary
    // nonetheless asserts a green suite mid-FACTS. (The b_s2 shape.)
    let s = summary(
        "re-invert the lore gate so ids unlock",
        "src/lib/achievements.ts",
        "keep the loreMatch check but flip the condition",
        "(none)",
        "all 37 tests passed; lint clean; achievements.ts:88 holds the gate",
        "flip the loreMatch condition and re-run the suite",
    );
    let sl = slice(&[
        "### assistant",
        "[tool_use Grep] {\"pattern\":\"loreMatch\"}",
        "### user",
        "[tool_result] achievements.ts:88: const ok = loreMatch(ids)",
    ]);
    let flags = detect_false_done(&s, &sl);
    assert!(
        flags.iter().any(|f| f.reason.contains("tests")),
        "must flag 'all 37 tests passed' when the slice has no test-run result; got {flags:?}"
    );
}

#[test]
fn flags_committed_with_no_commit_evidence_in_slice() {
    let s = summary(
        "wire phase-9b lore",
        "src/lib/lore.ts",
        "committed Task 4.2 after lint+test",
        "(none)",
        "lore.ts updated",
        "open the PR",
    );
    let sl = slice(&[
        "### assistant",
        "[tool_use Edit] {\"file_path\":\"src/lib/lore.ts\"}",
        "### user",
        "[tool_result] updated successfully",
    ]);
    let flags = detect_false_done(&s, &sl);
    assert!(
        flags.iter().any(|f| f.reason.contains("commit")),
        "must flag 'committed' when the slice shows only an Edit, no commit; got {flags:?}"
    );
}

#[test]
fn flags_false_done_buried_in_a_long_realistic_summary() {
    // Adversarial: the false claim sits in the middle of an otherwise-faithful,
    // long summary — the detector must still catch it (line-scan, not just head).
    let s = summary(
        "harden the migration runner",
        "drizzle/meta/_journal.json\nsrc/db/migrate.ts\nsrc/db/schema.ts",
        "use idempotent RLS blocks; keep journal idx at 3",
        "SyntaxError: Unexpected token '#' in _journal.json (earlier, now fixed)",
        "bun v4.1.7; the suite is green and all tests passed after the fix; journal idx 3",
        "regenerate snapshots for 0002/0003 then verify",
    );
    let sl = slice(&[
        "### assistant",
        "[tool_use Read] {\"path\":\"drizzle/meta/_journal.json\"}",
        "### user",
        "[tool_result] { \"version\": \"7\", \"entries\": [ ... ] }",
        "### assistant",
        "[tool_use Edit] {\"file_path\":\"src/db/migrate.ts\"}",
        "### user",
        "[tool_result] updated successfully",
    ]);
    let flags = detect_false_done(&s, &sl);
    assert!(
        !flags.is_empty(),
        "must flag the buried 'all tests passed' claim; the slice ran no tests; got {flags:?}"
    );
}

// --- TRUE-dones: real evidence in the slice must SUPPRESS the flag (no noise) ---

#[test]
fn does_not_flag_tests_passed_when_the_slice_has_a_real_test_run() {
    let s = summary(
        "finish the reconcile fix",
        "src/ledger.rs",
        "switched to checked_div",
        "(none)",
        "tests passed (37 passed; 0 failed)",
        "open the PR",
    );
    let sl = slice(&[
        "### assistant",
        "[tool_use Bash] {\"command\":\"cargo test\"}",
        "### user",
        "[tool_result] test result: ok. 37 passed; 0 failed; finished in 1.2s",
    ]);
    assert!(
        detect_false_done(&s, &sl).is_empty(),
        "a real test-run result in the slice must suppress the 'tests passed' flag"
    );
}

#[test]
fn does_not_flag_committed_when_the_slice_shows_a_commit() {
    let s = summary(
        "land the lore wiring",
        "src/lib/lore.ts",
        "committed the lore wiring",
        "(none)",
        "3 files changed",
        "push and open the PR",
    );
    let sl = slice(&[
        "### user",
        "[tool_result] [main 1a2b3c4] feat(phase-9b): lore wiring\n 3 files changed, 40 insertions(+)",
    ]);
    assert!(
        detect_false_done(&s, &sl).is_empty(),
        "a real commit echo in the slice must suppress the 'committed' flag"
    );
}

#[test]
fn does_not_flag_an_open_next_with_no_completion_claim() {
    // The canonical SAFE summary: states what's left, claims nothing done.
    let s = summary(
        "wire the lore gate",
        "src/lib/achievements.ts",
        "(none)",
        "(none)",
        "achievements.ts:88 holds the loreMatch gate",
        "locate and re-invert the loreMatch gate; then run the suite",
    );
    let sl = slice(&["### assistant", "Now I'll find the gate."]);
    assert!(
        detect_false_done(&s, &sl).is_empty(),
        "an open NEXT with no completion claim must not be flagged"
    );
}
