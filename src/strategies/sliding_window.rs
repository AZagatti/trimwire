//! `SlidingWindow` strategy.
//!
//! Drops (stubs) tool_use/tool_result pairs whose tool name matches the
//! denylist and whose enclosing assistant message is older than
//! `keep_recent_turns` assistant turns from the end of the messages array.
//!
//! Pair-aware: uses `pairing::PairingIndex` to drop both halves atomically, so
//! it never orphans a pair. It pre-validates its input (a failure means the input
//! was already broken — surfaced so the gateway forwards the body unmutated); the
//! orphan invariant on the OUTPUT is enforced once for the whole pipeline by
//! `strategies::run`'s single final validate (SPIKE.md §5).
//!
//! Stub format:
//! - `tool_use.input` → a recognizable `[trimwire: input elided …]` breadcrumb
//!   (or `{}` when that wouldn't shrink a tiny input; only an existing input key
//!   is ever stubbed — never added)
//! - `tool_result.content` → the configured stub string
//!
//! Faithful port of `tests/phase0/strategies.py::apply_sliding_window`; the
//! two produce identical output on the same input for exact-name denylists
//! (verified byte-for-byte on the fixture corpus). Two intentional additions
//! over the Python reference: `exempt_tools` (empty in the reference's call
//! sites) and glob (`*`) matching of denylist/exempt patterns (the reference
//! uses plain set membership — a pattern with no `*` is an exact match, so
//! the two agree whenever the denylist is glob-free).

use std::collections::HashSet;

use serde_json::{Value, json};

use crate::config::{SlidingWindowConfig, matches_any};
use crate::error::Result;
use crate::pairing::PairingIndex;
use crate::strategies::{Stats, assistant_cutoff, block_mut, role};

/// Stub denylisted tool pairs older than `cfg.keep_recent_turns` assistant
/// turns. Mutates `messages` in place (never resizes — we stub, not delete);
/// returns counts + byte deltas.
pub fn apply(messages: &mut [Value], cfg: &SlidingWindowConfig) -> Result<Stats> {
    super::with_stats(messages, |m| apply_counted(m, cfg))
}

