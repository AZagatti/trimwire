//! `SimHashDedup` strategy — collapse near-duplicate `tool_result` content.
//!
//! `cross_turn_dedup` catches exact and prefix-identical repeated tool_results.
//! It MISSES near-duplicates: e.g. two `cargo build` / `cargo test` outputs
//! that differ only in timestamps, PIDs, elapsed durations, or line counts.
//! `simhash_dedup` catches those: for each pair where an **older** result is
//! ≥90% similar to a **later** one (by 64-bit SimHash Hamming distance ≤
//! `hamming_threshold`), the older result is collapsed to a size-breadcrumb
//! marker; the newest verbatim copy is kept.
//!
//! ## Algorithm
//! 1. Pre-validate with `PairingIndex::build(messages).validate()?`.
//! 2. Compute the `assistant_cutoff` for `keep_recent_turns`. Only results
//!    older than the cutoff are candidates (the model is still actively using
//!    recent ones). Skip `is_error`, already-cleared, and tiny (<`min_bytes`)
//!    results. Skip results whose content already starts with `"[trimwire: "`.
//! 3. For each candidate, compute a **64-bit SimHash** over its tokenized
//!    string content (tokenize on non-alphanumeric boundaries; weight = term
//!    frequency; FNV-1a hash per token for determinism across runs).
//! 4. Build a near-duplicate index: if an older result A has Hamming distance
//!    ≤ `hamming_threshold` to any **later** result B (including recent ones),
//!    A is a near-duplicate and should be collapsed. B is kept verbatim (the
//!    newest near-duplicate in each cluster always survives).
//! 5. Collect collapse targets into a sorted `Vec`, apply shrink guard
//!    (only collapse when the marker is strictly smaller), then mutate.
//!    (Output orphan-safety is the orchestrator's single post-`run` validate;
//!    this strategy only overwrites `tool_result.content`, so it can't orphan.)
//!
//! ## Safety properties
//! - **Content-overwrite only**: never adds/removes/reorders blocks → orphan-safe
//!   and reprune-compatible.
//! - **Idempotent**: results already starting with `"[trimwire: "` are skipped.
//! - **Shrink guard**: only collapses when the marker is strictly smaller.
//! - **Deterministic**: uses FNV-1a (fixed, no ASLR), collects into a
//!   `Vec` sorted by location before mutating — no HashMap-iteration-order
//!   dependence in output.
//! - **min_bytes floor** (default 512) + tight **hamming_threshold** (default 3
//!   out of 64 bits, ~95% similarity) keep false-positive rate very low.
//!
//! **OPT-IN — off by default. Not enabled in any profile.** Enable explicitly:
//!
//! ```toml
//! [strategies.simhash_dedup]
//! enabled = true
//! ```

use serde_json::Value;

use crate::config::{SimHashDedupConfig, matches_any};
use crate::error::Result;
use crate::pairing::PairingIndex;
use crate::strategies::{
    Stats, assistant_cutoff, block_mut, elision_marker, is_already_cleared, role,
};

// ---- SimHash internals ----

/// FNV-1a 64-bit hash — deterministic, no ASLR, no random seed.
/// <https://en.wikipedia.org/wiki/Fowler%E2%80%93Noll%E2%80%93Vo_hash_function>
fn fnv1a_64(bytes: &[u8]) -> u64 {
    const FNV_OFFSET: u64 = 14_695_981_039_346_656_037;
    const FNV_PRIME: u64 = 1_099_511_628_211;
    let mut h = FNV_OFFSET;
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(FNV_PRIME);
    }
    h
}

/// Tokenize `text` on non-alphanumeric boundaries.
/// Returns `(token, frequency)` pairs in arbitrary order.
fn tokenize(text: &str) -> impl Iterator<Item = (&str, u32)> {
    // Build a frequency map; collect into a Vec so we can return a simple iterator.
    let mut freq: std::collections::HashMap<&str, u32> = std::collections::HashMap::new();
    let mut start: Option<usize> = None;
    for (i, c) in text.char_indices() {
        if c.is_alphanumeric() {
            if start.is_none() {
                start = Some(i);
            }
        } else if let Some(s) = start.take() {
            *freq.entry(&text[s..i]).or_insert(0) += 1;
        }
    }
    if let Some(s) = start {
        *freq.entry(&text[s..]).or_insert(0) += 1;
    }
    freq.into_iter()
}

