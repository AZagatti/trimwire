//! Strategy dispatch over the messages array.
//!
//! Each strategy is a pure function over `&mut Vec<Value>` plus its config
//! slice (no I/O — see the layer rules in AGENTS.md). The gateway hands raw
//! request bytes to [`apply_to_body`], which parses, runs the enabled
//! strategies over `messages[]`, and hands back either the original bytes
//! (nothing changed → cache prefix preserved) or freshly-serialized bytes.
//!
//! Shipped strategies, applied by [`run`] in this order:
//! - `FailedInputPurge`: clear `tool_use.input` of old errored calls.
//! - `StaleInputCap`: reduce `tool_use.input` of old *successful* calls
//!   (cache-safe content-overwrite — ON in `default`, off in `gentle`).
//! - `CrossTurnDedup`: keep only the most recent of identical repeated calls.
//! - `StaleReads`: elide superseded file-Read `tool_result` content when a
//!   later Read/Write/Edit/MultiEdit targets the same path
//!   (cache-safe content-overwrite — ON in `default`, off in `gentle`).
//! - `SimHashDedup`: collapse near-duplicate `tool_result` content (SimHash ≤3
//!   Hamming distance); **OPT-IN — off by default, not in any profile**. Runs
//!   here (5th, after StaleReads, before BloatCap) when enabled.
//! - `BloatCap`: trim each oversized old string `tool_result` to head+tail.
//! - `SlidingWindow`: stub denylisted tool pairs older than N turns.
//! - `ImageStrip`: replace old base64 image payloads with a marker.
//! - `ThinkingStrip`: remove `thinking` blocks from old assistant turns
//!   (ON in `default`, off in `gentle`).
//!
//! All EIGHT main strategies are ON in `default` (the aggressive shipped profile);
//! `gentle` runs a conservative subset (`CrossTurnDedup` + `FailedInputPurge` + a
//! conservative `BloatCap` + `ThinkingStrip` with a wide keep-window — without it
//! gentle saved ≈0% on real sessions). `ThinkingStrip` removes blocks, but reprune REPLAYS
//! those removals by signature (`reprune::apply_thinking_removals`) so the pruned
//! prefix stays byte-identical between checkpoints and the cache holds (one bust per
//! checkpoint, not per turn — live-confirmed 92% cache-hit). `StaleReads` additionally
//! elides superseded Read *results* and demand-pages old large current-view reads
//! (authored Write/Edit inputs are never touched — eliding them corrupted sessions, §13A).

pub mod bloat_cap;
pub mod cross_turn_dedup;
pub mod failed_input_purge;
pub mod image_strip;
pub mod simhash_dedup;
pub mod sliding_window;
pub mod stale_input_cap;
pub mod stale_reads;
pub mod system_shape_normalize;
pub mod thinking_strip;

use serde_json::Value;

use crate::config::Config;
use crate::error::Result;
use crate::pairing::PairingIndex;

/// Per-strategy outcome. `original_bytes`/`final_bytes` are the compact
/// serialized length of `messages[]` before/after the strategy ran.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stats {
    pub stubbed: usize,
    pub original_bytes: usize,
    pub final_bytes: usize,
}

impl Stats {
    /// Bytes removed (negative if the stub text grew the body — possible on
    /// tiny synthetic payloads; mirrors the Python reference's signed delta).
    pub fn elided_bytes(&self) -> i64 {
        self.original_bytes as i64 - self.final_bytes as i64
    }
}

