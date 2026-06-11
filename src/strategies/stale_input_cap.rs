//! `StaleInputCap` strategy — shape-preserving reduction of OLD **successful**
//! tool call inputs.
//!
//! Why: ~25% of a code session's wire content is `tool_use` INPUT. Today only
//! *failed* inputs are reduced (`failed_input_purge`). Successful old calls
//! carry the same bulk — large file content, heredocs, `new_string` diffs —
//! that the model no longer needs once the tool ran successfully. The result
//! (`tool_result`) already records the outcome; the input bulk is dead weight
//! on every subsequent turn.
//!
//! **Shape-preserving** reduction (not a blanket `{}`): keep the small scalar
//! fields — `command`, `file_path`, `description` — so the model still knows
//! *what* was done, and replace only the genuine bulk — large string values >512B,
//! nested arrays/objects (heredocs, MCP arg payloads) — with a content-free size
//! marker. A call whose input is already small is left untouched (true no-op).
//!
//! **The file-AUTHORING tools (`Write`/`Edit`/`MultiEdit`) are EXEMPT by default**
//! (alongside `Task`), so this never elides a `new_string`/file body. This is a
//! correctness requirement, not just a nicety: eliding authored file content makes
//! the model rebuild on a body it can no longer see and reproduce the elision
//! MARKER as the file content (observed live — a written Go file became
//! `[trimwire: NB input elided]`, breaking the build). So in practice this only
//! elides bulk from non-authoring inputs (Bash stdin/heredocs, MCP args).
//!
//! Reuses `reduce_failed_input` / `KEEP_VALUE_MAX` / `elided_marker` from
//! `failed_input_purge` (same shape-preserving logic, different firing
//! condition: `is_error` NOT true vs. `is_error` true). Failed calls are left
//! entirely to `failed_input_purge`; this strategy never double-processes them.
//!
//! Input-only mutation: never drops a pair, never touches `tool_result`, so it
//! cannot orphan anything. Deterministic and idempotent (existing markers are
//! kept; a re-run is a byte-identical no-op). Reprune-compatible: reprune
//! already records/replays `tool_use.input` overwrites; this strategy only
//! ever overwrites `input`, never removes blocks.
//!
//! **ON in the `default` profile** (cache-safe, `keep_recent_turns = 2`); **off
//! in `gentle`**. Override or disable explicitly:
//!
//! ```toml
//! [strategies.stale_input_cap]
//! enabled = true
//! keep_recent_turns = 4
//! exempt_tools = ["Task", "Write", "Edit", "MultiEdit"]
//! ```

use serde_json::Value;

use crate::config::{StaleInputCapConfig, matches_any};
use crate::error::Result;
use crate::pairing::PairingIndex;
use crate::strategies::failed_input_purge::reduce_failed_input;
use crate::strategies::{AUTHORING_TOOLS, Stats, assistant_cutoff, block_mut, role};

/// Reduce the inputs of old, successful tool calls.
pub fn apply(messages: &mut [Value], cfg: &StaleInputCapConfig) -> Result<Stats> {
    super::with_stats(messages, |m| apply_counted(m, cfg))
}