/// Compute a 64-bit SimHash over `text`.
///
/// For each unique token with frequency `w`:
///   - Hash it with FNV-1a to get a 64-bit value.
///   - For each bit position b: add +w if bit b is set, else -w to a
///     per-position accumulator.
///
/// Final hash: bit b = 1 if accumulator\[b\] > 0, else 0.
pub(crate) fn simhash(text: &str) -> u64 {
    let mut v = [0i64; 64];
    for (token, freq) in tokenize(text) {
        let h = fnv1a_64(token.as_bytes());
        let w = i64::from(freq);
        for b in 0u32..64 {
            if (h >> b) & 1 == 1 {
                v[b as usize] += w;
            } else {
                v[b as usize] -= w;
            }
        }
    }
    let mut result = 0u64;
    for b in 0u32..64 {
        if v[b as usize] > 0 {
            result |= 1u64 << b;
        }
    }
    result
}

/// Hamming distance between two 64-bit SimHash values.
#[inline]
fn hamming(a: u64, b: u64) -> u32 {
    (a ^ b).count_ones()
}

// ---- Strategy entry point ----

/// Collapse old near-duplicate `tool_result` content using SimHash.
pub fn apply(messages: &mut [Value], cfg: &SimHashDedupConfig) -> Result<Stats> {
    super::with_stats(messages, |m| apply_counted(m, cfg))
}

