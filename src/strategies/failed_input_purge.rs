//! `FailedInputPurge` strategy — mirrors opencode-dcp's default "purge errors".
//!
//! When a `tool_use` produced an error (its paired `tool_result.is_error ==
//! true`) and it is older than `keep_recent_turns` assistant turns, the *bulk*
//! of its `tool_use.input` is no longer useful — the error text in the result
//! already says what went wrong.
//!
//! **Shape-preserving** reduction (not a blanket `{}`): keep the small scalar
//! fields — `command`, `file_path`, flags — so the model still knows *which*
//! call failed (the comprehension win; a live capture showed the failed command
//! was otherwise lost), and replace only the genuine bulk — large string values
//! and nested arrays/objects (heredocs, file bodies, stdin) — with a
//! content-free size marker. A call whose input is already small is left
//! untouched (nothing to elide).
//!
//! Input-only mutation: never drops a pair, never touches `tool_result`, so it
//! cannot orphan anything. Deterministic and idempotent (existing markers are
//! kept; a re-run is a byte-identical no-op).

use serde_json::Value;

use crate::config::{FailedInputPurgeConfig, matches_any};
use crate::error::Result;
use crate::pairing::PairingIndex;
use crate::strategies::{AUTHORING_TOOLS, Stats, assistant_cutoff, block_mut, role};

/// Clear the inputs of old errored tool calls.
pub fn apply(messages: &mut [Value], cfg: &FailedInputPurgeConfig) -> Result<Stats> {
    super::with_stats(messages, |m| apply_counted(m, cfg))
}

/// Worker: returns the number of inputs purged. Byte accounting is threaded by
/// the caller ([`super::run`]); see [`apply`].
pub(crate) fn apply_counted(messages: &mut [Value], cfg: &FailedInputPurgeConfig) -> Result<usize> {
    let idx = PairingIndex::build(messages);
    idx.validate()?;

    let Some(cutoff) = assistant_cutoff(messages, cfg.keep_recent_turns) else {
        return Ok(0);
    };

    // Collect (use_loc) of old errored, non-exempt tool calls. Read-only pass
    // first so we don't hold a borrow across the mutation.
    let mut to_purge: Vec<(usize, usize)> = Vec::new();
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
            // Hard floor: never elide authored file content, even on a FAILED call
            // — the model may re-author from it on a retry and otherwise copies the
            // elision marker as the file body (§13A). Then honor the config exempt list.
            if AUTHORING_TOOLS.contains(&name) || matches_any(&cfg.exempt_tools, name) {
                continue;
            }
            let Some(id) = block.get("id").and_then(Value::as_str) else {
                continue;
            };
            // Only purge if the paired result is an error.
            let is_error = idx
                .results
                .get(id)
                .and_then(|&(rmi, rci)| messages[rmi].get("content")?.as_array()?.get(rci))
                .and_then(|res| res.get("is_error"))
                .and_then(Value::as_bool)
                .unwrap_or(false);
            if is_error {
                to_purge.push((mi, ci));
            }
        }
    }

    let mut stubbed = 0usize;
    for loc in to_purge {
        if let Some(block) = block_mut(messages, loc) {
            // Shape-preserving: keep small scalar fields (the command/path that
            // says what failed), elide only the bulk. Count only when something
            // was actually elided, so a small input is a true no-op.
            if let Some(reduced) = block.get("input").and_then(reduce_failed_input) {
                block["input"] = reduced;
                stubbed += 1;
            }
        }
    }

    Ok(stubbed)
}

/// Scalar string values at or under this length are kept verbatim (commands,
/// paths, flags); larger strings and any nested array/object are the bulk we
/// elide. Generous enough to keep a typical command/one-liner.
pub(crate) const KEEP_VALUE_MAX: usize = 512;

/// A content-free size marker for an elided input value.
pub(crate) fn elided_marker(n: usize) -> Value {
    Value::String(format!("[trimwire: {n}B input elided]"))
}