/// Worker: returns the number of inputs reduced. Byte accounting is threaded by
/// the caller ([`super::run`]); see [`apply`].
pub(crate) fn apply_counted(messages: &mut [Value], cfg: &StaleInputCapConfig) -> Result<usize> {
    let idx = PairingIndex::build(messages);
    idx.validate()?;

    let Some(cutoff) = assistant_cutoff(messages, cfg.keep_recent_turns) else {
        return Ok(0);
    };

    // Read-only pass first: collect locations of old, successful, non-exempt
    // tool_use blocks. We avoid holding a borrow across the mutation.
    let mut to_reduce: Vec<(usize, usize)> = Vec::new();
    for (mi, msg) in messages.iter().enumerate() {
        if mi > cutoff || role(msg) != Some("assistant") {
            continue;
        }
        let Some(content) = msg.get("content").and_then(Value::as_array) else {
            continue;
        };
        for (ci, block) in content.iter().enumerate() {
            if block.get("type").and_then(Value::as_str) != Some("tool_use") {
                continue;
            }
            let Some(name) = block.get("name").and_then(Value::as_str) else {
                continue;
            };
            // Hard floor: never elide authored file content, even if the user's
            // exempt_tools omits these (§13A corruption guard). Then honor the
            // configurable exempt list for everything else.
            if AUTHORING_TOOLS.contains(&name) || matches_any(&cfg.exempt_tools, name) {
                continue;
            }
            let Some(id) = block.get("id").and_then(Value::as_str) else {
                continue;
            };
            // Only reduce if the paired result is NOT an error (success or no
            // error flag). Failed calls belong to `failed_input_purge`.
            let is_error = idx
                .results
                .get(id)
                .and_then(|&(rmi, rci)| messages[rmi].get("content")?.as_array()?.get(rci))
                .and_then(|res| res.get("is_error"))
                .and_then(Value::as_bool)
                .unwrap_or(false);
            if !is_error {
                to_reduce.push((mi, ci));
            }
        }
    }

    let mut stubbed = 0usize;
    for loc in to_reduce {
        if let Some(block) = block_mut(messages, loc) {
            // Shape-preserving: keep small scalar fields (the command/path that
            // says what was done), elide only the bulk. Count only when
            // something was actually elided, so a small input is a true no-op.
            if let Some(reduced) = block.get("input").and_then(reduce_failed_input) {
                block["input"] = reduced;
                stubbed += 1;
            }
        }
    }

    Ok(stubbed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn cfg(keep: usize, exempt: &[&str]) -> StaleInputCapConfig {
        StaleInputCapConfig {
            enabled: true,
            keep_recent_turns: keep,
            exempt_tools: exempt.iter().map(|s| (*s).to_owned()).collect(),
        }
    }

    /// Build a session of `turns` assistant turns. Each turn is a Bash call with a
    /// large `stdin` (bulk) and a small `command` (structural). All calls succeed
    /// (no is_error on their results). Bash is a NON-authoring tool, so its bulk is
    /// a legitimate `stale_input_cap` target — authored Write/Edit/MultiEdit content
    /// is hard-exempt and must never be elided (§13A).
    fn successful_session(turns: usize) -> Vec<Value> {
        let mut msgs = Vec::new();
        for i in 0..turns {
            let uid = format!("toolu_{i}");
            msgs.push(json!({"role": "user", "content": [{"type": "text", "text": "go"}]}));
            msgs.push(json!({"role": "assistant", "content": [
                {"type": "tool_use", "id": uid, "name": "Bash", "input": {
                    "command": format!("psql -f dump_{i}.sql"),
                    "stdin": "x".repeat(2000)
                }}
            ]}));
            msgs.push(json!({"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": uid, "content": format!("ran {i}")}
            ]}));
        }
        msgs
    }

    /// Build a session with failed calls (is_error=true) — these should be
    /// left alone and counted as 0 by stale_input_cap.
    fn failed_session(turns: usize) -> Vec<Value> {
        let mut msgs = Vec::new();
        for i in 0..turns {
            let uid = format!("toolu_{i}");
            msgs.push(json!({"role": "user", "content": [{"type": "text", "text": "go"}]}));
            msgs.push(json!({"role": "assistant", "content": [
                {"type": "tool_use", "id": uid, "name": "Bash", "input": {
                    "command": format!("cmd-{i}"),
                    "stdin": "x".repeat(2000)
                }}
            ]}));
            msgs.push(json!({"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": uid, "is_error": true,
                 "content": format!("error: cmd-{i} failed")}
            ]}));
        }
        msgs
    }

    #[test]
    fn elides_bulk_of_old_successful_call_keeps_structural_fields() {
        let mut msgs = successful_session(10);
        let stats = apply(&mut msgs, &cfg(4, &[])).unwrap();
        // 10 turns, keep 4 → 6 old turns reduced.
        assert!(
            stats.stubbed >= 5,
            "old successful inputs reduced, got {}",
            stats.stubbed
        );
        // Turn 0 (successful, old): the bulky stdin is elided, but the small
        // command is kept — the model still sees what was run.
        assert_eq!(
            msgs[1]["content"][0]["input"]["command"],
            json!("psql -f dump_0.sql"),
            "command preserved (structural field)"
        );
        assert!(
            msgs[1]["content"][0]["input"]["stdin"]
                .as_str()
                .unwrap()
                .starts_with("[trimwire:"),
            "bulky stdin elided to a marker"
        );
        // The tool_result is untouched.
        assert_eq!(msgs[2]["content"][0]["content"], json!("ran 0"));
        PairingIndex::build(&msgs).validate().unwrap();
    }

    #[test]
    fn does_not_touch_recent_successful_call() {
        // keep_recent_turns=10 → nothing is old → nothing reduced.
        let mut msgs = successful_session(3);
        let stats = apply(&mut msgs, &cfg(10, &[])).unwrap();
        assert_eq!(stats.stubbed, 0, "no old turns → nothing reduced");
        // All inputs are intact.
        assert!(
            msgs[1]["content"][0]["input"]["stdin"]
                .as_str()
                .unwrap()
                .starts_with("x"),
            "recent call input left intact"
        );
    }

    #[test]
    fn does_not_touch_failed_calls() {
        // All calls are errored: stale_input_cap must not fire at all.
        let mut msgs = failed_session(10);
        let stats = apply(&mut msgs, &cfg(4, &[])).unwrap();
        assert_eq!(
            stats.stubbed, 0,
            "failed calls left to failed_input_purge; stale_input_cap stubbed 0"
        );
        // Verify the bulky stdin was NOT elided by us (still raw 'x' bytes).
        assert!(
            msgs[1]["content"][0]["input"]["stdin"]
                .as_str()
                .unwrap()
                .starts_with('x'),
            "failed call input untouched by stale_input_cap"
        );
    }

    #[test]
    fn skips_exempt_task_tool() {
        let mut msgs = Vec::new();
        let uid = "toolu_task";
        msgs.push(json!({"role": "user", "content": [{"type": "text", "text": "do it"}]}));
        msgs.push(json!({"role": "assistant", "content": [
            {"type": "tool_use", "id": uid, "name": "Task", "input": {
                "description": "run tests",
                "prompt": "x".repeat(2000)
            }}
        ]}));
        msgs.push(json!({"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": uid, "content": "done"}
        ]}));
        // Pad to make the Task call old.
        for extra in successful_session(6) {
            msgs.push(extra);
        }
        let stats = apply(&mut msgs, &cfg(4, &["Task"])).unwrap();
        // Task is exempt → its input is never elided.
        assert_eq!(
            msgs[1]["content"][0]["input"]["prompt"]
                .as_str()
                .unwrap()
                .len(),
            2000,
            "Task input left intact (exempt)"
        );
        // But the non-exempt Bash calls in the padding were reduced.
        assert!(
            stats.stubbed >= 1,
            "non-exempt calls still reduced, got {}",
            stats.stubbed
        );
    }

    #[test]
    fn authored_content_exempt_even_with_empty_exempt_tools() {
        // §13A hard floor: a user config that drops Write/Edit/MultiEdit from
        // exempt_tools must STILL not elide authored file content (it corrupts
        // sessions). Build old successful Writes with bulky authored bodies.
        let mut msgs = Vec::new();
        for i in 0..10 {
            let uid = format!("w_{i}");
            msgs.push(json!({"role": "user", "content": [{"type": "text", "text": "go"}]}));
            msgs.push(json!({"role": "assistant", "content": [
                {"type": "tool_use", "id": uid, "name": "Write", "input": {
                    "file_path": format!("/src/f_{i}.rs"),
                    "content": "x".repeat(2000)
                }}
            ]}));
            msgs.push(json!({"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": uid, "content": format!("wrote {i}")}
            ]}));
        }
        // cfg(4, &[]) → exempt_tools is EMPTY (user removed the protection).
        let stats = apply(&mut msgs, &cfg(4, &[])).unwrap();
        assert_eq!(
            stats.stubbed, 0,
            "Write inputs must never be elided even when exempt_tools is empty"
        );
        // The oldest Write's authored body is kept verbatim (raw 'x', not a marker).
        assert!(
            msgs[1]["content"][0]["input"]["content"]
                .as_str()
                .unwrap()
                .starts_with('x'),
            "authored Write body must be intact (hard-floor exemption)"
        );
    }

    #[test]
    fn notebookedit_authored_source_exempt_even_with_empty_exempt_tools() {
        // §13A hard floor extends to NotebookEdit (authors cell `new_source`).
        let mut msgs = Vec::new();
        for i in 0..10 {
            let uid = format!("n_{i}");
            msgs.push(json!({"role": "user", "content": [{"type": "text", "text": "go"}]}));
            msgs.push(json!({"role": "assistant", "content": [
                {"type": "tool_use", "id": uid, "name": "NotebookEdit", "input": {
                    "notebook_path": format!("/nb_{i}.ipynb"),
                    "new_source": "y".repeat(2000)
                }}
            ]}));
            msgs.push(json!({"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": uid, "content": format!("edited {i}")}
            ]}));
        }
        // Empty exempt_tools: the hard floor (AUTHORING_TOOLS) must still protect it.
        let stats = apply(&mut msgs, &cfg(4, &[])).unwrap();
        assert_eq!(
            stats.stubbed, 0,
            "NotebookEdit new_source must never be elided (hard floor)"
        );
        assert!(
            msgs[1]["content"][0]["input"]["new_source"]
                .as_str()
                .unwrap()
                .starts_with('y'),
            "authored notebook source must be intact"
        );
    }

    #[test]
    fn small_input_is_noop() {
        // A successful call whose input has no bulk (only small scalars) →
        // nothing to elide → stubbed stays 0.
        let mut msgs = vec![
            json!({"role": "user", "content": [{"type": "text", "text": "go"}]}),
            json!({"role": "assistant", "content": [
                {"type": "tool_use", "id": "u0", "name": "Bash",
                 "input": {"command": "echo hello"}}
            ]}),
            json!({"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "u0", "content": "hello"}
            ]}),
        ];
        // Pad to make u0 old.
        for extra in successful_session(6) {
            msgs.push(extra);
        }
        let stats = apply(&mut msgs, &cfg(4, &[])).unwrap();
        assert_eq!(
            msgs[1]["content"][0]["input"],
            json!({"command": "echo hello"}),
            "small input kept verbatim"
        );
        // stubbed may be > 0 from the padding, but the small call itself must
        // not have been counted.  Verify its input is unchanged (done above).
        let _ = stats;
    }

    #[test]
    fn idempotent_and_deterministic() {
        let mut msgs = successful_session(10);
        // First run reduces old calls.
        let first = apply(&mut msgs, &cfg(4, &[])).unwrap();
        assert!(first.stubbed > 0, "first run should reduce something");
        // Capture byte representation after first run.
        let after_first = serde_json::to_vec(&msgs).unwrap();
        // Second run is a byte-identical no-op.
        let second = apply(&mut msgs, &cfg(4, &[])).unwrap();
        assert_eq!(second.stubbed, 0, "re-run elides nothing");
        let after_second = serde_json::to_vec(&msgs).unwrap();
        assert_eq!(after_first, after_second, "byte-identical after second run");

        // Determinism: two independent runs from the same input are byte-equal.
        let mut msgs_b = successful_session(10);
        apply(&mut msgs_b, &cfg(4, &[])).unwrap();
        let after_b = serde_json::to_vec(&msgs_b).unwrap();
        assert_eq!(
            after_first, after_b,
            "two independent runs are byte-identical"
        );
    }

    #[test]
    fn orphan_free_after() {
        let mut msgs = successful_session(8);
        apply(&mut msgs, &cfg(3, &[])).unwrap();
        PairingIndex::build(&msgs)
            .validate()
            .expect("no orphans after stale_input_cap");
    }

    #[test]
    fn both_input_strategies_coexist() {
        use crate::config::{FailedInputPurgeConfig, StaleInputCapConfig};
        use crate::strategies::failed_input_purge;

        // Mixed session: some calls succeed, some fail, all with bulky inputs.
        let mut msgs = Vec::new();
        for i in 0..8 {
            let uid = format!("toolu_{i}");
            msgs.push(json!({"role": "user", "content": [{"type": "text", "text": "go"}]}));
            let input = json!({
                "command": format!("cmd-{i}"),
                "stdin": "x".repeat(2000)
            });
            msgs.push(json!({"role": "assistant", "content": [
                {"type": "tool_use", "id": uid, "name": "Bash", "input": input}
            ]}));
            let mut res = json!({
                "type": "tool_result", "tool_use_id": uid,
                "content": format!("out {i}")
            });
            if i % 2 == 0 {
                res["is_error"] = json!(true);
            }
            msgs.push(json!({"role": "user", "content": [res]}));
        }

        let fip_cfg = FailedInputPurgeConfig {
            enabled: true,
            keep_recent_turns: 4,
            exempt_tools: vec![],
        };
        let sic_cfg = StaleInputCapConfig {
            enabled: true,
            keep_recent_turns: 4,
            exempt_tools: vec![],
        };

        // Run both strategies in order (mirrors strategies::run ordering).
        let fip_stats = failed_input_purge::apply(&mut msgs, &fip_cfg).unwrap();
        let sic_stats = apply(&mut msgs, &sic_cfg).unwrap();

        // Each strategy handles its own domain: no double-processing.
        assert!(
            fip_stats.stubbed > 0,
            "failed_input_purge fired on errored calls"
        );
        assert!(
            sic_stats.stubbed > 0,
            "stale_input_cap fired on successful calls"
        );

        // The total coverage: every old call (failed or successful) had its
        // bulk elided. Re-running both changes nothing.
        let fip2 = failed_input_purge::apply(&mut msgs, &fip_cfg).unwrap();
        let sic2 = apply(&mut msgs, &sic_cfg).unwrap();
        assert_eq!(fip2.stubbed, 0, "fip idempotent on second run");
        assert_eq!(sic2.stubbed, 0, "sic idempotent on second run");

        // Orphan-free after both strategies.
        PairingIndex::build(&msgs)
            .validate()
            .expect("no orphans after both strategies");
    }
}
