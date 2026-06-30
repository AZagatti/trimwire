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
//! **Authoring tools (`Write`/`Edit`/`MultiEdit`/`NotebookEdit`) are AGE-GATED with
//! a RECOVERABLE marker** (issue #122), not permanently exempt. Their authored body
//! is kept verbatim while RECENT — within the wider `authoring_keep_recent_turns`
//! window (default 6 vs. `keep_recent_turns` 2 for ordinary inputs) — because that
//! is the active-editing zone where eliding it would make the model rebuild on a
//! body it can't see and reproduce the marker AS the file content (§13A: observed
//! live — a written Go file became `[trimwire: NB input elided]`, breaking the
//! build). Once the body ages PAST that window, it is replaced with a marker that
//! names the file and instructs a re-read — `[trimwire: wrote <path> (<N>B) — read
//! the file to restore]` — NOT the generic size marker. The call SUCCEEDED, so the
//! content is on disk; the model reads the marker as a meta-annotation it can
//! recover from, never as file content to reproduce. A FAILED authored call is never
//! touched here (its content never hit disk → the floor lives in `failed_input_purge`).
//!
//! Reuses `KEEP_VALUE_MAX` + the shape-preserving logic from `failed_input_purge`
//! (`reduce_failed_input` for non-authoring bulk; `reduce_authored_input` here for
//! the recoverable authored marker). Same firing split as failed_input_purge by
//! `is_error` (NOT true vs. true); failed calls are left entirely to that strategy.
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
//! keep_recent_turns = 2            # ordinary inputs (Bash stdin, MCP args)
//! authoring_keep_recent_turns = 6  # authored bodies (wider; recoverable marker)
//! exempt_tools = ["Task", "Agent"] # never reduced (subagent prompts)
//! ```

use serde_json::Value;

use crate::config::{StaleInputCapConfig, matches_any};
use crate::error::Result;
use crate::pairing::PairingIndex;
use crate::strategies::failed_input_purge::{KEEP_VALUE_MAX, reduce_failed_input};
use crate::strategies::{AUTHORING_TOOLS, Stats, assistant_cutoff, block_mut, role};

/// Which reduction to apply to a collected location.
enum Reduction {
    /// Generic size marker for non-authoring bulk (Bash stdin, MCP args).
    Generic,
    /// Recoverable "read the file" marker for an authored body (carries the tool
    /// name so the marker verb is right).
    Authored(String),
}

/// Shape-preserving reduction of an OLD, SUCCESSFUL authored tool call's input,
/// using a RECOVERABLE marker. Unlike the generic `[trimwire: NB input elided]`,
/// the marker names the file and instructs a re-read — so the model reads it as a
/// meta-annotation, not as file content to reproduce (the §13A corruption was the
/// generic marker landing on disk as the file body). The call succeeded, so the
/// content IS on disk and a re-read fully recovers it. Small structural fields
/// (`file_path`, short `old_string`) are kept verbatim. Idempotent (a value already
/// starting with `"[trimwire:"` is kept) and never-grow (swaps in the marker only
/// when it is strictly smaller). Returns `Some` only if something was elided.
fn reduce_authored_input(name: &str, input: &Value) -> Option<Value> {
    let Value::Object(map) = input else {
        return None;
    };
    let path = map
        .get("file_path")
        .or_else(|| map.get("notebook_path"))
        .or_else(|| map.get("path"))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .unwrap_or("the file");
    let verb = match name {
        "Write" => "wrote",
        "NotebookEdit" => "edited cell in",
        _ => "edited", // Edit, MultiEdit
    };
    // Recoverable marker for an elided field of `orig_len` bytes — only when it
    // strictly shrinks the field (never-grow holds even for a long path).
    let shrink = |orig_len: usize| -> Option<Value> {
        let m = format!("[trimwire: {verb} {path} ({orig_len}B) — read the file to restore]");
        (m.len() < orig_len).then_some(Value::String(m))
    };
    let mut out = serde_json::Map::with_capacity(map.len());
    let mut changed = false;
    for (k, v) in map {
        match v {
            // Already a trimwire marker → keep (idempotent), regardless of length.
            Value::String(s) if s.starts_with("[trimwire:") => {
                out.insert(k.clone(), v.clone());
            }
            // Large authored string (content / new_string / old_string / new_source).
            // Note: for an Edit's `old_string`, "read the file to restore" points at the
            // POST-edit content (old_string is gone) — but that's harmless: the model
            // must re-read to form any new edit anyway, and old_string is almost always
            // a small anchor (< KEEP_VALUE_MAX), so it rarely hits this branch.
            Value::String(s) if s.len() > KEEP_VALUE_MAX => match shrink(s.len()) {
                Some(m) => {
                    out.insert(k.clone(), m);
                    changed = true;
                }
                None => {
                    out.insert(k.clone(), v.clone());
                }
            },
            // Nested bulk (MultiEdit `edits`, structured cell source).
            Value::Array(_) | Value::Object(_) => {
                let n = serde_json::to_vec(v).map(|b| b.len()).unwrap_or(0);
                match (n > KEEP_VALUE_MAX).then(|| shrink(n)).flatten() {
                    Some(m) => {
                        out.insert(k.clone(), m);
                        changed = true;
                    }
                    None => {
                        out.insert(k.clone(), v.clone());
                    }
                }
            }
            // Scalars + small strings (file_path, flags, short old_string) → keep.
            _ => {
                out.insert(k.clone(), v.clone());
            }
        }
    }
    changed.then_some(Value::Object(out))
}

