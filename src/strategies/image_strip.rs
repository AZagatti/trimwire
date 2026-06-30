//! `ImageStrip` strategy.
//!
//! Walks `tool_result` blocks paired (via `PairingIndex`) to a `tool_use`
//! whose tool name matches `applies_to_tools` (e.g.
//! `mcp__playwright__browser_take_screenshot`). For each such result holding
//! a base64 image payload that is older than the `keep_recent_count` most
//! recent matching results, replace the content with the configured marker.
//!
//! Highest-leverage strategy by measured bloat (Playwright screenshots
//! observed at ~900KB each in real sessions; see SPIKE.md §2 / §12). Only
//! `tool_result.content` is rewritten — the pairing is untouched, so no
//! orphans are introduced.
//!
//! Faithful port of `tests/phase0/strategies.py::apply_image_strip`; the two
//! produce identical output on the same input for exact-name `applies_to`
//! lists. Two intentional extensions over the Python reference: `*`-glob
//! matching of `applies_to_tools`, and (shared with all strategies) it runs
//! after `SlidingWindow` in the pipeline.

use serde_json::Value;

use crate::config::{ImageStripConfig, matches_any};
use crate::error::Result;
use crate::pairing::PairingIndex;
use crate::strategies::{Stats, block_mut, role};

/// Minimum length for a bare string to be considered a base64 image blob.
/// Mirrors the Python reference's heuristic threshold. `str::len` counts
/// UTF-8 bytes; for base64 (ASCII-only) this equals Python's code-point
/// `len`, and any non-ASCII char fails the `is_base64_char` scan anyway.
const MIN_BASE64_LEN: usize = 4096;

/// Strip base64 images from older image-tool results, keeping the K most
/// recent. Mutates `messages` in place; returns counts + byte deltas.
pub fn apply(messages: &mut [Value], cfg: &ImageStripConfig) -> Result<Stats> {
    super::with_stats(messages, |m| apply_counted(m, cfg))
}

/// Worker: returns the number of images stripped. Byte accounting is threaded by
/// the caller ([`super::run`]); see [`apply`].
pub(crate) fn apply_counted(messages: &mut [Value], cfg: &ImageStripConfig) -> Result<usize> {
    let idx = PairingIndex::build(messages);
    idx.validate()?; // pre-check

    // Image-tool `tool_use` ids, in chronological (message) order.
    let mut image_use_ids: Vec<String> = Vec::new();
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
            if !matches_any(&cfg.applies_to_tools, name) {
                continue;
            }
            if let Some(id) = block.get("id").and_then(Value::as_str) {
                image_use_ids.push(id.to_owned());
            }
        }
    }

    // Keep the K most-recent matching results; the rest are stub candidates.
    // (Python: `ids[:-keep] if keep > 0 else ids`; `saturating_sub(0) == len`
    // collapses the keep==0 case into the same expression.)
    let stub_upto = image_use_ids.len().saturating_sub(cfg.keep_recent_count);

    let mut stubbed = 0usize;
    for tid in &image_use_ids[..stub_upto] {
        let Some(res_loc) = idx.results.get(tid).copied() else {
            continue;
        };
        if let Some(block) = block_mut(messages, res_loc) {
            let is_image = block.get("content").is_some_and(is_base64_image_content);
            if is_image {
                let content = block.get("content").cloned().unwrap_or(Value::Null);
                block["content"] = super::elision_marker(&cfg.stub, &content);
                stubbed += 1;
            }
        }
    }

    Ok(stubbed)
}

/// Heuristic: does this `tool_result.content` look like an image payload?
///
/// - A long string over the base64 alphabet (the form Claude Code's MCP
///   screenshot tools currently emit).
/// - A content-block list containing a structured `{"type":"image",...}`.
/// - A single structured `{"type":"image"}` object.
///
/// Mirrors `_is_base64_image_content` in the Python reference. The string
/// heuristic is permissive by design — any large base64-alphabet blob from a
/// nominated tool matches (tighter magic-number detection is a v0.2 candidate;
/// see the note in `tests/phase0/strategies.py`). It only ever fires on
/// results of `applies_to_tools`.
fn is_base64_image_content(content: &Value) -> bool {
    match content {
        Value::String(s) => s.len() >= MIN_BASE64_LEN && s.chars().all(is_base64_char),
        Value::Array(blocks) => blocks.iter().any(|b| {
            b.get("type").and_then(Value::as_str) == Some("image")
                && b.get("source").is_some_and(|src| !src.is_null())
        }),
        Value::Object(_) => content.get("type").and_then(Value::as_str) == Some("image"),
        _ => false,
    }
}