/// Run every enabled strategy in order over `messages`, returning the
/// `(name, Stats)` of each one that ran. Strategies mutate in place; a
/// strategy returning `Err` (e.g. pre/post orphan-validation failure) aborts
/// the run so the caller can roll back.
pub fn run(messages: &mut [Value], cfg: &Config) -> Result<Vec<(&'static str, Stats)>> {
    let mut fired = Vec::new();
    // Thread the serialized length across strategies instead of having each one
    // re-serialize the whole array twice for its own byte accounting. The array
    // is contiguous between strategies, so strategy N's "after" length is
    // strategy N+1's "before" length. We serialize once up front, then once after
    // any strategy that ACTUALLY changed something (stubbed > 0) — turning the old
    // ~2×(enabled) full-array serializations into 1 + (fired) ones. Measured: the
    // per-strategy accounting serialization was ~54% of `run()` on a 647 KB body.
    let mut prev_len = serialized_len(messages);

    if cfg.strategies.failed_input_purge.enabled {
        let n = failed_input_purge::apply_counted(messages, &cfg.strategies.failed_input_purge)?;
        push_stats(&mut fired, "failed_input_purge", n, messages, &mut prev_len);
    }
    // ON in `default`, off in `gentle`. Runs right after failed_input_purge
    // so both input strategies cover their respective domains in one pass; the
    // two strategies are mutually exclusive on each call (failed vs. successful).
    if cfg.strategies.stale_input_cap.enabled {
        let n = stale_input_cap::apply_counted(messages, &cfg.strategies.stale_input_cap)?;
        push_stats(&mut fired, "stale_input_cap", n, messages, &mut prev_len);
    }
    if cfg.strategies.cross_turn_dedup.enabled {
        let n = cross_turn_dedup::apply_counted(messages, &cfg.strategies.cross_turn_dedup)?;
        push_stats(&mut fired, "cross_turn_dedup", n, messages, &mut prev_len);
    }
    // ON in `default`, off in `gentle`. Runs after cross_turn_dedup so
    // dedup can first collapse identical Read pairs (same-input exact repeats)
    // before stale_reads handles the broader case where later Write/Edit
    // supersede earlier Reads of the same path.
    if cfg.strategies.stale_reads.enabled {
        let n = stale_reads::apply_counted(messages, &cfg.strategies.stale_reads)?;
        push_stats(&mut fired, "stale_reads", n, messages, &mut prev_len);
    }
    // OPT-IN (off by default, not in any profile): catches near-duplicate
    // tool_results that cross_turn_dedup misses (SimHash ≤ hamming_threshold).
    if cfg.strategies.simhash_dedup.enabled {
        let n = simhash_dedup::apply_counted(messages, &cfg.strategies.simhash_dedup)?;
        push_stats(&mut fired, "simhash_dedup", n, messages, &mut prev_len);
    }
    if cfg.strategies.bloat_cap.enabled {
        let n = bloat_cap::apply_counted(messages, &cfg.strategies.bloat_cap)?;
        push_stats(&mut fired, "bloat_cap", n, messages, &mut prev_len);
    }
    if cfg.strategies.sliding_window.enabled {
        let n = sliding_window::apply_counted(messages, &cfg.strategies.sliding_window)?;
        push_stats(&mut fired, "sliding_window", n, messages, &mut prev_len);
    }
    if cfg.strategies.image_strip.enabled {
        let n = image_strip::apply_counted(messages, &cfg.strategies.image_strip)?;
        push_stats(&mut fired, "image_strip", n, messages, &mut prev_len);
    }
    // ON in `default`, off in `gentle`. Runs last because it REMOVES blocks
    // (thinking) rather than rewriting content; reprune replays those removals by
    // signature (`reprune::apply_thinking_removals`) so the cache still holds.
    if cfg.strategies.thinking_strip.enabled {
        let n = thinking_strip::apply_counted(messages, &cfg.strategies.thinking_strip)?;
        push_stats(&mut fired, "thinking_strip", n, messages, &mut prev_len);
    }
    // Single post-validate backstop (I4b). Each strategy still PRE-validates its
    // input (catching an orphan from the prior strategy or a malformed body); no
    // strategy removes a tool_use/tool_result block (they only overwrite content
    // or remove thinking blocks), so none can orphan a pair — the former
    // per-strategy POST-validate was redundant. One final check here guards the
    // last strategy's output and future strategies, so an orphan still rolls the
    // whole request back to original bytes (callers forward on Err).
    PairingIndex::build(messages).validate()?;
    Ok(fired)
}

