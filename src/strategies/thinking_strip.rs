//! `ThinkingStrip` — remove `thinking` / `redacted_thinking` blocks from
//! assistant turns **older** than `keep_recent_turns`, keeping recent turns'
//! reasoning verbatim.
//!
//! Why: on a real code session the accumulated OLD thinking is ~22% of the wire
//! body (confirmed via the metadata audit — `num_thinking_blocks`/`thinking_bytes`
//! grow turn over turn; Claude Code does not strip it). The model does not need
//! its own stale reasoning to continue — its *conclusions* live in the text,
//! `tool_use`, and `tool_result` blocks, which carry forward untouched. So
//! dropping old thinking is the single largest model-free reduction available on
//! code sessions, where the surgical strategies (dedup/bloat_cap) barely fire.
//!
//! API contract: Anthropic's extended-thinking rules require a thinking block
//! only for the *in-progress* tool-use turn; completed prior turns' thinking may
//! be dropped. Thinking blocks are also signed — they cannot be *edited* (a stub
//! would fail signature validation), so removal is the only valid transform.
//! We therefore (a) only ever strip turns OUTSIDE the recent window and (b) never
//! touch a `thinking` block's bytes, only drop the whole block.
//!
//! **ON in the `default` profile (2026-06-03).** Both former gates are resolved:
//!   1. Live API-safety — confirmed against real sessions (223 reqs, zero 4xx;
//!      Anthropic accepts a body with OLD thinking removed; the in-progress turn's
//!      thinking is always kept).
//!   2. Cache cost — reprune now REPLAYS thinking-block removals
//!      (`reprune::apply_thinking_removals`, keyed by signature), so the pruned
//!      prefix is byte-identical between checkpoints and the cache holds (one bust
//!      per checkpoint, not per turn). Live run confirmed 92% cache-hit with it on.
//!
//! **Also ON in `gentle` (2026-06-05)** with a conservative `keep_recent_turns = 8`
//! (vs default's 4): it was the only lever that gave gentle meaningful savings on real
//! sessions (real tool output rarely exceeds gentle's 32KB bloat_cap), and it only
//! drops OLD reasoning — never tool_results/inputs/facts — so it stays within gentle's
//! "don't touch the working set" promise. gentle still excludes the aggressive levers
//! (stale_reads, stale_input_cap, sliding_window, image_strip).
//!
//! Safety, by construction: removes only `thinking`/`redacted_thinking` — never
//! `tool_use`/`tool_result`/`text` — so tool pairs (`PairingIndex`) are untouched
//! and `system` is never seen. Never empties an assistant message (a thinking-only
//! turn is left intact). Pure, deterministic, idempotent.
//!
//! Separately, `strip_empty` is an ALWAYS-ON correctness sanitize (not gated by
//! profile or `keep_recent_turns`): it drops API-rejected empty blocks — empty
//! `thinking` (Claude Code re-emits these on resume) and empty `text` (the API
//! emits these alongside `tool_use`, then 400s on round-trip) — from EVERY turn,
//! including the recent window this strategy protects.

use serde_json::Value;

use crate::config::ThinkingStripConfig;
use crate::error::Result;
use crate::pairing::PairingIndex;
use crate::strategies::{Stats, assistant_cutoff, role};

/// Is this content block a thinking block (signed or redacted)?
fn is_thinking(block: &Value) -> bool {
    matches!(
        block.get("type").and_then(Value::as_str),
        Some("thinking") | Some("redacted_thinking")
    )
}

/// A signature-only `thinking` block with an EMPTY `thinking` field
/// (`{"type":"thinking","thinking":"","signature":"…"}`). Claude Code re-emits
/// these on session-resume replay, and Anthropic **rejects them permanently**
/// ("each thinking block must contain thinking") — a hard 400 that bricks the
/// session. (`redacted_thinking` carries `data`, never an empty `thinking`, so
/// it is intentionally excluded.)
fn is_empty_thinking(block: &Value) -> bool {
    block.get("type").and_then(Value::as_str) == Some("thinking")
        && block
            .get("thinking")
            .and_then(Value::as_str)
            .unwrap_or("")
            .is_empty()
}

/// An empty `text` block (`{"type":"text","text":""}`). The Anthropic API itself
/// emits these alongside `tool_use` in assistant turns, then **rejects them with
/// a 400 on the next round-trip** — a wall long tool-heavy sessions hit
/// (confirmed across proxies, e.g. LiteLLM #22930). They carry no content, so
/// dropping them is loss-free.
fn is_empty_text(block: &Value) -> bool {
    block.get("type").and_then(Value::as_str) == Some("text")
        && block
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or("")
            .is_empty()
}

