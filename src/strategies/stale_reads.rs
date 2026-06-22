//! `StaleReads` strategy — elide superseded file-Read results.
//!
//! A `tool_result` for a file Read at turn T is definitionally stale if any
//! later turn (> T) touches the same path: either another Read of the same
//! path (which gives the model a fresher view), or a Write/Edit/MultiEdit of
//! that path (which changes the on-disk content). In both cases the earlier
//! Read result carries dead weight: the model's fresh view of the file is
//! already in the latest Read/Write result, making every preceding Read of that
//! path redundant.
//!
//! **Supersession rule (per path P):**
//! A Read result for P at message index M is STALE iff there exists, at some
//! message index M' > M, a `tool_use` targeting P whose tool name is one of
//! `Read`, `Write`, `Edit`, or `MultiEdit`. The LAST operation on each path
//! is never elided. Bash and all other tools are deliberately excluded (v1):
//! no command-string parsing, zero false positives.
//!
//! **Two behaviors (all content OVERWRITES — never add/remove blocks):**
//!   1. Elide superseded Read **results** (above).
//!   2. DEMAND-PAGE the last (current-view) Read of a path when `page_min_bytes`>0
//!      and it's old (> `keep_recent_turns`) + larger than that threshold → a
//!      recoverable marker naming the path ("re-read to restore"); the model
//!      self-heals (CC returns fresh content). Read results only; never inputs/errors.
//!      **Hot-path guard (§13B):** a path the model has Read more than once is never
//!      paged — paging the current view of a file it keeps needing forces yet
//!      another re-read, which trimwire would page out again → a read-spiral
//!      (observed live: "stuck re-reading the same file repeatedly").
//!
//! **Authored content (`Write`/`Edit`/`MultiEdit` `content`/`new_string`/
//! `old_string`) is NEVER elided** — only Read *results* are. An earlier version of
//! this strategy also collapsed "superseded" authored inputs (a write a later op on
//! the same path made non-authoritative). That corrupted real sessions (§13A): in
//! the ubiquitous create→read→edit flow, behavior #1 elides the Read result AND the
//! collapse removed the Write body, leaving the model with no faithful copy of the
//! file it was editing → it reproduced the elision marker as the file body (a 9.5KB
//! Write landed on disk as `[trimwire: 9558B input elided]`). The "later write
//! supersedes it" reasoning was wrong: a later *Read* also triggered it without
//! changing the file, and even a later Write doesn't stop the model re-authoring
//! from the earlier version. Authored content is load-bearing; we keep it. Write
//! inputs still age out via the keep-recent window of the other levers, not here.
//!
//! **Shape-preserving:** overwrites `tool_result.content` / `tool_use.input` only;
//! never adds/removes/reorders blocks → orphan-safe and reprune-compatible (reprune
//! replays content + input overwrites).
//!
//! Deterministic: decisions are made from a message-order snapshot, mutations
//! applied in sorted location order.
//!
//! Idempotent: results whose content already starts with `"[trimwire: "` (or
//! the CC `"[Old tool result content cleared]"` marker) are skipped.
//!
//! Shrink guard: a result is only elided when the marker is strictly smaller
//! than the current content (mirrors bloat_cap's "only if it shrinks" guard).
//!
//! **ON in the `default` profile** (cache-safe); **off in `gentle`**. Override
//! or disable explicitly:
//!
//! ```toml
//! [strategies.stale_reads]
//! enabled = true
//! exempt_tools = []
//! ```
//!
//! **Known v1 limitation:** Bash `cat`/`head`/`tail` etc. are not tracked as
//! file-path operations (would require command-string parsing, which introduces
//! false positives). A future version may add optional Bash-path heuristics.

use serde_json::Value;

use crate::config::{StaleReadsConfig, matches_any};
use crate::error::Result;
use crate::pairing::PairingIndex;
use crate::strategies::{
    Stats, assistant_cutoff, block_mut, elision_marker, is_already_cleared, role,
};

/// The set of tool names whose `input` carries a clean, structured path field.
/// Only these are indexed for supersession tracking. Case-sensitive (CC names).
const PATH_TOOLS: &[&str] = &["Read", "Write", "Edit", "MultiEdit"];

/// Extract the file path from a `tool_use` input object, if the tool is one of
/// the tracked path-carrying tools. Returns `None` for any other tool.
fn extract_path<'a>(name: &str, input: &'a Value) -> Option<&'a str> {
    if !PATH_TOOLS.contains(&name) {
        return None;
    }
    // Primary field: file_path (Write, Edit, MultiEdit); fallback: path (Read).
    input
        .get("file_path")
        .or_else(|| input.get("path"))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
}

/// Elide the `tool_result` content of stale file Read results.
///
/// A Read result for path P is stale if, later in the message list, the same
/// path P is accessed by any of Read/Write/Edit/MultiEdit. The *last* operation
/// on each path is never elided.
pub fn apply(messages: &mut [Value], cfg: &StaleReadsConfig) -> Result<Stats> {
    super::with_stats(messages, |m| apply_counted(m, cfg))
}