/// Base64 alphabet plus padding and ASCII whitespace (the Python reference's
/// `^[A-Za-z0-9+/=\s]+$`).
fn is_base64_char(c: char) -> bool {
    c.is_ascii_alphanumeric()
        || matches!(
            c,
            '+' | '/' | '=' | ' ' | '\t' | '\n' | '\r' | '\x0b' | '\x0c'
        )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// `n` screenshot turns, each result a `payload_len`-byte base64-ish blob.
    fn screenshot_session(n: usize, payload_len: usize) -> Vec<Value> {
        let payload = "A".repeat(payload_len); // all base64-alphabet chars
        let mut msgs = Vec::new();
        for i in 0..n {
            let uid = format!("toolu_s{i}");
            msgs.push(
                json!({"role": "user", "content": [{"type": "text", "text": format!("shot {i}")}]}),
            );
            msgs.push(json!({"role": "assistant", "content": [
                {"type": "tool_use", "id": uid, "name": "mcp__playwright__browser_take_screenshot", "input": {}}
            ]}));
            msgs.push(json!({"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": uid, "content": payload.clone()}
            ]}));
        }
        msgs
    }

    fn cfg(tools: &[&str], keep: usize) -> ImageStripConfig {
        ImageStripConfig {
            enabled: true,
            applies_to_tools: tools.iter().map(|s| (*s).to_owned()).collect(),
            keep_recent_count: keep,
            stub: "[trimwire: image stripped]".to_owned(),
        }
    }

    const SCREENSHOT_TOOL: &str = "mcp__playwright__browser_take_screenshot";

    /// The DEFAULT `applies_to_tools` now includes `*snapshot*`, so accessibility/
    /// DOM/heap snapshot tools — which also return large base64 image blobs but
    /// aren't named "screenshot" — are stripped (they previously persisted
    /// unbounded and re-billed every turn).
    #[test]
    fn default_glob_strips_snapshot_tools() {
        let payload = "A".repeat(8192);
        let mk = |tool: &str| {
            let mut msgs = Vec::new();
            for i in 0..3 {
                let uid = format!("t{i}");
                msgs.push(json!({"role": "assistant", "content": [
                    {"type": "tool_use", "id": uid, "name": tool, "input": {}}
                ]}));
                msgs.push(json!({"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": uid, "content": payload.clone()}
                ]}));
            }
            msgs
        };
        let def = ImageStripConfig {
            enabled: true,
            keep_recent_count: 0, // strip all matching
            ..ImageStripConfig::default()
        };
        for tool in [
            "mcp__chrome-devtools__take_snapshot",
            "mcp__playwright__browser_snapshot",
            "mcp__chrome-devtools__take_heapsnapshot",
        ] {
            let mut msgs = mk(tool);
            let stats = apply(&mut msgs, &def).unwrap();
            assert_eq!(
                stats.stubbed, 3,
                "{tool} images should be stripped by the default *snapshot* glob"
            );
        }
    }

    /// 5 screenshots, keep 3 → 2 oldest stripped (the Python keeps-recent test).
    #[test]
    fn keeps_k_most_recent() {
        let mut msgs = screenshot_session(5, 8192);
        let stats = apply(&mut msgs, &cfg(&[SCREENSHOT_TOOL], 3)).unwrap();
        assert_eq!(stats.stubbed, 2);
        // Oldest result stubbed; newest untouched.
        assert!(
            msgs[2]["content"][0]["content"]
                .as_str()
                .unwrap()
                .starts_with("[trimwire: image stripped]")
        );
        assert_ne!(
            msgs[14]["content"][0]["content"],
            json!("[trimwire: image stripped]")
        );
        PairingIndex::build(&msgs).validate().unwrap();
    }

    /// Screenshot-heavy session strips ≥90% of bytes (DEVELOPMENT acceptance).
    #[test]
    fn screenshot_heavy_reduction_over_90pct() {
        let mut msgs = screenshot_session(20, 50_000);
        let stats = apply(&mut msgs, &cfg(&[SCREENSHOT_TOOL], 1)).unwrap();
        assert_eq!(stats.stubbed, 19);
        let ratio = stats.elided_bytes() as f64 / stats.original_bytes as f64;
        assert!(
            ratio > 0.9,
            "expected >90% reduction, got {:.1}%",
            ratio * 100.0
        );
        PairingIndex::build(&msgs).validate().unwrap();
    }

    /// keep_recent_count = 0 strips every matching image.
    #[test]
    fn keep_zero_strips_all() {
        let mut msgs = screenshot_session(4, 8192);
        let stats = apply(&mut msgs, &cfg(&[SCREENSHOT_TOOL], 0)).unwrap();
        assert_eq!(stats.stubbed, 4);
    }

    /// A glob `applies_to` pattern matches the screenshot tool.
    #[test]
    fn glob_applies_to_matches() {
        let mut msgs = screenshot_session(3, 8192);
        let stats = apply(&mut msgs, &cfg(&["*screenshot*"], 0)).unwrap();
        assert_eq!(stats.stubbed, 3);
    }

    /// A short, non-image result is never stubbed even if its tool matches.
    #[test]
    fn small_text_result_not_stripped() {
        let mut msgs = vec![
            json!({"role": "user", "content": [{"type": "text", "text": "shot"}]}),
            json!({"role": "assistant", "content": [
                {"type": "tool_use", "id": "toolu_s0", "name": SCREENSHOT_TOOL, "input": {}}
            ]}),
            json!({"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "toolu_s0", "content": "ok, captured"}
            ]}),
        ];
        let stats = apply(&mut msgs, &cfg(&[SCREENSHOT_TOOL], 0)).unwrap();
        assert_eq!(stats.stubbed, 0);
    }

    /// Structured `[{"type":"image","source":...}]` content is detected.
    #[test]
    fn structured_image_block_detected() {
        let mut msgs = vec![
            json!({"role": "user", "content": [{"type": "text", "text": "shot"}]}),
            json!({"role": "assistant", "content": [
                {"type": "tool_use", "id": "toolu_s0", "name": SCREENSHOT_TOOL, "input": {}}
            ]}),
            json!({"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "toolu_s0", "content": [
                    {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "abc"}}
                ]}
            ]}),
        ];
        let stats = apply(&mut msgs, &cfg(&[SCREENSHOT_TOOL], 0)).unwrap();
        assert_eq!(stats.stubbed, 1);
        assert!(
            msgs[2]["content"][0]["content"]
                .as_str()
                .unwrap()
                .starts_with("[trimwire: image stripped]")
        );
    }

    /// Non-matching tools are ignored entirely.
    #[test]
    fn non_matching_tool_is_noop() {
        let mut msgs = screenshot_session(5, 8192);
        let stats = apply(&mut msgs, &cfg(&["Bash"], 0)).unwrap();
        assert_eq!(stats.stubbed, 0);
        assert_eq!(stats.elided_bytes(), 0);
    }

    #[test]
    fn base64_heuristic_threshold() {
        assert!(!is_base64_image_content(&json!("short")));
        assert!(is_base64_image_content(&json!("A".repeat(MIN_BASE64_LEN))));
        // Long but not base64 alphabet (contains a newline-free non-b64 char).
        let dirty = format!("{}!", "A".repeat(MIN_BASE64_LEN));
        assert!(!is_base64_image_content(&json!(dirty)));
    }

    /// Structured-content edge cases must not false-positive.
    #[test]
    fn structured_image_edge_cases() {
        assert!(!is_base64_image_content(&json!(null)));
        // image block with no source, or a null source, is not a payload.
        assert!(!is_base64_image_content(&json!([{"type": "image"}])));
        assert!(!is_base64_image_content(
            &json!([{"type": "image", "source": null}])
        ));
        assert!(!is_base64_image_content(
            &json!([{"type": "text", "text": "hi"}])
        ));
    }
}