/// Record one strategy's `Stats` while threading the running serialized length:
/// `original_bytes` is the length before it ran (`*prev_len`); `final_bytes` is
/// the length after — recomputed only if the strategy changed something, else it
/// is unchanged (no serialization). Updates `*prev_len` for the next strategy.
/// Produces byte-identical per-strategy accounting to the former in-strategy
/// double-serialize, so the ledger telemetry is unchanged.
fn push_stats(
    fired: &mut Vec<(&'static str, Stats)>,
    name: &'static str,
    stubbed: usize,
    messages: &[Value],
    prev_len: &mut usize,
) {
    let final_bytes = if stubbed > 0 {
        serialized_len(messages)
    } else {
        *prev_len
    };
    fired.push((
        name,
        Stats {
            stubbed,
            original_bytes: *prev_len,
            final_bytes,
        },
    ));
    *prev_len = final_bytes;
}

/// Wrap a counting strategy worker (`apply_counted`) into the public `Stats`-
/// returning `apply` form used by tests and external callers. Serializes the
/// array before/after to fill `original_bytes`/`final_bytes`; the production hot
/// path uses `apply_counted` + [`push_stats`] threading instead, so it never pays
/// this double serialization.
pub(crate) fn with_stats(
    messages: &mut [Value],
    work: impl FnOnce(&mut [Value]) -> Result<usize>,
) -> Result<Stats> {
    let original_bytes = serialized_len(messages);
    let stubbed = work(messages)?;
    let final_bytes = serialized_len(messages);
    Ok(Stats {
        stubbed,
        original_bytes,
        final_bytes,
    })
}

// ---- Shared helpers for the strategy modules (read/mutate messages[]) ----

/// `messages[i].role` as a string slice.
pub(crate) fn role(msg: &Value) -> Option<&str> {
    msg.get("role").and_then(Value::as_str)
}

/// Mutable reference to `messages[mi].content[ci]`.
pub(crate) fn block_mut(messages: &mut [Value], (mi, ci): (usize, usize)) -> Option<&mut Value> {
    messages
        .get_mut(mi)?
        .get_mut("content")?
        .as_array_mut()?
        .get_mut(ci)
}

/// Compact serialized byte length of the messages array (matches Python's
/// `json.dumps(messages, separators=(",", ":"))` length).
pub(crate) fn serialized_len(messages: &[Value]) -> usize {
    serde_json::to_vec(messages).map(|v| v.len()).unwrap_or(0)
}

/// Returns `true` when `content` is Claude Code's own micro-compact marker
/// `"[Old tool result content cleared]"` — which appears on the wire when the user
/// has already compacted the context. Matches BOTH the bare-string form and the
/// single-text-block array form `[{"type":"text","text":"[Old tool result content
/// cleared]"}]` (Claude Code has emitted both shapes). Strategies skip results
/// carrying this marker to avoid double-elision (their own stub would only grow the
/// body, and the marker means the model already knows the content is gone).
pub(crate) fn is_already_cleared(content: &Value) -> bool {
    const MARKER: &str = "[Old tool result content cleared]";
    match content {
        Value::String(s) => s == MARKER,
        // A single text block whose text is exactly the marker (nothing else).
        Value::Array(blocks) => {
            blocks.len() == 1
                && blocks[0].get("type").and_then(Value::as_str) == Some("text")
                && blocks[0].get("text").and_then(Value::as_str) == Some(MARKER)
        }
        _ => false,
    }
}

/// Build an elision marker that records *how much* was dropped:
/// `"<stub> (<N>B elided)"`, where N is the serialized byte length of the
/// `content` being replaced. This is a deterministic, **content-free** marker:
/// it keeps the elision auditable (and the ledger's byte accounting honest) and
/// makes each marker distinct so identical elisions don't collapse — without
/// reintroducing any of the dropped content (so it can never leak stripped
/// image bytes). It is *not* a model-actionable summary; a raw byte count isn't
/// something the model can act on (bloat_cap, by contrast, keeps real head+tail
/// text). N is `serde_json::to_string(content).len()` (raw UTF-8); the Python
/// reference matches it with `ensure_ascii=False`, sorted-key, compact.
pub(crate) fn elision_marker(stub: &str, content: &Value) -> Value {
    let n = serde_json::to_string(content).map(|s| s.len()).unwrap_or(0);
    Value::String(format!("{stub} ({n}B elided)"))
}

/// File-authoring tools whose `input` carries the file body the model wrote
/// (`content` / `new_string` / `old_string`). NO input-elision strategy may ever
/// reduce these — it is a HARD floor the user config cannot remove. Eliding
/// authored content makes the model rebuild on a body it can no longer see and
/// reproduce the elision MARKER as the file content (§13A: confirmed live across
/// multiple real sessions — Writes/Edits landed on disk as `[trimwire: NNNN input
/// elided]`, and the marker then cascaded as corrupted files were re-read and
/// re-copied). There is no legitimate reason to elide authored content (the model
/// re-authors from it on a retry/iterate), so the exemption is unconditional and
/// applies to BOTH successful (`stale_input_cap`) and failed (`failed_input_purge`)
/// authored calls. `NotebookEdit` is included — it authors cell source
/// (`new_source`), the same corruption class as a file body.
pub(crate) const AUTHORING_TOOLS: &[&str] = &["Write", "Edit", "MultiEdit", "NotebookEdit"];

#[cfg(test)]
#[test]
fn is_already_cleared_matches_exact_marker() {
    use serde_json::json;
    assert!(
        is_already_cleared(&json!("[Old tool result content cleared]")),
        "exact CC marker must return true"
    );
    assert!(
        !is_already_cleared(&json!("[Old tool result content cleared] (123B elided)")),
        "must not match when marker has a suffix"
    );
    assert!(
        !is_already_cleared(&json!("some other content")),
        "must not match other strings"
    );
    assert!(!is_already_cleared(&json!(null)), "must not match null");
    assert!(!is_already_cleared(&json!({})), "must not match objects");
    // The single-text-block array form CC also emits.
    assert!(
        is_already_cleared(&json!([{"type": "text", "text": "[Old tool result content cleared]"}])),
        "single text block carrying the marker must return true"
    );
    assert!(
        !is_already_cleared(&json!([
            {"type": "text", "text": "[Old tool result content cleared]"},
            {"type": "text", "text": "and more"}
        ])),
        "must not match when the array has extra content"
    );
    assert!(
        !is_already_cleared(&json!([{"type": "text", "text": "real output"}])),
        "must not match an ordinary single text block"
    );
}

#[cfg(test)]
#[test]
fn elision_marker_size_is_raw_utf8_len() {
    // Locks the Rust side of the Rust↔Python parity: N is the raw-UTF-8 byte
    // length, NOT an ASCII-escaped length. (`"héllo"` → 6 content bytes + 2
    // quotes = 8; an ASCII-escaped count would be 12 and break parity.)
    let m = elision_marker("[trimwire: x]", &Value::String("héllo".to_owned()));
    assert_eq!(m.as_str().unwrap(), "[trimwire: x] (8B elided)");
}

/// Index of the oldest message still inside the "recent" window: walking from
/// the end, the message holding the `(keep_recent_turns + 1)`-th assistant
/// turn. Everything at or before it is "old". `None` if history is too short.
/// Shared by `SlidingWindow` and `FailedInputPurge`.
pub(crate) fn assistant_cutoff(messages: &[Value], keep_recent_turns: usize) -> Option<usize> {
    let mut assistant_seen = 0usize;
    for (i, msg) in messages.iter().enumerate().rev() {
        if role(msg) == Some("assistant") {
            assistant_seen += 1;
            if assistant_seen > keep_recent_turns {
                return Some(i);
            }
        }
    }
    None
}

/// Result of applying strategies to a raw request body.
pub enum BodyOutcome {
    /// Forward the original bytes verbatim — not JSON, no `messages[]`,
    /// nothing fired, or a strategy errored (rolled back). Keeping the exact
    /// original bytes preserves Anthropic's prompt-cache prefix (SPIKE.md §9).
    Unchanged,
    /// The body was mutated; carries the re-serialized bytes and per-strategy
    /// stats.
    Mutated {
        bytes: Vec<u8>,
        fired: Vec<(&'static str, Stats)>,
    },
}

/// Parse `body`, run enabled strategies over `messages[]`, and return whether
/// the body changed. Never panics and never partially mutates the forwarded
/// body: any parse error, missing `messages[]`, no-op run, or strategy error
/// yields [`BodyOutcome::Unchanged`] (the caller forwards the original bytes).
///
/// Only `messages[]` is ever touched — the `system` field and everything else
/// in the request are left exactly as received (SPIKE.md §1 / §9).
pub fn apply_to_body(body: &[u8], cfg: &Config) -> BodyOutcome {
    let Ok(mut root) = serde_json::from_slice::<Value>(body) else {
        return BodyOutcome::Unchanged;
    };

    // POC, opt-in (default OFF): repair a stray `messages[0].role:"system"` into
    // the top-level `system` field before pruning, turning a guaranteed Anthropic
    // 400 into a valid request. Operates on the already-parsed root (no extra
    // parse) and only on that malformed shape; well-formed bodies are untouched.
    let normalized = cfg.strategies.system_shape_normalize.enabled
        && system_shape_normalize::normalize(&mut root);

    let (fired, empty_thinking_removed) = {
        let Some(messages) = root.get_mut("messages").and_then(Value::as_array_mut) else {
            return BodyOutcome::Unchanged;
        };
        let fired = match run(messages, cfg) {
            Ok(fired) => fired,
            // Pre/post validation failed (orphan): forward the ORIGINAL body. Log it
            // (WARN) — this is a graceful degradation, but a SILENT one was
            // undiagnosable; `TRIMWIRE_LOG=warn` now surfaces how often it happens.
            Err(e) => {
                tracing::warn!(error = %e, "trimwire: pruning rolled back (orphan/validation) — forwarding the original unpruned body");
                return BodyOutcome::Unchanged;
            }
        };
        // Always-on correctness sanitize (independent of any strategy or profile,
        // and pairing-safe by construction): drop empty-`thinking` blocks that
        // Claude Code re-emits on resume and Anthropic rejects with a hard 400.
        // Runs on the already-parsed body, so it adds no extra parse.
        let empty_thinking_removed = thinking_strip::strip_empty(messages);
        (fired, empty_thinking_removed)
    };

    // Nothing actually changed → forward original bytes (exact cache prefix).
    if fired.iter().all(|(_, s)| s.stubbed == 0) && empty_thinking_removed == 0 && !normalized {
        return BodyOutcome::Unchanged;
    }

    match serde_json::to_vec(&root) {
        Ok(bytes) => BodyOutcome::Mutated { bytes, fired },
        // Re-serializing a Value we just parsed essentially never fails; if it does,
        // forward the original rather than corrupt the request — but log it.
        Err(e) => {
            tracing::warn!(error = %e, "trimwire: re-serialize after pruning failed — forwarding the original body");
            BodyOutcome::Unchanged
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{
        BloatCapConfig, Config, CrossTurnDedupConfig, FailedInputPurgeConfig, ImageStripConfig,
        SlidingWindowConfig, StaleReadsConfig,
    };
    use serde_json::json;

    /// A config with SlidingWindow enabled (denylist Bash, keep 1).
    fn firing_config() -> Config {
        let mut cfg = Config::default();
        cfg.strategies.sliding_window = SlidingWindowConfig {
            enabled: true,
            keep_recent_turns: 1,
            denylist_tools: vec!["Bash".to_owned()],
            exempt_tools: vec![],
            stub: "[trimwire: elided, older than sliding window]".to_owned(),
        };
        cfg
    }

    #[test]
    fn system_shape_normalize_gated_and_end_to_end() {
        // A body Anthropic would 400: role:"system" as messages[0], no top-level system.
        let body = serde_json::to_vec(&json!({
            "model": "claude",
            "messages": [
                {"role": "system", "content": "you are helpful"},
                {"role": "user", "content": [{"type":"text","text":"hi"}]}
            ]
        }))
        .unwrap();

        // OFF by default → trimwire forwards the malformed body unchanged (no repair).
        let off = Config::default();
        assert!(
            matches!(apply_to_body(&body, &off), BodyOutcome::Unchanged),
            "off by default → malformed body forwarded unchanged"
        );

        // ON → the stray system is lifted to the top level and the body becomes valid.
        let mut on = Config::default();
        on.strategies.system_shape_normalize.enabled = true;
        match apply_to_body(&body, &on) {
            BodyOutcome::Mutated { bytes, .. } => {
                let v: Value = serde_json::from_slice(&bytes).unwrap();
                assert_eq!(v["system"], json!("you are helpful"), "system lifted");
                let msgs = v["messages"].as_array().unwrap();
                assert_eq!(msgs.len(), 1, "stray system message removed");
                assert_eq!(msgs[0]["role"], json!("user"));
            }
            BodyOutcome::Unchanged => panic!("enabled normalize must mutate the malformed body"),
        }
    }

    /// A full `/v1/messages` envelope with `turns` old Bash tool pairs.
    fn body_with_turns(turns: usize) -> Vec<u8> {
        let mut messages = Vec::new();
        for i in 0..turns {
            let uid = format!("toolu_{i}");
            messages.push(
                json!({"role": "user", "content": [{"type": "text", "text": format!("t{i}")}]}),
            );
            messages.push(json!({"role": "assistant", "content": [
                {"type": "tool_use", "id": uid, "name": "Bash", "input": {"command": format!("echo {i}")}}
            ]}));
            messages.push(json!({"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": uid, "content": format!("out {i}")}
            ]}));
        }
        let body = json!({
            "model": "claude-3-5-sonnet-20241022",
            "max_tokens": 1024,
            "system": "You are Claude.",
            "tools": [{"name": "Bash", "input_schema": {"type": "object"}}],
            "messages": messages,
        });
        serde_json::to_vec(&body).unwrap()
    }

    /// A firing mutation leaves every top-level field except `messages`
    /// byte-identical — the cache-prefix + don't-touch-`system` guarantee
    /// (SPIKE.md §1 / §9).
    #[test]
    fn mutation_preserves_everything_except_messages() {
        let body = body_with_turns(6);
        let out = match apply_to_body(&body, &firing_config()) {
            BodyOutcome::Mutated { bytes, fired } => {
                assert!(
                    fired.iter().any(|(_, s)| s.stubbed > 0),
                    "should have stubbed"
                );
                bytes
            }
            BodyOutcome::Unchanged => panic!("expected a mutation"),
        };
        let before: Value = serde_json::from_slice(&body).unwrap();
        let after: Value = serde_json::from_slice(&out).unwrap();
        for key in ["model", "max_tokens", "system", "tools"] {
            assert_eq!(
                before[key], after[key],
                "top-level `{key}` must be untouched"
            );
        }
        assert_ne!(
            before["messages"], after["messages"],
            "messages should change"
        );
    }

    /// No-op run forwards the original bytes verbatim (exact cache prefix).
    #[test]
    fn noop_run_is_unchanged() {
        // Denylist matches nothing in this body → stubbed == 0.
        let mut cfg = firing_config();
        cfg.strategies.sliding_window.denylist_tools = vec!["NoSuchTool".to_owned()];
        let body = body_with_turns(6);
        assert!(matches!(apply_to_body(&body, &cfg), BodyOutcome::Unchanged));
    }

    /// Strategies disabled → never mutates, even on a prunable body.
    #[test]
    fn disabled_strategies_are_unchanged() {
        let body = body_with_turns(6);
        assert!(matches!(
            apply_to_body(&body, &Config::default()),
            BodyOutcome::Unchanged
        ));
    }

    /// Malformed JSON forwards verbatim (rollback path).
    #[test]
    fn malformed_json_is_unchanged() {
        assert!(matches!(
            apply_to_body(b"this is not json{", &firing_config()),
            BodyOutcome::Unchanged
        ));
    }

    /// A body with no `messages[]` forwards verbatim.
    #[test]
    fn missing_messages_is_unchanged() {
        assert!(matches!(
            apply_to_body(br#"{"model":"claude"}"#, &firing_config()),
            BodyOutcome::Unchanged
        ));
    }

    /// `messages` present but not an array forwards verbatim.
    #[test]
    fn non_array_messages_is_unchanged() {
        assert!(matches!(
            apply_to_body(br#"{"messages":42}"#, &firing_config()),
            BodyOutcome::Unchanged
        ));
    }

    /// Both strategies enabled run in order (SlidingWindow then ImageStrip)
    /// over one body without double-stubbing or introducing orphans.
    #[test]
    fn both_strategies_fire_together() {
        // 5 old Bash turns + 5 old screenshot turns; keep 1 of each.
        let mut messages = Vec::new();
        for i in 0..5 {
            let uid = format!("toolu_b{i}");
            messages.push(
                json!({"role": "user", "content": [{"type": "text", "text": format!("b{i}")}]}),
            );
            messages.push(json!({"role": "assistant", "content": [
                {"type": "tool_use", "id": uid, "name": "Bash", "input": {"command": format!("echo {i}")}}
            ]}));
            messages.push(json!({"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": uid, "content": "x".repeat(300)}
            ]}));
        }
        for i in 0..5 {
            let uid = format!("toolu_s{i}");
            messages.push(
                json!({"role": "user", "content": [{"type": "text", "text": format!("s{i}")}]}),
            );
            messages.push(json!({"role": "assistant", "content": [
                {"type": "tool_use", "id": uid, "name": "mcp__playwright__browser_take_screenshot", "input": {}}
            ]}));
            messages.push(json!({"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": uid, "content": "A".repeat(5000)}
            ]}));
        }
        let body = serde_json::to_vec(&json!({"model": "claude", "messages": messages})).unwrap();

        let mut cfg = Config::default();
        cfg.strategies.sliding_window = SlidingWindowConfig {
            enabled: true,
            keep_recent_turns: 1,
            denylist_tools: vec!["Bash".to_owned()],
            exempt_tools: vec![],
            stub: "[trimwire: elided, older than sliding window]".to_owned(),
        };
        cfg.strategies.image_strip = ImageStripConfig {
            enabled: true,
            applies_to_tools: vec!["mcp__playwright__browser_take_screenshot".to_owned()],
            keep_recent_count: 1,
            stub: "[trimwire: image stripped]".to_owned(),
        };

        let out = match apply_to_body(&body, &cfg) {
            BodyOutcome::Mutated { bytes, fired } => {
                let names: Vec<_> = fired.iter().map(|(n, _)| *n).collect();
                assert_eq!(
                    names,
                    vec!["sliding_window", "image_strip"],
                    "ordered, both fired"
                );
                assert!(fired.iter().all(|(_, s)| s.stubbed > 0));
                bytes
            }
            BodyOutcome::Unchanged => panic!("expected both strategies to fire"),
        };

        // Forwarded body is orphan-free, and both stub markers are present.
        let after: Value = serde_json::from_slice(&out).unwrap();
        let messages = after["messages"].as_array().unwrap();
        crate::pairing::PairingIndex::build(messages)
            .validate()
            .expect("no orphans after combined run");
        let text = serde_json::to_string(messages).unwrap();
        assert!(text.contains("older than sliding window"));
        assert!(text.contains("image stripped"));
    }

    /// The two default workhorses (dedup + failed-input-purge) fire together
    /// through `apply_to_body`: both run, the prefix/system survive, and the
    /// result is orphan-free.
    #[test]
    fn both_new_strategies_fire_together() {
        let mut messages = vec![
            json!({"role": "assistant", "content": [
                {"type": "tool_use", "id": "b0", "name": "Bash",
                 "input": {"command": "boom", "stdin": "y".repeat(2000)}}
            ]}),
            json!({"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "b0", "is_error": true, "content": "boom failed"}
            ]}),
        ];
        // 6 identical reads (dedup → 5 superseded); the errored Bash above is
        // old once these are appended, so failed_input_purge clears its input.
        for i in 0..6 {
            let uid = format!("r{i}");
            messages.push(json!({"role": "user", "content": [{"type": "text", "text": "read"}]}));
            messages.push(json!({"role": "assistant", "content": [
                {"type": "tool_use", "id": uid, "name": "Read", "input": {"path": "/f"}}
            ]}));
            messages.push(json!({"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": uid, "content": "x".repeat(100)}
            ]}));
        }
        let body =
            serde_json::to_vec(&json!({"model": "claude", "system": "S", "messages": messages}))
                .unwrap();

        let mut cfg = Config::default();
        cfg.strategies.cross_turn_dedup = CrossTurnDedupConfig {
            enabled: true,
            ..Default::default()
        };
        cfg.strategies.failed_input_purge = FailedInputPurgeConfig {
            enabled: true,
            ..Default::default()
        };

        let out = match apply_to_body(&body, &cfg) {
            BodyOutcome::Mutated { bytes, fired } => {
                let names: Vec<_> = fired.iter().map(|(n, _)| *n).collect();
                assert!(
                    names.contains(&"failed_input_purge") && names.contains(&"cross_turn_dedup")
                );
                bytes
            }
            BodyOutcome::Unchanged => panic!("expected both strategies to fire"),
        };
        let before: Value = serde_json::from_slice(&body).unwrap();
        let after: Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(before["system"], after["system"], "system preserved");
        assert_eq!(before["model"], after["model"]);
        let msgs = after["messages"].as_array().unwrap();
        crate::pairing::PairingIndex::build(msgs)
            .validate()
            .expect("no orphans");
        let text = serde_json::to_string(msgs).unwrap();
        assert!(text.contains("superseded by a later identical call"));
        // Shape-preserving purge: the bulky stdin is elided, the command is kept.
        let purged_input = &after["messages"][0]["content"][0]["input"];
        assert_eq!(purged_input["command"], json!("boom"), "command kept");
        assert!(
            purged_input["stdin"]
                .as_str()
                .unwrap()
                .starts_with("[trimwire:"),
            "bulky stdin elided"
        );
    }

    /// StaleReads + BloatCap compose safely on one body. They can now BOTH target an
    /// old Read result (since the "Read coverage gap" fix, bloat_cap age-gates Read
    /// instead of exempting it outright), but stale_reads runs first and stamps a
    /// `[trimwire: ` marker, which bloat_cap then skips (marker-idempotency) — so a
    /// superseded Read and an old bulky Bash result are each handled once, no
    /// double-stub, no orphan, and the pass is idempotent.
    #[test]
    fn stale_reads_and_bloat_cap_compose_without_orphans() {
        let mut cfg = Config::default();
        cfg.strategies.stale_reads = StaleReadsConfig {
            enabled: true,
            ..Default::default()
        };
        cfg.strategies.bloat_cap = BloatCapConfig {
            enabled: true,
            threshold_bytes: 200,
            head_bytes: 20,
            tail_bytes: 20,
            keep_recent_turns: 1,
            ..Default::default()
        };

        let big = "x".repeat(1000);
        let mut messages = Vec::new();
        // turn 0: Read a.txt with a big result (later superseded → StaleReads stubs it).
        messages.push(json!({"role": "user", "content": [{"type": "text", "text": "read it"}]}));
        messages.push(json!({"role": "assistant", "content": [
            {"type": "tool_use", "id": "r0", "name": "Read", "input": {"file_path": "a.txt"}}
        ]}));
        messages.push(json!({"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": "r0", "content": big.clone()}
        ]}));
        // turn 1: a big Bash result (not exempt, ages past keep_recent → BloatCap trims).
        messages.push(json!({"role": "assistant", "content": [
            {"type": "tool_use", "id": "b1", "name": "Bash", "input": {"command": "cat big"}}
        ]}));
        messages.push(json!({"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": "b1", "content": big.clone()}
        ]}));
        // a couple more turns so the Bash result is older than keep_recent_turns.
        for i in 0..2 {
            messages.push(json!({"role": "assistant", "content": [
                {"type": "tool_use", "id": format!("n{i}"), "name": "Bash", "input": {"command": "echo"}}
            ]}));
            messages.push(json!({"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": format!("n{i}"), "content": "ok"}
            ]}));
        }
        // last turn: Read a.txt AGAIN → supersedes the turn-0 Read.
        messages.push(json!({"role": "assistant", "content": [
            {"type": "tool_use", "id": "r9", "name": "Read", "input": {"file_path": "a.txt"}}
        ]}));
        messages.push(json!({"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": "r9", "content": "fresh view"}
        ]}));

        let body = serde_json::to_vec(&json!({
            "model": "claude-3-5-sonnet-20241022",
            "max_tokens": 1024,
            "messages": messages,
        }))
        .unwrap();

        let out = match apply_to_body(&body, &cfg) {
            BodyOutcome::Mutated { bytes, fired } => {
                let names: Vec<&str> = fired
                    .iter()
                    .filter(|(_, s)| s.stubbed > 0)
                    .map(|(n, _)| *n)
                    .collect();
                assert!(
                    names.contains(&"stale_reads"),
                    "stale_reads should fire: {names:?}"
                );
                assert!(
                    names.contains(&"bloat_cap"),
                    "bloat_cap should fire: {names:?}"
                );
                bytes
            }
            BodyOutcome::Unchanged => panic!("expected both strategies to mutate"),
        };

        // Idempotent: a second pass over the already-pruned body is a no-op
        // (pairing held — apply_to_body validates pre/post, so reaching here proves it).
        assert!(
            matches!(apply_to_body(&out, &cfg), BodyOutcome::Unchanged),
            "second pass must be a stable no-op"
        );
    }

    /// A pre-existing orphan makes the strategy error → rollback to original.
    #[test]
    fn orphaned_input_rolls_back() {
        let body = json!({
            "model": "claude",
            "messages": [
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "toolu_ghost", "content": "x"}
                ]}
            ],
        });
        let bytes = serde_json::to_vec(&body).unwrap();
        assert!(matches!(
            apply_to_body(&bytes, &firing_config()),
            BodyOutcome::Unchanged
        ));
    }
}