/// API-rejected empty blocks — empty `thinking` OR empty `text` — that must be
/// dropped before forwarding regardless of recency (both 400 on round-trip).
fn is_empty_invalid(block: &Value) -> bool {
    is_empty_thinking(block) || is_empty_text(block)
}

/// Always-on correctness sanitize: drop API-rejected empty blocks (empty
/// `thinking` and empty `text`) from EVERY assistant turn, including the recent
/// window that [`apply_counted`] protects.
///
/// Why this is separate from (and unconditional vs) [`apply`]: `thinking_strip`
/// deliberately keeps *recent* turns' reasoning verbatim (the in-progress
/// tool-use turn's signed thinking must survive). But an EMPTY thinking/text
/// block carries no content to protect and is API-invalid wherever it sits, so it
/// must go even inside the recent window. The model never emits these — they are
/// Claude-Code resume-serialization / API round-trip artifacts — so removing them
/// can never drop real reasoning or content.
///
/// Cache-safe by construction: on a long (reprune) session the old/stable
/// prefix already has all thinking removed-and-replayed, so this only ever acts
/// on the divergent recent tail (which is not part of the cached prefix).
/// Pure, deterministic, idempotent; never empties an assistant message; touches
/// no `tool_use`/`tool_result` block so pairing is unaffected. Returns the
/// number of blocks removed.
pub(crate) fn strip_empty(messages: &mut [Value]) -> usize {
    let mut removed = 0usize;
    for msg in messages.iter_mut() {
        if role(msg) != Some("assistant") {
            continue;
        }
        let Some(content) = msg.get_mut("content").and_then(Value::as_array_mut) else {
            continue;
        };
        // Never empty the message: only strip if a non-empty-invalid block
        // remains (an all-empty turn is left intact — emptying it to `[]` would
        // itself be an invalid message).
        if !content.iter().any(|b| !is_empty_invalid(b)) {
            continue;
        }
        let before = content.len();
        content.retain(|b| !is_empty_invalid(b));
        removed += before - content.len();
    }
    removed
}

/// Remove thinking blocks from assistant turns older than `keep_recent_turns`.
/// `Stats::stubbed` counts the number of thinking blocks removed.
pub fn apply(messages: &mut [Value], cfg: &ThinkingStripConfig) -> Result<Stats> {
    super::with_stats(messages, |m| apply_counted(m, cfg))
}

