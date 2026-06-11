//! Tool-use ↔ tool-result pairing index.
//!
//! THE load-bearing correctness module. See SPIKE.md §5 "Pairing invariants".
//! Mirrors the Python reference at `tests/phase0/pairing.py`; the two must
//! produce identical results on the shared fixture corpus.
//!
//! Invariants enforced around every strategy:
//! 1. Pre-mutation: every `tool_result.tool_use_id` has a matching
//!    `tool_use.id` in an earlier message.
//! 2. Strategies always drop pairs atomically (both halves of a pair, never
//!    one side) — they look both halves up here via [`PairingIndex::pair`].
//! 3. Parallel tool calls (multiple `tool_use` blocks in one assistant
//!    message) are treated independently — each pair stands on its own.
//! 4. Post-mutation: `strategies::run` re-runs the orphan check ONCE after the
//!    whole pipeline (each strategy still pre-validates its input). If any orphan
//!    appeared, `run` errors → the caller rolls back to the original body.
//!
//! This module **only reads** `messages[]`; mutation lives in `strategies/`.

use std::collections::HashMap;

use serde_json::Value;

use crate::error::{Error, Result};

/// Location of a content block within the request: `(message_idx, content_idx)`.
type Loc = (usize, usize);

/// Maps each `tool_use_id` to the location of its `tool_use` block and its
/// `tool_result` block. Built in a single pass over `messages[]`.
///
/// Both maps are keyed by the same id string (`tool_use.id` ==
/// `tool_result.tool_use_id`), so a fully-paired call appears once in each.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct PairingIndex {
    /// `tool_use.id` → location of the `tool_use` block.
    pub uses: HashMap<String, Loc>,
    /// `tool_result.tool_use_id` → location of the `tool_result` block.
    pub results: HashMap<String, Loc>,
}

impl PairingIndex {
    /// Build the index in one pass. Non-list `content` and non-object blocks
    /// are skipped (matches the Python reference's defensive `isinstance`
    /// guards — Anthropic also allows a bare-string `content`, which carries
    /// no tool blocks). On duplicate ids, last write wins, mirroring the
    /// Python `dict` assignment.
    pub fn build(messages: &[Value]) -> Self {
        let mut idx = PairingIndex::default();
        for (mi, msg) in messages.iter().enumerate() {
            let Some(content) = msg.get("content").and_then(Value::as_array) else {
                continue;
            };
            for (ci, block) in content.iter().enumerate() {
                match block.get("type").and_then(Value::as_str) {
                    Some("tool_use") => {
                        if let Some(id) = block.get("id").and_then(Value::as_str) {
                            idx.uses.insert(id.to_owned(), (mi, ci));
                        }
                    }
                    Some("tool_result") => {
                        if let Some(id) = block.get("tool_use_id").and_then(Value::as_str) {
                            idx.results.insert(id.to_owned(), (mi, ci));
                        }
                    }
                    _ => {}
                }
            }
        }
        idx
    }

    /// Return `Err(OrphanResult)` if any `tool_result` has no matching
    /// `tool_use`. Checks only the `tool_result → tool_use` direction: the
    /// reverse (a `tool_use` with no `tool_result`) is legitimate mid-turn
    /// (the assistant turn that issued the call may be the last message),
    /// and Anthropic's API tolerates it. See the note in
    /// `tests/phase0/pairing.py`.
    ///
    /// Ordering is intentionally **not** enforced. SPIKE.md §5 invariant 1
    /// describes a matching `tool_use` "in an earlier message" — that is
    /// descriptively true of real Claude Code traffic, but pre-mutation the
    /// contract is to forward already-broken input unmutated, and
    /// post-mutation any reordering is the strategy's bug to avoid (Step 3).
    /// The index stores `message_idx`, so a `result_idx > use_idx` check can
    /// be added trivially if a strategy ever needs it.
    ///
    /// The reported id is the lexicographically smallest orphan (via `min`),
    /// so the error is deterministic regardless of `HashMap` iteration order.
    pub fn validate(&self) -> Result<()> {
        self.results
            .keys()
            .filter(|id| !self.uses.contains_key(id.as_str()))
            .min()
            .map_or(Ok(()), |id| Err(Error::OrphanResult(id.clone())))
    }