/// Worker: returns the number of reads elided/paged. Byte accounting is threaded
/// by the caller ([`super::run`]); see [`apply`].
pub(crate) fn apply_counted(messages: &mut [Value], cfg: &StaleReadsConfig) -> Result<usize> {
    let idx = PairingIndex::build(messages);
    idx.validate()?;

    // --- Read-only pass 1: build per-path operation log ---
    //
    // For each path P, record (in message order) every tool_use that targets P.
    // Each entry is (message_idx, tool_use_id, tool_name).
    //
    // We work message-by-message so the order is stable (deterministic).
    // Per path: (message_idx, tool_use_id, tool_name) in message order. We only
    // ever elide Read *results* (looked up by id) and never touch tool inputs, so
    // the content index is not needed.
    let mut path_ops: std::collections::HashMap<String, Vec<(usize, String, String)>> =
        std::collections::HashMap::new();

    for (mi, msg) in messages.iter().enumerate() {
        if role(msg) != Some("assistant") {
            continue;
        }
        let Some(content) = msg.get("content").and_then(Value::as_array) else {
            continue;
        };
        for block in content.iter() {
            if block.get("type").and_then(Value::as_str) != Some("tool_use") {
                continue;
            }
            let Some(name) = block.get("name").and_then(Value::as_str) else {
                continue;
            };
            // Skip exempt tools.
            if matches_any(&cfg.exempt_tools, name) {
                continue;
            }
            let Some(id) = block.get("id").and_then(Value::as_str) else {
                continue;
            };
            let input = block.get("input").unwrap_or(&Value::Null);
            let Some(path) = extract_path(name, input) else {
                continue;
            };
            // B-9: a protected path is never tracked, so it can be neither
            // superseded-elided (pass 2) nor demand-paged (both read `path_ops`).
            if matches_any(&cfg.protected_file_patterns, path) {
                continue;
            }
            path_ops
                .entry(path.to_owned())
                .or_default()
                .push((mi, id.to_owned(), name.to_owned()));
        }
    }

    // --- Read-only pass 2: collect stale Read result locations ---
    //
    // For each path, the last entry in path_ops[path] is the most-recent
    // operation → never elided. All earlier entries that are Read calls are
    // candidates for elision (their results are stale).
    //
    // We collect (result_location, current_content_value) for each stale Read
    // so we can apply the shrink-guard and idempotency check in one place.
    let mut to_elide: Vec<(usize, usize)> = Vec::new();

    for ops in path_ops.values() {
        // ops is in message order (ascending mi) because we iterated messages
        // in order above and pushed into the Vec in that order.
        let Some((_last_mi, _last_id, _last_name)) = ops.last() else {
            continue;
        };
        // All entries except the last (the current state) may be stale.
        let stale_candidates = &ops[..ops.len() - 1];
        for (_mi, id, name) in stale_candidates {
            // Only superseded READ *results* are elided. Superseded Write/Edit/
            // MultiEdit *inputs* are deliberately NOT collapsed: authored content
            // is the model's only faithful copy of what it wrote and is
            // load-bearing for re-authoring — eliding it corrupts sessions (§13A).
            if name == "Read" {
                if let Some(&res_loc) = idx.results.get(id.as_str()) {
                    to_elide.push(res_loc);
                }
            }
        }
    }

    // Sort by location (message_idx, content_idx) for deterministic application
    // order, independent of HashMap iteration order.
    to_elide.sort_unstable();
    to_elide.dedup(); // guard against a tool_use_id appearing twice (shouldn't happen, but safe)

    // --- Mutation pass: apply elisions ---
    let mut stubbed = 0usize;
    for loc in to_elide {
        // Fetch the result block ONCE (shared) and read content + is_error from
        // it together; the mutation below re-borrows mutably (the required
        // shared-then-mutable pattern, not a redundant lookup).
        let (mi, ci) = loc;
        let Some(block) = messages
            .get(mi)
            .and_then(|m| m.get("content"))
            .and_then(Value::as_array)
            .and_then(|a| a.get(ci))
        else {
            continue;
        };
        let Some(current_content) = block.get("content").cloned() else {
            continue;
        };
        // Skip is_error results: a failed Read isn't an authoritative view and
        // its content is an error message, not file content.
        if block
            .get("is_error")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            continue;
        }
        // Idempotency: skip if already carrying any trimwire marker or CC's
        // own "[Old tool result content cleared]" marker.
        if current_content
            .as_str()
            .is_some_and(|s| s.starts_with("[trimwire: "))
            || is_already_cleared(&current_content)
        {
            continue;
        }
        // Build the marker and apply the shrink guard (same pattern as bloat_cap
        // Phase 2 / cross_turn_dedup Phase 3): only elide when the marker is
        // strictly smaller than the current content. Tiny Read results (e.g.
        // "not found") must not be grown by the stub.
        let marker = elision_marker(&cfg.stub, &current_content);
        let marker_len = serde_json::to_string(&marker)
            .map(|s| s.len())
            .unwrap_or(usize::MAX);
        let content_len = serde_json::to_string(&current_content)
            .map(|s| s.len())
            .unwrap_or(0);
        if marker_len >= content_len {
            // Marker would not shrink the body — skip (true no-op).
            continue;
        }
        // Apply the mutation.
        if let Some(block) = block_mut(messages, loc) {
            block["content"] = marker;
            stubbed += 1;
        }
    }

    // Demand-paging (opt-in, page_min_bytes > 0): page out the LAST (current-view)
    // Read of each path when it's OLD (<= keep_recent cutoff) and LARGE. The model
    // self-heals — the marker names the path so it can re-read (CC returns FRESH
    // content). Addressable Read content only; never inputs, never errors.
    if cfg.page_min_bytes > 0 {
        if let Some(cutoff) = assistant_cutoff(messages, cfg.keep_recent_turns.max(1)) {
            // Deterministic order: collect (location, path) then sort by location.
            let mut to_page: Vec<((usize, usize), String)> = Vec::new();
            for (path, ops) in &path_ops {
                let Some((mi, id, name)) = ops.last() else {
                    continue;
                };
                if name != "Read" || *mi > cutoff {
                    continue; // not a current-view read, or too recent to page
                }
                // Hot-path guard (§13B): never page a path the model has Read more
                // than once. A re-read proves the model keeps needing the file;
                // paging its current view forces yet another re-read, which we'd
                // page out again → the read-spiral observed live. The guard
                // terminates the spiral after at most one forced re-read (the
                // re-read makes read_count == 2). Page only genuinely one-shot huge
                // reads the model hasn't returned to.
                let read_count = ops.iter().filter(|(_, _, n)| n == "Read").count();
                if read_count > 1 {
                    continue;
                }
                if let Some(&res_loc) = idx.results.get(id.as_str()) {
                    to_page.push((res_loc, path.clone()));
                }
            }
            to_page.sort_unstable();
            for (loc, path) in to_page {
                let (mi, ci) = loc;
                // Fetch the result block once; read content + is_error together.
                let Some(block) = messages
                    .get(mi)
                    .and_then(|m| m.get("content"))
                    .and_then(Value::as_array)
                    .and_then(|a| a.get(ci))
                else {
                    continue;
                };
                let Some(content) = block.get("content").cloned() else {
                    continue;
                };
                if block
                    .get("is_error")
                    .and_then(Value::as_bool)
                    .unwrap_or(false)
                {
                    continue;
                }
                if content
                    .as_str()
                    .is_some_and(|s| s.starts_with("[trimwire: "))
                    || is_already_cleared(&content)
                {
                    continue;
                }
                let content_len = serde_json::to_string(&content)
                    .map(|s| s.len())
                    .unwrap_or(0);
                if content_len <= cfg.page_min_bytes {
                    continue;
                }
                // Report the RAW content byte count in the marker, not the serialized
                // length (which adds the 2 JSON-string quote bytes). The gate above
                // intentionally compares the serialized length; the human-/model-facing
                // marker should state the actual file-content size (audit P3-3).
                let raw_len = content.as_str().map(str::len).unwrap_or(content_len);
                let marker = Value::String(format!(
                    "[trimwire: paged out — Read {path} ({raw_len} bytes); re-read the file to restore]"
                ));
                let marker_len = serde_json::to_string(&marker)
                    .map(|s| s.len())
                    .unwrap_or(usize::MAX);
                if marker_len >= content_len {
                    continue;
                }
                if let Some(block) = block_mut(messages, loc) {
                    block["content"] = marker;
                    stubbed += 1;
                }
            }
        }
    }

    Ok(stubbed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn cfg(exempt: &[&str]) -> StaleReadsConfig {
        StaleReadsConfig {
            enabled: true,
            exempt_tools: exempt.iter().map(|s| (*s).to_owned()).collect(),
            stub: "[trimwire: stale read — file changed or re-read later]".to_owned(),
            page_min_bytes: 0,
            keep_recent_turns: 4,
            protected_file_patterns: Vec::new(),
        }
    }
    /// Config with demand-paging on (page reads older than keep_recent + larger
    /// than `min`).
    fn cfg_paging(min: usize, keep: usize) -> StaleReadsConfig {
        StaleReadsConfig {
            enabled: true,
            exempt_tools: Vec::new(),
            stub: "[trimwire: stale read — file changed or re-read later]".to_owned(),
            page_min_bytes: min,
            keep_recent_turns: keep,
            protected_file_patterns: Vec::new(),
        }
    }

    /// Build a minimal assistant turn with a tool_use block.
    fn tool_use_msg(id: &str, name: &str, input: Value) -> Value {
        json!({"role": "assistant", "content": [
            {"type": "tool_use", "id": id, "name": name, "input": input}
        ]})
    }

    /// Build a user turn with a successful tool_result.
    fn tool_result_msg(id: &str, content: &str) -> Value {
        json!({"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": id, "content": content}
        ]})
    }

    /// Build a user turn with an errored tool_result.
    fn error_result_msg(id: &str, content: &str) -> Value {
        json!({"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": id, "is_error": true, "content": content}
        ]})
    }

    /// Enough padding to ensure the stub marker is smaller than the content.
    fn big(s: &str) -> String {
        format!("{}{}", s, "x".repeat(120))
    }

    // ---- Core supersession tests ----

    /// Scenario: Read P at turn 0, Write P at turn 1.
    /// Expected: the Read result (turn 0) is elided; the later state (Write) is kept.
    #[test]
    fn read_superseded_by_later_write() {
        let content = big("file contents before edit");
        let mut msgs = vec![
            tool_use_msg("r0", "Read", json!({"path": "/src/lib.rs"})),
            tool_result_msg("r0", &content),
            // Later the agent writes to the same file.
            tool_use_msg(
                "w0",
                "Write",
                json!({"file_path": "/src/lib.rs", "new_string": "new"}),
            ),
            tool_result_msg("w0", "wrote 3 bytes"),
        ];
        let stats = apply(&mut msgs, &cfg(&[])).unwrap();
        assert_eq!(stats.stubbed, 1, "the stale Read result should be elided");
        // Read result is now a stub.
        let read_content = msgs[1]["content"][0]["content"].as_str().unwrap();
        assert!(
            read_content.starts_with("[trimwire: stale read"),
            "Read result should carry the stale marker; got: {read_content}"
        );
        // Write result is untouched (it is the cause of supersession, not the victim).
        assert_eq!(
            msgs[3]["content"][0]["content"],
            json!("wrote 3 bytes"),
            "Write result must be preserved verbatim"
        );
        PairingIndex::build(&msgs).validate().unwrap();
    }

    /// B-9: a protected path's superseded Read result is NOT elided.
    #[test]
    fn protected_path_superseded_read_not_elided() {
        let content = big("AGENTS.md contents");
        let build = || {
            vec![
                tool_use_msg("r0", "Read", json!({"path": "/repo/AGENTS.md"})),
                tool_result_msg("r0", &content),
                tool_use_msg(
                    "w0",
                    "Write",
                    json!({"file_path": "/repo/AGENTS.md", "new_string": "x"}),
                ),
                tool_result_msg("w0", "wrote"),
            ]
        };
        // Sanity: unprotected → the superseded read IS elided.
        let mut unprot = build();
        assert_eq!(apply(&mut unprot, &cfg(&[])).unwrap().stubbed, 1);
        // Protected → NOT elided.
        let mut prot = build();
        let mut c = cfg(&[]);
        c.protected_file_patterns = vec!["*AGENTS.md".to_owned()];
        assert_eq!(
            apply(&mut prot, &c).unwrap().stubbed,
            0,
            "protected read kept"
        );
        assert_eq!(
            prot[1]["content"][0]["content"].as_str().unwrap(),
            content,
            "protected read content is intact"
        );
        PairingIndex::build(&prot).validate().unwrap();
    }

    /// B-9: a protected path is not demand-paged either (same path_ops exclusion).
    #[test]
    fn protected_path_not_demand_paged() {
        let mk = || {
            let mut msgs = vec![
                tool_use_msg("r0", "Read", json!({"path": "/repo/AGENTS.md"})),
                tool_result_msg("r0", &"z".repeat(9000)), // > 8192 page threshold
            ];
            for i in 0..6 {
                let id = format!("p{i}");
                msgs.push(tool_use_msg(
                    &id,
                    "Bash",
                    json!({"command": format!("e{i}")}),
                ));
                msgs.push(tool_result_msg(&id, "ok"));
            }
            msgs
        };
        // Sanity: unprotected → the old large read IS paged.
        let mut unprot = mk();
        assert!(apply(&mut unprot, &cfg_paging(8192, 4)).unwrap().stubbed >= 1);
        // Protected → NOT paged.
        let mut prot = mk();
        let mut c = cfg_paging(8192, 4);
        c.protected_file_patterns = vec!["*AGENTS.md".to_owned()];
        assert_eq!(
            apply(&mut prot, &c).unwrap().stubbed,
            0,
            "protected read not paged"
        );
        assert!(
            prot[1]["content"][0]["content"]
                .as_str()
                .unwrap()
                .starts_with('z'),
            "protected read content is intact"
        );
    }

    /// §13A: a superseded Write's authored INPUT is NEVER collapsed. Authored
    /// content is the model's only faithful copy of what it wrote and is
    /// load-bearing for re-authoring; eliding it corrupted real sessions (the
    /// model reproduced the elision marker as the file body). Both the superseded
    /// and the latest authored bodies survive verbatim.
    #[test]
    fn does_not_collapse_superseded_write_input() {
        let body = "y".repeat(2000);
        let mut msgs = vec![
            tool_use_msg(
                "w0",
                "Write",
                json!({"file_path": "/src/a.rs", "new_string": body.clone()}),
            ),
            tool_result_msg("w0", "wrote"),
            tool_use_msg(
                "w1",
                "Edit",
                json!({"file_path": "/src/a.rs", "new_string": "fn final_fn() {}"}),
            ),
            tool_result_msg("w1", "edited"),
        ];
        let stats = apply(&mut msgs, &cfg(&[])).unwrap();
        // No Read results here, and authored inputs are never collapsed → no-op.
        assert_eq!(
            stats.stubbed, 0,
            "authored Write/Edit inputs must never be elided by stale_reads"
        );
        assert_eq!(
            msgs[0]["content"][0]["input"]["new_string"],
            json!(body),
            "superseded authored body must be kept verbatim (§13A corruption guard)"
        );
        assert_eq!(
            msgs[2]["content"][0]["input"]["new_string"],
            json!("fn final_fn() {}"),
            "latest write must be intact"
        );
        PairingIndex::build(&msgs).validate().unwrap();
    }

    /// Demand-paging: an OLD, LARGE, non-superseded Read is paged with a re-read
    /// marker naming the path; a RECENT read is left intact.
    #[test]
    fn pages_old_large_nonsuperseded_read() {
        let mut msgs = vec![
            tool_use_msg("r0", "Read", json!({"path": "/src/big.rs"})),
            tool_result_msg("r0", &"z".repeat(9000)), // > 8192 page threshold
        ];
        for i in 0..6 {
            let id = format!("p{i}");
            msgs.push(tool_use_msg(
                &id,
                "Bash",
                json!({"command": format!("echo {i}")}),
            ));
            msgs.push(tool_result_msg(&id, "ok"));
        }
        // A RECENT large read (last turn) — must NOT be paged.
        msgs.push(tool_use_msg(
            "r_recent",
            "Read",
            json!({"path": "/src/recent.rs"}),
        ));
        msgs.push(tool_result_msg("r_recent", &"w".repeat(9000)));

        let stats = apply(&mut msgs, &cfg_paging(8192, 4)).unwrap();
        assert!(stats.stubbed >= 1, "old large read should be paged");
        let r0 = msgs[1]["content"][0]["content"].as_str().unwrap();
        assert!(
            r0.contains("paged out") && r0.contains("/src/big.rs") && r0.contains("re-read"),
            "paged marker must name the path + instruct re-read; got: {r0}"
        );
        assert!(
            msgs.last().unwrap()["content"][0]["content"]
                .as_str()
                .unwrap()
                .starts_with('w'),
            "recent read must NOT be paged"
        );
        PairingIndex::build(&msgs).validate().unwrap();
    }

    /// Boundary pin for the page-min check's JSON-serialization semantics.
    ///
    /// The gate is `serde_json::to_string(content).len() <= page_min_bytes ⇒ skip`.
    /// A bare JSON string serializes to `raw + 2` bytes (the surrounding quotes),
    /// so paging fires iff `raw + 2 > page_min`, i.e. the EFFECTIVE raw-content
    /// threshold is `page_min - 1`: raw == page_min-2 is NOT paged (serialized ==
    /// page_min), raw == page_min-1 IS paged (serialized == page_min+1).
    #[test]
    fn page_min_boundary_accounts_for_json_quote_overhead() {
        // True iff an OLD single-view Read of `raw_len` bytes is demand-paged.
        let paged_at = |raw_len: usize, page_min: usize| -> bool {
            let mut msgs = vec![
                tool_use_msg("r0", "Read", json!({"path": "/src/boundary.rs"})),
                tool_result_msg("r0", &"z".repeat(raw_len)),
            ];
            // Age the read past keep_recent (single-view, non-superseded, not hot).
            for i in 0..6 {
                let id = format!("p{i}");
                msgs.push(tool_use_msg(
                    &id,
                    "Bash",
                    json!({"command": format!("echo {i}")}),
                ));
                msgs.push(tool_result_msg(&id, "ok"));
            }
            apply(&mut msgs, &cfg_paging(page_min, 4)).unwrap();
            msgs[1]["content"][0]["content"]
                .as_str()
                .unwrap()
                .contains("paged out")
        };

        let min = 1024;
        assert!(
            !paged_at(min - 2, min),
            "raw == page_min-2 → serialized == page_min → NOT paged (<= gate)"
        );
        assert!(
            paged_at(min - 1, min),
            "raw == page_min-1 → serialized == page_min+1 → paged"
        );
    }

    /// §13B hot-path guard: a path the model has Read more than once is NEVER
    /// demand-paged, even when its current view is old + huge — paging it would
    /// force another re-read → the read-spiral. (Module-level unit coverage; the
    /// harm gate covers the same invariant end-to-end across the default profile.)
    #[test]
    fn hot_reread_path_is_not_paged() {
        let mut msgs = vec![
            tool_use_msg("r0", "Read", json!({"path": "/src/hot.rs"})),
            tool_result_msg("r0", &"z".repeat(40_000)),
            // Re-read the SAME path later → read_count == 2 (a hot path).
            tool_use_msg("r1", "Read", json!({"path": "/src/hot.rs"})),
            tool_result_msg("r1", &"q".repeat(40_000)),
        ];
        // Age both reads past keep_recent so paging would otherwise target r1.
        for i in 0..6 {
            let id = format!("p{i}");
            msgs.push(tool_use_msg(
                &id,
                "Bash",
                json!({"command": format!("echo {i}")}),
            ));
            msgs.push(tool_result_msg(&id, "ok"));
        }
        apply(&mut msgs, &cfg_paging(8192, 4)).unwrap();
        // r0 (superseded) is elided by behavior #1; but r1 (current view) must NOT
        // be paged because the path was read twice.
        let r1 = msgs[3]["content"][0]["content"].as_str().unwrap();
        assert!(
            r1.starts_with('q'),
            "hot re-read path's current view must NOT be paged; got: {}",
            &r1[..r1.len().min(48)]
        );
        PairingIndex::build(&msgs).validate().unwrap();
    }

    /// Scenario: Read P at turn 0, Read P again at turn 1 (file unchanged or changed).
    /// Expected: the earlier Read is elided; the latest Read is kept.
    #[test]
    fn earlier_read_superseded_by_later_read() {
        let content_v1 = big("file contents version 1");
        let content_v2 = big("file contents version 2 after edit");
        let mut msgs = vec![
            tool_use_msg("r0", "Read", json!({"path": "/src/lib.rs"})),
            tool_result_msg("r0", &content_v1),
            // Same path read again later (content may have changed).
            tool_use_msg("r1", "Read", json!({"path": "/src/lib.rs"})),
            tool_result_msg("r1", &content_v2),
        ];
        let stats = apply(&mut msgs, &cfg(&[])).unwrap();
        assert_eq!(stats.stubbed, 1, "earlier Read should be elided");
        let read0_content = msgs[1]["content"][0]["content"].as_str().unwrap();
        assert!(
            read0_content.starts_with("[trimwire: stale read"),
            "earlier Read result should carry the stale marker"
        );
        // Latest Read is untouched.
        assert_eq!(
            msgs[3]["content"][0]["content"],
            json!(content_v2),
            "latest Read must be preserved verbatim"
        );
        PairingIndex::build(&msgs).validate().unwrap();
    }

    /// Scenario: Read P never written or re-read → untouched.
    #[test]
    fn read_with_no_later_operation_is_untouched() {
        let content = big("only read once, never superseded");
        let mut msgs = vec![
            tool_use_msg("r0", "Read", json!({"path": "/solo.txt"})),
            tool_result_msg("r0", &content),
        ];
        let stats = apply(&mut msgs, &cfg(&[])).unwrap();
        assert_eq!(stats.stubbed, 0, "sole Read must never be elided");
        assert_eq!(
            msgs[1]["content"][0]["content"],
            json!(content),
            "sole Read result must be preserved verbatim"
        );
    }

    /// The LAST operation on each path is never elided, even when many precede it.
    #[test]
    fn last_operation_per_path_is_never_elided() {
        let v1 = big("version 1");
        let v2 = big("version 2");
        let v3 = big("version 3 — the freshest view");
        let mut msgs = vec![
            tool_use_msg("r0", "Read", json!({"path": "/f.rs"})),
            tool_result_msg("r0", &v1),
            tool_use_msg("r1", "Read", json!({"path": "/f.rs"})),
            tool_result_msg("r1", &v2),
            tool_use_msg("r2", "Read", json!({"path": "/f.rs"})),
            tool_result_msg("r2", &v3),
        ];
        let stats = apply(&mut msgs, &cfg(&[])).unwrap();
        // First two Reads are stale; the last is kept.
        assert_eq!(
            stats.stubbed, 2,
            "only the two stale reads should be elided"
        );
        assert!(
            msgs[1]["content"][0]["content"]
                .as_str()
                .unwrap()
                .starts_with("[trimwire: stale read"),
            "first Read should be elided"
        );
        assert!(
            msgs[3]["content"][0]["content"]
                .as_str()
                .unwrap()
                .starts_with("[trimwire: stale read"),
            "second Read should be elided"
        );
        assert_eq!(
            msgs[5]["content"][0]["content"],
            json!(v3),
            "last Read must be kept verbatim"
        );
        PairingIndex::build(&msgs).validate().unwrap();
    }

    /// is_error Read results are skipped (a failed read is not an authoritative view).
    #[test]
    fn error_read_result_is_skipped() {
        let mut msgs = vec![
            tool_use_msg("r0", "Read", json!({"path": "/missing.txt"})),
            error_result_msg("r0", "file not found"),
            // Later read of the same path (non-error).
            tool_use_msg("r1", "Read", json!({"path": "/missing.txt"})),
            tool_result_msg("r1", &big("now it exists")),
        ];
        let stats = apply(&mut msgs, &cfg(&[])).unwrap();
        // The error result is skipped; the later Read is kept (it is the last op).
        assert_eq!(stats.stubbed, 0, "is_error result must be skipped");
        assert_eq!(
            msgs[1]["content"][0]["content"],
            json!("file not found"),
            "error result must be preserved verbatim"
        );
    }

    /// Exempt tools are not tracked as path operations (so they don't cause
    /// supersession of earlier Reads).
    #[test]
    fn exempt_tool_not_tracked() {
        let content = big("file contents");
        let mut msgs = vec![
            tool_use_msg("r0", "Read", json!({"path": "/f.rs"})),
            tool_result_msg("r0", &content),
            // If Read were exempt, the earlier Read would not be superseded by
            // this later Read — but we test the *tool* is exempt, meaning it is
            // not tracked as an operation at all.
            tool_use_msg("r1", "Read", json!({"path": "/f.rs"})),
            tool_result_msg("r1", &big("newer content")),
        ];
        // With Read exempt: no operations are tracked → nothing is stale.
        let stats = apply(&mut msgs, &cfg(&["Read"])).unwrap();
        assert_eq!(
            stats.stubbed, 0,
            "when Read is exempt it is not tracked; nothing is elided"
        );
        // Both results intact.
        assert_eq!(msgs[1]["content"][0]["content"], json!(content));
    }

    /// Shrink guard: a tiny Read result must not be grown by the stub marker.
    #[test]
    fn tiny_read_not_grown_by_shrink_guard() {
        // "ok" is 2 bytes; the marker is much longer → elision would grow body.
        let mut msgs = vec![
            tool_use_msg("r0", "Read", json!({"path": "/tiny.txt"})),
            tool_result_msg("r0", "ok"),
            tool_use_msg(
                "w0",
                "Write",
                json!({"file_path": "/tiny.txt", "new_string": "ok2"}),
            ),
            tool_result_msg("w0", "wrote"),
        ];
        let stats = apply(&mut msgs, &cfg(&[])).unwrap();
        assert_eq!(
            stats.stubbed, 0,
            "tiny Read result must not be stubbed (would grow body)"
        );
        assert_eq!(msgs[1]["content"][0]["content"], json!("ok"));
    }

    // ---- Idempotency, determinism, orphan-free ----

    /// Running apply twice: the second run stubs 0 (idempotent).
    #[test]
    fn idempotent_second_run_stubs_zero() {
        let content = big("file contents");
        let mut msgs = vec![
            tool_use_msg("r0", "Read", json!({"path": "/f.rs"})),
            tool_result_msg("r0", &content),
            tool_use_msg(
                "w0",
                "Write",
                json!({"file_path": "/f.rs", "new_string": "new"}),
            ),
            tool_result_msg("w0", "wrote"),
        ];
        let first = apply(&mut msgs, &cfg(&[])).unwrap();
        assert!(first.stubbed > 0, "first run must elide something");
        let after_first = serde_json::to_vec(&msgs).unwrap();

        let second = apply(&mut msgs, &cfg(&[])).unwrap();
        assert_eq!(second.stubbed, 0, "second run must be a no-op");
        let after_second = serde_json::to_vec(&msgs).unwrap();
        assert_eq!(after_first, after_second, "byte-identical after second run");
    }

    /// Two independent runs from the same input produce byte-identical output.
    #[test]
    fn deterministic_two_runs() {
        let mk = || {
            vec![
                tool_use_msg("r0", "Read", json!({"path": "/a.rs"})),
                tool_result_msg("r0", &big("v1")),
                tool_use_msg("r1", "Read", json!({"path": "/a.rs"})),
                tool_result_msg("r1", &big("v2")),
                tool_use_msg(
                    "e0",
                    "Edit",
                    json!({"file_path": "/a.rs", "old_string": "x", "new_string": "y"}),
                ),
                tool_result_msg("e0", "edited"),
            ]
        };
        let mut a = mk();
        let mut b = mk();
        apply(&mut a, &cfg(&[])).unwrap();
        apply(&mut b, &cfg(&[])).unwrap();
        assert_eq!(
            serde_json::to_vec(&a).unwrap(),
            serde_json::to_vec(&b).unwrap(),
            "two independent runs must be byte-identical"
        );
    }

    /// PairingIndex is valid both pre- and post-apply (orphan-free).
    #[test]
    fn orphan_free_pre_and_post() {
        let mut msgs = vec![
            tool_use_msg("r0", "Read", json!({"path": "/x.rs"})),
            tool_result_msg("r0", &big("contents")),
            tool_use_msg("r1", "Read", json!({"path": "/x.rs"})),
            tool_result_msg("r1", &big("contents updated")),
        ];
        PairingIndex::build(&msgs).validate().unwrap();
        apply(&mut msgs, &cfg(&[])).unwrap();
        PairingIndex::build(&msgs)
            .validate()
            .expect("no orphans after stale_reads");
    }

    // ---- Needle tests ----

    /// A needle in the KEPT (latest) Read result survives.
    #[test]
    fn needle_in_kept_latest_read_survives() {
        let needle = "NEEDLE_STALE_READS_KEEP_4f8a";
        let stale_content = big("stale version of the file");
        let fresh_content = format!(
            "fresh content with {needle} that must survive{}",
            "x".repeat(80)
        );

        let mut msgs = vec![
            tool_use_msg("r0", "Read", json!({"path": "/f.rs"})),
            tool_result_msg("r0", &stale_content),
            tool_use_msg("r1", "Read", json!({"path": "/f.rs"})),
            tool_result_msg("r1", &fresh_content),
        ];
        apply(&mut msgs, &cfg(&[])).unwrap();
        // Latest Read (r1) must still contain the needle.
        let kept = msgs[3]["content"][0]["content"].as_str().unwrap();
        assert!(
            kept.contains(needle),
            "needle must survive in the kept latest Read; got: {kept}"
        );
    }

    /// A needle ONLY in a stale (superseded) Read is elided — the file changed;
    /// the model's fresh view (via Write/Edit) no longer contains that version.
    ///
    /// # Design note
    /// This is intentional and correct: once the file was overwritten by a
    /// Write/Edit, the previous Read's content is no longer the ground truth.
    /// Eliding it is equivalent to what the agent itself did (overwrote the file).
    /// The model should use the new file state, not the pre-edit snapshot.
    #[test]
    fn needle_only_in_stale_read_is_elided_by_design() {
        let needle = "NEEDLE_STALE_READS_GONE_b7c3";
        let stale_content = format!(
            "content with {needle} that gets overwritten{}",
            "x".repeat(80)
        );

        let mut msgs = vec![
            tool_use_msg("r0", "Read", json!({"path": "/f.rs"})),
            tool_result_msg("r0", &stale_content),
            // Agent wrote a completely new version — the stale Read is dead weight.
            tool_use_msg(
                "w0",
                "Write",
                json!({"file_path": "/f.rs", "new_string": "completely new content without the needle"}),
            ),
            tool_result_msg("w0", "wrote 42 bytes"),
        ];
        apply(&mut msgs, &cfg(&[])).unwrap();
        // The stale Read result no longer contains the needle (elided).
        let elided = msgs[1]["content"][0]["content"].as_str().unwrap();
        assert!(
            !elided.contains(needle),
            "needle in stale Read is correctly elided after Write; got: {elided}"
        );
        assert!(
            elided.starts_with("[trimwire: stale read"),
            "stale Read must carry the stub marker; got: {elided}"
        );
    }

    // ---- Multiple paths ----

    /// Two different paths: each is tracked independently; only the stale Reads
    /// for each path are elided.
    #[test]
    fn multiple_paths_tracked_independently() {
        let a_v1 = big("path A version 1");
        let a_v2 = big("path A version 2");
        let b_v1 = big("path B version 1 — never re-read");

        let mut msgs = vec![
            tool_use_msg("ra0", "Read", json!({"path": "/a.rs"})),
            tool_result_msg("ra0", &a_v1),
            tool_use_msg("rb0", "Read", json!({"path": "/b.rs"})),
            tool_result_msg("rb0", &b_v1),
            // Only /a.rs is re-read; /b.rs is never touched again.
            tool_use_msg("ra1", "Read", json!({"path": "/a.rs"})),
            tool_result_msg("ra1", &a_v2),
        ];
        let stats = apply(&mut msgs, &cfg(&[])).unwrap();
        // Only the first Read of /a.rs is stale.
        assert_eq!(stats.stubbed, 1, "only the stale Read of /a.rs is elided");
        assert!(
            msgs[1]["content"][0]["content"]
                .as_str()
                .unwrap()
                .starts_with("[trimwire: stale read"),
            "/a.rs first Read should be elided"
        );
        // /b.rs is never re-read → its sole Read is kept.
        assert_eq!(
            msgs[3]["content"][0]["content"],
            json!(b_v1),
            "/b.rs Read must be untouched"
        );
        // Latest /a.rs read kept.
        assert_eq!(
            msgs[5]["content"][0]["content"],
            json!(a_v2),
            "latest /a.rs Read must be kept verbatim"
        );
        PairingIndex::build(&msgs).validate().unwrap();
    }

    /// Read superseded by Edit (uses file_path field).
    #[test]
    fn read_superseded_by_edit() {
        let content = big("before edit");
        let mut msgs = vec![
            tool_use_msg("r0", "Read", json!({"path": "/f.rs"})),
            tool_result_msg("r0", &content),
            tool_use_msg(
                "e0",
                "Edit",
                json!({"file_path": "/f.rs", "old_string": "x", "new_string": "y"}),
            ),
            tool_result_msg("e0", "edit applied"),
        ];
        let stats = apply(&mut msgs, &cfg(&[])).unwrap();
        assert_eq!(stats.stubbed, 1, "Read superseded by Edit must be elided");
        assert!(
            msgs[1]["content"][0]["content"]
                .as_str()
                .unwrap()
                .starts_with("[trimwire: stale read")
        );
    }

    /// Read superseded by MultiEdit.
    #[test]
    fn read_superseded_by_multiedit() {
        let content = big("before multiedit");
        let mut msgs = vec![
            tool_use_msg("r0", "Read", json!({"path": "/f.rs"})),
            tool_result_msg("r0", &content),
            tool_use_msg(
                "me0",
                "MultiEdit",
                json!({"file_path": "/f.rs", "edits": []}),
            ),
            tool_result_msg("me0", "multiedit applied"),
        ];
        let stats = apply(&mut msgs, &cfg(&[])).unwrap();
        assert_eq!(
            stats.stubbed, 1,
            "Read superseded by MultiEdit must be elided"
        );
    }
}
