//! `CrossTurnDedup` strategy — mirrors opencode-dcp's default "deduplication".
//!
//! When the same tool is called more than once with identical arguments
//! (e.g. reading the same file across turns), only the most recent result is
//! current truth; the earlier identical results are stale duplicates. This
//! stubs the **content** of every earlier identical `tool_result`, keeping the
//! most recent one intact.
//!
//! A second phase also **append-collapses** near-duplicates: a result whose
//! string content is a strict byte-*prefix* of a later same-tool result (a log
//! that grew, a re-run with more output appended) is fully contained in that
//! later one, so it's stubbed too — lossless, because prefix is transitive and
//! the longest result in each chain is always retained.
//!
//! A third phase collapses **cross-tool identical content**: tool_results whose
//! string content is byte-identical across *different* tools or inputs (the case
//! Phase 1 misses because it keys on `(name, input)`). For example, a Bash
//! `cat foo.txt` and a later `Read foo.txt` that both return the same bytes —
//! the earlier result is stale. For each group of results sharing identical
//! string content, every member except the latest (by message order) is stubbed.
//! Array-typed content is out of scope and left untouched. Results already
//! carrying a stub marker are skipped so Phase 3 is idempotent and never
//! double-counts Phase 1/2 stubs.
//!
//! Safe + deterministic: it only rewrites `tool_result.content` (never drops a
//! pair, never touches `tool_use.input`), and "most recent" is well-defined by
//! message order. Unlike `SlidingWindow`, it never removes a *unique* result —
//! only ones provably superseded by a later identical or extending call — so it
//! is safe to run on any tool (including `Read`) by default.

use std::collections::HashMap;

use serde_json::Value;

use crate::config::{CrossTurnDedupConfig, matches_any};
use crate::error::Result;
use crate::pairing::PairingIndex;
use crate::strategies::{Stats, block_mut, role};

/// Dedupe identical tool calls, stubbing all but the most recent result.
pub fn apply(messages: &mut [Value], cfg: &CrossTurnDedupConfig) -> Result<Stats> {
    super::with_stats(messages, |m| apply_counted(m, cfg))
}