/// Worker: returns the number of thinking blocks removed. Byte accounting is
/// threaded by the caller ([`super::run`]); see [`apply`].
pub(crate) fn apply_counted(messages: &mut [Value], cfg: &ThinkingStripConfig) -> Result<usize> {
    PairingIndex::build(messages).validate()?;

    // Keep at least the most-recent turn (the in-progress tool-use turn's
    // thinking must survive). `assistant_cutoff` returns the index of the oldest
    // message still inside the recent window's boundary; everything at index
    // <= cutoff is "old". `None` ⇒ history too short to have any old turn.
    let keep = cfg.keep_recent_turns.max(1);
    let Some(cutoff) = assistant_cutoff(messages, keep) else {
        return Ok(0);
    };

    let mut stripped = 0usize;
    for (mi, msg) in messages.iter_mut().enumerate() {
        if mi > cutoff {
            break; // recent window: keep thinking verbatim
        }
        if role(msg) != Some("assistant") {
            continue;
        }
        let Some(content) = msg.get_mut("content").and_then(Value::as_array_mut) else {
            continue;
        };
        // Never empty the message: only strip if a non-thinking block remains
        // (a thinking-only assistant turn is left intact — it'd be invalid empty).
        if !content.iter().any(|b| !is_thinking(b)) {
            continue;
        }
        let before = content.len();
        content.retain(|b| !is_thinking(b));
        stripped += before - content.len();
    }

    Ok(stripped)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn cfg(keep: usize) -> ThinkingStripConfig {
        ThinkingStripConfig {
            enabled: true,
            keep_recent_turns: keep,
        }
    }

    /// Build a session of `turns` assistant turns, each: thinking + tool_use,
    /// then a paired tool_result. Thinking content is `think {i}`.
    fn session(turns: usize) -> Vec<Value> {
        let mut m = Vec::new();
        for i in 0..turns {
            let id = format!("u{i}");
            m.push(json!({"role":"user","content":[{"type":"text","text":format!("ask {i}")}]}));
            m.push(json!({"role":"assistant","content":[
                {"type":"thinking","thinking":format!("think {i} ......................"),"signature":"sig"},
                {"type":"tool_use","id":id,"name":"Bash","input":{"command":format!("run {i}")}}
            ]}));
            m.push(json!({"role":"user","content":[
                {"type":"tool_result","tool_use_id":id,"content":format!("out {i}")}
            ]}));
        }
        m
    }

    fn thinking_count(messages: &[Value]) -> usize {
        messages
            .iter()
            .filter_map(|m| m.get("content").and_then(Value::as_array))
            .flatten()
            .filter(|b| is_thinking(b))
            .count()
    }

    #[test]
    fn strips_old_thinking_keeps_recent() {
        let mut m = session(10);
        let stats = apply(&mut m, &cfg(3)).unwrap();
        // 10 turns, keep 3 → 7 old turns' thinking removed.
        assert_eq!(stats.stubbed, 7, "7 old thinking blocks removed");
        assert_eq!(
            thinking_count(&m),
            3,
            "exactly the recent 3 turns keep thinking"
        );
        assert!(stats.final_bytes < stats.original_bytes, "body shrank");
        // The recent turns' thinking survives verbatim.
        let kept: Vec<&str> = m
            .iter()
            .filter_map(|x| x.get("content").and_then(Value::as_array))
            .flatten()
            .filter(|b| is_thinking(b))
            .filter_map(|b| b.get("thinking").and_then(Value::as_str))
            .collect();
        assert!(kept.iter().all(|t| t.starts_with("think 7")
            || t.starts_with("think 8")
            || t.starts_with("think 9")));
    }

    #[test]
    fn tool_pairs_survive_orphan_free() {
        let mut m = session(8);
        apply(&mut m, &cfg(2)).unwrap();
        PairingIndex::build(&m)
            .validate()
            .expect("no orphans after strip");
        // Every tool_use still has its result and vice-versa: count preserved.
        let uses = m
            .iter()
            .filter_map(|x| x.get("content").and_then(Value::as_array))
            .flatten()
            .filter(|b| b.get("type").and_then(Value::as_str) == Some("tool_use"))
            .count();
        assert_eq!(uses, 8, "no tool_use removed");
    }

    #[test]
    fn idempotent() {
        let mut m = session(10);
        let first = apply(&mut m, &cfg(3)).unwrap();
        let second = apply(&mut m, &cfg(3)).unwrap();
        assert_eq!(first.stubbed, 7);
        assert_eq!(
            second.stubbed, 0,
            "re-run removes nothing (already stripped)"
        );
    }

    #[test]
    fn deterministic() {
        let mut a = session(12);
        let mut b = session(12);
        apply(&mut a, &cfg(4)).unwrap();
        apply(&mut b, &cfg(4)).unwrap();
        assert_eq!(
            serde_json::to_string(&a).unwrap(),
            serde_json::to_string(&b).unwrap()
        );
    }

    #[test]
    fn short_history_is_noop() {
        let mut m = session(3);
        let stats = apply(&mut m, &cfg(6)).unwrap();
        assert_eq!(
            stats.stubbed, 0,
            "fewer turns than keep window → nothing old"
        );
        assert_eq!(thinking_count(&m), 3);
    }

    #[test]
    fn never_empties_a_thinking_only_assistant_turn() {
        // An (unusual) assistant turn that is ONLY a thinking block, made old by
        // appending more turns. It must be left intact, not emptied to [].
        let mut m = vec![json!({"role":"assistant","content":[
            {"type":"thinking","thinking":"lonely old thought","signature":"s"}
        ]})];
        m.extend(session(8));
        let _ = apply(&mut m, &cfg(2)).unwrap();
        let first = m[0]["content"].as_array().unwrap();
        assert_eq!(
            first.len(),
            1,
            "thinking-only turn left intact (never emptied)"
        );
        assert!(is_thinking(&first[0]));
    }

    #[test]
    fn strip_empty_removes_empty_thinking_even_in_recent_window() {
        // An empty (signature-only) thinking block on the MOST-RECENT assistant
        // turn — the one `apply` protects — must still be removed by `strip_empty`,
        // while a real recent thinking block is untouched.
        let mut m = session(3);
        // Make the last assistant turn carry BOTH a real and an empty thinking block.
        let last_assistant = m.len() - 2; // ...user, assistant, user(result)
        m[last_assistant]["content"] = json!([
            {"type":"thinking","thinking":"real recent reasoning","signature":"s"},
            {"type":"thinking","thinking":"","signature":"sig-empty"},
            {"type":"tool_use","id":"ux","name":"Bash","input":{"command":"x"}}
        ]);
        m.push(json!({"role":"user","content":[{"type":"tool_result","tool_use_id":"ux","content":"o"}]}));
        let removed = strip_empty(&mut m);
        assert_eq!(removed, 1, "exactly the empty-thinking block is removed");
        let blocks = m[last_assistant]["content"].as_array().unwrap();
        assert!(
            blocks.iter().any(|b| b.get("thinking").and_then(Value::as_str)
                == Some("real recent reasoning")),
            "the real recent reasoning survives"
        );
        assert!(
            !blocks.iter().any(is_empty_thinking),
            "no empty-thinking block remains"
        );
    }

    #[test]
    fn strip_empty_is_idempotent_and_noop_when_clean() {
        let mut m = session(5); // no empty-thinking blocks anywhere
        assert_eq!(strip_empty(&mut m), 0, "clean session → nothing removed");
        // Plant one, remove it, then a second pass is a no-op.
        m[1]["content"]
            .as_array_mut()
            .unwrap()
            .insert(0, json!({"type":"thinking","thinking":"","signature":"e"}));
        assert_eq!(strip_empty(&mut m), 1);
        assert_eq!(strip_empty(&mut m), 0, "second pass removes nothing");
    }

    #[test]
    fn strip_empty_never_empties_a_message() {
        // An assistant turn that is ONLY an empty-thinking block: leave it intact
        // rather than produce an invalid empty `content: []`.
        let mut m = vec![json!({"role":"assistant","content":[
            {"type":"thinking","thinking":"","signature":"only"}
        ]})];
        assert_eq!(
            strip_empty(&mut m),
            0,
            "lone empty-thinking turn left intact"
        );
        assert_eq!(m[0]["content"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn strip_empty_leaves_tool_pairs_and_redacted_intact() {
        let mut m = session(4);
        // redacted_thinking (carries `data`, no `thinking`) must NOT be treated as empty.
        m[1]["content"]
            .as_array_mut()
            .unwrap()
            .insert(0, json!({"type":"redacted_thinking","data":"abc"}));
        let removed = strip_empty(&mut m);
        assert_eq!(removed, 0, "redacted_thinking is not empty-thinking");
        PairingIndex::build(&m)
            .validate()
            .expect("tool pairs intact after strip_empty");
    }

    #[test]
    fn strip_empty_also_drops_empty_text_blocks() {
        // The API emits {type:text,text:""} alongside tool_use, then 400s on resend.
        let mut m = vec![
            json!({"role":"assistant","content":[
                {"type":"text","text":""},
                {"type":"text","text":"real answer"},
                {"type":"tool_use","id":"a","name":"Bash","input":{"command":"x"}}
            ]}),
            json!({"role":"user","content":[{"type":"tool_result","tool_use_id":"a","content":"o"}]}),
        ];
        let removed = strip_empty(&mut m);
        assert_eq!(removed, 1, "the empty text block is removed");
        let blocks = m[0]["content"].as_array().unwrap();
        assert!(!blocks.iter().any(is_empty_text), "no empty text remains");
        assert!(
            blocks
                .iter()
                .any(|b| b.get("text").and_then(Value::as_str) == Some("real answer")),
            "real text survives"
        );
        assert!(
            blocks
                .iter()
                .any(|b| b.get("type").and_then(Value::as_str) == Some("tool_use")),
            "tool_use survives"
        );
        PairingIndex::build(&m).validate().unwrap();
    }

    #[test]
    fn strip_empty_leaves_an_all_empty_turn_intact() {
        // An assistant turn that is ONLY empty blocks (text + thinking): leave it
        // rather than produce an invalid empty `content: []`.
        let mut m = vec![json!({"role":"assistant","content":[
            {"type":"text","text":""},
            {"type":"thinking","thinking":"","signature":"s"}
        ]})];
        assert_eq!(strip_empty(&mut m), 0, "all-empty turn left intact");
        assert_eq!(m[0]["content"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn keep_zero_is_clamped_to_one() {
        // keep_recent_turns=0 must NOT strip the latest turn's thinking (the
        // in-progress tool-use turn) — it's clamped to 1.
        let mut m = session(5);
        apply(&mut m, &cfg(0)).unwrap();
        assert_eq!(thinking_count(&m), 1, "the most-recent turn keeps thinking");
    }
}