/// Shape-preserving reduction of a tool call's `input`: keep small scalar
/// fields, replace only large strings and nested arrays/objects with a size
/// marker. Returns `Some(reduced)` ONLY if something was actually elided — so an
/// already-small (or already-reduced) input is left byte-identical (no needless
/// re-serialize, no cache churn). Idempotent: a value that is already a
/// `[trimwire: …]` marker is small and kept, so re-running changes nothing.
///
/// Shared by `failed_input_purge` (errored calls) and `stale_input_cap`
/// (successful calls) — both strategies want the same shape-preserving
/// reduction; only the condition that gates firing differs.
pub(crate) fn reduce_failed_input(input: &Value) -> Option<Value> {
    let Value::Object(map) = input else {
        return None; // non-object inputs are rare for tool_use; leave as-is
    };
    let mut out = serde_json::Map::with_capacity(map.len());
    let mut changed = false;
    for (k, v) in map {
        match v {
            // Large string value (e.g. stdin/heredoc) → elide. (A marker string
            // is short and won't match, so this is idempotent.)
            Value::String(s) if s.len() > KEEP_VALUE_MAX => {
                out.insert(k.clone(), elided_marker(s.len()));
                changed = true;
            }
            // Nested array/object: elide only the genuine bulk (a big Write
            // `content` / payload); keep small structured args verbatim.
            Value::Array(_) | Value::Object(_) => {
                let n = serde_json::to_vec(v).map(|b| b.len()).unwrap_or(0);
                if n > KEEP_VALUE_MAX {
                    out.insert(k.clone(), elided_marker(n));
                    changed = true;
                } else {
                    out.insert(k.clone(), v.clone());
                }
            }
            // Scalars + small strings (command, path, flags, the marker) → keep.
            _ => {
                out.insert(k.clone(), v.clone());
            }
        }
    }
    changed.then_some(Value::Object(out))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn cfg(keep: usize, exempt: &[&str]) -> FailedInputPurgeConfig {
        FailedInputPurgeConfig {
            enabled: true,
            keep_recent_turns: keep,
            exempt_tools: exempt.iter().map(|s| (*s).to_owned()).collect(),
        }
    }

    /// §13A hard floor: a FAILED authored call (Write/Edit/MultiEdit) must keep its
    /// body verbatim, even when not in exempt_tools — the model may re-author from
    /// it on a retry and otherwise copies the elision marker as the file content.
    #[test]
    fn authored_content_exempt_on_failed_calls_even_with_empty_exempt_tools() {
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
                {"type": "tool_result", "tool_use_id": uid, "is_error": true,
                 "content": "error: permission denied"}
            ]}));
        }
        // Empty exempt_tools: the user removed the protection.
        let stats = apply(&mut msgs, &cfg(2, &[])).unwrap();
        assert_eq!(
            stats.stubbed, 0,
            "failed authored Write content must never be elided (hard floor)"
        );
        assert!(
            msgs[1]["content"][0]["input"]["content"]
                .as_str()
                .unwrap()
                .starts_with('x'),
            "failed Write body kept verbatim for safe re-authoring"
        );
    }

    /// Drift fix: the PRODUCTION default exempt_tools must preserve a FAILED subagent
    /// call's bulky input (the sub-task prompt) for BOTH subagent names (Task + Agent);
    /// ordinary failed tools (Bash) still get their bulk elided; no broad exemption.
    #[test]
    fn subagent_task_and_agent_failed_inputs_exempt_by_default() {
        // one OLD failed call of `name` carrying a bulky `field`, then recent padding.
        let mk = |name: &str, field: &str| {
            let mut msgs = Vec::new();
            msgs.push(json!({"role": "user", "content": [{"type": "text", "text": "go"}]}));
            msgs.push(json!({"role": "assistant", "content": [
                {"type": "tool_use", "id": "sub0", "name": name,
                 "input": {"description": "t", field: "x".repeat(2000)}}
            ]}));
            msgs.push(json!({"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "sub0", "is_error": true,
                 "content": "error: failed"}
            ]}));
            for i in 0..8 {
                let u = format!("p{i}");
                msgs.push(json!({"role": "assistant", "content": [
                    {"type": "tool_use", "id": u, "name": "Bash", "input": {"command": "echo"}}
                ]}));
                msgs.push(json!({"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": u, "content": "ok"}
                ]}));
            }
            msgs
        };
        // Use the REAL default exempt list (now ["Task","Agent"]); tweak only enable/window.
        let base = FailedInputPurgeConfig {
            enabled: true,
            keep_recent_turns: 2,
            ..FailedInputPurgeConfig::default()
        };
        for name in ["Task", "Agent"] {
            let mut m = mk(name, "prompt");
            let s = apply(&mut m, &base).unwrap();
            assert_eq!(
                s.stubbed, 0,
                "{name} failed input must be exempt (subagent preservation)"
            );
            assert!(
                m[1]["content"][0]["input"]["prompt"]
                    .as_str()
                    .unwrap()
                    .starts_with('x'),
                "{name} bulky sub-task prompt kept verbatim"
            );
        }
        // Ordinary non-exempt failed tool (Bash) still gets its bulk elided.
        let mut mb = mk("Bash", "stdin");
        let s = apply(&mut mb, &base).unwrap();
        assert!(s.stubbed >= 1, "ordinary failed Bash input still purged");
        assert!(
            mb[1]["content"][0]["input"]["stdin"]
                .as_str()
                .unwrap()
                .starts_with("[trimwire:"),
            "Bash bulky stdin elided to a marker"
        );
    }

    /// N turns, each a Bash call; even-indexed ones errored AND carry a bulky
    /// `stdin` (the kind of payload worth eliding). Old errored calls keep their
    /// `command` but lose the bulk; the error text always stays.
    fn session(turns: usize) -> Vec<Value> {
        let mut msgs = Vec::new();
        for i in 0..turns {
            let uid = format!("toolu_{i}");
            msgs.push(json!({"role": "user", "content": [{"type": "text", "text": "go"}]}));
            let input = if i % 2 == 0 {
                json!({"command": format!("cmd-{i} --flag"), "stdin": "x".repeat(2000)})
            } else {
                json!({"command": format!("cmd-{i} --flag")})
            };
            msgs.push(json!({"role": "assistant", "content": [
                {"type": "tool_use", "id": uid, "name": "Bash", "input": input}
            ]}));
            let mut res =
                json!({"type": "tool_result", "tool_use_id": uid, "content": format!("out {i}")});
            if i % 2 == 0 {
                res["is_error"] = json!(true);
                res["content"] = json!(format!("error: cmd-{i} failed"));
            }
            msgs.push(json!({"role": "user", "content": [res]}));
        }
        msgs
    }

    #[test]
    fn elides_bulk_of_old_errored_inputs_but_keeps_the_command() {
        let mut msgs = session(10); // 10 assistant turns
        let stats = apply(&mut msgs, &cfg(4, &[])).unwrap();
        // Errored turns 0,2,4,6 are old (turn 8 is within the recent-4 window).
        assert!(
            stats.stubbed >= 3,
            "old errored inputs reduced, got {}",
            stats.stubbed
        );
        // Turn 0 (errored, old): the bulky stdin is elided, but the COMMAND is
        // kept — the model still sees which call failed.
        assert_eq!(
            msgs[1]["content"][0]["input"]["command"],
            json!("cmd-0 --flag"),
            "command preserved (comprehension)"
        );
        assert!(
            msgs[1]["content"][0]["input"]["stdin"]
                .as_str()
                .unwrap()
                .starts_with("[trimwire:"),
            "bulky stdin elided to a marker"
        );
        // Error text preserved.
        assert_eq!(
            msgs[2]["content"][0]["content"],
            json!("error: cmd-0 failed")
        );
        // A successful old call keeps its full input (turn 1, assistant idx 4).
        assert_eq!(
            msgs[4]["content"][0]["input"],
            json!({"command": "cmd-1 --flag"})
        );
        PairingIndex::build(&msgs).validate().unwrap();
    }

    #[test]
    fn small_failed_input_is_kept_and_purge_is_idempotent() {
        // A failed call whose input is ONLY a small command → nothing to elide →
        // input kept verbatim (the comprehension win vs the old blanket `{}`).
        let mut msgs = vec![
            json!({"role": "user", "content": [{"type": "text", "text": "go"}]}),
            json!({"role": "assistant", "content": [
                {"type": "tool_use", "id": "u0", "name": "Bash", "input": {"command": "deploy --prod"}}
            ]}),
            json!({"role": "user", "content": [{"type": "tool_result", "tool_use_id": "u0", "content": "boom", "is_error": true}]}),
        ];
        for extra in session(6) {
            msgs.push(extra);
        }
        apply(&mut msgs, &cfg(4, &[])).unwrap();
        assert_eq!(
            msgs[1]["content"][0]["input"],
            json!({"command": "deploy --prod"}),
            "small failed command kept verbatim"
        );
        // Idempotent: a second pass elides nothing more and leaves bytes identical.
        let before = serde_json::to_vec(&msgs).unwrap();
        let stats2 = apply(&mut msgs, &cfg(4, &[])).unwrap();
        assert_eq!(stats2.stubbed, 0, "re-run elides nothing");
        assert_eq!(serde_json::to_vec(&msgs).unwrap(), before, "byte-identical");
    }

    #[test]
    fn recent_errors_are_kept() {
        // keep_recent_turns large → nothing is old → nothing purged.
        let mut msgs = session(3);
        let stats = apply(&mut msgs, &cfg(10, &[])).unwrap();
        assert_eq!(stats.stubbed, 0);
    }

    #[test]
    fn successful_calls_untouched() {
        // All successful (odd-only errors but start at 1): use 1-turn no-error.
        let mut msgs = vec![
            json!({"role": "user", "content": [{"type": "text", "text": "go"}]}),
            json!({"role": "assistant", "content": [
                {"type": "tool_use", "id": "u0", "name": "Bash", "input": {"command": "ok"}}
            ]}),
            json!({"role": "user", "content": [{"type": "tool_result", "tool_use_id": "u0", "content": "fine"}]}),
        ];
        // pad to make it old
        for extra in session(6) {
            msgs.push(extra);
        }
        let stats = apply(&mut msgs, &cfg(4, &[])).unwrap();
        // u0 succeeded → its input is never cleared.
        assert_eq!(msgs[1]["content"][0]["input"], json!({"command": "ok"}));
        assert!(
            stats.stubbed >= 1,
            "errored ones in the padding still purged"
        );
    }
}