/// Worker: returns the number of pairs stubbed. Byte accounting is threaded by
/// the caller ([`super::run`]); see [`apply`].
pub(crate) fn apply_counted(messages: &mut [Value], cfg: &SlidingWindowConfig) -> Result<usize> {
    let idx = PairingIndex::build(messages);
    idx.validate()?; // pre-check: orphaned input → caller rolls back

    // Walk backwards counting assistant turns; the first message index past
    // the `keep_recent_turns` window is the (inclusive) cutoff. Clamp to ≥1
    // (mirrors thinking_strip): a configured 0 would otherwise let the cutoff reach
    // the in-progress assistant turn and stub a tool_use the model is actively using.
    let cutoff = assistant_cutoff(messages, cfg.keep_recent_turns.max(1));

    // Collect denylisted (and non-exempt) tool_use ids at or before the
    // cutoff that the index actually knows about.
    let mut ids_to_stub: HashSet<String> = HashSet::new();
    if let Some(cutoff) = cutoff {
        for msg in messages.iter().take(cutoff + 1) {
            if role(msg) != Some("assistant") {
                continue;
            }
            let Some(content) = msg.get("content").and_then(Value::as_array) else {
                continue;
            };
            for block in content {
                if block.get("type").and_then(Value::as_str) != Some("tool_use") {
                    continue;
                }
                let Some(name) = block.get("name").and_then(Value::as_str) else {
                    continue;
                };
                // AUTHORING_TOOLS is an unconditional hard floor (§13A): a
                // misconfigured denylist must never wipe Write/Edit/MultiEdit/
                // NotebookEdit inputs to `{}` — the model would rebuild on a body
                // it can no longer see. This mirrors the floor in stale_input_cap /
                // failed_input_purge and is not removable by config.
                if !matches_any(&cfg.denylist_tools, name)
                    || matches_any(&cfg.exempt_tools, name)
                    || super::AUTHORING_TOOLS.contains(&name)
                {
                    continue;
                }
                if let Some(id) = block.get("id").and_then(Value::as_str) {
                    if idx.uses.contains_key(id) {
                        ids_to_stub.insert(id.to_owned());
                    }
                }
            }
        }
    }

    // Stub both halves of each pair atomically. Order is irrelevant — each
    // pair lives in distinct content blocks — so the output is deterministic.
    let empty = json!({});
    // Recognizable breadcrumb so the model can tell an old denylisted input was
    // blanked BY trimwire (not a tool genuinely called with no arguments). Stays
    // shrink-only: fall back to `{}` when the marker wouldn't be strictly smaller
    // than the current input, so the body never grows. Idempotent on either shape.
    let elided_input =
        json!({ "_trimwire": "[trimwire: input elided — older than the sliding window]" });
    let json_len = |v: &Value| serde_json::to_string(v).map_or(usize::MAX, |s| s.len());
    let mut stubbed = 0usize;
    for id in &ids_to_stub {
        let (use_loc, res_loc) = idx.pair(id);
        // Count only pairs we actually change, so a body already in the stubbed
        // shape isn't reported as a mutation (which would force a needless
        // re-serialize and bust the cache prefix for no benefit).
        let mut changed = false;
        if let Some(loc) = use_loc {
            if let Some(block) = block_mut(messages, loc) {
                let cur = block.get("input");
                // Only stub an input that EXISTS — never add an `input` key that
                // wasn't there (that would GROW the body on malformed traffic and
                // count a false mutation).
                let exists = cur.is_some();
                let already_stubbed = cur == Some(&empty) || cur == Some(&elided_input);
                let cur_len = cur.map_or(0, json_len);
                if exists && !already_stubbed {
                    block["input"] = if json_len(&elided_input) < cur_len {
                        elided_input.clone()
                    } else {
                        empty.clone()
                    };
                    changed = true;
                }
            }
        }
        if let Some(loc) = res_loc {
            if let Some(block) = block_mut(messages, loc) {
                let content = block.get("content").cloned().unwrap_or(Value::Null);
                // Skip content already carrying our marker → idempotent
                // re-application. Guard on the configured stub prefix so it holds
                // even if the user customises `stub`.
                // Also skip results already cleared by Claude Code's own micro-compact
                // (the "[Old tool result content cleared]" marker) — re-eliding those
                // would only grow the body for no benefit.
                let already_our_stub = content
                    .as_str()
                    .is_some_and(|s| !cfg.stub.is_empty() && s.starts_with(cfg.stub.as_str()));
                if !already_our_stub && !super::is_already_cleared(&content) {
                    block["content"] = super::elision_marker(&cfg.stub, &content);
                    changed = true;
                }
            }
        }
        if changed {
            stubbed += 1;
        }
    }

    Ok(stubbed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::SlidingWindowConfig;

    /// Build an N-turn session of paired Bash calls (mirrors the Python
    /// `fixture_long_session`: user / assistant(tool_use) / user(tool_result)).
    fn long_session(turns: usize) -> Vec<Value> {
        let mut msgs = Vec::new();
        for i in 0..turns {
            let uid = format!("toolu_{i:04}");
            msgs.push(
                json!({"role": "user", "content": [{"type": "text", "text": format!("turn {i}")}]}),
            );
            msgs.push(json!({"role": "assistant", "content": [
                {"type": "tool_use", "id": uid, "name": "Bash", "input": {"command": format!("echo {i}")}}
            ]}));
            msgs.push(json!({"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": uid, "content": i.to_string()}
            ]}));
        }
        msgs
    }

    fn cfg(denylist: &[&str], keep: usize) -> SlidingWindowConfig {
        SlidingWindowConfig {
            enabled: true,
            keep_recent_turns: keep,
            denylist_tools: denylist.iter().map(|s| (*s).to_owned()).collect(),
            exempt_tools: Vec::new(),
            stub: "[trimwire: elided, older than sliding window]".to_owned(),
        }
    }

    /// 10 assistant turns, keep 4 → 6 stubbed (the Python off-by-one test).
    #[test]
    fn off_by_one_ten_turns_keep_four() {
        let mut msgs = long_session(10);
        let stats = apply(&mut msgs, &cfg(&["Bash"], 4)).unwrap();
        assert_eq!(stats.stubbed, 6);
        // No orphans after mutation.
        PairingIndex::build(&msgs).validate().unwrap();
        // We stub, never delete.
        assert_eq!(msgs.len(), 30);
    }

    /// The 4 most-recent assistant turns keep their real input/content.
    #[test]
    fn recent_turns_untouched_old_turns_stubbed() {
        let mut msgs = long_session(10);
        apply(&mut msgs, &cfg(&["Bash"], 4)).unwrap();
        // Oldest turn (assistant at index 1) is stubbed.
        assert_eq!(msgs[1]["content"][0]["input"], json!({}));
        assert!(
            msgs[2]["content"][0]["content"]
                .as_str()
                .unwrap()
                .starts_with("[trimwire: elided, older than sliding window]")
        );
        // Most-recent turn (assistant at index 28) is untouched.
        assert_eq!(
            msgs[28]["content"][0]["input"],
            json!({"command": "echo 9"})
        );
        assert_eq!(msgs[29]["content"][0]["content"], json!("9"));
    }

    /// Empty denylist → nothing matches → no mutation, byte-stable.
    #[test]
    fn empty_denylist_is_noop() {
        let mut msgs = long_session(10);
        let before = serde_json::to_vec(&msgs).unwrap();
        let stats = apply(&mut msgs, &cfg(&[], 4)).unwrap();
        assert_eq!(stats.stubbed, 0);
        assert_eq!(stats.elided_bytes(), 0);
        assert_eq!(serde_json::to_vec(&msgs).unwrap(), before);
    }

    /// AUTHORING_TOOLS are an unconditional hard floor: even with `Write` in the
    /// denylist, its authored input + result must survive — otherwise the model
    /// rebuilds on a body it can no longer see (§13A class of corruption).
    #[test]
    fn authoring_tool_in_denylist_is_never_stubbed() {
        let body = format!("// authored\n{}", "x".repeat(2000));
        let mut msgs = vec![
            json!({"role":"assistant","content":[
                {"type":"tool_use","id":"w0","name":"Write",
                 "input":{"file_path":"/src/a.rs","content": body}}
            ]}),
            json!({"role":"user","content":[
                {"type":"tool_result","tool_use_id":"w0","content":"wrote"}
            ]}),
        ];
        // Eight recent Bash turns age the Write past keep_recent_turns = 4.
        for i in 0..8 {
            let uid = format!("b{i}");
            msgs.push(json!({"role":"assistant","content":[
                {"type":"tool_use","id":uid,"name":"Bash","input":{"command":format!("echo {i}")}}
            ]}));
            msgs.push(json!({"role":"user","content":[
                {"type":"tool_result","tool_use_id":uid,"content":i.to_string()}
            ]}));
        }
        // Deny BOTH Write and Bash — the floor must still protect Write.
        apply(&mut msgs, &cfg(&["Write", "Bash"], 4)).unwrap();
        assert_eq!(
            msgs[0]["content"][0]["input"]["content"],
            json!(body),
            "authored Write input must survive even when Write is denylisted"
        );
        assert_eq!(msgs[1]["content"][0]["content"], json!("wrote"));
        PairingIndex::build(&msgs).validate().unwrap();
    }

    /// A large old denylisted input is replaced with a RECOGNIZABLE trimwire
    /// breadcrumb (not a silent `{}`); a small input stays `{}` (shrink-safe,
    /// covered by `recent_turns_untouched_old_turns_stubbed`).
    #[test]
    fn large_old_input_gets_recognizable_breadcrumb() {
        let big_cmd = format!("echo {}", "a".repeat(200));
        let mut msgs = vec![
            json!({"role":"assistant","content":[
                {"type":"tool_use","id":"b0","name":"Bash","input":{"command": big_cmd}}
            ]}),
            json!({"role":"user","content":[
                {"type":"tool_result","tool_use_id":"b0","content":"ok"}
            ]}),
        ];
        for i in 0..8 {
            let uid = format!("r{i}");
            msgs.push(json!({"role":"assistant","content":[
                {"type":"tool_use","id":uid,"name":"Bash","input":{"command":format!("echo {i}")}}
            ]}));
            msgs.push(json!({"role":"user","content":[
                {"type":"tool_result","tool_use_id":uid,"content":i.to_string()}
            ]}));
        }
        apply(&mut msgs, &cfg(&["Bash"], 4)).unwrap();
        let blanked = &msgs[0]["content"][0]["input"];
        assert_ne!(
            *blanked,
            json!({}),
            "large input should carry a breadcrumb, not a silent {{}}"
        );
        assert!(
            serde_json::to_string(blanked)
                .unwrap()
                .contains("[trimwire: input elided"),
            "breadcrumb must be recognizable as trimwire; got {blanked}"
        );
        PairingIndex::build(&msgs).validate().unwrap();
        // Idempotent: a second pass leaves the breadcrumb untouched.
        let before = serde_json::to_vec(&msgs).unwrap();
        apply(&mut msgs, &cfg(&["Bash"], 4)).unwrap();
        assert_eq!(
            serde_json::to_vec(&msgs).unwrap(),
            before,
            "breadcrumb is idempotent"
        );
    }

    /// A denylisted tool that is also exempt is skipped.
    #[test]
    fn exempt_overrides_denylist() {
        let mut msgs = long_session(10);
        let mut c = cfg(&["Bash"], 4);
        c.exempt_tools = vec!["Bash".to_owned()];
        let stats = apply(&mut msgs, &c).unwrap();
        assert_eq!(stats.stubbed, 0);
    }

    /// Re-applying to an already-stubbed body counts zero changes (so it
    /// reports a no-op and the caller forwards verbatim — no cache-prefix churn).
    #[test]
    fn reapplying_to_stubbed_body_is_a_noop() {
        let mut msgs = long_session(6);
        let c = cfg(&["Bash"], 1);
        let first = apply(&mut msgs, &c).unwrap();
        assert!(first.stubbed > 0, "first pass stubs the old turns");
        let second = apply(&mut msgs, &c).unwrap();
        assert_eq!(second.stubbed, 0, "second pass changes nothing");
    }

    /// Glob denylist matches MCP tool names.
    #[test]
    fn glob_denylist_matches() {
        let mut msgs = vec![
            json!({"role": "user", "content": [{"type": "text", "text": "go"}]}),
            json!({"role": "assistant", "content": [
                {"type": "tool_use", "id": "toolu_a", "name": "mcp__playwright__navigate", "input": {"url": "x"}}
            ]}),
            json!({"role": "user", "content": [{"type": "tool_result", "tool_use_id": "toolu_a", "content": "ok"}]}),
        ];
        // Pad with recent turns so the MCP turn falls outside the window.
        for extra in long_session(5) {
            msgs.push(extra);
        }
        let stats = apply(&mut msgs, &cfg(&["mcp__playwright__*"], 4)).unwrap();
        assert_eq!(stats.stubbed, 1);
        assert_eq!(msgs[1]["content"][0]["input"], json!({}));
    }

    /// Parallel tool_use blocks in one old assistant turn are all stubbed
    /// atomically (no orphans).
    #[test]
    fn parallel_pairs_dropped_atomically() {
        let mut msgs = vec![
            json!({"role": "user", "content": [{"type": "text", "text": "go"}]}),
            json!({"role": "assistant", "content": [
                {"type": "tool_use", "id": "toolu_a", "name": "Bash", "input": {"command": "a"}},
                {"type": "tool_use", "id": "toolu_b", "name": "Bash", "input": {"command": "b"}},
                {"type": "tool_use", "id": "toolu_c", "name": "Bash", "input": {"command": "c"}}
            ]}),
            json!({"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "toolu_a", "content": "a"},
                {"type": "tool_result", "tool_use_id": "toolu_b", "content": "b"},
                {"type": "tool_result", "tool_use_id": "toolu_c", "content": "c"}
            ]}),
        ];
        for extra in long_session(5) {
            msgs.push(extra);
        }
        let stats = apply(&mut msgs, &cfg(&["Bash"], 4)).unwrap();
        assert!(stats.stubbed >= 3);
        PairingIndex::build(&msgs).validate().unwrap();
    }

    /// A pre-existing orphan makes pre-validation fail → Err (caller rolls back).
    #[test]
    fn orphan_input_returns_err() {
        let mut msgs = vec![json!({"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": "toolu_ghost", "content": "x"}
        ]})];
        assert!(apply(&mut msgs, &cfg(&["Bash"], 4)).is_err());
    }

    /// History shorter than the window → no cutoff → nothing stubbed.
    #[test]
    fn short_history_keeps_everything() {
        let mut msgs = long_session(3);
        let stats = apply(&mut msgs, &cfg(&["Bash"], 4)).unwrap();
        assert_eq!(stats.stubbed, 0);
    }

    /// With realistic-size payloads, stubbing nets a byte reduction.
    #[test]
    fn padded_payloads_shrink() {
        let mut msgs = long_session(50);
        for msg in msgs.iter_mut() {
            if role(msg) != Some("user") {
                continue;
            }
            if let Some(content) = msg.get_mut("content").and_then(Value::as_array_mut) {
                for block in content {
                    if block.get("type").and_then(Value::as_str) == Some("tool_result") {
                        block["content"] = Value::String("x".repeat(200));
                    }
                }
            }
        }
        let stats = apply(&mut msgs, &cfg(&["Bash"], 4)).unwrap();
        assert!(stats.stubbed > 0);
        assert!(stats.elided_bytes() > 0);
    }
}
