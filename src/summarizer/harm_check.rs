//! Deterministic false-done detector for the harm gate (EVAL ONLY — never runs on
//! the proxy / cache-stable replay path). The planted-fact retention gate is
//! one-directional: it catches DROPPED load-bearing facts but NOT INJECTED false
//! completions ("all tests passed" when no test ran; "committed" when nothing was
//! committed). A false-completion is the most dangerous local-model failure — a
//! resuming agent trusts a completion that never happened (the exact b_s2-style
//! false-done this project has been catching by hand all along).
//!
//! This scans a summary for completion CLAIMS and flags any the source slice shows
//! no evidence for. It is a high-precision heuristic (advisory): it flags ONLY when
//! the slice contains NONE of the relevant evidence signatures, so a false alarm is
//! unlikely (precision over recall — a noisy gate erodes trust). NO model call, no
//! network. Inspired by NabaOS "tool receipts" (arXiv 2603.10060), reduced to the
//! deterministic, runtime-free subset (we can't HMAC-sign tool calls from a proxy,
//! but we CAN check a claim against the slice's own tool-result text).

/// A summary completion-claim with no supporting evidence in the source slice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FalseDoneFlag {
    /// The summary line that made the unsupported claim.
    pub claim: String,
    /// Why it is suspected to be a false-done.
    pub reason: &'static str,
}

/// Scan `summary` for completion claims unsupported by `slice_text` (the serialized
/// excerpt the summary was produced from — its `[tool_result]` / `[tool_use …]`
/// text is the evidence). Returns one flag per unsupported claim. High-precision:
/// a claim is flagged only when the slice contains NONE of the relevant evidence
/// signatures.
pub fn detect_false_done(summary: &str, slice_text: &str) -> Vec<FalseDoneFlag> {
    let ev = slice_text.to_ascii_lowercase();
    let mut flags = Vec::new();
    for raw in summary.lines() {
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }
        let l = line.to_ascii_lowercase();

        // (1) "tests passed / suite green" — the canonical false-done.
        let claims_tests_pass = (l.contains("test") && (l.contains("pass") || l.contains("green")))
            || l.contains("suite passed");
        if claims_tests_pass {
            // A real test run leaves an unmistakable signature in the slice.
            // `passed`/`failed` are required to co-occur with `test`/`spec` so a
            // bare "the option was passed in" can't silently suppress the flag
            // (false-negative); the framework-specific signals stand alone.
            let runlike = ev.contains("test") || ev.contains("spec");
            let test_evidence = ev.contains("test result") // cargo/rust
                || (ev.contains("passed") && runlike)
                || (ev.contains("failed") && runlike) // a run that reported failures is still a run
                || ev.contains(" passing") // jest/mocha "N passing"
                || ev.contains("pytest")
                || ev.contains("vitest")
                || ev.contains("✓");
            if !test_evidence {
                flags.push(FalseDoneFlag {
                    claim: line.to_owned(),
                    reason: "claims tests passed, but the slice contains no test-run result",
                });
            }
        }

        // (2) "committed" — claims a commit that never appears in the slice.
        let claims_commit =
            l.contains("committed") || l.contains("git commit") || l.contains("commit hash");
        if claims_commit {
            let commit_evidence = ev.contains("commit")
                || ev.contains("files changed")
                || ev.contains("[main ")
                || ev.contains("[master ");
            if !commit_evidence {
                flags.push(FalseDoneFlag {
                    claim: line.to_owned(),
                    reason: "claims a commit, but the slice contains no commit evidence",
                });
            }
        }
    }
    flags
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flags_tests_passed_without_a_test_run_in_slice() {
        let summary = "GOAL: wire the lore gate\nFACTS: all 37 tests passed, lint clean\nNEXT: re-invert the gate";
        // The slice only LOCATED the gate — no test was ever run (the b_s2 false-done shape).
        let slice = "### assistant\n[tool_use Grep] {\"pattern\":\"loreMatch\"}\n### user\n[tool_result] achievements.ts:88: loreMatch gate";
        let flags = detect_false_done(summary, slice);
        assert_eq!(
            flags.len(),
            1,
            "must flag the unsupported 'tests passed' claim"
        );
        assert!(flags[0].reason.contains("tests passed"));
    }

    #[test]
    fn does_not_flag_tests_passed_when_slice_has_a_test_run() {
        let summary = "FACTS: tests passed (37 passed)";
        let slice = "### user\n[tool_result] test result: ok. 37 passed; 0 failed";
        assert!(
            detect_false_done(summary, slice).is_empty(),
            "a real test-run result in the slice must suppress the flag"
        );
    }

    #[test]
    fn does_not_flag_when_summary_makes_no_completion_claim() {
        let summary = "GOAL: wire the lore gate\nNEXT: locate and re-invert the loreMatch gate in achievements.ts";
        let slice = "### assistant\nNow I'll find the gate.";
        assert!(
            detect_false_done(summary, slice).is_empty(),
            "an open NEXT (no completion claim) must not be flagged"
        );
    }

    #[test]
    fn flags_committed_claim_without_commit_evidence() {
        let summary = "DECIDED: committed Task 4.2 (lint, test, commit)";
        let slice = "### assistant\n[tool_use Edit] {\"file_path\":\"lore.ts\"}\n### user\n[tool_result] updated successfully";
        let flags = detect_false_done(summary, slice);
        assert_eq!(
            flags.len(),
            1,
            "must flag the unsupported 'committed' claim"
        );
        assert!(flags[0].reason.contains("commit"));
    }

    #[test]
    fn does_not_flag_committed_when_slice_shows_a_commit() {
        let summary = "DECIDED: committed the lore wiring";
        let slice = "### user\n[tool_result] [main 1a2b3c4] feat(phase-9b): lore wiring\n 3 files changed, 40 insertions";
        assert!(
            detect_false_done(summary, slice).is_empty(),
            "a real commit echo in the slice must suppress the flag"
        );
    }
}