    /// Look up both halves of a pair by `tool_use_id`. Strategies use this to
    /// drop a pair atomically (invariant 2). Either half may be `None` if the
    /// id is unknown or only one side is present.
    pub fn pair(&self, tool_use_id: &str) -> (Option<Loc>, Option<Loc>) {
        (
            self.uses.get(tool_use_id).copied(),
            self.results.get(tool_use_id).copied(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Empty messages → empty index, validate passes.
    #[test]
    fn empty_messages() {
        let idx = PairingIndex::build(&[]);
        assert!(idx.uses.is_empty());
        assert!(idx.results.is_empty());
        assert_eq!(idx.validate(), Ok(()));
    }

    /// One use + one result → index size 1+1, validate passes, `pair` resolves.
    #[test]
    fn one_pair() {
        let messages = vec![
            json!({"role": "user", "content": "run echo hi"}),
            json!({"role": "assistant", "content": [
                {"type": "tool_use", "id": "toolu_1", "name": "Bash", "input": {"command": "echo hi"}}
            ]}),
            json!({"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "toolu_1", "content": "hi"}
            ]}),
        ];
        let idx = PairingIndex::build(&messages);
        assert_eq!(idx.uses.len(), 1);
        assert_eq!(idx.results.len(), 1);
        assert_eq!(idx.validate(), Ok(()));
        assert_eq!(idx.pair("toolu_1"), (Some((1, 0)), Some((2, 0))));
    }

    /// Three parallel tool_use blocks in one assistant message + three results
    /// → index size 3+3, validate passes, each pair stands alone (invariant 3).
    #[test]
    fn parallel_tool_use() {
        let messages = vec![
            json!({"role": "user", "content": "run a, b, c"}),
            json!({"role": "assistant", "content": [
                {"type": "text", "text": "running"},
                {"type": "tool_use", "id": "toolu_a", "name": "Bash", "input": {}},
                {"type": "tool_use", "id": "toolu_b", "name": "Bash", "input": {}},
                {"type": "tool_use", "id": "toolu_c", "name": "Bash", "input": {}}
            ]}),
            json!({"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "toolu_a", "content": "a"},
                {"type": "tool_result", "tool_use_id": "toolu_b", "content": "b"},
                {"type": "tool_result", "tool_use_id": "toolu_c", "content": "c"}
            ]}),
        ];
        let idx = PairingIndex::build(&messages);
        assert_eq!(idx.uses.len(), 3);
        assert_eq!(idx.results.len(), 3);
        assert_eq!(idx.validate(), Ok(()));
        // text block at content[0] pushes the uses to indices 1, 2, 3.
        assert_eq!(idx.pair("toolu_b"), (Some((1, 2)), Some((2, 1))));
    }

    /// A tool_result with no matching tool_use → validate returns OrphanResult.
    #[test]
    fn orphan_result() {
        let messages = vec![json!({"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": "toolu_ghost", "content": "x"}
        ]})];
        let idx = PairingIndex::build(&messages);
        assert!(idx.uses.is_empty());
        assert_eq!(idx.results.len(), 1);
        assert_eq!(
            idx.validate(),
            Err(Error::OrphanResult("toolu_ghost".to_owned()))
        );
    }

    /// A tool_use with no matching tool_result is NOT an orphan (mid-turn is
    /// legitimate); validate passes.
    #[test]
    fn lone_use_is_not_orphan() {
        let messages = vec![json!({"role": "assistant", "content": [
            {"type": "tool_use", "id": "toolu_pending", "name": "Bash", "input": {}}
        ]})];
        let idx = PairingIndex::build(&messages);
        assert_eq!(idx.uses.len(), 1);
        assert!(idx.results.is_empty());
        assert_eq!(idx.validate(), Ok(()));
    }

    /// `pair()` on an unknown id resolves to neither half.
    #[test]
    fn pair_unknown_id() {
        let idx = PairingIndex::build(&[]);
        assert_eq!(idx.pair("toolu_nope"), (None, None));
    }

    /// `pair()` on a lone tool_use returns the use half only (no result yet).
    #[test]
    fn pair_lone_use() {
        let messages = vec![json!({"role": "assistant", "content": [
            {"type": "tool_use", "id": "toolu_pending", "name": "Bash", "input": {}}
        ]})];
        let idx = PairingIndex::build(&messages);
        assert_eq!(idx.pair("toolu_pending"), (Some((0, 0)), None));
    }

    /// With multiple orphans, the smallest id is reported (deterministic).
    #[test]
    fn orphan_reporting_is_deterministic() {
        let messages = vec![json!({"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": "toolu_zzz", "content": "z"},
            {"type": "tool_result", "tool_use_id": "toolu_aaa", "content": "a"}
        ]})];
        let idx = PairingIndex::build(&messages);
        assert_eq!(
            idx.validate(),
            Err(Error::OrphanResult("toolu_aaa".to_owned()))
        );
    }

    /// Bare-string `content` carries no tool blocks and is skipped cleanly.
    #[test]
    fn string_content_is_skipped() {
        let messages = vec![
            json!({"role": "user", "content": "just text"}),
            json!({"role": "assistant", "content": "also just text"}),
        ];
        let idx = PairingIndex::build(&messages);
        assert!(idx.uses.is_empty());
        assert!(idx.results.is_empty());
        assert_eq!(idx.validate(), Ok(()));
    }
}