/// Worker: performs the dedup and returns the number of results stubbed. Byte
/// accounting is threaded by the caller ([`super::run`]); see [`apply`].
pub(crate) fn apply_counted(messages: &mut [Value], cfg: &CrossTurnDedupConfig) -> Result<usize> {
    let idx = PairingIndex::build(messages);
    idx.validate()?;

    // Group tool_use ids by (tool name, canonical input), in chronological
    // order. serde_json serializes object keys sorted (no preserve_order), so
    // the canonical form is deterministic.
    let mut groups: HashMap<String, Vec<String>> = HashMap::new();
    for msg in messages.iter() {
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
            if matches_any(&cfg.exempt_tools, name) {
                continue;
            }
            let Some(id) = block.get("id").and_then(Value::as_str) else {
                continue;
            };
            // Skip lone tool_uses with no paired result — nothing to supersede.
            if !idx.results.contains_key(id) {
                continue;
            }
            let input = block.get("input").unwrap_or(&Value::Null);
            let key = format!(
                "{name}\u{0}{}",
                serde_json::to_string(input).unwrap_or_default()
            );
            groups.entry(key).or_default().push(id.to_owned());
        }
    }

    // For each group of identical calls, stub every result except the last.
    let mut stubbed = 0usize;
    for ids in groups.values() {
        if ids.len() < 2 {
            continue;
        }
        for id in &ids[..ids.len() - 1] {
            if let Some(res_loc) = idx.results.get(id).copied() {
                if let Some(block) = block_mut(messages, res_loc) {
                    let content = block.get("content").cloned().unwrap_or(Value::Null);
                    // Skip a result already carrying our marker, so re-applying
                    // is a no-op (the size-bearing marker isn't a fixed string).
                    // Guard on the configured stub *prefix* so this stays robust
                    // even if the user customises `stub` away from "[trimwire:".
                    if content
                        .as_str()
                        .is_some_and(|s| !cfg.stub.is_empty() && s.starts_with(cfg.stub.as_str()))
                    {
                        continue;
                    }
                    block["content"] = super::elision_marker(&cfg.stub, &content);
                    stubbed += 1;
                }
            }
        }
    }

    // ---- Phase 2: append-collapse near-duplicates ----
    // A result whose STRING content is a strict byte-prefix of a LATER same-tool
    // result is fully contained in that later one, so stubbing it is lossless
    // (think a log re-read that grew, or `cmd` then `cmd` with more output
    // appended). Safe even when intermediate results are also collapsed: prefix
    // is transitive and the maximal result in each chain is never superseded, so
    // it's retained and still contains every collapsed prefix. Decisions are
    // computed from a snapshot of current contents, in message order, so the
    // result is deterministic and independent of mutation/iteration order.
    //
    // Computed once and shared with Phase 3: neither phase mutates a `tool_use`
    // block or the pairing index (only `tool_result.content` changes), so the
    // ordered (id, name) list is identical for both — re-deriving it would be a
    // redundant full-message walk on the hot path.
    let ordered = ordered_paired_uses(messages, &idx, cfg);
    let contents: Vec<Option<String>> = ordered
        .iter()
        .map(|(id, _)| {
            let (mi, ci) = idx.results.get(id).copied()?;
            let s = messages
                .get(mi)?
                .get("content")?
                .as_array()?
                .get(ci)?
                .get("content")?
                .as_str()?;
            // Skip content already carrying our marker (e.g. stubbed in phase 1).
            if !cfg.stub.is_empty() && s.starts_with(cfg.stub.as_str()) {
                return None;
            }
            Some(s.to_owned())
        })
        .collect();

    let mut to_collapse: Vec<((usize, usize), String)> = Vec::new();
    for i in 0..ordered.len() {
        let Some(ci) = &contents[i] else {
            continue;
        };
        let name_i = &ordered[i].1;
        let superseded = ((i + 1)..ordered.len()).any(|j| {
            ordered[j].1 == *name_i
                && contents[j]
                    .as_deref()
                    .is_some_and(|cj| cj.len() > ci.len() && cj.starts_with(ci.as_str()))
        });
        if superseded {
            if let Some(loc) = idx.results.get(&ordered[i].0).copied() {
                to_collapse.push((loc, ci.clone()));
            }
        }
    }
    for (loc, content) in to_collapse {
        if let Some(block) = block_mut(messages, loc) {
            block["content"] = super::elision_marker(&cfg.stub, &Value::String(content));
            stubbed += 1;
        }
    }

    // ---- Phase 3: cross-tool identical-content dedup ----
    // Collapse tool_results whose STRING content is byte-identical across
    // DIFFERENT tools or inputs — the case Phase 1 misses (it keys on
    // (name,input)). Example: Bash `cat foo.txt` and a later Read `foo.txt`
    // yielding the same bytes. For each content-identical group with ≥2
    // members, every member except the latest (by message order) is stubbed.
    //
    // Decisions are computed from a SNAPSHOT of current string contents in
    // message order — taken after Phase 2 mutations — so results already
    // stubbed by earlier phases carry the marker, not the original content.
    // The stub-prefix guard ensures we skip those entries, making Phase 3
    // strictly additive and idempotent.
    //
    // Determinism: grouping keys are the owned String content values; which
    // members to stub is determined by message-index position (all but the
    // last in each group), collected into a Vec and applied in order —
    // independent of HashMap iteration order.
    {
        // Reuses `ordered` from Phase 2 (same list — see the note there).

        // Build a snapshot: (id, message_index_of_result, string_content).
        // We record the message index so "latest" is well-defined.
        struct Entry {
            msg_idx: usize,
            content: String,
            loc: (usize, usize),
        }
        let mut entries: Vec<Entry> = Vec::new();
        for (id, _name) in &ordered {
            let Some(loc) = idx.results.get(id).copied() else {
                continue;
            };
            let (mi, ci) = loc;
            let Some(block) = messages.get(mi).and_then(|m| {
                m.get("content")
                    .and_then(Value::as_array)
                    .and_then(|a| a.get(ci))
            }) else {
                continue;
            };
            let raw = block.get("content").unwrap_or(&Value::Null);
            // Only handle string content; skip arrays (out of scope).
            let Some(s) = raw.as_str() else {
                continue;
            };
            // Skip content already carrying our stub marker (Phase 1/2 output).
            if !cfg.stub.is_empty() && s.starts_with(cfg.stub.as_str()) {
                continue;
            }
            // Skip the CC-cleared marker too.
            if super::is_already_cleared(raw) {
                continue;
            }
            entries.push(Entry {
                msg_idx: mi,
                content: s.to_owned(),
                loc,
            });
        }

        // Group by content string, preserving insertion (message) order within
        // each bucket. We need: for each group, stub all but the max msg_idx.
        // Use a HashMap<content, Vec<index-into-entries>> keyed by content.
        let mut content_groups: HashMap<String, Vec<usize>> = HashMap::new();
        for (ei, entry) in entries.iter().enumerate() {
            content_groups
                .entry(entry.content.clone())
                .or_default()
                .push(ei);
        }

        // Collect (loc, original_content) for every entry that is NOT the
        // latest in its group. Collect into a Vec first for deterministic order.
        let mut to_stub_p3: Vec<((usize, usize), String)> = Vec::new();
        for indices in content_groups.values() {
            if indices.len() < 2 {
                continue;
            }
            // Find the index (into `entries`) with the maximum message index.
            // `indices` is non-empty here (len >= 2 checked above) so this is
            // always `Some`; the let-else keeps the prune path panic-free (and
            // thus fail-open) even if a future refactor broke that invariant.
            let Some(&latest_ei) = indices.iter().max_by_key(|&&ei| entries[ei].msg_idx) else {
                continue;
            };
            for &ei in indices {
                if ei == latest_ei {
                    continue;
                }
                to_stub_p3.push((entries[ei].loc, entries[ei].content.clone()));
            }
        }
        // Sort by location for deterministic application order.
        to_stub_p3.sort_by_key(|(loc, _)| *loc);

        for (loc, content) in to_stub_p3 {
            if let Some(block) = block_mut(messages, loc) {
                let value = Value::String(content);
                let marker = super::elision_marker(&cfg.stub, &value);
                // Only stub when it actually SHRINKS the body. For tiny identical
                // content (e.g. "", "OK") the size-bearing marker is larger than
                // the original, so stubbing would both grow the request and churn
                // the prompt-cache prefix for negative benefit — skip those.
                let len = |v: &Value| serde_json::to_string(v).map_or(usize::MAX, |s| s.len());
                if len(&marker) < len(&value) {
                    block["content"] = marker;
                    stubbed += 1;
                }
            }
        }
    }

    Ok(stubbed)
}