/// Reduce the inputs of old, successful tool calls.
pub fn apply(messages: &mut [Value], cfg: &StaleInputCapConfig) -> Result<Stats> {
    super::with_stats(messages, |m| apply_counted(m, cfg))
}

/// Worker: returns the number of inputs reduced. Byte accounting is threaded by
/// the caller ([`super::run`]); see [`apply`].
pub(crate) fn apply_counted(messages: &mut [Value], cfg: &StaleInputCapConfig) -> Result<usize> {
    let idx = PairingIndex::build(messages);
    idx.validate()?;

    // Two recency windows (both clamped to >=1, mirroring the sibling strategies, so
    // a configured 0 can't reach the most-recent COMPLETED turn the model just used):
    //  - `cutoff`: NON-authoring inputs (Bash stdin, MCP args) age out at the tight
    //    `keep_recent_turns`.
    //  - `auth_cutoff`: AUTHORING bodies (Write/Edit/MultiEdit/NotebookEdit) age out
    //    at the wider `authoring_keep_recent_turns`, and only then, replaced with a
    //    RECOVERABLE "read the file" marker (the content is on disk). Recent authored
    //    content is left verbatim — that is what prevents the §13A loop.
    let cutoff = assistant_cutoff(messages, cfg.keep_recent_turns.max(1));
    let auth_cutoff = assistant_cutoff(messages, cfg.authoring_keep_recent_turns.max(1));
    if cutoff.is_none() && auth_cutoff.is_none() {
        return Ok(0); // history shorter than both windows → nothing is old
    }

    // Read-only pass first: collect (location, reduction-kind) of old, successful,
    // non-exempt tool_use blocks. We avoid holding a borrow across the mutation.
    let mut to_reduce: Vec<((usize, usize), Reduction)> = Vec::new();
    for (mi, msg) in messages.iter().enumerate() {
        if role(msg) != Some("assistant") {
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
            // User exempt list (subagent Task/Agent by default) — never reduced.
            if matches_any(&cfg.exempt_tools, name) {
                continue;
            }
            // Authoring bodies use the wider window + recoverable marker; everything
            // else uses the tight window + generic marker. A block not yet old enough
            // for its window is skipped (recent → keep verbatim).
            let is_authoring = AUTHORING_TOOLS.contains(&name);
            let old_enough = if is_authoring {
                auth_cutoff.is_some_and(|c| mi <= c)
            } else {
                cutoff.is_some_and(|c| mi <= c)
            };
            if !old_enough {
                continue;
            }
            let Some(id) = block.get("id").and_then(Value::as_str) else {
                continue;
            };
            // Only reduce SUCCESSFUL calls (no error flag). Failed calls belong to
            // `failed_input_purge` — and a failed authored call's body never hit disk,
            // so the "read the file to restore" marker would be a lie (that floor
            // stays unconditional in failed_input_purge).
            let is_error = idx
                .results
                .get(id)
                .and_then(|&(rmi, rci)| messages[rmi].get("content")?.as_array()?.get(rci))
                .and_then(|res| res.get("is_error"))
                .and_then(Value::as_bool)
                .unwrap_or(false);
            if !is_error {
                let kind = if is_authoring {
                    Reduction::Authored(name.to_owned())
                } else {
                    Reduction::Generic
                };
                to_reduce.push(((mi, ci), kind));
            }
        }
    }

    let mut stubbed = 0usize;
    for (loc, kind) in to_reduce {
        if let Some(block) = block_mut(messages, loc) {
            // Shape-preserving: keep small scalar fields, elide only the bulk. Count
            // only when something was actually elided, so a small input is a no-op.
            let reduced = match &kind {
                Reduction::Generic => block.get("input").and_then(reduce_failed_input),
                Reduction::Authored(name) => block
                    .get("input")
                    .and_then(|i| reduce_authored_input(name, i)),
            };
            if let Some(reduced) = reduced {
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
            // Default the authoring window to the same value so non-authoring tests
            // are unaffected; authoring-specific tests set it explicitly.
            authoring_keep_recent_turns: keep,
            exempt_tools: exempt.iter().map(|s| (*s).to_owned()).collect(),
        }
    }

    /// Build a session of `turns` assistant turns. Each turn is a Bash call with a
    /// large `stdin` (bulk) and a small `command` (structural). All calls succeed
    /// (no is_error on their results). Bash is a NON-authoring tool, so its bulk goes
    /// through the generic (size-marker) reduction path. Authored Write/Edit/MultiEdit/
    /// NotebookEdit content is age-gated instead (#122): recent bodies stay verbatim
    /// (§13A guard), old ones get a recoverable "read the file" marker — exercised by
    /// the authoring-specific tests, not this Bash helper.
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

    /// `keep_recent_turns = 0` must be clamped to 1 (like every other age-gated
    /// strategy): the MOST-RECENT completed turn's input is the one the model just
    /// used and must never be reduced. Two successful Bash calls; at keep=0 the older
    /// is reduced but the latest survives verbatim.
    #[test]
    fn keep_recent_zero_is_clamped_protecting_the_last_turn() {
        let mut msgs = successful_session(2);
        let stats = apply(&mut msgs, &cfg(0, &[])).unwrap();
        assert_eq!(
            stats.stubbed, 1,
            "only the older successful turn is reduced"
        );
        assert!(
            msgs[4]["content"][0]["input"]["stdin"]
                .as_str()
                .unwrap()
                .starts_with('x'),
            "the most-recent completed turn's input must survive the keep=0 clamp"
        );
        assert!(
            msgs[1]["content"][0]["input"]["stdin"]
                .as_str()
                .unwrap()
                .starts_with("[trimwire:"),
            "the older successful input is still reduced"
        );
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

    /// The SHIPPED default exempt list (`StaleInputCapConfig::default()`) protects
    /// BOTH subagent tool names — `Task` AND the drifted `Agent` — so an old
    /// subagent call's prompt is never elided, while non-exempt Bash bulk still
    /// reduces. Mirrors the bloat_cap exemption regression test; uses the real
    /// default exempt list (only `keep_recent_turns` is sized for the test).
    #[test]
    fn default_exempt_preserves_both_task_and_agent_subagent_inputs() {
        let mut msgs = Vec::new();
        for name in ["Task", "Agent"] {
            let uid = format!("toolu_{name}");
            msgs.push(json!({"role": "user", "content": [{"type": "text", "text": "go"}]}));
            msgs.push(json!({"role": "assistant", "content": [
                {"type": "tool_use", "id": uid, "name": name, "input": {
                    "description": "delegate", "prompt": "x".repeat(2000)
                }}
            ]}));
            msgs.push(json!({"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": uid, "content": "done"}
            ]}));
        }
        // Pad with non-exempt Bash so the subagent calls are old AND there is
        // something reducible (non-vacuous).
        for extra in successful_session(6) {
            msgs.push(extra);
        }
        let c = StaleInputCapConfig {
            enabled: true,
            keep_recent_turns: 4,
            ..StaleInputCapConfig::default() // the REAL exempt list
        };
        let stats = apply(&mut msgs, &c).unwrap();
        // msgs[1] = Task call, msgs[4] = Agent call — both prompts intact.
        assert_eq!(
            msgs[1]["content"][0]["input"]["prompt"]
                .as_str()
                .unwrap()
                .len(),
            2000,
            "Task prompt intact (default-exempt)"
        );
        assert_eq!(
            msgs[4]["content"][0]["input"]["prompt"]
                .as_str()
                .unwrap()
                .len(),
            2000,
            "Agent prompt intact (default-exempt)"
        );
        assert!(
            stats.stubbed >= 1,
            "non-exempt Bash bulk still reduced, got {}",
            stats.stubbed
        );
    }

    /// §13A guard AND the two-window branch: an authored body PAST the generic window
    /// but still inside the WIDER authoring window must be kept verbatim, while ordinary
    /// (Bash) inputs at the same age ARE reduced. This exercises the per-block authoring
    /// recency branch (not the early-exit): 8 turns, keep_recent_turns=2,
    /// authoring_keep_recent_turns=6 → the Write at turn 2 is old for the generic window
    /// but recent for the authoring window.
    #[test]
    fn authoring_window_protects_body_past_the_generic_window() {
        let push_turn = |m: &mut Vec<Value>, id: &str, name: &str, input: Value| {
            m.push(json!({"role": "assistant", "content": [
                {"type": "tool_use", "id": id, "name": name, "input": input}
            ]}));
            m.push(json!({"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": id, "content": "ok"}
            ]}));
        };
        let mut m = Vec::new();
        push_turn(
            &mut m,
            "b0",
            "Bash",
            json!({"command": "c", "stdin": "y".repeat(2000)}),
        );
        push_turn(
            &mut m,
            "b1",
            "Bash",
            json!({"command": "c", "stdin": "y".repeat(2000)}),
        );
        push_turn(
            &mut m,
            "w2",
            "Write",
            json!({"file_path": "/src/a.rs", "content": "x".repeat(2000)}),
        );
        for i in 3..8 {
            push_turn(&mut m, &format!("p{i}"), "Bash", json!({"command": "echo"}));
        }
        let c = StaleInputCapConfig {
            enabled: true,
            keep_recent_turns: 2,
            authoring_keep_recent_turns: 6,
            exempt_tools: vec![],
        };
        let stats = apply(&mut m, &c).unwrap();
        // Only the two OLD Bash stdins are reduced; the Write is protected by the wider
        // authoring window even though it is past the generic window.
        assert_eq!(stats.stubbed, 2, "only the two old Bash inputs are reduced");
        assert!(
            m[4]["content"][0]["input"]["content"]
                .as_str()
                .unwrap()
                .starts_with('x'),
            "authored body past the generic window must still be protected by the \
             wider authoring window (§13A guard)"
        );
        assert!(
            m[0]["content"][0]["input"]["stdin"]
                .as_str()
                .unwrap()
                .starts_with("[trimwire:"),
            "an ordinary old Bash input at the same age IS reduced"
        );
    }

    /// Age-gate (replaces the old all-ages exemption): an OLD successful Write's
    /// authored body is reduced to a RECOVERABLE marker that names the file and
    /// instructs a re-read (content is on disk) — NOT the generic size marker — while
    /// the file_path stays verbatim and the recent Write is untouched.
    #[test]
    fn old_authored_content_reduced_to_recoverable_marker() {
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
        // keep=4, authoring=4 → turns 0..=5 are old, 6..=9 recent.
        let stats = apply(&mut msgs, &cfg(4, &[])).unwrap();
        assert!(
            stats.stubbed >= 1,
            "old authored Writes are reduced, got {}",
            stats.stubbed
        );
        // Oldest Write (turn 0): authored body → recoverable marker.
        let oldest = msgs[1]["content"][0]["input"]["content"].as_str().unwrap();
        assert!(
            oldest.starts_with("[trimwire:")
                && oldest.contains("/src/f_0.rs")
                && oldest.contains("read the file"),
            "old authored body must become the recoverable marker; got: {oldest}"
        );
        assert!(
            !oldest.contains("input elided"),
            "must use the recoverable marker, NOT the generic one: {oldest}"
        );
        // file_path is a small scalar → kept verbatim (the model can act on it).
        assert_eq!(
            msgs[1]["content"][0]["input"]["file_path"],
            json!("/src/f_0.rs")
        );
        // Most-recent Write (turn 9, in-window) kept verbatim.
        assert!(
            msgs[28]["content"][0]["input"]["content"]
                .as_str()
                .unwrap()
                .starts_with('x'),
            "recent authored body kept verbatim (§13A guard)"
        );
    }

    /// NotebookEdit `new_source` is age-gated the same way (recoverable marker
    /// names the notebook + re-read; recent kept verbatim).
    #[test]
    fn old_notebookedit_source_reduced_to_recoverable_marker() {
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
        let stats = apply(&mut msgs, &cfg(4, &[])).unwrap();
        assert!(stats.stubbed >= 1, "old NotebookEdit sources are reduced");
        let oldest = msgs[1]["content"][0]["input"]["new_source"]
            .as_str()
            .unwrap();
        assert!(
            oldest.starts_with("[trimwire:")
                && oldest.contains("/nb_0.ipynb")
                && oldest.contains("read the file"),
            "old notebook source must become the recoverable marker; got: {oldest}"
        );
        assert!(
            msgs[28]["content"][0]["input"]["new_source"]
                .as_str()
                .unwrap()
                .starts_with('y'),
            "recent notebook source kept verbatim"
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
            authoring_keep_recent_turns: 4,
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