/// Worker: returns the number of results collapsed. Byte accounting is threaded
/// by the caller ([`super::run`]); see [`apply`].
pub(crate) fn apply_counted(messages: &mut [Value], cfg: &SimHashDedupConfig) -> Result<usize> {
    let idx = PairingIndex::build(messages);
    idx.validate()?;

    // Nothing can be "old" if the conversation is too short.
    let Some(cutoff) = assistant_cutoff(messages, cfg.keep_recent_turns) else {
        return Ok(0);
    };

    // ---- Pass 1: collect all tool_result locations with their string content,
    //              tool name, and message index. ----
    //
    // We need to know the tool name for exempt_tools, and the message index
    // to distinguish "old" (≤ cutoff) from "recent" (> cutoff).
    struct ResultEntry {
        loc: (usize, usize),
        msg_idx: usize,
        content: String,
    }

    let mut entries: Vec<ResultEntry> = Vec::new();

    for (mi, msg) in messages.iter().enumerate() {
        // tool_results live in user messages.
        if role(msg) != Some("user") {
            continue;
        }
        let Some(content_arr) = msg.get("content").and_then(Value::as_array) else {
            continue;
        };
        for (ci, block) in content_arr.iter().enumerate() {
            if block.get("type").and_then(Value::as_str) != Some("tool_result") {
                continue;
            }
            // Look up the paired tool_use to get the tool name for exempt check.
            let tool_use_id = block
                .get("tool_use_id")
                .and_then(Value::as_str)
                .unwrap_or("");
            // Resolve the tool name via the pairing index. If we can't find the
            // paired tool_use, we still process it (no name → no exempt match).
            let tool_name: Option<String> = idx.uses.get(tool_use_id).and_then(|(umi, uci)| {
                messages
                    .get(*umi)
                    .and_then(|m| m.get("content"))
                    .and_then(Value::as_array)
                    .and_then(|a| a.get(*uci))
                    .and_then(|b| b.get("name"))
                    .and_then(Value::as_str)
                    .map(|s| s.to_owned())
            });
            if let Some(ref name) = tool_name {
                if matches_any(&cfg.exempt_tools, name) {
                    continue;
                }
            }
            // Skip is_error.
            if block
                .get("is_error")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            {
                continue;
            }
            let raw_content = block.get("content").unwrap_or(&Value::Null);
            // Only handle string content.
            let Some(s) = raw_content.as_str() else {
                continue;
            };
            // Idempotency: skip already-cleared or already-marked content.
            if s.starts_with("[trimwire: ") || is_already_cleared(raw_content) {
                continue;
            }
            // min_bytes floor — check serialized length.
            let slen = serde_json::to_string(raw_content)
                .map(|r| r.len())
                .unwrap_or(0);
            if slen < cfg.min_bytes {
                continue;
            }
            entries.push(ResultEntry {
                loc: (mi, ci),
                msg_idx: mi,
                content: s.to_owned(),
            });
        }
    }

    // ---- Pass 2: compute SimHashes for all entries. ----
    // We hash ALL entries (old + recent) so that a recent result can serve as
    // the "keeper" that makes an older one a near-duplicate.
    let hashes: Vec<u64> = entries.iter().map(|e| simhash(&e.content)).collect();

    // ---- Pass 3: mark old entries that have a later near-duplicate. ----
    //
    // For entry i (old, i.e. msg_idx ≤ cutoff): check if any entry j > i
    // has hamming(hashes[i], hashes[j]) ≤ hamming_threshold. If yes, i is
    // collapsed (j, the later one, is kept verbatim regardless of whether j
    // is itself old or recent).
    //
    // Collect collapse targets as (loc, original_content) for the mutation pass.
    // We collect into a Vec first and sort by location for deterministic order.
    let mut to_collapse: Vec<((usize, usize), String)> = Vec::new();

    for i in 0..entries.iter().len() {
        // Only collapse OLD entries.
        if entries[i].msg_idx > cutoff {
            continue;
        }
        let has_later_near_dup = ((i + 1)..entries.len())
            .any(|j| hamming(hashes[i], hashes[j]) <= cfg.hamming_threshold);
        if has_later_near_dup {
            to_collapse.push((entries[i].loc, entries[i].content.clone()));
        }
    }

    // Sort by location (msg_idx, content_idx) for deterministic mutation order.
    to_collapse.sort_unstable_by_key(|(loc, _)| *loc);
    to_collapse.dedup_by_key(|(loc, _)| *loc);

    // ---- Pass 4: apply mutations with shrink guard. ----
    let mut stubbed = 0usize;
    for (loc, content) in to_collapse {
        let value = Value::String(content);
        let marker = elision_marker(&cfg.stub, &value);
        let len = |v: &Value| serde_json::to_string(v).map_or(usize::MAX, |s| s.len());
        if len(&marker) >= len(&value) {
            // Marker would not shrink the body — skip.
            continue;
        }
        if let Some(block) = block_mut(messages, loc) {
            block["content"] = marker;
            stubbed += 1;
        }
    }

    Ok(stubbed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Default config with simhash_dedup enabled.
    fn cfg() -> SimHashDedupConfig {
        SimHashDedupConfig {
            enabled: true,
            keep_recent_turns: 4,
            hamming_threshold: 3,
            min_bytes: 512,
            exempt_tools: Vec::new(),
            stub: "[trimwire: near-duplicate of a later result]".to_owned(),
        }
    }

    /// Build an assistant turn (tool_use) + user turn (tool_result) pair.
    fn pair(id: &str, name: &str, content: &str) -> Vec<Value> {
        vec![
            json!({"role": "assistant", "content": [
                {"type": "tool_use", "id": id, "name": name, "input": {}}
            ]}),
            json!({"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": id, "content": content}
            ]}),
        ]
    }

    /// Build enough padding turns to push a result past the `keep_recent` cutoff.
    fn padding_turns(n: usize) -> Vec<Value> {
        let mut msgs = Vec::new();
        for i in 0..n {
            let id = format!("pad{i}");
            msgs.extend(pair(&id, "Bash", &format!("padding {i}")));
        }
        msgs
    }

    /// Cargo-build-like output with a timestamp and duration that vary between runs.
    fn cargo_build_output(timestamp: &str, duration: &str) -> String {
        // Make it large enough to clear min_bytes=512.
        format!(
            "   Compiling mylib v0.1.0 (/workspace/mylib)\n\
                Compiling myapp v0.1.0 (/workspace/myapp)\n\
             [timestamp: {timestamp}]\n\
             warning: unused variable `x` in src/main.rs:42\n\
             warning: dead_code in src/lib.rs:17\n\
             warning: 2 warnings emitted\n\
                 Finished dev [unoptimized + debuginfo] target(s) in {duration}\n\
             {}",
            "x".repeat(400)
        )
    }

    // ---- Core near-dedup tests ----

    /// Two cargo-build outputs differing only in timestamp/duration:
    /// the older one is collapsed, the newer one is kept verbatim.
    #[test]
    fn near_duplicate_older_collapsed_newer_kept() {
        let old_output = cargo_build_output("2024-01-01T10:00:00Z", "3.21s");
        let new_output = cargo_build_output("2024-01-01T10:05:00Z", "3.19s");

        // Quick sanity: these are not byte-identical.
        assert_ne!(old_output, new_output);

        let mut msgs = Vec::new();
        // Old result (will be pushed past cutoff by padding).
        msgs.extend(pair("old1", "Bash", &old_output));
        // 5 padding turns to push old1 past keep_recent=4.
        msgs.extend(padding_turns(5));
        // Newer near-duplicate.
        msgs.extend(pair("new1", "Bash", &new_output));

        let stats = apply(&mut msgs, &cfg()).unwrap();
        assert!(
            stats.stubbed >= 1,
            "older near-duplicate should be collapsed"
        );

        // Find the old result (it's at index 1 of msgs, content[0]).
        let old_content = msgs[1]["content"][0]["content"].as_str().unwrap();
        assert!(
            old_content.starts_with("[trimwire: near-duplicate"),
            "older result must carry the near-dedup marker; got: {old_content}"
        );

        // The newer result must still contain the distinct timestamp.
        let new_result_msg = msgs.last().unwrap();
        let new_content = new_result_msg["content"][0]["content"].as_str().unwrap();
        assert!(
            new_content.contains("2024-01-01T10:05:00Z"),
            "newer result must be kept verbatim; got: {new_content}"
        );

        PairingIndex::build(&msgs).validate().unwrap();
    }

    /// Genuinely different results (e.g. success vs. failure output) must NOT
    /// be collapsed — they are far apart in SimHash space.
    #[test]
    fn genuinely_different_results_not_collapsed() {
        // These two are structurally very different: one is a build success,
        // the other is a completely different compilation error log.
        let output_a = format!(
            "   Compiling myapp v0.1.0\n\
             warning: unused variable\n\
             warning: 1 warning emitted\n\
                 Finished dev target(s) in 3.21s\n\
             {}",
            "a".repeat(460)
        );
        let output_b = format!(
            "SELECT id, name, email FROM users WHERE active = true ORDER BY created_at DESC;\n\
             Results: 42 rows\n\
             +----+------------------+---------------------------+\n\
             | id | name             | email                     |\n\
             +----+------------------+---------------------------+\n\
             |  1 | Alice Johnson    | alice@example.com         |\n\
             |  2 | Bob Smith        | bob@example.com           |\n\
             +----+------------------+---------------------------+\n\
             {}",
            "b".repeat(370)
        );

        assert_ne!(output_a, output_b);
        let h_a = simhash(&output_a);
        let h_b = simhash(&output_b);
        let dist = hamming(h_a, h_b);
        // Verify these are far apart (should be > 3 for truly different content).
        assert!(
            dist > 3,
            "genuinely-different outputs should have Hamming distance > 3; got {dist}"
        );

        let mut msgs = Vec::new();
        msgs.extend(pair("a1", "Bash", &output_a));
        msgs.extend(padding_turns(5));
        msgs.extend(pair("b1", "Bash", &output_b));

        let stats = apply(&mut msgs, &cfg()).unwrap();
        assert_eq!(
            stats.stubbed, 0,
            "genuinely different results must NOT be collapsed"
        );

        // Both results must be intact.
        let content_a = msgs[1]["content"][0]["content"].as_str().unwrap();
        assert!(
            content_a.contains("Finished dev target"),
            "output_a must be untouched"
        );
        let content_b = msgs.last().unwrap()["content"][0]["content"]
            .as_str()
            .unwrap();
        assert!(
            content_b.contains("SELECT id"),
            "output_b must be untouched"
        );

        PairingIndex::build(&msgs).validate().unwrap();
    }

    /// Results within `keep_recent_turns` must NOT be collapsed, even if they
    /// are near-duplicates of each other.
    #[test]
    fn recent_results_not_collapsed() {
        let out1 = cargo_build_output("2024-01-01T10:00:00Z", "3.21s");
        let out2 = cargo_build_output("2024-01-01T10:05:00Z", "3.19s");

        // No padding — both results are recent (within keep_recent=4).
        let mut msgs = Vec::new();
        msgs.extend(pair("r1", "Bash", &out1));
        msgs.extend(pair("r2", "Bash", &out2));

        let stats = apply(&mut msgs, &cfg()).unwrap();
        assert_eq!(
            stats.stubbed, 0,
            "recent results must not be collapsed even if near-duplicate"
        );

        PairingIndex::build(&msgs).validate().unwrap();
    }

    // ---- Skip conditions ----

    /// is_error results are skipped.
    #[test]
    fn is_error_result_is_skipped() {
        let big_error = format!("error: {}", "E".repeat(600));
        let mut msgs = Vec::new();
        // Error result — should never be collapsed.
        msgs.push(json!({"role": "assistant", "content": [
            {"type": "tool_use", "id": "e1", "name": "Bash", "input": {}}
        ]}));
        msgs.push(json!({"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": "e1", "is_error": true, "content": big_error}
        ]}));
        msgs.extend(padding_turns(5));
        // A near-duplicate (non-error) — this is the "later" one.
        let near_dup = format!("error: {}", "E".repeat(598));
        msgs.extend(pair("b1", "Bash", &near_dup));

        let stats = apply(&mut msgs, &cfg()).unwrap();
        assert_eq!(stats.stubbed, 0, "is_error results must be skipped");
        PairingIndex::build(&msgs).validate().unwrap();
    }

    /// Already-marked content (`[trimwire: ...`) is skipped (idempotency).
    #[test]
    fn already_marked_is_skipped() {
        let marker = format!(
            "[trimwire: near-duplicate of a later result] ({}B elided)",
            600
        );
        let mut msgs = vec![
            json!({"role": "assistant", "content": [
                {"type": "tool_use", "id": "m1", "name": "Bash", "input": {}}
            ]}),
            json!({"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "m1", "content": marker}
            ]}),
        ];
        msgs.extend(padding_turns(5));
        let near_dup = cargo_build_output("2024-01-01T10:05:00Z", "3.19s");
        msgs.extend(pair("b1", "Bash", &near_dup));

        let before = serde_json::to_vec(&msgs).unwrap();
        let stats = apply(&mut msgs, &cfg()).unwrap();
        let after = serde_json::to_vec(&msgs).unwrap();
        assert_eq!(stats.stubbed, 0, "already-marked content must be skipped");
        assert_eq!(before, after, "no mutation when already marked");
        PairingIndex::build(&msgs).validate().unwrap();
    }

    /// Tiny results (< min_bytes) are skipped.
    #[test]
    fn tiny_result_is_skipped() {
        let tiny = "tiny output";
        assert!(tiny.len() < 512, "must be < min_bytes for this test");
        let mut msgs = Vec::new();
        msgs.extend(pair("t1", "Bash", tiny));
        msgs.extend(padding_turns(5));
        msgs.extend(pair("t2", "Bash", tiny));

        let stats = apply(&mut msgs, &cfg()).unwrap();
        assert_eq!(
            stats.stubbed, 0,
            "tiny results must be skipped (< min_bytes)"
        );
        PairingIndex::build(&msgs).validate().unwrap();
    }

    // ---- Determinism ----

    /// Two independent runs on identical input produce byte-identical output.
    #[test]
    fn deterministic_two_runs() {
        let mk = || {
            let out1 = cargo_build_output("2024-01-01T10:00:00Z", "3.21s");
            let out2 = cargo_build_output("2024-01-01T10:05:00Z", "3.19s");
            let mut msgs = Vec::new();
            msgs.extend(pair("r1", "Bash", &out1));
            msgs.extend(padding_turns(5));
            msgs.extend(pair("r2", "Bash", &out2));
            msgs
        };
        let mut a = mk();
        let mut b = mk();
        apply(&mut a, &cfg()).unwrap();
        apply(&mut b, &cfg()).unwrap();
        assert_eq!(
            serde_json::to_vec(&a).unwrap(),
            serde_json::to_vec(&b).unwrap(),
            "two independent runs must be byte-identical"
        );
    }

    /// Running apply twice: the second run stubs 0 (idempotent).
    #[test]
    fn idempotent_second_run() {
        let out1 = cargo_build_output("2024-01-01T10:00:00Z", "3.21s");
        let out2 = cargo_build_output("2024-01-01T10:05:00Z", "3.19s");
        let mut msgs = Vec::new();
        msgs.extend(pair("r1", "Bash", &out1));
        msgs.extend(padding_turns(5));
        msgs.extend(pair("r2", "Bash", &out2));

        let first = apply(&mut msgs, &cfg()).unwrap();
        assert!(first.stubbed > 0, "first run must elide something");
        let after_first = serde_json::to_vec(&msgs).unwrap();

        let second = apply(&mut msgs, &cfg()).unwrap();
        assert_eq!(second.stubbed, 0, "second run must be a no-op");
        let after_second = serde_json::to_vec(&msgs).unwrap();
        assert_eq!(after_first, after_second, "byte-identical after second run");
    }

    // ---- Orphan-free ----

    /// PairingIndex is valid both pre- and post-apply.
    #[test]
    fn orphan_free_pre_and_post() {
        let out1 = cargo_build_output("2024-01-01T10:00:00Z", "3.21s");
        let out2 = cargo_build_output("2024-01-01T10:05:00Z", "3.19s");
        let mut msgs = Vec::new();
        msgs.extend(pair("r1", "Bash", &out1));
        msgs.extend(padding_turns(5));
        msgs.extend(pair("r2", "Bash", &out2));

        PairingIndex::build(&msgs).validate().unwrap();
        apply(&mut msgs, &cfg()).unwrap();
        PairingIndex::build(&msgs)
            .validate()
            .expect("no orphans after simhash_dedup");
    }

    // ---- SimHash unit tests ----

    /// SimHash is deterministic: same input always gives same hash.
    #[test]
    fn simhash_is_deterministic() {
        let text = "   Compiling myapp v0.1.0\nFinished dev in 3.21s\n";
        assert_eq!(simhash(text), simhash(text));
    }

    /// Two near-identical texts have low Hamming distance.
    #[test]
    fn near_identical_texts_low_hamming() {
        let a = cargo_build_output("2024-01-01T10:00:00Z", "3.21s");
        let b = cargo_build_output("2024-01-01T10:05:00Z", "3.19s");
        let dist = hamming(simhash(&a), simhash(&b));
        assert!(
            dist <= 3,
            "near-identical outputs must have Hamming ≤ 3; got {dist}"
        );
    }

    /// Empty string hashes consistently.
    #[test]
    fn simhash_empty_string() {
        assert_eq!(simhash(""), simhash(""));
    }

    // ---- Exempt tools ----

    /// An exempt tool's results are never collapsed.
    #[test]
    fn exempt_tool_is_not_collapsed() {
        let out1 = cargo_build_output("2024-01-01T10:00:00Z", "3.21s");
        let out2 = cargo_build_output("2024-01-01T10:05:00Z", "3.19s");

        let mut msgs = Vec::new();
        msgs.extend(pair("r1", "ExemptTool", &out1));
        msgs.extend(padding_turns(5));
        msgs.extend(pair("r2", "ExemptTool", &out2));

        let mut c = cfg();
        c.exempt_tools = vec!["ExemptTool".to_owned()];
        let stats = apply(&mut msgs, &c).unwrap();
        assert_eq!(stats.stubbed, 0, "exempt tool must not be collapsed");
        PairingIndex::build(&msgs).validate().unwrap();
    }
}