/// Non-exempt assistant `tool_use`s that have a paired result, as `(id, name)`
/// in message order. Shared by the append-collapse phase.
fn ordered_paired_uses(
    messages: &[Value],
    idx: &PairingIndex,
    cfg: &CrossTurnDedupConfig,
) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for msg in messages.iter() {
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
            if matches_any(&cfg.exempt_tools, name) {
                continue;
            }
            let Some(id) = block.get("id").and_then(Value::as_str) else {
                continue;
            };
            if !idx.results.contains_key(id) {
                continue;
            }
            out.push((id.to_owned(), name.to_owned()));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn cfg(exempt: &[&str]) -> CrossTurnDedupConfig {
        CrossTurnDedupConfig {
            enabled: true,
            exempt_tools: exempt.iter().map(|s| (*s).to_owned()).collect(),
            stub: "[trimwire: superseded by a later identical call]".to_owned(),
        }
    }

    /// Three reads of the same file → the two older results are stubbed, the
    /// most recent is kept.
    fn read_thrice() -> Vec<Value> {
        let mut msgs = Vec::new();
        for i in 0..3 {
            let uid = format!("toolu_r{i}");
            msgs.push(json!({"role": "user", "content": [{"type": "text", "text": "read it"}]}));
            msgs.push(json!({"role": "assistant", "content": [
                {"type": "tool_use", "id": uid, "name": "Read", "input": {"path": "/a.txt"}}
            ]}));
            msgs.push(json!({"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": uid, "content": format!("contents v{i}")}
            ]}));
        }
        msgs
    }

    #[test]
    fn keeps_most_recent_identical_result() {
        let mut msgs = read_thrice();
        let stats = apply(&mut msgs, &cfg(&[])).unwrap();
        assert_eq!(stats.stubbed, 2, "two older identical reads superseded");
        // Older two stubbed (marker carries the elided size); newest kept verbatim.
        let prefix = "[trimwire: superseded by a later identical call]";
        let marked = |m: &Value| {
            m["content"][0]["content"]
                .as_str()
                .unwrap()
                .starts_with(prefix)
        };
        assert!(marked(&msgs[2]) && marked(&msgs[5]));
        assert_eq!(msgs[8]["content"][0]["content"], json!("contents v2"));
        PairingIndex::build(&msgs).validate().unwrap();
    }

    #[test]
    fn distinct_inputs_are_not_deduped() {
        let mut msgs = vec![
            json!({"role": "assistant", "content": [
                {"type": "tool_use", "id": "u1", "name": "Read", "input": {"path": "/a"}}
            ]}),
            json!({"role": "user", "content": [{"type": "tool_result", "tool_use_id": "u1", "content": "A"}]}),
            json!({"role": "assistant", "content": [
                {"type": "tool_use", "id": "u2", "name": "Read", "input": {"path": "/b"}}
            ]}),
            json!({"role": "user", "content": [{"type": "tool_result", "tool_use_id": "u2", "content": "B"}]}),
        ];
        let stats = apply(&mut msgs, &cfg(&[])).unwrap();
        assert_eq!(stats.stubbed, 0, "different paths are not duplicates");
    }

    #[test]
    fn exempt_tool_is_skipped() {
        let mut msgs = read_thrice();
        let stats = apply(&mut msgs, &cfg(&["Read"])).unwrap();
        assert_eq!(stats.stubbed, 0);
    }

    /// Subagent results survive cross_turn_dedup under the SHIPPED DEFAULT PROFILE.
    /// (The struct default now carries the same Task/Agent exemption too — see
    /// `struct_default_exempts_task_and_agent_matching_profile`.) The negative arm
    /// uses an EXPLICIT empty exempt list (`cfg(&[])`) to prove the fixture is real
    /// duplicates that would otherwise be deduped — i.e. the test is non-vacuous.
    #[test]
    fn default_profile_exempts_task_and_agent_results_from_dedup() {
        let prof = crate::config::profile_baseline("default")
            .strategies
            .cross_turn_dedup;
        assert!(
            prof.exempt_tools.iter().any(|t| t == "Task")
                && prof.exempt_tools.iter().any(|t| t == "Agent"),
            "default profile must exempt Task+Agent in cross_turn_dedup; got {:?}",
            prof.exempt_tools
        );

        let triple = |name: &str| {
            let mut msgs = Vec::new();
            for i in 0..3 {
                let uid = format!("toolu_{name}{i}");
                msgs.push(json!({"role": "assistant", "content": [
                    {"type": "tool_use", "id": uid, "name": name, "input": {"prompt": "same"}}
                ]}));
                msgs.push(json!({"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": uid, "content": "identical finding"}
                ]}));
            }
            msgs
        };

        for name in ["Task", "Agent"] {
            // Shipped default profile → exempt → all three survive.
            let mut kept = triple(name);
            let s = apply(&mut kept, &prof).unwrap();
            assert_eq!(s.stubbed, 0, "{name} results exempt under default profile");
            // Without the exemption (an EXPLICIT empty exempt list) → the two older
            // identical calls ARE deduped — proves the fixture is real duplicates.
            let mut deduped = triple(name);
            let s2 = apply(&mut deduped, &cfg(&[])).unwrap();
            assert_eq!(
                s2.stubbed, 2,
                "{name} results are genuine duplicates (deduped without the exemption)"
            );
        }
    }

    /// Consistency hygiene: `CrossTurnDedupConfig::default()` now exempts the
    /// subagent tools (Task/Agent) directly — matching the default profile and the
    /// sibling strategies' struct defaults — so a direct struct-default caller
    /// doesn't silently dedup subagent results. Ordinary tools still dedup.
    #[test]
    fn struct_default_exempts_task_and_agent_matching_profile() {
        // The struct default's exempt list contains both subagent names, and it
        // matches the default profile for those names.
        let d = CrossTurnDedupConfig::default();
        assert!(
            d.exempt_tools.iter().any(|t| t == "Task")
                && d.exempt_tools.iter().any(|t| t == "Agent"),
            "struct default must exempt Task+Agent; got {:?}",
            d.exempt_tools
        );
        let prof = crate::config::profile_baseline("default")
            .strategies
            .cross_turn_dedup;
        for t in ["Task", "Agent"] {
            assert_eq!(
                d.exempt_tools.contains(&t.to_owned()),
                prof.exempt_tools.contains(&t.to_owned()),
                "struct default and default profile agree on exempting {t}"
            );
        }

        let triple = |name: &str| {
            let mut msgs = Vec::new();
            for i in 0..3 {
                let uid = format!("toolu_{name}{i}");
                msgs.push(json!({"role": "assistant", "content": [
                    {"type": "tool_use", "id": uid, "name": name, "input": {"prompt": "same"}}
                ]}));
                msgs.push(json!({"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": uid, "content": "identical finding"}
                ]}));
            }
            msgs
        };
        // Use the struct default's exempt list, with the strategy enabled for the test.
        let enabled_default = CrossTurnDedupConfig {
            enabled: true,
            ..CrossTurnDedupConfig::default()
        };
        for name in ["Task", "Agent"] {
            let mut kept = triple(name);
            let s = apply(&mut kept, &enabled_default).unwrap();
            assert_eq!(
                s.stubbed, 0,
                "{name} results exempt under the struct default"
            );
        }
        // Control: an ordinary (non-subagent) tool is NOT exempt → still deduped.
        let mut ordinary = triple("Read");
        let s = apply(&mut ordinary, &enabled_default).unwrap();
        assert_eq!(s.stubbed, 2, "ordinary duplicate results still dedup");
    }

    #[test]
    fn append_collapse_supersedes_prefix_results() {
        // Same tool, DIFFERENT inputs (so exact-dedup won't group them), but each
        // result is a strict byte-prefix of the next — a growing log. The earlier
        // two are fully contained in the last → collapsed; the last is kept.
        let mut msgs = vec![
            json!({"role":"assistant","content":[{"type":"tool_use","id":"u1","name":"Bash","input":{"c":"r1"}}]}),
            json!({"role":"user","content":[{"type":"tool_result","tool_use_id":"u1","content":"line1\n"}]}),
            json!({"role":"assistant","content":[{"type":"tool_use","id":"u2","name":"Bash","input":{"c":"r2"}}]}),
            json!({"role":"user","content":[{"type":"tool_result","tool_use_id":"u2","content":"line1\nline2\n"}]}),
            json!({"role":"assistant","content":[{"type":"tool_use","id":"u3","name":"Bash","input":{"c":"r3"}}]}),
            json!({"role":"user","content":[{"type":"tool_result","tool_use_id":"u3","content":"line1\nline2\nline3\n"}]}),
        ];
        let stats = apply(&mut msgs, &cfg(&[])).unwrap();
        assert_eq!(
            stats.stubbed, 2,
            "the two prefix results collapse into the last"
        );
        let prefix = "[trimwire: superseded by a later identical call]";
        let marked = |m: &Value| {
            m["content"][0]["content"]
                .as_str()
                .unwrap()
                .starts_with(prefix)
        };
        assert!(marked(&msgs[1]) && marked(&msgs[3]));
        assert_eq!(
            msgs[5]["content"][0]["content"],
            json!("line1\nline2\nline3\n")
        );
        PairingIndex::build(&msgs).validate().unwrap();
    }

    #[test]
    fn append_collapse_leaves_non_prefix_results_alone() {
        // Same tool, but outputs that are NOT prefixes of each other → both kept.
        let mut msgs = vec![
            json!({"role":"assistant","content":[{"type":"tool_use","id":"u1","name":"Bash","input":{"c":"a"}}]}),
            json!({"role":"user","content":[{"type":"tool_result","tool_use_id":"u1","content":"apple"}]}),
            json!({"role":"assistant","content":[{"type":"tool_use","id":"u2","name":"Bash","input":{"c":"b"}}]}),
            json!({"role":"user","content":[{"type":"tool_result","tool_use_id":"u2","content":"banana"}]}),
        ];
        let stats = apply(&mut msgs, &cfg(&[])).unwrap();
        assert_eq!(stats.stubbed, 0, "non-prefix outputs are not collapsed");
    }

    #[test]
    fn deterministic_across_runs() {
        let mut a = read_thrice();
        let mut b = read_thrice();
        apply(&mut a, &cfg(&[])).unwrap();
        apply(&mut b, &cfg(&[])).unwrap();
        assert_eq!(
            serde_json::to_string(&a).unwrap(),
            serde_json::to_string(&b).unwrap()
        );
    }

    /// DELIBERATE behavior: when the same input yields *different* output over
    /// time (read → edit → read), dedup supersedes the EARLIER (now-stale)
    /// result and keeps the latest (the current truth). The model loses the
    /// historical before-state — an accepted trade-off (mirrors opencode-dcp).
    #[test]
    fn same_input_different_output_supersedes_the_stale_earlier_one() {
        let mut msgs = vec![
            json!({"role": "assistant", "content": [
                {"type": "tool_use", "id": "u1", "name": "Read", "input": {"path": "/f"}}
            ]}),
            json!({"role": "user", "content": [{"type": "tool_result", "tool_use_id": "u1", "content": "BEFORE edit"}]}),
            json!({"role": "assistant", "content": [
                {"type": "tool_use", "id": "u2", "name": "Read", "input": {"path": "/f"}}
            ]}),
            json!({"role": "user", "content": [{"type": "tool_result", "tool_use_id": "u2", "content": "AFTER edit"}]}),
        ];
        let stats = apply(&mut msgs, &cfg(&[])).unwrap();
        assert_eq!(stats.stubbed, 1);
        // Earlier (stale) read superseded; latest (current truth) kept verbatim.
        assert!(
            msgs[1]["content"][0]["content"]
                .as_str()
                .unwrap()
                .starts_with("[trimwire: superseded by a later identical call]")
        );
        assert_eq!(msgs[3]["content"][0]["content"], json!("AFTER edit"));
    }

    /// Input key order must not matter (canonical sorted-key comparison).
    #[test]
    fn input_key_order_does_not_defeat_dedup() {
        let mut msgs = vec![
            json!({"role": "assistant", "content": [
                {"type": "tool_use", "id": "u1", "name": "Bash", "input": {"a": 1, "b": 2}}
            ]}),
            json!({"role": "user", "content": [{"type": "tool_result", "tool_use_id": "u1", "content": "x"}]}),
            json!({"role": "assistant", "content": [
                {"type": "tool_use", "id": "u2", "name": "Bash", "input": {"b": 2, "a": 1}}
            ]}),
            json!({"role": "user", "content": [{"type": "tool_result", "tool_use_id": "u2", "content": "x"}]}),
        ];
        let stats = apply(&mut msgs, &cfg(&[])).unwrap();
        assert_eq!(
            stats.stubbed, 1,
            "same args in different key order are duplicates"
        );
    }

    // ---- Phase 3 tests ----

    /// A Bash result and a later Read result with byte-identical content
    /// (different tool, different input) → earlier one is stubbed, latest kept.
    #[test]
    fn cross_tool_identical_content_collapses_keeping_latest() {
        // Long enough that the size-bearing marker is smaller than the content
        // (realistic for cat/Read output) so the shrink-guard lets it collapse.
        let shared = "a fairly long line of identical file content that exceeds \
                      the stub marker length so the shrink guard lets it collapse";
        let mut msgs = vec![
            json!({"role": "assistant", "content": [
                {"type": "tool_use", "id": "u1", "name": "Bash", "input": {"command": "cat foo.txt"}}
            ]}),
            json!({"role": "user", "content": [{"type": "tool_result", "tool_use_id": "u1", "content": shared}]}),
            json!({"role": "assistant", "content": [
                {"type": "tool_use", "id": "u2", "name": "Read", "input": {"path": "foo.txt"}}
            ]}),
            json!({"role": "user", "content": [{"type": "tool_result", "tool_use_id": "u2", "content": shared}]}),
        ];
        let stats = apply(&mut msgs, &cfg(&[])).unwrap();
        assert_eq!(stats.stubbed, 1, "earlier cross-tool duplicate stubbed");
        // Earlier (Bash) result is stubbed.
        let prefix = "[trimwire: superseded by a later identical call]";
        assert!(
            msgs[1]["content"][0]["content"]
                .as_str()
                .unwrap()
                .starts_with(prefix),
            "earlier Bash result should be stubbed"
        );
        // Later (Read) result is kept verbatim.
        assert_eq!(
            msgs[3]["content"][0]["content"],
            json!(shared),
            "latest Read result must be kept verbatim"
        );
        PairingIndex::build(&msgs).validate().unwrap();
    }

    /// Plant a unique token in identical content across an early Bash and a
    /// late Read. After apply the token must still be present in the kept
    /// (latest) copy — the harm guarantee: dedup never loses the bytes.
    #[test]
    fn needle_in_kept_cross_tool_copy_survives() {
        let needle = "NEEDLE_XTOOL_a1b2";
        let shared = format!(
            "a fairly long line of file content containing the {needle} somewhere \
             in the middle, plus padding so it beats the marker-length shrink guard"
        );
        let mut msgs = vec![
            json!({"role": "assistant", "content": [
                {"type": "tool_use", "id": "u1", "name": "Bash", "input": {"command": "cat secret"}}
            ]}),
            json!({"role": "user", "content": [{"type": "tool_result", "tool_use_id": "u1", "content": shared}]}),
            json!({"role": "assistant", "content": [
                {"type": "tool_use", "id": "u2", "name": "Read", "input": {"path": "secret"}}
            ]}),
            json!({"role": "user", "content": [{"type": "tool_result", "tool_use_id": "u2", "content": shared}]}),
        ];
        apply(&mut msgs, &cfg(&[])).unwrap();
        // The latest copy (Read, msgs[3]) must still contain the needle.
        let kept = msgs[3]["content"][0]["content"].as_str().unwrap();
        assert!(
            kept.contains(needle),
            "needle must survive in the kept latest copy; got: {kept}"
        );
        // The earlier copy (Bash, msgs[1]) should be stubbed (not contain needle).
        let stubbed_val = msgs[1]["content"][0]["content"].as_str().unwrap();
        assert!(
            !stubbed_val.contains(needle),
            "needle must NOT appear in the stubbed earlier copy; got: {stubbed_val}"
        );
    }

    /// Different content across tools → no Phase 3 stubs.
    #[test]
    fn cross_tool_distinct_content_not_collapsed() {
        let mut msgs = vec![
            json!({"role": "assistant", "content": [
                {"type": "tool_use", "id": "u1", "name": "Bash", "input": {"command": "echo a"}}
            ]}),
            json!({"role": "user", "content": [{"type": "tool_result", "tool_use_id": "u1", "content": "output A"}]}),
            json!({"role": "assistant", "content": [
                {"type": "tool_use", "id": "u2", "name": "Read", "input": {"path": "/b"}}
            ]}),
            json!({"role": "user", "content": [{"type": "tool_result", "tool_use_id": "u2", "content": "output B"}]}),
        ];
        let stats = apply(&mut msgs, &cfg(&[])).unwrap();
        assert_eq!(stats.stubbed, 0, "distinct content must not be collapsed");
    }

    /// Running apply twice stubs the same count the first time and 0 net new the
    /// second (contents are now markers → Phase 3 skip guard applies).
    #[test]
    fn cross_tool_dedup_is_idempotent() {
        let shared = "same long output produced by two different tools, long enough \
                      to clear the shrink-guard so the earlier copy is collapsed";
        let mut msgs = vec![
            json!({"role": "assistant", "content": [
                {"type": "tool_use", "id": "u1", "name": "Bash", "input": {"command": "cat x"}}
            ]}),
            json!({"role": "user", "content": [{"type": "tool_result", "tool_use_id": "u1", "content": shared}]}),
            json!({"role": "assistant", "content": [
                {"type": "tool_use", "id": "u2", "name": "Read", "input": {"path": "x"}}
            ]}),
            json!({"role": "user", "content": [{"type": "tool_result", "tool_use_id": "u2", "content": shared}]}),
        ];
        let first = apply(&mut msgs, &cfg(&[])).unwrap();
        assert_eq!(first.stubbed, 1, "first run stubs the earlier duplicate");
        let second = apply(&mut msgs, &cfg(&[])).unwrap();
        assert_eq!(second.stubbed, 0, "second run is a no-op (idempotent)");
    }

    /// Tiny identical content across tools must NOT be stubbed: the size-bearing
    /// marker is larger than the content, so collapsing it would grow the body
    /// and churn the cache for negative benefit. The shrink-guard skips it.
    #[test]
    fn tiny_cross_tool_content_is_not_stubbed() {
        let shared = "OK"; // 2 bytes — far smaller than the stub marker
        let mut msgs = vec![
            json!({"role": "assistant", "content": [
                {"type": "tool_use", "id": "u1", "name": "Bash", "input": {"command": "true"}}
            ]}),
            json!({"role": "user", "content": [{"type": "tool_result", "tool_use_id": "u1", "content": shared}]}),
            json!({"role": "assistant", "content": [
                {"type": "tool_use", "id": "u2", "name": "Status", "input": {"q": "x"}}
            ]}),
            json!({"role": "user", "content": [{"type": "tool_result", "tool_use_id": "u2", "content": shared}]}),
        ];
        let stats = apply(&mut msgs, &cfg(&[])).unwrap();
        assert_eq!(
            stats.stubbed, 0,
            "tiny identical content must not be stubbed (would grow the body)"
        );
        // Both copies left verbatim.
        assert_eq!(msgs[1]["content"][0]["content"], json!("OK"));
        assert_eq!(msgs[3]["content"][0]["content"], json!("OK"));
    }

    #[test]
    fn serde_json_sorts_object_keys() {
        // The dedup canonical key and the ledger's prefix_hash both rely on
        // serde_json emitting object keys in sorted (BTreeMap) order. If a
        // transitive dep ever enabled `serde_json/preserve_order`, that flips
        // to insertion order and silently breaks dedup + the §9 stability hash.
        // This pins the assumption so such a regression fails loudly here.
        assert_eq!(
            serde_json::to_string(&json!({"b": 1, "a": 2})).unwrap(),
            r#"{"a":2,"b":1}"#,
        );
    }

    #[test]
    fn already_stubbed_result_is_not_recounted() {
        // Two identical calls where the earlier result already equals the stub:
        // dedup must not count it as a (re-)mutation.
        let c = cfg(&[]);
        let stub = c.stub.clone();
        let mut msgs = vec![
            json!({"role": "assistant", "content": [
                {"type": "tool_use", "id": "u1", "name": "Bash", "input": {"c": "x"}}
            ]}),
            json!({"role": "user", "content": [{"type": "tool_result", "tool_use_id": "u1", "content": stub}]}),
            json!({"role": "assistant", "content": [
                {"type": "tool_use", "id": "u2", "name": "Bash", "input": {"c": "x"}}
            ]}),
            json!({"role": "user", "content": [{"type": "tool_result", "tool_use_id": "u2", "content": "real"}]}),
        ];
        let stats = apply(&mut msgs, &c).unwrap();
        assert_eq!(
            stats.stubbed, 0,
            "earlier result already == stub → no change counted"
        );
    }
}
