//! Stable-prefix re-pruning: the stateful layer over the pure `strategies`.
//!
//! The gateway is otherwise stateless — it re-prunes every `/v1/messages` from
//! scratch, so as messages age past `keep_recent_turns` the pruned prefix shifts
//! turn-to-turn and busts Anthropic's prompt cache (the documented cost
//! regression; SPIKE.md §9). This module remembers, per session, the pruning
//! *decisions* made at the last full prune ("checkpoint") and re-applies them
//! while the conversation stays an append-only extension of that checkpoint —
//! keeping the pruned prefix **byte-identical** so the cache survives. It only
//! re-prunes from scratch when the tail grows past a threshold, or when the
//! prefix changes (compaction/edit).
//!
//! **Safety, by construction:**
//! - Overwrite decisions are keyed by the stable `tool_use_id` and only replace
//!   `tool_result.content` / `tool_use.input` of existing blocks. Thinking-block
//!   *removals* (the one structural edit, from `thinking_strip`) are recorded by
//!   the block's immutable `signature` and replayed by deleting only those exact
//!   blocks — never a `tool_use`/`tool_result`, so a pair can't be orphaned and
//!   `system` is never touched. Both are content-addressed, so they survive the
//!   conversation growing (positions shift; ids/signatures don't).
//! - On cold state, threshold exceeded, OR any prefix change (the compaction
//!   guard), it does a full prune — which is **byte-identical to the stateless
//!   `apply_to_body`**. So correctness never depends on the cache being warm or
//!   the conversation being append-only; the worst case is a cache miss, never
//!   wrong output.
//! - This is what lets `thinking_strip` (which removes blocks) stay cache-stable:
//!   the strip-set is recomputed only at checkpoints and replayed verbatim between
//!   them, so the pruned prefix is byte-identical turn-to-turn (one bust per
//!   checkpoint interval — "batching" — instead of one per turn).

use std::collections::{HashMap, HashSet};
use std::time::Instant;

use serde_json::Value;

use crate::config::Config;
use crate::pairing::PairingIndex;
use crate::strategies::{self, BodyOutcome};

/// Per-session memory of the last checkpoint's pruning decisions.
pub struct PruneState {
    /// `tool_use_id` → the mutated `tool_result.content` recorded at checkpoint.
    result_decisions: HashMap<String, Value>,
    /// `tool_use_id` → the mutated `tool_use.input` recorded at checkpoint.
    input_decisions: HashMap<String, Value>,
    /// Immutable keys (thinking `signature` / redacted_thinking `data`) of the
    /// thinking blocks `thinking_strip` REMOVED at the checkpoint. Replayed by
    /// deletion on stable turns so the pruned prefix stays byte-identical.
    stripped_thinking: HashSet<String>,
    /// `messages.len()` of the (input) array at the last checkpoint.
    checkpoint_len: usize,
    /// The parsed input prefix `messages[..checkpoint_len]` captured at the
    /// checkpoint — the compaction guard. On each stable turn the current prefix
    /// is compared structurally (`PartialEq`) against this snapshot; if it
    /// differs, history was rewritten and the stored decisions are stale →
    /// re-checkpoint. Comparing the already-parsed `Value`s is exact
    /// (collision-free, strictly stronger than the former SHA-256 fingerprint),
    /// key-order-independent, and avoids re-serializing+hashing the whole prefix
    /// on every turn — the dominant long-session hot-path cost (≈14× faster:
    /// ~62µs vs ~888µs on a 645 KB session). Memory tradeoff: this holds the full
    /// *original* prefix (including large tool-result content), so it is heavier
    /// than `result_decisions`/`input_decisions` (which keep only the small pruned
    /// values) — one snapshot per live session, bounded by TTL eviction. We trade
    /// that bounded memory for dropping the per-turn serialize+SHA-256.
    checkpoint_prefix: Vec<Value>,
    /// OPT-IN local-model summary of the OLD slice, replayed by range
    /// substitution every turn (see `summarizer::slice`). A CHAIN of frozen,
    /// contiguous segments: the default (single-summary) mode keeps exactly ONE
    /// element (re-summarization REPLACES it); the opt-in accumulator mode APPENDS
    /// a delta segment per re-summarization so older segments stay byte-frozen.
    /// Empty until the async gateway populates it; cleared when CC rewrites history
    /// (anchors go stale). Inert when summarizer engine = model-free (the default).
    summary: Vec<crate::summarizer::slice::SummaryDecision>,
    /// True while an async summarization task is in flight for this session, so
    /// the gateway doesn't spawn duplicate model calls turn after turn.
    summary_inflight: bool,
    /// Monotonic id of the current in-flight summarization, bumped on each
    /// `begin_summary`. A background task captures its epoch and only clears /
    /// records under it, so that after a TTL/LRU eviction recreates this entry
    /// (epoch reset to 0) a stale in-flight task can't clear a *newer* summary's
    /// flag or splice a stale result. (Wraps; collision needs 2^64 summaries on
    /// one recreated key while one task is suspended — not reachable.)
    summary_epoch: u64,
    /// Last access, for TTL eviction by the owner.
    last_used: Instant,
    initialized: bool,
}

impl Default for PruneState {
    fn default() -> Self {
        Self {
            result_decisions: HashMap::new(),
            input_decisions: HashMap::new(),
            stripped_thinking: HashSet::new(),
            checkpoint_len: 0,
            checkpoint_prefix: Vec::new(),
            summary: Vec::new(),
            summary_inflight: false,
            summary_epoch: 0,
            last_used: Instant::now(),
            initialized: false,
        }
    }
}

impl PruneState {
    /// Seconds since this session was last touched (for TTL eviction).
    pub fn idle_secs(&self) -> u64 {
        self.last_used.elapsed().as_secs()
    }

    /// Backdate `last_used` so the state reports ~`secs` of idleness — lets the
    /// gateway's eviction logic be unit-tested without sleeping. Saturates at the
    /// monotonic epoch (so it can't panic on a freshly-booted host); callers use
    /// small offsets where saturation never bites.
    #[cfg(test)]
    pub fn set_idle_for_test(&mut self, secs: u64) {
        let now = std::time::Instant::now();
        self.last_used = now
            .checked_sub(std::time::Duration::from_secs(secs))
            .unwrap_or(now);
    }
}

/// Summarizer summary accessors (opt-in, off unless `[summarizer] engine` != `"model-free"`).
/// The async gateway uses these to read checkpoint state and install/clear the cached
/// summary; reprune's sync replay (above) consumes the field directly.
impl PruneState {
    /// Has a checkpoint been recorded yet (so the stable region is known)?
    pub fn is_initialized(&self) -> bool {
        self.initialized
    }

    /// Length of the input array at the last checkpoint (the stable boundary).
    pub fn checkpoint_len(&self) -> usize {
        self.checkpoint_len
    }

    /// `end` index of the cached summary chain's LAST segment, if any. The gateway
    /// uses this to batch re-summarization (only re-summarize once the slice has
    /// grown materially past where the chain currently reaches).
    pub fn summary_slice_end(&self) -> Option<usize> {
        self.summary.last().map(|d| d.end)
    }

    /// Number of frozen segments in the cached summary chain (0 = none, 1 = the
    /// default single-summary mode, >1 = the accumulator has appended deltas). For
    /// observability / tests / the cost-replay accumulator arm.
    pub fn summary_segment_count(&self) -> usize {
        self.summary.len()
    }

    /// Does the cached summary chain still anchor to `messages` — i.e. does EVERY
    /// segment's summarized slice remain byte-identical to the current history?
    /// False if the chain is empty or any segment went stale (CC rewrote history).
    /// The gateway uses this so a STALE chain doesn't suppress re-summarization via
    /// the batching gate, and the accumulator only appends onto an intact chain.
    pub fn summary_anchor_matches(&self, messages: &[Value]) -> bool {
        !self.summary.is_empty()
            && self
                .summary
                .iter()
                .all(|d| crate::summarizer::slice::anchor_matches(messages, d))
    }

    /// Install a freshly-generated summary in REPLACE mode (default / single-summary):
    /// the new decision becomes the whole chain, dropping any prior segments. This is
    /// the historical behavior — re-summarization rewrites the cached prefix.
    pub fn set_summary(&mut self, d: crate::summarizer::slice::SummaryDecision) {
        self.summary = vec![d];
    }

    /// ACCUMULATOR mode: append a frozen DELTA segment onto the existing chain so the
    /// earlier segments stay byte-frozen (only the new delta busts the cache). The
    /// segment MUST be contiguous with the chain tail (`d.start == last.end`); an empty
    /// chain seeds the first segment. Returns `true` iff appended. On a NON-contiguous
    /// `d` (a gap/overlap — shouldn't happen, the gateway computes `start` from the
    /// chain end) it `debug_assert`s (caught in dev/CI) + logs an error, then returns
    /// `false` WITHOUT mutating, leaving the caller to fall back to
    /// [`set_summary`](Self::set_summary) (REPLACE) so the chain never becomes
    /// inconsistent and live traffic never panics.
    #[must_use]
    pub fn append_summary(&mut self, d: crate::summarizer::slice::SummaryDecision) -> bool {
        match self.summary.last() {
            Some(last) if d.start == last.end => {
                self.summary.push(d);
                true
            }
            None => {
                self.summary.push(d);
                true
            }
            Some(last) => {
                // Non-contiguous. The gateway always computes `d.start` from the
                // chain end (`summary_slice_end`), so reaching here means an
                // upstream bug miscomputed the delta start. Make that LOUD —
                // `debug_assert` fails it in dev/test, `tracing::error` surfaces
                // it in release — but never panic on live traffic and never
                // corrupt the chain: return `false` so the caller falls back to
                // REPLACE ([`set_summary`]). (Without this the silent REPLACE
                // would just drop the frozen chain and mask the bug.)
                let (got, want) = (d.start, last.end);
                debug_assert!(
                    false,
                    "append_summary: non-contiguous segment (d.start={got} != chain end={want}); \
                     the caller computes start from the chain end, so this is a bug"
                );
                tracing::error!(
                    d_start = got,
                    chain_end = want,
                    "trimwire: append_summary got a non-contiguous segment (bug upstream); \
                     falling back to REPLACE"
                );
                false
            }
        }
    }

    /// Is an async summarization already running for this session?
    pub fn summary_inflight(&self) -> bool {
        self.summary_inflight
    }

    /// Mark a summarization task as started (claim the in-flight slot) and return
    /// its epoch. The caller passes the epoch back to [`Self::end_summary_if`] /
    /// [`Self::summary_active`] so a stale task can't act on a recycled entry.
    ///
    /// The epoch is drawn from a PROCESS-GLOBAL counter (not a per-entry one), so
    /// it stays unique even after this key is evicted and a fresh `PruneState`
    /// (epoch back at 0) is created for it — otherwise a recycled entry's first
    /// summary could collide with a still-running stale task's epoch.
    pub fn begin_summary(&mut self) -> u64 {
        static SUMMARY_EPOCH_SEQ: std::sync::atomic::AtomicU64 =
            std::sync::atomic::AtomicU64::new(1);
        self.summary_inflight = true;
        self.summary_epoch = SUMMARY_EPOCH_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.summary_epoch
    }

    /// True iff a summary with this exact `epoch` is still the in-flight one — i.e.
    /// this entry wasn't evicted+recreated and no newer summary superseded it.
    pub fn summary_active(&self, epoch: u64) -> bool {
        self.summary_inflight && self.summary_epoch == epoch
    }

    /// Release the in-flight slot, but only if `epoch` is still the active one
    /// (no-op otherwise — protects a recycled entry / a newer in-flight summary).
    pub fn end_summary_if(&mut self, epoch: u64) {
        if self.summary_inflight && self.summary_epoch == epoch {
            self.summary_inflight = false;
        }
    }
}

/// The immutable identity of a thinking block: a `thinking` block's `signature`
/// or a `redacted_thinking` block's `data`. `None` for any other block (and for
/// an UNSIGNED thinking block — which we then can't replay-remove). NOTE: an
/// unsigned old thinking block is therefore LEFT IN on the stable replay path
/// (the original is forwarded), which differs from the stateless prune that strips
/// it by position — i.e. not a pure "cache miss". This is benign because real
/// Anthropic responses always sign thinking blocks, so the case doesn't arise in
/// production traffic (verified: real session transcripts carry signed blocks or
/// none); a non-standard client emitting unsigned thinking just keeps its old
/// reasoning a little longer. Content-addressed → position-stable.
fn thinking_block_key(block: &Value) -> Option<String> {
    match block.get("type").and_then(Value::as_str) {
        Some("thinking") => block
            .get("signature")
            .and_then(Value::as_str)
            .map(str::to_owned),
        Some("redacted_thinking") => block.get("data").and_then(Value::as_str).map(str::to_owned),
        _ => None,
    }
}

/// Compute what the checkpoint prune did, keyed by STABLE identifiers (not
/// position): tool-block overwrites by `tool_use_id`, and removed thinking blocks
/// by `signature`/`data`. Id/signature keys are position-independent, so this is
/// correct even though `thinking_strip` shortens a message's content array
/// (a positional zip would misalign after a removal — that's why this is id-keyed).
///
/// Returns the `(result_decisions, input_decisions, stripped_thinking)` sets as
/// owned collections rather than writing them into `state` — the caller commits
/// them into `PruneState` only once the outgoing body has serialized, so a failed
/// re-serialize never advances the checkpoint to a body that never went on the
/// wire (#144).
fn compute_decisions(
    orig: &[Value],
    pruned: &[Value],
) -> (
    HashMap<String, Value>,
    HashMap<String, Value>,
    HashSet<String>,
) {
    let mut result_decisions: HashMap<String, Value> = HashMap::new();
    let mut input_decisions: HashMap<String, Value> = HashMap::new();
    let mut stripped_thinking: HashSet<String> = HashSet::new();

    // Index the ORIGINAL tool values + thinking keys by their stable identifier.
    let mut orig_results: HashMap<&str, &Value> = HashMap::new();
    let mut orig_inputs: HashMap<&str, &Value> = HashMap::new();
    let mut orig_thinking: HashSet<String> = HashSet::new();
    for m in orig {
        let Some(content) = m.get("content").and_then(Value::as_array) else {
            continue;
        };
        for b in content {
            match b.get("type").and_then(Value::as_str) {
                Some("tool_result") => {
                    if let (Some(id), Some(c)) = (
                        b.get("tool_use_id").and_then(Value::as_str),
                        b.get("content"),
                    ) {
                        orig_results.insert(id, c);
                    }
                }
                Some("tool_use") => {
                    if let (Some(id), Some(inp)) =
                        (b.get("id").and_then(Value::as_str), b.get("input"))
                    {
                        orig_inputs.insert(id, inp);
                    }
                }
                _ => {
                    if let Some(k) = thinking_block_key(b) {
                        orig_thinking.insert(k);
                    }
                }
            }
        }
    }

    // Walk PRUNED blocks: record overwrites (value differs from orig); collect the
    // thinking keys that SURVIVED (to diff against orig → removals).
    let mut pruned_thinking: HashSet<String> = HashSet::new();
    for m in pruned {
        let Some(content) = m.get("content").and_then(Value::as_array) else {
            continue;
        };
        for b in content {
            match b.get("type").and_then(Value::as_str) {
                Some("tool_result") => {
                    if let (Some(id), Some(c)) = (
                        b.get("tool_use_id").and_then(Value::as_str),
                        b.get("content"),
                    ) {
                        if orig_results.get(id).copied() != Some(c) {
                            result_decisions.insert(id.to_owned(), c.clone());
                        }
                    }
                }
                Some("tool_use") => {
                    if let (Some(id), Some(inp)) =
                        (b.get("id").and_then(Value::as_str), b.get("input"))
                    {
                        if orig_inputs.get(id).copied() != Some(inp) {
                            input_decisions.insert(id.to_owned(), inp.clone());
                        }
                    }
                }
                _ => {
                    if let Some(k) = thinking_block_key(b) {
                        pruned_thinking.insert(k);
                    }
                }
            }
        }
    }
    // Thinking keys in orig but gone from pruned = removed by thinking_strip.
    for k in orig_thinking {
        if !pruned_thinking.contains(&k) {
            stripped_thinking.insert(k);
        }
    }
    (result_decisions, input_decisions, stripped_thinking)
}

/// Install a freshly-computed checkpoint into `state`: the decision sets plus the
/// advanced checkpoint pointer + prefix snapshot. Called only after the outgoing
/// body has serialized (or on a no-op re-checkpoint that forwards the original
/// bytes), so the checkpoint never advances to a body that failed to serialize (#144).
///
/// `clear_summary` drops the cached summary as part of the SAME commit — it's
/// durable state made stale by a prefix change, so it clears here (post-serialize)
/// rather than eagerly, matching the decision-set discipline (#164).
fn commit_checkpoint(
    state: &mut PruneState,
    result_decisions: HashMap<String, Value>,
    input_decisions: HashMap<String, Value>,
    stripped_thinking: HashSet<String>,
    checkpoint_len: usize,
    checkpoint_prefix: Vec<Value>,
    clear_summary: bool,
) {
    state.result_decisions = result_decisions;
    state.input_decisions = input_decisions;
    state.stripped_thinking = stripped_thinking;
    state.checkpoint_len = checkpoint_len;
    state.checkpoint_prefix = checkpoint_prefix;
    state.initialized = true;
    if clear_summary {
        state.summary.clear();
    }
}

/// Re-apply the checkpoint's decisions to `out` by `tool_use_id`. Only overwrites
/// existing blocks; never adds/removes/reorders → can't orphan. Returns whether
/// anything changed.
fn apply_decisions(out: &mut [Value], state: &PruneState) -> bool {
    let mut changed = false;
    for m in out.iter_mut() {
        let Some(content) = m.get_mut("content").and_then(Value::as_array_mut) else {
            continue;
        };
        for block in content.iter_mut() {
            match block.get("type").and_then(Value::as_str) {
                Some("tool_result") => {
                    if let Some(id) = block.get("tool_use_id").and_then(Value::as_str) {
                        if let Some(c) = state.result_decisions.get(id) {
                            if block.get("content") != Some(c) {
                                block["content"] = c.clone();
                                changed = true;
                            }
                        }
                    }
                }
                Some("tool_use") => {
                    if let Some(id) = block.get("id").and_then(Value::as_str) {
                        if let Some(inp) = state.input_decisions.get(id) {
                            if block.get("input") != Some(inp) {
                                block["input"] = inp.clone();
                                changed = true;
                            }
                        }
                    }
                }
                _ => {}
            }
        }
    }
    changed
}

/// Replay the checkpoint's thinking removals: delete exactly the thinking blocks
/// whose `signature`/`data` was recorded as stripped. Only removes thinking blocks
/// (never a `tool_use`/`tool_result`), so a pair can't be orphaned. Returns whether
/// anything was removed.
///
/// **Invariant relied upon:** a thinking block's `signature` (and a
/// redacted_thinking block's `data`) is globally UNIQUE within a conversation —
/// guaranteed by Anthropic's signing protocol, so a recent/in-progress block can
/// never share a key with an old (stripped) one. We additionally never strip a
/// thinking-only message (defense-in-depth, mirroring `thinking_strip`'s own guard)
/// so replay can't empty a message even if that invariant were ever violated.
fn apply_thinking_removals(out: &mut [Value], state: &PruneState) -> bool {
    if state.stripped_thinking.is_empty() {
        return false;
    }
    let mut changed = false;
    for m in out.iter_mut() {
        let Some(content) = m.get_mut("content").and_then(Value::as_array_mut) else {
            continue;
        };
        // Never empty a message: only strip when a non-thinking block remains.
        if !content.iter().any(|b| thinking_block_key(b).is_none()) {
            continue;
        }
        let before = content.len();
        content.retain(|b| match thinking_block_key(b) {
            Some(k) => !state.stripped_thinking.contains(&k),
            None => true,
        });
        if content.len() != before {
            changed = true;
        }
    }
    changed
}

/// Build the replayed output from the recorded checkpoint decisions, optionally
/// substituting the cached local-model summary first. Returns `(out, changed)`,
/// or `None` if the result would orphan a pair (caller forwards the original).
///
/// When the summarizer engine is active and a valid summary is cached, the OLD
/// slice is replaced by the summary message-pair BEFORE the content-overwrite +
/// thinking-removal decisions are replayed on the (now-shorter) array. The
/// decisions are keyed by `tool_use_id` / thinking signature, so removing a
/// contiguous middle slice doesn't disturb them. If the summary splice doesn't
/// apply (no summary, stale anchor) or would orphan, it falls back to the
/// model-free replay — the summary is never load-bearing.
fn replay_decisions(messages: &[Value], state: &PruneState) -> Option<(Vec<Value>, bool)> {
    if !state.summary.is_empty() {
        let mut out = messages.to_vec();
        let spliced = crate::summarizer::slice::apply_summaries(&mut out, messages, &state.summary);
        tracing::debug!(
            segments = state.summary.len(),
            spliced,
            "trimwire: replay summary splice attempt"
        );
        if spliced {
            let overwrote = apply_decisions(&mut out, state);
            let removed = apply_thinking_removals(&mut out, state);
            if PairingIndex::build(&out).validate().is_ok() {
                return Some((out, spliced || overwrote || removed));
            }
            // Summary splice orphaned a pair → fall through to model-free replay.
        }
    }
    let mut out = messages.to_vec();
    let overwrote = apply_decisions(&mut out, state);
    let removed = apply_thinking_removals(&mut out, state);
    if PairingIndex::build(&out).validate().is_err() {
        return None;
    }
    Some((out, overwrote || removed))
}

/// #138 — pick the forward-original outcome for a rollback. When the INPUT was
/// valid, the rollback is trimwire's fault (a strategy/replay orphaned a pair, or
/// re-serialize failed) → [`BodyOutcome::RolledBack`], flagged for `report
/// --auto`. When the input was already malformed, it's the client's fault →
/// blameless [`BodyOutcome::Unchanged`]. Mirrors `strategies::apply_to_body`.
fn rollback_outcome(input_valid: bool) -> BodyOutcome {
    if input_valid {
        BodyOutcome::RolledBack
    } else {
        BodyOutcome::Unchanged
    }
}

/// Serialize `root` (with re-applied decisions) into a `Mutated` outcome. Tags a
/// synthetic `stable_reprune` "fired" entry so the ledger records it as a pruning
/// turn — not a no-op-that-changed-the-prefix (which is its cache-bust tripwire).
/// A re-serialize failure is trimwire-caused, so it rolls back via
/// [`rollback_outcome`] (#138), flagged when the input was valid.
fn finish(
    root: Value,
    original_bytes: usize,
    final_bytes: usize,
    input_valid: bool,
) -> BodyOutcome {
    match serde_json::to_vec(&root) {
        Ok(bytes) => BodyOutcome::Mutated {
            bytes,
            fired: vec![(
                "stable_reprune",
                strategies::Stats {
                    stubbed: 1,
                    original_bytes,
                    final_bytes,
                },
            )],
        },
        Err(_) => rollback_outcome(input_valid),
    }
}

/// Sum of `tool_result.content` serialized bytes in the appended tail
/// `messages[from..]`. Drives the byte-based re-checkpoint trigger (fix #2):
/// large NEW tool_result content is exactly what the age-gated `bloat_cap` can
/// trim once it ages, so when a genuine volume of it lands we re-checkpoint
/// promptly instead of freezing it behind the stable replay. Counts only
/// `tool_result` `content` (string length, or summed `text` blocks for the array
/// shape) — not structural overhead — so ordinary small-result growth doesn't trip
/// it. Allocation-free (no serialization) to honour the stable path's no-serialize
/// design (~14× hot-path saving). `from` is always `≤ messages.len()` here (only
/// called on the append-only path).
fn new_tail_result_bytes(messages: &[Value], from: usize) -> usize {
    messages
        .iter()
        .skip(from)
        .filter_map(|m| m.get("content").and_then(Value::as_array))
        .flatten()
        .filter(|b| b.get("type").and_then(Value::as_str) == Some("tool_result"))
        .map(|b| match b.get("content") {
            Some(Value::String(s)) => s.len(),
            // Array content (the common MCP/structured shape): sum the `text` block
            // lengths directly. Non-text blocks (images) aren't bloat_cap-trimmable,
            // so undercounting them is correct — and we avoid serializing on the hot path.
            Some(Value::Array(arr)) => arr
                .iter()
                .filter_map(|blk| blk.get("text").and_then(Value::as_str))
                .map(str::len)
                .sum(),
            _ => 0,
        })
        .sum()
}

/// Structural equality of two message arrays that IGNORES `cache_control` markers.
///
/// Anthropic's prompt-cache breakpoint (`cache_control`) moves forward each turn,
/// toggling on/off blocks INSIDE the checkpoint prefix. That is transport cache
/// metadata, not a history rewrite — but a byte-exact `==` treats it as a prefix
/// change, forcing a re-checkpoint EVERY turn. That defeats stable replay and (via
/// `prefix_changed`) clears the cached summary so it never reaches the wire — the
/// reason accepted summaries were never applied in live Claude Code sessions
/// (manual-test F10). Comparing with `cache_control` ignored keeps `append_only`
/// stable across cache churn, while any REAL content change (text / tool_use /
/// tool_result / role / other keys) still differs and forces a re-checkpoint.
///
/// Wire-safety: this governs only the stability DECISION. The replayed output is
/// built from the live `messages` (with their current `cache_control` intact) — we
/// never strip `cache_control` from the bytes sent upstream.
fn messages_eq_ignoring_cache_control(a: &[Value], b: &[Value]) -> bool {
    a.len() == b.len()
        && a.iter()
            .zip(b)
            .all(|(x, y)| value_eq_ignoring_cache_control(x, y))
}

/// Anthropic treats a message `content` of `"text"` and `[{"type":"text","text":"text"}]`
/// as equivalent, and Claude Code RE-RENDERS a user turn between the two shapes as it moves
/// from the CURRENT turn (block-list form) to history (bare-string form). That cosmetic flip
/// is not a history rewrite, but a byte-exact compare reads it as a prefix change — forcing a
/// re-checkpoint that (via `prefix_changed`) CLEARS the cached summary so it never reaches the
/// wire (the same failure class as `cache_control` churn / F10, found on the no-deterministic-
/// prune geometry). Canonicalizing a bare string to its single-text-block equivalent keeps
/// `append_only` stable across the flip; any REAL content change (different text, extra blocks,
/// non-text block) still differs. Like the `cache_control` handling, this governs ONLY the
/// stability DECISION — the wire bytes are always rebuilt from live `messages`.
fn content_eq_ignoring_cosmetic(a: &Value, b: &Value) -> bool {
    // Only the string-vs-block shape flip is special-cased; everything else compares
    // structurally (ignoring cache_control) so genuine content changes still differ.
    if matches!(a, Value::String(_)) || matches!(b, Value::String(_)) {
        if let (Some(ta), Some(tb)) = (content_as_text(a), content_as_text(b)) {
            return ta == tb;
        }
    }
    value_eq_ignoring_cache_control(a, b)
}

/// A message `content` rendered as either a bare string `S` or a single text block
/// `[{ "type":"text", "text":S }]` (cache_control, if present, is irrelevant to text
/// identity). Returns `S` for those two shapes, `None` for anything else.
fn content_as_text(v: &Value) -> Option<&str> {
    match v {
        Value::String(s) => Some(s.as_str()),
        Value::Array(arr) if arr.len() == 1 => {
            let blk = arr.first()?;
            if blk.get("type").and_then(Value::as_str) == Some("text") {
                blk.get("text").and_then(Value::as_str)
            } else {
                None
            }
        }
        _ => None,
    }
}

fn value_eq_ignoring_cache_control(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Object(am), Value::Object(bm)) => {
            let live = |m: &serde_json::Map<String, Value>| {
                m.keys().filter(|k| k.as_str() != "cache_control").count()
            };
            if live(am) != live(bm) {
                return false;
            }
            // The current→history string/block shape flip happens on TOP-LEVEL message
            // `content` (objects with a `role`). Scope the shape-insensitive compare to those;
            // a nested `content` (e.g. a `tool_result` block) and all other keys compare
            // structurally so genuine changes there still differ.
            let is_message = am.contains_key("role");
            am.iter()
                .filter(|(k, _)| k.as_str() != "cache_control")
                .all(|(k, av)| {
                    bm.get(k).is_some_and(|bv| {
                        if k == "content" && is_message {
                            content_eq_ignoring_cosmetic(av, bv)
                        } else {
                            value_eq_ignoring_cache_control(av, bv)
                        }
                    })
                })
        }
        (Value::Array(aa), Value::Array(ba)) => {
            aa.len() == ba.len()
                && aa
                    .iter()
                    .zip(ba)
                    .all(|(x, y)| value_eq_ignoring_cache_control(x, y))
        }
        _ => a == b,
    }
}

/// Stateful equivalent of [`strategies::apply_to_body`]. Reuses the last
/// checkpoint's decisions while the conversation is an append-only extension of
/// it (cache-stable); otherwise — cold, tail grew past `threshold`, or the prefix
/// changed (compaction) — does a full prune identical to the stateless path and
/// records a fresh checkpoint. Never produces orphaned or `system`-mutated output.
pub fn stable_apply_to_body(
    body: &[u8],
    cfg: &Config,
    state: &mut PruneState,
    threshold: usize,
) -> BodyOutcome {
    state.last_used = Instant::now();

    let Ok(mut root) = serde_json::from_slice::<Value>(body) else {
        return BodyOutcome::Unchanged;
    };
    // POC pre-pass (opt-in, default OFF): repair a stray `messages[0].role:"system"`
    // into the top-level `system` field (prevents a hard 400). No-op on well-formed
    // bodies; on the rare malformed turn it removes messages[0], which changes the
    // shape and correctly forces a re-checkpoint below. `normalized` is folded into
    // the Unchanged guards so a repaired-but-otherwise-unpruned body is still
    // forwarded as Mutated (not the original malformed bytes).
    let normalized = cfg.strategies.system_shape_normalize.enabled
        && strategies::system_shape_normalize::normalize(&mut root);
    let Some(messages) = root.get("messages").and_then(Value::as_array) else {
        return BodyOutcome::Unchanged;
    };
    // #138: was the body trimwire RECEIVED valid? A rollback on an already-
    // malformed input is the client's fault → never flagged. `normalize` above
    // only repairs toward validity, so checking here (post-normalize) never
    // mis-blames a repaired body.
    let input_valid = PairingIndex::build(messages).validate().is_ok();
    // Decline a malformed input up-front — forward the original bytes verbatim,
    // exactly as the stateless `apply_to_body` does. Without this, the stable
    // (replay) branch could replay checkpoint decisions onto the valid prefix and
    // emit `Mutated`, forwarding a body that is STILL orphaned in its new tail
    // (Anthropic 400) — trimwire mutating a body it can't safely prune. Past this
    // point the input is known-valid, so any rollback below is trimwire-CAUSED.
    if !input_valid {
        return BodyOutcome::Unchanged;
    }

    let len = messages.len();

    // Append-only iff initialized, not shrunk, and the checkpoint prefix is
    // unchanged (the compaction guard). The comparison ignores `cache_control`
    // markers (transport cache metadata Anthropic moves every turn — F10) so cache
    // churn doesn't masquerade as a history rewrite; any real content change still
    // differs. Short-circuits on the first differing block and never re-serializes —
    // the bulk of the long-session hot-path saving. Any failure → full re-checkpoint.
    let append_only = state.initialized
        && len >= state.checkpoint_len
        && messages_eq_ignoring_cache_control(
            &messages[..state.checkpoint_len],
            &state.checkpoint_prefix,
        );
    let grew = len.saturating_sub(state.checkpoint_len);

    // Fix #2 — byte-based re-checkpoint: even within `grew <= threshold` MESSAGES,
    // a genuine volume of NEWLY-appended tool_result content forces a re-checkpoint
    // so the deterministic strategies (incl. the age-gated bloat_cap) run on the
    // grown history now, instead of the stable branch freezing big new results
    // behind a replay of stale (possibly empty) decisions — the live short-but-
    // large session that compressed ~0% (CANARY-01). Bounded by a byte threshold so
    // ordinary small-result growth keeps batching. OFF when the knob is 0. Only
    // evaluated on the append-only path (the only one the stable branch governs);
    // the tail it scans is at most `grew` messages, so it stays cheap.
    let big_new_tail = cfg.reprune.recheckpoint_result_bytes > 0
        && append_only
        && new_tail_result_bytes(messages, state.checkpoint_len)
            > cfg.reprune.recheckpoint_result_bytes;

    tracing::debug!(
        initialized = state.initialized,
        len,
        checkpoint_len = state.checkpoint_len,
        grew,
        threshold,
        append_only,
        big_new_tail,
        summary_segments = state.summary.len(),
        "trimwire: reprune branch decision"
    );

    if append_only && grew <= threshold && !big_new_tail {
        // STABLE: replay the checkpoint's decisions + thinking removals (+ the
        // cached local-model summary, when the feature is on); newer messages are
        // untouched (their ids/signatures aren't in the recorded sets). Paranoia:
        // the replay can't normally orphan, but `replay_decisions` re-validates
        // and yields `None` so we forward the original body if it ever did.
        let Some((mut out, changed)) = replay_decisions(messages, state) else {
            // Replaying the checkpoint's recorded decisions would orphan a pair:
            // a trimwire replay bug (#138) → flag when the input was valid.
            return rollback_outcome(input_valid);
        };
        // Always-on correctness sanitize: drop empty-thinking blocks Anthropic
        // rejects on resume. The stable prefix already has all thinking
        // removed-and-replayed, so this only touches the divergent recent tail →
        // no cache-prefix impact; deterministic + run in both branches keeps the
        // prefix byte-identical. May flip an unchanged replay to changed.
        let empty_removed = strategies::thinking_strip::strip_empty(&mut out);
        if !changed && empty_removed == 0 && !normalized {
            return BodyOutcome::Unchanged;
        }
        // Record the REAL messages[] delta so `stats` attributes the bytes the
        // replay actually saved to `stable_reprune` (not 0 — which under-reported
        // the per-strategy breakdown on every stable turn).
        let orig_len = serde_json::to_vec(messages).map(|v| v.len()).unwrap_or(0);
        let out_len = serde_json::to_vec(&out).map(|v| v.len()).unwrap_or(0);
        root["messages"] = Value::Array(out);
        finish(root, orig_len, out_len, input_valid)
    } else {
        // CHECKPOINT: full prune (== stateless apply_to_body), then record.
        // Detect a history rewrite (CC's own compaction) up front: initialized
        // but the prefix no longer matches → any cached summary anchor is stale.
        let prefix_changed = state.initialized && !append_only;

        // Was this re-checkpoint forced ONLY by the byte trigger (we'd otherwise have
        // stayed STABLE)? A byte-forced re-checkpoint can fire BEFORE any result has
        // aged past `keep_recent` (e.g. 3 large reads exceed the byte threshold but
        // none is old enough to trim yet). If such a fire prunes NOTHING, advancing
        // the checkpoint anyway would burn it and STARVE the later message-count
        // re-checkpoint that WOULD trim the now-aged content — a live-canary regression
        // (run B: a short large-read session ended at 0% because the premature fire
        // reset the checkpoint). So when byte-forced, snapshot the checkpoint state and
        // roll it back if the prune is a no-op, leaving the tail to keep accumulating.

        // #144 nit1: capture the OLD-decision replay length NOW, before the byte-forced
        // `rollback` below drains the decision sets via `mem::take`. The marginal-vs-
        // replay telemetry (further down) must diff the pruned body against replaying
        // the OLD decisions on the longer array; reading it after the drain would replay
        // against emptied decisions and mis-report total (not marginal) savings. Only on
        // the append-only rebase (what the deferred gate governs) and only under DEBUG.
        let debug_replay_len: Option<usize> = (append_only
            && tracing::enabled!(tracing::Level::DEBUG))
        .then(|| {
            replay_decisions(messages, state)
                .and_then(|(o, _)| serde_json::to_vec(&o).ok())
                .map(|v| v.len())
        })
        .flatten();

        let byte_forced = big_new_tail && append_only && grew <= threshold && !prefix_changed;
        // Load-bearing invariant: a byte-forced re-checkpoint is append-only with an
        // unchanged prefix, so it can NEVER coincide with a history rewrite. Several
        // arguments below rely on this (the byte-forced rollback snapshot doesn't cover
        // `state.summary`, which only `apply_checkpoint_summary` clears — and only when
        // `prefix_changed`). Assert it so a future edit to either predicate can't silently
        // break the mutual exclusion.
        debug_assert!(
            !(prefix_changed && byte_forced),
            "byte_forced requires !prefix_changed by construction"
        );
        let rollback = byte_forced.then(|| {
            (
                state.checkpoint_len,
                state.initialized,
                std::mem::take(&mut state.result_decisions),
                std::mem::take(&mut state.input_decisions),
                std::mem::take(&mut state.stripped_thinking),
                std::mem::take(&mut state.checkpoint_prefix),
            )
        });

        let mut pruned = messages.clone();
        let fired = match strategies::run(&mut pruned, cfg) {
            Ok(f) => f,
            // Full-prune orphaned a pair → forward original, FLAGGED (#138: input
            // is known-valid past the guard above, so this is trimwire-caused).
            // Restore the byte-forced snapshot first (mirrors the no-op restore
            // below) so a rollback never leaves `state` half-drained — otherwise
            // `result_decisions`/`checkpoint_prefix` would stay empty while
            // `checkpoint_len`/`initialized` advanced, forcing a spurious
            // re-checkpoint next turn.
            Err(_) => {
                if let Some((cl, init, results, inputs, thinking, prefix)) = rollback {
                    state.checkpoint_len = cl;
                    state.initialized = init;
                    state.result_decisions = results;
                    state.input_decisions = inputs;
                    state.stripped_thinking = thinking;
                    state.checkpoint_prefix = prefix;
                }
                return rollback_outcome(input_valid);
            }
        };
        // Telemetry for the (deferred) minimum-savings gate (IMPROVEMENTS P0 #2): the
        // decision metric is the MARGINAL savings of re-checkpointing vs just replaying
        // the OLD decisions on the longer array — NOT the total-vs-raw savings. Uses the
        // pre-drain replay length captured above (#144 nit1). Guarded: `debug_replay_len`
        // is `None` unless DEBUG tracing is on and this is the append-only rebase.
        if let Some(replay_len) = debug_replay_len {
            let pruned_len = serde_json::to_vec(&pruned).map(|v| v.len()).unwrap_or(0);
            tracing::debug!(
                grew,
                checkpoint_len = len,
                marginal_saved = replay_len.saturating_sub(pruned_len),
                "trimwire: re-checkpoint marginal-vs-replay"
            );
        }
        // #144 nit2: compute the checkpoint's decisions but DON'T write them into
        // `state` yet — the decision sets + advanced checkpoint pointer are committed
        // (`commit_checkpoint`) only once the outgoing body has serialized below, so a
        // failed re-serialize can never advance the checkpoint to a body that never went
        // on the wire (which would replay stale decisions next turn).
        let (results, inputs, thinking) = compute_decisions(messages, &pruned);

        // Apply the cached summary (if any) to the freshly-pruned checkpoint so its
        // prefix matches the stable turns'. Reverted if it would orphan; cleared on a
        // history rewrite. Never load-bearing. (Reads/clears `state.summary` only — it
        // does not touch the not-yet-committed decision sets.)
        let (mut pruned, fired, summary_applied) =
            apply_checkpoint_summary(pruned, fired, messages, state, prefix_changed);

        // Always-on correctness sanitize (see strategies::apply_to_body): drop
        // empty-thinking blocks Anthropic rejects on resume. Run AFTER
        // compute_decisions so it is not recorded as a replayable decision — the
        // stable branch re-applies the same deterministic pass, so the pruned
        // prefix stays byte-identical across turns (cache-safe).
        let empty_removed = strategies::thinking_strip::strip_empty(&mut pruned);

        // Snapshot the checkpoint prefix as an OWNED value now, while `messages` (a
        // borrow of `root`) is still freely borrowable — `commit_checkpoint` installs it
        // AFTER `pruned` is moved into `root` below (which mutably borrows `root`), so it
        // can't re-borrow `messages` at that point (#144 nit2 reorder).
        let new_prefix = messages.to_vec();

        if fired.iter().all(|(_, s)| s.stubbed == 0)
            && !summary_applied
            && empty_removed == 0
            && !normalized
        {
            // No-op re-checkpoint (original bytes preserved). If it was byte-FORCED,
            // restore the pre-drain snapshot so the checkpoint does NOT advance on a fire
            // that trimmed nothing (the tail keeps accumulating for a later re-checkpoint
            // once content ages). Otherwise this is a normal grown-past-threshold
            // re-checkpoint that happened to prune nothing — advance it as before.
            match rollback {
                Some((cl, init, r, i, t, prefix)) => {
                    state.checkpoint_len = cl;
                    state.initialized = init;
                    state.result_decisions = r;
                    state.input_decisions = i;
                    state.stripped_thinking = t;
                    state.checkpoint_prefix = prefix;
                }
                None => commit_checkpoint(
                    state,
                    results,
                    inputs,
                    thinking,
                    len,
                    new_prefix,
                    prefix_changed,
                ),
            }
            return BodyOutcome::Unchanged; // exact-original bytes preserved
        }
        // Telemetry: a re-checkpoint MUTATES the prefix → busts the Anthropic prompt
        // cache. Logging `grew` + bytes saved reveals, on real sessions, how often
        // re-checkpoints fire for SMALL savings — the data that would justify (and set
        // the threshold for) a minimum-savings bust gate (IMPROVEMENTS-RESEARCH.md
        // P0 #2; design ready, deferred pending this telemetry). Runs BEFORE `pruned`
        // is moved into `root`, and is guarded so the extra serialization only runs when
        // DEBUG tracing is enabled.
        if tracing::enabled!(tracing::Level::DEBUG) {
            let orig = serde_json::to_vec(messages).map(|v| v.len()).unwrap_or(0);
            let kept = serde_json::to_vec(&pruned).map(|v| v.len()).unwrap_or(0);
            tracing::debug!(
                grew,
                checkpoint_len = len,
                saved_bytes = orig.saturating_sub(kept),
                "trimwire: reprune re-checkpoint (cache bust)"
            );
        }
        // #144 nit2: serialize the mutated body BEFORE committing the checkpoint
        // advance. If the (near-impossible) re-serialize of a just-parsed Value fails we
        // forward the original bytes with `state` left exactly as it was — the byte-
        // forced snapshot restored, and the non-byte-forced path never mutated `state`
        // (decisions still held as locals). Only on success do we install the advanced
        // checkpoint and drop the snapshot.
        root["messages"] = Value::Array(pruned);
        let bytes = match serde_json::to_vec(&root) {
            Ok(bytes) => bytes,
            // Re-serialize of a mutated valid body failed → trimwire-caused (#138).
            Err(_) => {
                if let Some((cl, init, r, i, t, prefix)) = rollback {
                    state.checkpoint_len = cl;
                    state.initialized = init;
                    state.result_decisions = r;
                    state.input_decisions = i;
                    state.stripped_thinking = t;
                    state.checkpoint_prefix = prefix;
                }
                return rollback_outcome(input_valid);
            }
        };
        // Serialize succeeded → commit the advanced checkpoint (clearing the now-
        // stale summary on a prefix change) and discard the byte-forced snapshot.
        commit_checkpoint(
            state,
            results,
            inputs,
            thinking,
            len,
            new_prefix,
            prefix_changed,
        );
        drop(rollback);
        BodyOutcome::Mutated { bytes, fired }
    }
}

/// Splice the cached summary into a freshly-pruned CHECKPOINT array (if a valid
/// summary is cached), so the checkpoint's prefix matches the stable turns'. The
/// summary is anchored to the ORIGINAL slice and spliced into `pruned` (same
/// indexing — no strategy adds or removes whole messages); reverted if it would
/// orphan a pair. A stale summary (CC rewrote history → `prefix_changed`) is
/// SKIPPED here and cleared later at the commit point (`commit_checkpoint`), not
/// dropped eagerly — so a failed re-serialize can't lose it before the
/// checkpoint that replaces it actually commits (#164; mirrors the #144
/// decision-set fix). Reads `state.summary` only — never mutates it. Returns the
/// (possibly updated) `pruned` + `fired` and whether the summary applied.
/// When `[summarizer] engine = "model-free"` (the default), `summary` is always empty
/// and this is a cheap no-op.
fn apply_checkpoint_summary(
    mut pruned: Vec<Value>,
    mut fired: Vec<(&'static str, strategies::Stats)>,
    messages: &[Value],
    state: &PruneState,
    prefix_changed: bool,
) -> (Vec<Value>, Vec<(&'static str, strategies::Stats)>, bool) {
    // Skip a stale summary on a prefix change (don't splice it into rewritten
    // history); the clear is deferred to `commit_checkpoint`.
    let applied = if prefix_changed || state.summary.is_empty() {
        false
    } else {
        let pre = strategies::serialized_len(&pruned);
        let mut spliced = pruned.clone();
        if crate::summarizer::slice::apply_summaries(&mut spliced, messages, &state.summary)
            && PairingIndex::build(&spliced).validate().is_ok()
        {
            let post = strategies::serialized_len(&spliced);
            pruned = spliced;
            fired.push((
                "local_compaction",
                strategies::Stats {
                    stubbed: 1,
                    original_bytes: pre,
                    final_bytes: post,
                },
            ));
            true
        } else {
            false
        }
    };
    (pruned, fired, applied)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A growing Bash session: each turn appends an assistant tool_use + its
    /// result; every 4th result is a big (>16 KB) log that bloat_cap trims once
    /// it ages past the keep-recent window.
    fn body_at(turns: usize) -> Vec<u8> {
        let mut m = Vec::new();
        for i in 0..turns {
            let id = format!("u{i}");
            let out = if i % 4 == 0 {
                "x".repeat(20_000)
            } else {
                format!("step {i} ok")
            };
            m.push(json!({"role":"assistant","content":[
                {"type":"tool_use","id":id,"name":"Bash","input":{"command":format!("run {i}")}}
            ]}));
            m.push(json!({"role":"user","content":[
                {"type":"tool_result","tool_use_id":id,"content":out}
            ]}));
        }
        serde_json::to_vec(&json!({
            "model":"claude","system":[{"type":"text","text":"sys"}],"messages":m
        }))
        .unwrap()
    }

    fn cfg() -> Config {
        crate::config::profile_baseline("default")
    }

    fn bytes_of(o: &BodyOutcome, original: &[u8]) -> Vec<u8> {
        match o {
            // Both forward the original body verbatim.
            BodyOutcome::Unchanged | BodyOutcome::RolledBack => original.to_vec(),
            BodyOutcome::Mutated { bytes, .. } => bytes.clone(),
        }
    }

    /// #138 — the classifier that decides blame for a forward-original rollback.
    /// (The trimwire-caused rollback *branches* themselves are backstops for a
    /// future orphaning strategy — no current strategy can orphan a valid body —
    /// so this pins the decision logic they all funnel through.)
    #[test]
    fn rollback_outcome_classifies_by_input_validity() {
        assert!(
            matches!(rollback_outcome(true), BodyOutcome::RolledBack),
            "valid input + rollback ⇒ trimwire's fault ⇒ flagged"
        );
        assert!(
            matches!(rollback_outcome(false), BodyOutcome::Unchanged),
            "invalid input ⇒ client's fault ⇒ blameless"
        );
    }

    /// #138 — a CLIENT-malformed body (orphaned `tool_result`) routed through the
    /// stateful reprune path must forward the original as `Unchanged`, never
    /// `RolledBack`: the full-prune's `run()` rejects the orphaned input, and
    /// `input_valid=false` classifies it as blameless.
    #[test]
    fn stable_client_malformed_input_is_unchanged_not_rolled_back() {
        let body = serde_json::to_vec(&json!({
            "model": "claude-sonnet-4-5",
            "max_tokens": 1024,
            "messages": [
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "toolu_ghost",
                     "content": "x".repeat(9000)}
                ]},
            ],
        }))
        .unwrap();
        let mut state = PruneState::default();
        match stable_apply_to_body(&body, &cfg(), &mut state, 8) {
            BodyOutcome::Unchanged => {}
            BodyOutcome::RolledBack => {
                panic!("client-malformed input must NOT be flagged as a trimwire rollback")
            }
            BodyOutcome::Mutated { .. } => panic!("must not mutate a malformed input"),
        }
    }

    /// A growing session where every assistant turn carries a real (signed)
    /// thinking block; when `empty_on_last`, the most-recent turn ALSO carries an
    /// empty signature-only thinking block — the kind Claude Code re-emits on
    /// resume and Anthropic rejects with a hard 400. Turns 0..(turns-1) are
    /// identical regardless of the flag, so two bodies share a stable prefix.
    fn body_think(turns: usize, empty_on_last: bool) -> Vec<u8> {
        let mut m = Vec::new();
        for i in 0..turns {
            let id = format!("u{i}");
            let mut content = vec![
                json!({"type":"thinking","thinking":format!("reasoning {i} ....................."),"signature":format!("sig{i}")}),
                json!({"type":"tool_use","id":id,"name":"Bash","input":{"command":format!("run {i}")}}),
            ];
            if empty_on_last && i == turns - 1 {
                content.insert(
                    1,
                    json!({"type":"thinking","thinking":"","signature":"empty-sig"}),
                );
            }
            m.push(json!({"role":"assistant","content":content}));
            m.push(json!({"role":"user","content":[
                {"type":"tool_result","tool_use_id":id,"content":format!("out {i}")}
            ]}));
        }
        serde_json::to_vec(&json!({
            "model":"claude","system":[{"type":"text","text":"sys"}],"messages":m
        }))
        .unwrap()
    }

    fn count_empty_thinking(body: &[u8]) -> usize {
        let v: Value = serde_json::from_slice(body).unwrap();
        v["messages"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|m| m.get("content").and_then(Value::as_array))
            .flatten()
            .filter(|b| {
                b.get("type").and_then(Value::as_str) == Some("thinking")
                    && b.get("thinking")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .is_empty()
            })
            .count()
    }

    #[test]
    fn empty_thinking_sanitized_recent_window_and_cache_safe() {
        let cfg = cfg();
        // (A) stateless path removes a recent-window empty-thinking block (one
        //     that `thinking_strip` deliberately protects).
        let small = body_think(12, true);
        assert_eq!(
            count_empty_thinking(&small),
            1,
            "fixture has one empty-thinking"
        );
        let s = bytes_of(&strategies::apply_to_body(&small, &cfg), &small);
        assert_eq!(
            count_empty_thinking(&s),
            0,
            "stateless path sanitizes the recent-window empty-thinking"
        );

        // (B) reprune: a clean checkpoint, then a STABLE turn whose new tail
        //     carries an empty-thinking block — removed without re-checkpointing
        //     and without disturbing the byte-identical cached prefix.
        let mut state = PruneState::default();
        let b20 = body_think(20, false);
        let out20 = bytes_of(&stable_apply_to_body(&b20, &cfg, &mut state, 8), &b20);
        let cp_len = state.checkpoint_len;
        assert_eq!(count_empty_thinking(&out20), 0, "clean checkpoint has none");

        let b22 = body_think(22, true); // empty on turn 21: recent tail, beyond cp_len
        let out22 = bytes_of(&stable_apply_to_body(&b22, &cfg, &mut state, 8), &b22);
        assert_eq!(
            state.checkpoint_len, cp_len,
            "no re-checkpoint within threshold"
        );
        assert_eq!(
            count_empty_thinking(&out22),
            0,
            "stable branch sanitizes the recent-tail empty-thinking"
        );
        let p20: Value = serde_json::from_slice(&out20).unwrap();
        let p22: Value = serde_json::from_slice(&out22).unwrap();
        assert_eq!(
            serde_json::to_vec(&p20["messages"].as_array().unwrap()[..cp_len]).unwrap(),
            serde_json::to_vec(&p22["messages"].as_array().unwrap()[..cp_len]).unwrap(),
            "cached prefix stays byte-identical with the sanitize on"
        );
    }

    /// Defense-in-depth: an empty-thinking block in the OLD region (inside the
    /// checkpoint prefix) is removed by the regular `thinking_strip` path
    /// (recorded + replayed), so `strip_empty` never has to touch the prefix — and
    /// the prefix stays byte-identical on the next STABLE turn.
    #[test]
    fn empty_thinking_in_old_prefix_handled_by_replay_path() {
        let cfg = cfg();
        // Plant an empty-thinking block at an OLD turn (index 4 = turn 2), well
        // outside the recent window, so it lands in the checkpoint prefix.
        let plant = |turns: usize| -> Vec<u8> {
            let mut v: Value = serde_json::from_slice(&body_think(turns, false)).unwrap();
            v["messages"][4]["content"].as_array_mut().unwrap().insert(
                0,
                json!({"type":"thinking","thinking":"","signature":"old-empty"}),
            );
            serde_json::to_vec(&v).unwrap()
        };
        let b20 = plant(20);
        assert_eq!(
            count_empty_thinking(&b20),
            1,
            "fixture plants one in the old region"
        );

        let mut state = PruneState::default();
        let out20 = bytes_of(&stable_apply_to_body(&b20, &cfg, &mut state, 8), &b20);
        let cp_len = state.checkpoint_len;
        assert_eq!(
            count_empty_thinking(&out20),
            0,
            "old-region empty-thinking removed at the checkpoint (thinking_strip path)"
        );

        let b22 = plant(22);
        let out22 = bytes_of(&stable_apply_to_body(&b22, &cfg, &mut state, 8), &b22);
        assert_eq!(
            state.checkpoint_len, cp_len,
            "no re-checkpoint within threshold"
        );
        assert_eq!(count_empty_thinking(&out22), 0);
        let p20: Value = serde_json::from_slice(&out20).unwrap();
        let p22: Value = serde_json::from_slice(&out22).unwrap();
        assert_eq!(
            serde_json::to_vec(&p20["messages"].as_array().unwrap()[..cp_len]).unwrap(),
            serde_json::to_vec(&p22["messages"].as_array().unwrap()[..cp_len]).unwrap(),
            "prefix byte-identical: the old-region empty is handled by the recorded+replayed path"
        );
    }

    #[test]
    fn system_shape_normalize_fires_on_reprune_path() {
        // A malformed body (role:"system" as messages[0]) that Anthropic 400s.
        let body = serde_json::to_vec(&json!({
            "model":"claude",
            "messages":[
                {"role":"system","content":"sys"},
                {"role":"user","content":[{"type":"text","text":"hi"}]}
            ]
        }))
        .unwrap();

        // ENABLED on the reprune (default long-session) path → repaired.
        let mut c = cfg();
        c.strategies.system_shape_normalize.enabled = true;
        let mut state = PruneState::default();
        let out = bytes_of(&stable_apply_to_body(&body, &c, &mut state, 8), &body);
        let v: Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(
            v["system"],
            json!("sys"),
            "system lifted on the reprune path"
        );
        assert_eq!(
            v["messages"][0]["role"],
            json!("user"),
            "stray system removed on the reprune path"
        );

        // DISABLED (default) → the malformed body is forwarded unchanged.
        let mut state_off = PruneState::default();
        let off = bytes_of(
            &stable_apply_to_body(&body, &cfg(), &mut state_off, 8),
            &body,
        );
        let v_off: Value = serde_json::from_slice(&off).unwrap();
        assert_eq!(
            v_off["messages"][0]["role"],
            json!("system"),
            "off by default → malformed body forwarded as-is"
        );
    }

    #[test]
    fn cold_and_checkpoint_match_stateless() {
        // First call (cold) must equal the pure stateless prune, byte-for-byte.
        let body = body_at(30);
        let mut state = PruneState::default();
        let stateful = bytes_of(&stable_apply_to_body(&body, &cfg(), &mut state, 8), &body);
        let stateless = bytes_of(&strategies::apply_to_body(&body, &cfg()), &body);
        assert_eq!(
            stateful, stateless,
            "cold stateful output must equal stateless"
        );
    }

    #[test]
    fn stable_keeps_prefix_byte_identical_then_rechckpoints() {
        let cfg = cfg();
        let mut state = PruneState::default();
        // Checkpoint at turn 20.
        let b20 = body_at(20);
        let out20 = bytes_of(&stable_apply_to_body(&b20, &cfg, &mut state, 8), &b20);
        let cp_len = state.checkpoint_len;
        // A few more turns within the threshold → STABLE: the pruned prefix
        // (first cp_len messages) is byte-identical to turn 20's output.
        let b22 = body_at(22);
        let out22 = bytes_of(&stable_apply_to_body(&b22, &cfg, &mut state, 8), &b22);
        let p20: Value = serde_json::from_slice(&out20).unwrap();
        let p22: Value = serde_json::from_slice(&out22).unwrap();
        let m20 = p20["messages"].as_array().unwrap();
        let m22 = p22["messages"].as_array().unwrap();
        assert_eq!(
            serde_json::to_vec(&m20[..cp_len]).unwrap(),
            serde_json::to_vec(&m22[..cp_len]).unwrap(),
            "stable branch must keep the checkpoint prefix byte-identical"
        );
        assert_eq!(
            state.checkpoint_len, cp_len,
            "no re-checkpoint within threshold"
        );
        // Grow well past the threshold → re-checkpoint (== stateless at that len).
        let b40 = body_at(40);
        let out40 = bytes_of(&stable_apply_to_body(&b40, &cfg, &mut state, 8), &b40);
        assert!(
            state.checkpoint_len > cp_len,
            "tail past threshold forces a re-checkpoint"
        );
        let stateless40 = bytes_of(&strategies::apply_to_body(&b40, &cfg), &b40);
        assert_eq!(
            out40, stateless40,
            "a re-checkpoint equals the stateless prune"
        );
    }

    /// The new demand-page marker text (Part B) is deterministic — it embeds the
    /// file path + raw byte count — so reprune replays it identically across turns,
    /// keeping the pruned prefix byte-identical (cache-stable). Runs
    /// `stable_apply_to_body` at turn N and again at turn N+2 on a stable prefix
    /// and asserts the first `checkpoint_len` messages are byte-for-byte identical.
    /// Also verifies the new marker text (`trimwire report`) appears in the prefix.
    #[test]
    fn new_markers_keep_prefix_byte_identical() {
        /// A session: turn 0 is a large Read (will be demand-paged by the default
        /// profile), followed by small Bash calls that age the Read past
        /// `keep_recent_turns = 4`.
        fn body_with_reads(total_turns: usize) -> Vec<u8> {
            let mut m = Vec::new();
            // Turn 0: large Read — 20 KB > page_min_bytes=16384 in the default profile.
            let file_content = "x".repeat(20_000);
            m.push(json!({"role":"assistant","content":[
                {"type":"tool_use","id":"r0","name":"Read","input":{"path":"/src/large.rs"}}
            ]}));
            m.push(json!({"role":"user","content":[
                {"type":"tool_result","tool_use_id":"r0","content":file_content}
            ]}));
            // Remaining turns: small Bash results to age the Read past keep_recent=4.
            for i in 1..total_turns {
                let id = format!("u{i}");
                m.push(json!({"role":"assistant","content":[
                    {"type":"tool_use","id":id,"name":"Bash","input":{"command":format!("run {i}")}}
                ]}));
                m.push(json!({"role":"user","content":[
                    {"type":"tool_result","tool_use_id":id,"content":format!("step {i} ok")}
                ]}));
            }
            serde_json::to_vec(&json!({
                "model": "claude",
                "system": [{"type":"text","text":"sys"}],
                "messages": m
            }))
            .unwrap()
        }

        let cfg = cfg(); // default profile: stale_reads.page_min_bytes=16384, keep_recent=4
        let mut state = PruneState::default();

        // Turn N=10: cold checkpoint. stale_reads demand-pages the large Read
        // (read_count=1, age=9 assistant turns > keep_recent=4, size=20000 > 16384).
        let b10 = body_with_reads(10);
        let out10 = bytes_of(&stable_apply_to_body(&b10, &cfg, &mut state, 8), &b10);
        let cp_len = state.checkpoint_len;

        // The demand-page marker must be present in the checkpoint prefix.
        let p10: Value = serde_json::from_slice(&out10).unwrap();
        let m10 = p10["messages"].as_array().unwrap();
        let read_content = m10[1]["content"][0]["content"].as_str().unwrap_or("");
        assert!(
            read_content.starts_with("[trimwire: paged out"),
            "expected demand-page marker in checkpoint prefix at turn 10; got: {}",
            &read_content[..read_content.len().min(80)]
        );
        assert!(
            read_content.contains("trimwire report"),
            "new marker text must reference `trimwire report`; got: {read_content}"
        );

        // Turn N+2=12: stable branch (grew=4 <= threshold=8, new tail is tiny).
        // The demand-page marker is replayed from state.result_decisions → byte-identical.
        let b12 = body_with_reads(12);
        let out12 = bytes_of(&stable_apply_to_body(&b12, &cfg, &mut state, 8), &b12);
        assert_eq!(
            state.checkpoint_len, cp_len,
            "no re-checkpoint within threshold"
        );

        let p12: Value = serde_json::from_slice(&out12).unwrap();
        let m12 = p12["messages"].as_array().unwrap();
        assert_eq!(
            serde_json::to_vec(&m10[..cp_len]).unwrap(),
            serde_json::to_vec(&m12[..cp_len]).unwrap(),
            "new demand-page marker must be replayed byte-identically (cache-stable)"
        );
    }

    /// Same conversation, but every object's keys are emitted in a DIFFERENT wire
    /// order than the checkpoint body. serde_json parses objects into a key-sorted
    /// `Map`, so the structural compaction guard sees the prefix as unchanged and
    /// stays append-only — no spurious re-checkpoint — and the pruned prefix stays
    /// byte-identical (the cache holds). This pins the new guard's key-order
    /// independence and would fail loudly if a transitive `serde_json/preserve_order`
    /// feature flip ever made key order significant.
    #[test]
    fn reordered_prefix_keys_stay_append_only() {
        // Hand-emit the body so we control object key order on the wire (the json!
        // macro + to_vec would canonicalize it away). `swap` reverses every object's
        // keys; both forms parse to the same key-sorted `Value`.
        fn body_keyorder(turns: usize, swap: bool) -> Vec<u8> {
            let mut s = String::from(
                "{\"model\":\"claude\",\"system\":[{\"type\":\"text\",\"text\":\"sys\"}],\"messages\":[",
            );
            for i in 0..turns {
                if i > 0 {
                    s.push(',');
                }
                if swap {
                    s.push_str(&format!(
                        "{{\"content\":[{{\"input\":{{\"command\":\"run {i}\"}},\"name\":\"Bash\",\"id\":\"u{i}\",\"type\":\"tool_use\"}}],\"role\":\"assistant\"}},\
                         {{\"content\":[{{\"content\":\"step {i} ok\",\"tool_use_id\":\"u{i}\",\"type\":\"tool_result\"}}],\"role\":\"user\"}}"
                    ));
                } else {
                    s.push_str(&format!(
                        "{{\"role\":\"assistant\",\"content\":[{{\"type\":\"tool_use\",\"id\":\"u{i}\",\"name\":\"Bash\",\"input\":{{\"command\":\"run {i}\"}}}}]}},\
                         {{\"role\":\"user\",\"content\":[{{\"type\":\"tool_result\",\"tool_use_id\":\"u{i}\",\"content\":\"step {i} ok\"}}]}}"
                    ));
                }
            }
            s.push_str("]}");
            s.into_bytes()
        }

        let cfg = cfg();
        let mut state = PruneState::default();
        // Cold checkpoint at 10 turns (20 messages), canonical key order.
        let b10 = body_keyorder(10, false);
        let out10 = bytes_of(&stable_apply_to_body(&b10, &cfg, &mut state, 8), &b10);
        let cp = state.checkpoint_len;
        assert!(cp > 0 && state.initialized, "cold call must checkpoint");
        // A stable turn whose prefix has every key reordered. The structural guard
        // must still see it as append-only (no re-checkpoint).
        let b11 = body_keyorder(11, true);
        let out11 = bytes_of(&stable_apply_to_body(&b11, &cfg, &mut state, 8), &b11);
        assert_eq!(
            state.checkpoint_len, cp,
            "reordered keys must NOT trigger a spurious re-checkpoint"
        );
        assert_eq!(
            msgs_prefix(&out10, cp),
            msgs_prefix(&out11, cp),
            "pruned prefix must stay byte-identical despite reordered input keys"
        );
    }

    #[test]
    fn compaction_forces_recheckpoint_not_stale_decisions() {
        // Checkpoint on a long session, then "compact" to a shorter, rewritten
        // history (NOT an append extension). The prefix-hash guard must detect it
        // and re-checkpoint, producing exactly the stateless prune of the new
        // array — never applying stale decisions to rewritten messages.
        let cfg = cfg();
        let mut state = PruneState::default();
        let long = body_at(30);
        let _ = stable_apply_to_body(&long, &cfg, &mut state, 8);

        // Compacted: a summary turn + the 3 most recent turns (fresh ids/content).
        let mut m = vec![json!({"role":"user","content":"[summary of earlier work]"})];
        for i in 100..103 {
            let id = format!("c{i}");
            m.push(json!({"role":"assistant","content":[
                {"type":"tool_use","id":id,"name":"Bash","input":{"command":format!("post {i}")}}
            ]}));
            m.push(json!({"role":"user","content":[
                {"type":"tool_result","tool_use_id":id,"content":format!("fresh {i}")}
            ]}));
        }
        let compacted = serde_json::to_vec(
            &json!({"model":"claude","system":[{"type":"text","text":"sys"}],"messages":m}),
        )
        .unwrap();

        let stateful = bytes_of(
            &stable_apply_to_body(&compacted, &cfg, &mut state, 8),
            &compacted,
        );
        let stateless = bytes_of(&strategies::apply_to_body(&compacted, &cfg), &compacted);
        assert_eq!(
            stateful, stateless,
            "a prefix change (compaction) must re-checkpoint to the stateless result"
        );
        // And the output is orphan-free + system intact.
        let v: Value = serde_json::from_slice(&stateful).unwrap();
        assert_eq!(
            v.get("system"),
            serde_json::from_slice::<Value>(&compacted)
                .unwrap()
                .get("system")
        );
        PairingIndex::build(v["messages"].as_array().unwrap())
            .validate()
            .unwrap();
    }

    #[test]
    fn missing_messages_or_bad_json_is_unchanged() {
        let mut state = PruneState::default();
        assert!(matches!(
            stable_apply_to_body(b"not json", &cfg(), &mut state, 8),
            BodyOutcome::Unchanged
        ));
        let no_msgs = serde_json::to_vec(&json!({"model":"x"})).unwrap();
        assert!(matches!(
            stable_apply_to_body(&no_msgs, &cfg(), &mut state, 8),
            BodyOutcome::Unchanged
        ));
    }

    // --- thinking_strip bypass guard (reprune can't replay block removals) ---

    fn cfg_with_thinking_strip() -> Config {
        let mut c = cfg();
        c.strategies.thinking_strip.enabled = true;
        c.strategies.thinking_strip.keep_recent_turns = 3;
        c
    }

    /// A growing session where each assistant turn is a WIRE-shaped grouped
    /// `[thinking, tool_use]` content array (the form thinking_strip targets).
    fn thinking_body_at(turns: usize) -> Vec<u8> {
        let mut m = Vec::new();
        for i in 0..turns {
            let id = format!("u{i}");
            m.push(json!({"role":"assistant","content":[
                {"type":"thinking","thinking":format!("reasoning about step {i}, with enough text to matter"),"signature":format!("sig{i}")},
                {"type":"tool_use","id":id,"name":"Bash","input":{"command":format!("run {i}")}}
            ]}));
            m.push(json!({"role":"user","content":[
                {"type":"tool_result","tool_use_id":id,"content":format!("step {i} ok")}
            ]}));
        }
        serde_json::to_vec(&json!({
            "model":"claude","system":[{"type":"text","text":"sys"}],"messages":m
        }))
        .unwrap()
    }

    /// Cold (first) call with thinking_strip ON takes the CHECKPOINT path, which
    /// must be byte-identical to the stateless prune (the correctness floor — the
    /// stateful path never produces different output, only better cache behavior).
    #[test]
    fn thinking_strip_cold_checkpoint_equals_stateless() {
        let body = thinking_body_at(20);
        let on = cfg_with_thinking_strip();
        let mut state = PruneState::default();
        let stateful = bytes_of(&stable_apply_to_body(&body, &on, &mut state, 8), &body);
        let stateless = bytes_of(&strategies::apply_to_body(&body, &on), &body);
        assert_eq!(
            stateful, stateless,
            "cold checkpoint with thinking_strip must equal the stateless prune"
        );
    }

    /// Serialize `messages[..k]` from an outcome's bytes — the pruned prefix whose
    /// byte-stability across turns is exactly what keeps Anthropic's cache warm.
    fn msgs_prefix(bytes: &[u8], k: usize) -> Vec<u8> {
        let v: Value = serde_json::from_slice(bytes).unwrap();
        let msgs = v.get("messages").and_then(Value::as_array).unwrap();
        serde_json::to_vec(&msgs[..k.min(msgs.len())]).unwrap()
    }

    // ---- Offline repros for the "Read coverage gap" fix (#1 age-gate + #2 byte re-checkpoint) ----

    /// A growing read-heavy session: a `start` user message, then `reads` Read
    /// turns (each = assistant tool_use + user tool_result of `size` bytes) with
    /// DISTINCT file paths. `reads == 0` is just the start message (a cold,
    /// empty-decision checkpoint — the live regime that froze CANARY-01).
    fn read_session_body(reads: usize, size: usize) -> Vec<u8> {
        let mut m = vec![json!({"role":"user","content":[{"type":"text","text":"start"}]})];
        for i in 0..reads {
            let id = format!("r{i}");
            m.push(json!({"role":"assistant","content":[
                {"type":"tool_use","id":id,"name":"Read","input":{"file_path":format!("/f{i}")}}
            ]}));
            m.push(json!({"role":"user","content":[
                {"type":"tool_result","tool_use_id":id,"content":"z".repeat(size)}
            ]}));
        }
        serde_json::to_vec(&json!({
            "model":"claude","system":[{"type":"text","text":"sys"}],"messages":m
        }))
        .unwrap()
    }

    /// Focused config that isolates the two fixes: reprune on (with the byte-based
    /// re-checkpoint knob) + the age-gated bloat_cap, everything else off. Small
    /// head/tail so a trimmed result falls cleanly under threshold (idempotent).
    fn read_fix_cfg(recheckpoint: usize) -> Config {
        let mut c = Config::default();
        c.reprune.enabled = true;
        c.reprune.recheckpoint_result_bytes = recheckpoint;
        c.strategies.bloat_cap.enabled = true;
        c.strategies.bloat_cap.threshold_bytes = 4_096;
        c.strategies.bloat_cap.head_bytes = 512;
        c.strategies.bloat_cap.tail_bytes = 512;
        c.strategies.bloat_cap.keep_recent_turns = 2;
        c.strategies.bloat_cap.exempt_tools = ["Edit", "Write", "MultiEdit", "Task", "Agent"]
            .iter()
            .map(|s| (*s).to_owned())
            .collect();
        c.strategies.bloat_cap.exempt_recent_only_tools = vec!["Read".to_owned()];
        c
    }

    /// Repro #6 — short-but-large: an empty first checkpoint, then a few BIG reads
    /// land within `grew <= threshold` MESSAGES. WITHOUT the byte trigger the stable
    /// branch replays the empty checkpoint forever (the live ~0%); WITH it the large
    /// new tail forces a re-checkpoint and the age-gated bloat_cap trims the old read.
    #[test]
    fn byte_based_recheckpoint_trims_old_reads_short_session() {
        let cold = read_session_body(0, 0); // just the 1-msg start
        let body = read_session_body(4, 24_000); // start + 4 big Read turns (len = 9)

        // --- knob OFF: stays stable, replays the empty checkpoint → 0% (the bug) ---
        {
            let cfg = read_fix_cfg(0);
            let mut state = PruneState::default();
            let _ = stable_apply_to_body(&cold, &cfg, &mut state, 20);
            assert_eq!(
                state.checkpoint_len, 1,
                "cold checkpoint at the 1-msg start"
            );
            let out = stable_apply_to_body(&body, &cfg, &mut state, 20);
            assert_eq!(
                state.checkpoint_len, 1,
                "knob off + grew<=threshold → no re-checkpoint"
            );
            assert!(
                matches!(out, BodyOutcome::Unchanged),
                "knob off → big new reads pass through untrimmed (the live short-session 0% gap)"
            );
        }

        // --- knob ON: the big new tail forces a re-checkpoint + trims the old read ---
        let cfg = read_fix_cfg(65_536);
        let mut state = PruneState::default();
        let _ = stable_apply_to_body(&cold, &cfg, &mut state, 20);
        assert_eq!(state.checkpoint_len, 1);
        let out = stable_apply_to_body(&body, &cfg, &mut state, 20);
        assert!(
            state.checkpoint_len > 1,
            "byte-based re-checkpoint fired despite grew<=threshold"
        );
        let bytes = bytes_of(&out, &body);
        assert!(bytes.len() < body.len(), "old reads trimmed → smaller body");
        let v: Value = serde_json::from_slice(&bytes).unwrap();
        let m = v["messages"].as_array().unwrap();
        assert!(
            serde_json::to_string(m)
                .unwrap()
                .contains("[trimwire: trimmed"),
            "an OLD Read result was bloat_capped"
        );
        assert_eq!(
            m.last().unwrap()["content"][0]["content"]
                .as_str()
                .unwrap()
                .len(),
            24_000,
            "the most-recent read is left intact (recent-only exemption)"
        );
        PairingIndex::build(m).validate().unwrap();
    }

    /// Repro #7 — longer growing session: old reads trimmed, recent reads intact,
    /// and the pruned prefix stays byte-identical between re-checkpoints (cache holds).
    #[test]
    fn long_growing_read_session_trims_old_keeps_recent_cache_stable() {
        let cfg = read_fix_cfg(65_536); // reads stay under the byte trigger → message cadence governs
        let mut state = PruneState::default();

        // Cold checkpoint at 20 reads. Old reads trimmed; the most-recent intact.
        let b20 = read_session_body(20, 8_000);
        let out20 = bytes_of(&stable_apply_to_body(&b20, &cfg, &mut state, 8), &b20);
        let cp = state.checkpoint_len;
        assert!(cp > 0 && state.initialized, "cold call must checkpoint");
        let v20: Value = serde_json::from_slice(&out20).unwrap();
        let m20 = v20["messages"].as_array().unwrap();
        assert!(
            serde_json::to_string(m20)
                .unwrap()
                .contains("[trimwire: trimmed"),
            "old reads are trimmed at the checkpoint"
        );
        assert_eq!(
            m20.last().unwrap()["content"][0]["content"]
                .as_str()
                .unwrap()
                .len(),
            8_000,
            "the most-recent read is intact"
        );

        // Grow within the message threshold → STABLE: pruned prefix byte-identical.
        let b22 = read_session_body(22, 8_000);
        let out22 = bytes_of(&stable_apply_to_body(&b22, &cfg, &mut state, 8), &b22);
        assert_eq!(
            state.checkpoint_len, cp,
            "no re-checkpoint within the message threshold"
        );
        assert_eq!(
            msgs_prefix(&out20, cp),
            msgs_prefix(&out22, cp),
            "pruned prefix stays byte-identical between re-checkpoints (cache holds)"
        );

        // Grow past the threshold → re-checkpoint == the stateless prune.
        let b30 = read_session_body(30, 8_000);
        let out30 = bytes_of(&stable_apply_to_body(&b30, &cfg, &mut state, 8), &b30);
        assert!(
            state.checkpoint_len > cp,
            "tail past threshold forces a re-checkpoint"
        );
        let stateless30 = bytes_of(&strategies::apply_to_body(&b30, &cfg), &b30);
        assert_eq!(
            out30, stateless30,
            "a re-checkpoint equals the stateless prune"
        );
        let v30: Value = serde_json::from_slice(&out30).unwrap();
        PairingIndex::build(v30["messages"].as_array().unwrap())
            .validate()
            .unwrap();
    }

    /// Repro #8 (live-canary run-B regression) — a byte-FORCED re-checkpoint that
    /// prunes NOTHING (fired before any read aged past keep_recent) must NOT advance
    /// the checkpoint, or it starves the later message-count re-checkpoint that WOULD
    /// trim the now-aged reads (run B ended at 0% because of this). The fix rolls the
    /// checkpoint state back on a byte-forced no-op.
    #[test]
    fn byte_forced_noop_recheckpoint_does_not_starve_later_trim() {
        let cfg = read_fix_cfg(131_072); // the shipped default knob
        let mut state = PruneState::default();

        // Cold checkpoint at the 1-msg start (mirrors CC's early empty checkpoint).
        let cold = read_session_body(0, 0);
        let _ = stable_apply_to_body(&cold, &cfg, &mut state, 8);
        assert_eq!(
            state.checkpoint_len, 1,
            "cold checkpoint at the start message"
        );

        // 3 big reads: byte tail = 3×58 KB = 174 KB > 128 KB → byte trigger fires at
        // grew=6 (≤ threshold). But 3 reads with keep_recent=2 → 0 are old enough to
        // trim → no-op. The rollback must leave the checkpoint at 1.
        let b3 = read_session_body(3, 58_000);
        let out3 = stable_apply_to_body(&b3, &cfg, &mut state, 8);
        assert!(
            matches!(out3, BodyOutcome::Unchanged),
            "premature byte-forced re-checkpoint trims nothing (nothing aged yet)"
        );
        assert_eq!(
            state.checkpoint_len, 1,
            "a byte-forced NO-OP must NOT advance the checkpoint (rolled back)"
        );

        // Grow to 5 reads: from the (un-advanced) cp=1, grew=10 > threshold → a normal
        // message-count re-checkpoint fires and trims the now-aged reads. (Pre-fix the
        // checkpoint would have advanced to 7, leaving grew=4 here → no re-checkpoint
        // → 0%, the regression.)
        let b5 = read_session_body(5, 58_000);
        let out5 = stable_apply_to_body(&b5, &cfg, &mut state, 8);
        let bytes = bytes_of(&out5, &b5);
        assert!(
            bytes.len() < b5.len(),
            "the later re-checkpoint trims the aged reads — no starvation"
        );
        assert!(
            state.checkpoint_len > 1,
            "the trimming re-checkpoint advanced the checkpoint"
        );
        let v: Value = serde_json::from_slice(&bytes).unwrap();
        assert!(
            serde_json::to_string(&v["messages"])
                .unwrap()
                .contains("[trimwire: trimmed"),
            "old reads were bloat_capped at the later re-checkpoint"
        );
        PairingIndex::build(v["messages"].as_array().unwrap())
            .validate()
            .unwrap();
    }

    /// #144 nit2 — a MUTATING re-checkpoint advances the checkpoint pointer and
    /// installs its decision sets *together* (`commit_checkpoint`, only after the body
    /// serializes). This pins the invariant the fix guarantees: the pointer never
    /// advances without its decisions (a half-advanced state would replay stale/empty
    /// decisions next turn). We can't force the near-impossible `to_vec(&Value)` failure
    /// that motivated the fix, so we assert the post-state is internally consistent AND
    /// that the committed decisions replay byte-identically on the next stable turn.
    #[test]
    fn recheckpoint_commits_pointer_and_decisions_atomically() {
        let cfg = read_fix_cfg(65_536); // message-cadence governs (reads under the byte trigger)
        let mut state = PruneState::default();

        // Cold checkpoint over 20 reads: a real mutating prune (old reads bloat_capped).
        let b20 = read_session_body(20, 8_000);
        let out20 = bytes_of(&stable_apply_to_body(&b20, &cfg, &mut state, 8), &b20);
        let cp = state.checkpoint_len;

        // Pointer, prefix, and decisions all advanced together — never half-committed.
        assert!(
            state.initialized && cp == 41,
            "checkpoint advanced to 1 start + 20×2"
        );
        assert_eq!(
            state.checkpoint_prefix.len(),
            cp,
            "checkpoint_prefix installed alongside checkpoint_len"
        );
        assert!(
            !state.result_decisions.is_empty(),
            "a mutating prune's decisions are committed with the pointer, not left empty"
        );

        // The committed decisions are the REAL ones: a stable turn within the message
        // threshold replays them to a byte-identical pruned prefix (would diverge if
        // commit_checkpoint had installed the wrong/empty set).
        let b22 = read_session_body(22, 8_000);
        let out22 = bytes_of(&stable_apply_to_body(&b22, &cfg, &mut state, 8), &b22);
        assert_eq!(state.checkpoint_len, cp, "stable turn — no re-checkpoint");
        assert_eq!(
            msgs_prefix(&out20, cp),
            msgs_prefix(&out22, cp),
            "committed decisions replay byte-identically (cache holds)"
        );
    }

    /// #144 nit1 (MECHANISM) — isolates *why* the telemetry must read decisions before
    /// the byte-forced `rollback` drains them: replaying the recorded decisions on the
    /// grown array yields a materially SMALLER body than replaying against an emptied
    /// state (which equals the raw body). So a metric computed AFTER the drain diffs the
    /// pruned body against RAW and over-reports. This is a unit test of `replay_decisions`
    /// in isolation — it pins the mechanism, not the telemetry call site itself. (An
    /// end-to-end capture of the logged `marginal_saved` was prototyped but dropped: it
    /// requires a DEBUG tracing subscriber, and `tracing`'s process-global callsite
    /// interest cache makes such a capture race with other tests in a parallel binary —
    /// a flaky test is worse than none for DEBUG-only telemetry with no runtime effect.)
    #[test]
    fn marginal_replay_must_read_decisions_before_byte_forced_drain() {
        let cfg = read_fix_cfg(65_536);
        let mut state = PruneState::default();
        // A checkpoint over 20 reads records real trim decisions (old reads capped).
        let b20 = read_session_body(20, 8_000);
        let _ = stable_apply_to_body(&b20, &cfg, &mut state, 8);
        assert!(
            !state.result_decisions.is_empty(),
            "the checkpoint recorded trim decisions to replay"
        );

        // A grown, append-only array carrying the same ids r0.. plus two new reads.
        let b22 = read_session_body(22, 8_000);
        let v: Value = serde_json::from_slice(&b22).unwrap();
        let msgs = v["messages"].as_array().unwrap().clone();
        let raw_len = serde_json::to_vec(&msgs).unwrap().len();

        // Replaying the INTACT decisions trims the old reads → smaller than raw.
        let (intact_out, _) =
            replay_decisions(&msgs, &state).expect("overwrite replay never orphans");
        let intact_len = serde_json::to_vec(&intact_out).unwrap().len();

        // Drain the decisions exactly as the byte-forced `rollback` snapshot does.
        let mut drained = state;
        let _ = std::mem::take(&mut drained.result_decisions);
        let _ = std::mem::take(&mut drained.input_decisions);
        let _ = std::mem::take(&mut drained.stripped_thinking);
        let (drained_out, _) =
            replay_decisions(&msgs, &drained).expect("empty replay is a no-op, never orphans");
        let drained_len = serde_json::to_vec(&drained_out).unwrap().len();

        assert_eq!(
            drained_len, raw_len,
            "a drained (empty) replay equals the raw body"
        );
        assert!(
            intact_len < drained_len,
            "intact replay ({intact_len}) is smaller than the drained/raw body ({drained_len}); \
             reading the marginal metric AFTER the drain would diff against raw and over-report"
        );
    }

    /// #144 nit2 (WIRING) — `compute_decisions`/`commit_checkpoint` thread three
    /// same-shaped collections positionally, so a swapped argument would type-check
    /// silently. Install three DISTINGUISHABLE sets and assert each lands in its own
    /// `PruneState` field (result vs input vs thinking), plus the pointer/prefix.
    #[test]
    fn commit_checkpoint_installs_each_set_in_its_own_field() {
        let mut state = PruneState::default();
        let mut results = HashMap::new();
        results.insert("res".to_owned(), json!("R"));
        let mut inputs = HashMap::new();
        inputs.insert("inp".to_owned(), json!("I"));
        let mut thinking = HashSet::new();
        thinking.insert("thk".to_owned());
        let prefix = vec![json!({"role": "user", "content": "p"})];

        // clear_summary=false leaves a cached summary intact (no prefix change).
        state.summary = vec![crate::summarizer::slice::SummaryDecision {
            start: 0,
            end: 1,
            slice_hash: "h".to_owned(),
            messages: vec![],
        }];
        commit_checkpoint(
            &mut state,
            results,
            inputs,
            thinking,
            7,
            prefix.clone(),
            false,
        );

        assert!(state.initialized);
        assert_eq!(state.checkpoint_len, 7);
        assert_eq!(state.checkpoint_prefix, prefix);
        // Each set in its OWN field — catches an argument-order swap.
        assert_eq!(state.result_decisions.get("res"), Some(&json!("R")));
        assert_eq!(state.input_decisions.get("inp"), Some(&json!("I")));
        assert!(state.stripped_thinking.contains("thk"));
        assert!(!state.result_decisions.contains_key("inp"));
        assert!(!state.input_decisions.contains_key("res"));
        assert!(
            !state.summary.is_empty(),
            "clear_summary=false keeps the summary"
        );

        // #164: clear_summary=true drops the (now-stale) summary as part of the
        // same commit — only reachable here, post-serialize, never eagerly.
        commit_checkpoint(
            &mut state,
            HashMap::new(),
            HashMap::new(),
            HashSet::new(),
            8,
            prefix,
            true,
        );
        assert!(
            state.summary.is_empty(),
            "clear_summary=true drops the summary"
        );
    }

    /// THE cache-stability guarantee for thinking_strip: across consecutive
    /// append-only STABLE turns, the pruned messages PREFIX is byte-identical, so
    /// the prompt cache holds (one bust per checkpoint, not per turn). Without the
    /// removal-replay this would change every turn as blocks age out (the old bug).
    #[test]
    fn thinking_strip_stable_prefix_is_byte_identical() {
        let on = cfg_with_thinking_strip();
        let mut state = PruneState::default();
        // Cold checkpoint at 10 turns (20 messages).
        let b10 = thinking_body_at(10);
        let out10 = bytes_of(&stable_apply_to_body(&b10, &on, &mut state, 8), &b10);
        let cp = state.checkpoint_len;
        assert!(cp > 0 && state.initialized, "cold call must checkpoint");
        let prefix0 = msgs_prefix(&out10, cp);
        // Two append-only stable turns must REPLAY the same removals + overwrites.
        let b11 = thinking_body_at(11);
        let out11 = bytes_of(&stable_apply_to_body(&b11, &on, &mut state, 8), &b11);
        let b12 = thinking_body_at(12);
        let out12 = bytes_of(&stable_apply_to_body(&b12, &on, &mut state, 8), &b12);
        assert_eq!(
            state.checkpoint_len, cp,
            "turns 11/12 must stay on the same checkpoint"
        );
        assert_eq!(
            prefix0,
            msgs_prefix(&out11, cp),
            "stable turn 11 changed the pruned prefix → cache bust (replay broken)"
        );
        assert_eq!(
            prefix0,
            msgs_prefix(&out12, cp),
            "stable turn 12 changed the pruned prefix → cache bust (replay broken)"
        );
        // And the replay genuinely STRIPPED the old thinking (sig0) while keeping a
        // recent one (sig9) — proving it's not just "forwarded everything".
        let s11 = String::from_utf8(out11).unwrap();
        assert!(
            !s11.contains("\"sig0\""),
            "old thinking (sig0) must be stripped on stable turns"
        );
        assert!(
            s11.contains("\"sig9\""),
            "recent thinking (sig9) must be kept"
        );
    }

    /// Determinism on BOTH paths: two independent states with thinking_strip ON
    /// produce byte-identical output for the cold CHECKPOINT and a later STABLE turn.
    #[test]
    fn thinking_strip_reprune_is_deterministic() {
        let on = cfg_with_thinking_strip();
        let b10 = thinking_body_at(10);
        let b11 = thinking_body_at(11);
        let mut s1 = PruneState::default();
        let mut s2 = PruneState::default();
        // Checkpoint path.
        let a1 = bytes_of(&stable_apply_to_body(&b10, &on, &mut s1, 8), &b10);
        let b1 = bytes_of(&stable_apply_to_body(&b10, &on, &mut s2, 8), &b10);
        assert_eq!(a1, b1, "checkpoint outputs must be identical");
        // Stable path (append-only) on each warmed state.
        let a2 = bytes_of(&stable_apply_to_body(&b11, &on, &mut s1, 8), &b11);
        let b2 = bytes_of(&stable_apply_to_body(&b11, &on, &mut s2, 8), &b11);
        assert_eq!(a2, b2, "stable-branch outputs must also be identical");
    }

    /// The prefix stays byte-identical ACROSS a re-checkpoint too: cold → stable →
    /// grow past threshold (re-checkpoint) → stable. Proves the SECOND checkpoint's
    /// removal set is rebuilt correctly and its stable turns hold the cache.
    #[test]
    fn thinking_strip_holds_prefix_across_a_recheckpoint() {
        let on = cfg_with_thinking_strip();
        let mut state = PruneState::default();
        // Cold checkpoint at 10 turns; one stable turn.
        let _ = stable_apply_to_body(&thinking_body_at(10), &on, &mut state, 8);
        let _ = stable_apply_to_body(&thinking_body_at(11), &on, &mut state, 8);
        // Grow past threshold (20→30 messages, grew=10 > 8) → forced re-checkpoint.
        let b15 = thinking_body_at(15);
        let out15 = bytes_of(&stable_apply_to_body(&b15, &on, &mut state, 8), &b15);
        let cp2 = state.checkpoint_len;
        assert_eq!(cp2, 30, "should have re-checkpointed at 15 turns");
        let prefix2 = msgs_prefix(&out15, cp2);
        // Stable turns on the 2nd checkpoint must replay it byte-identically.
        let b16 = thinking_body_at(16);
        let out16 = bytes_of(&stable_apply_to_body(&b16, &on, &mut state, 8), &b16);
        assert_eq!(
            state.checkpoint_len, cp2,
            "turn 16 must be stable on the 2nd checkpoint"
        );
        assert_eq!(
            prefix2,
            msgs_prefix(&out16, cp2),
            "2nd-checkpoint stable prefix must be byte-identical (cache holds across re-checkpoints)"
        );
        let s16 = String::from_utf8(out16).unwrap();
        assert!(
            !s16.contains("\"sig0\""),
            "old thinking still stripped after the re-checkpoint"
        );
    }

    // --- local-model summary replay (opt-in, off unless enabled in config) ---

    fn messages_of(body: &[u8]) -> Vec<Value> {
        let v: Value = serde_json::from_slice(body).unwrap();
        v["messages"].as_array().unwrap().clone()
    }

    /// A cached summary is replayed by range-substitution on every STABLE turn,
    /// keeping the (now-shorter) summarized prefix byte-identical turn-to-turn —
    /// the same cache-stability guarantee thinking_strip relies on. The summarized
    /// tool ids vanish; the summary text appears; pairing stays valid.
    #[test]
    fn summary_replays_byte_stable_across_stable_turns() {
        use crate::summarizer::slice::SummaryDecision;
        let cfg = cfg();
        let mut state = PruneState::default();
        // Cold checkpoint at 10 turns (20 messages).
        let b10 = body_at(10);
        let _ = stable_apply_to_body(&b10, &cfg, &mut state, 8);
        let cp = state.checkpoint_len();
        // Inject a summary over an OLD slice (4 whole pairs) inside the prefix.
        let msgs = messages_of(&b10);
        let d = SummaryDecision::new(&msgs, 0, 8, "## Goal\nLOCAL_SUMMARY_NEEDLE").unwrap();
        state.set_summary(d);
        // Two append-only stable turns must replay the summary identically.
        let b11 = body_at(11);
        let out11 = bytes_of(&stable_apply_to_body(&b11, &cfg, &mut state, 8), &b11);
        let b12 = body_at(12);
        let out12 = bytes_of(&stable_apply_to_body(&b12, &cfg, &mut state, 8), &b12);
        assert_eq!(state.checkpoint_len(), cp, "stayed on the same checkpoint");
        let s11 = String::from_utf8(out11.clone()).unwrap();
        assert!(
            s11.contains("LOCAL_SUMMARY_NEEDLE"),
            "summary text must appear on the stable turn"
        );
        assert!(
            !s11.contains("\"u0\""),
            "a summarized tool id (u0) must be gone"
        );
        let p11: Value = serde_json::from_slice(&out11).unwrap();
        let p12: Value = serde_json::from_slice(&out12).unwrap();
        let m11 = p11["messages"].as_array().unwrap();
        let m12 = p12["messages"].as_array().unwrap();
        PairingIndex::build(m11).validate().expect("no orphans");
        assert_eq!(
            serde_json::to_vec(&m11[..6]).unwrap(),
            serde_json::to_vec(&m12[..6]).unwrap(),
            "summarized prefix must be byte-identical across stable turns (cache holds)"
        );
    }

    /// When Claude Code rewrites history (its own compaction → the prefix
    /// fingerprint changes), the cached summary's anchor is stale, so it must be
    /// DROPPED and the output must equal the plain stateless prune.
    #[test]
    fn summary_cleared_when_history_rewritten() {
        use crate::summarizer::slice::SummaryDecision;
        let cfg = cfg();
        let mut state = PruneState::default();
        let long = body_at(20);
        let _ = stable_apply_to_body(&long, &cfg, &mut state, 8);
        let msgs = messages_of(&long);
        state.set_summary(SummaryDecision::new(&msgs, 0, 8, "STALE_NEEDLE").unwrap());
        assert!(state.summary_slice_end().is_some(), "summary installed");

        // A shorter, rewritten history (fresh ids) — fails the prefix guard.
        let mut m = Vec::new();
        for i in 100..104 {
            let id = format!("c{i}");
            m.push(json!({"role":"assistant","content":[
                {"type":"tool_use","id":id,"name":"Bash","input":{"command":format!("post {i}")}}
            ]}));
            m.push(json!({"role":"user","content":[
                {"type":"tool_result","tool_use_id":id,"content":format!("fresh {i}")}
            ]}));
        }
        let compacted = serde_json::to_vec(
            &json!({"model":"claude","system":[{"type":"text","text":"sys"}],"messages":m}),
        )
        .unwrap();
        let out = bytes_of(
            &stable_apply_to_body(&compacted, &cfg, &mut state, 8),
            &compacted,
        );
        let stateless = bytes_of(&strategies::apply_to_body(&compacted, &cfg), &compacted);
        assert_eq!(
            out, stateless,
            "rewritten history → summary dropped, output equals the stateless prune"
        );
        assert!(
            state.summary_slice_end().is_none(),
            "stale summary must be cleared on a history rewrite"
        );
        assert!(!String::from_utf8(out).unwrap().contains("STALE_NEEDLE"));
    }

    /// A summary whose anchor no longer matches (content changed but length/shape
    /// preserved) is silently skipped — the model-free output is forwarded, never
    /// a mis-spliced body.
    #[test]
    fn summary_with_stale_anchor_is_skipped_not_misspliced() {
        use crate::summarizer::slice::SummaryDecision;
        let cfg = cfg();
        let mut state = PruneState::default();
        let b10 = body_at(10);
        let _ = stable_apply_to_body(&b10, &cfg, &mut state, 8);
        // Build the decision against a DIFFERENT slice content than what will be
        // replayed (simulate an anchor that won't match the real prefix).
        let mut other = messages_of(&b10);
        other[2]["content"][0]["content"] = json!("DIFFERENT");
        let d = SummaryDecision::new(&other, 0, 8, "SHOULD_NOT_APPEAR").unwrap();
        state.set_summary(d);
        let b11 = body_at(11);
        let out = bytes_of(&stable_apply_to_body(&b11, &cfg, &mut state, 8), &b11);
        let s = String::from_utf8(out).unwrap();
        assert!(
            !s.contains("SHOULD_NOT_APPEAR"),
            "stale-anchor summary must NOT be spliced in"
        );
        // u0 is still present because the summary did not apply (model-free only).
        assert!(
            s.contains("\"u0\""),
            "model-free output retains the un-summarized turns"
        );
    }

    // ---- F10: cache_control must not defeat append_only / summary replay ----

    #[test]
    fn eq_ignoring_cache_control_true_when_only_marker_moves() {
        // Same content; the cache_control breakpoint moved from block A to block B.
        let a = json!([
            {"role":"assistant","content":[{"type":"text","text":"hi","cache_control":{"type":"ephemeral"}}]},
            {"role":"user","content":[{"type":"text","text":"yo"}]},
        ]);
        let b = json!([
            {"role":"assistant","content":[{"type":"text","text":"hi"}]},
            {"role":"user","content":[{"type":"text","text":"yo","cache_control":{"type":"ephemeral"}}]},
        ]);
        assert!(messages_eq_ignoring_cache_control(
            a.as_array().unwrap(),
            b.as_array().unwrap()
        ));
    }

    #[test]
    fn eq_ignoring_cache_control_false_on_real_content_change() {
        let a = json!([{"role":"user","content":[{"type":"text","text":"hi"}]}]);
        let b = json!([{"role":"user","content":[{"type":"text","text":"HELLO"}]}]);
        assert!(!messages_eq_ignoring_cache_control(
            a.as_array().unwrap(),
            b.as_array().unwrap()
        ));
    }

    #[test]
    fn eq_ignoring_cache_control_handles_nested_marker_on_tool_result() {
        // cache_control deep inside a tool_result content block must be ignored.
        let a = json!([{"role":"user","content":[
            {"type":"tool_result","tool_use_id":"u0","content":"out","cache_control":{"type":"ephemeral"}}
        ]}]);
        let b = json!([{"role":"user","content":[
            {"type":"tool_result","tool_use_id":"u0","content":"out"}
        ]}]);
        assert!(messages_eq_ignoring_cache_control(
            a.as_array().unwrap(),
            b.as_array().unwrap()
        ));
        // ...but a different tool_use_id (real change) is still caught.
        let c = json!([{"role":"user","content":[
            {"type":"tool_result","tool_use_id":"DIFFERENT","content":"out"}
        ]}]);
        assert!(!messages_eq_ignoring_cache_control(
            a.as_array().unwrap(),
            c.as_array().unwrap()
        ));
    }

    #[test]
    fn eq_ignoring_cache_control_false_on_asymmetric_real_key() {
        // One side swaps cache_control for a DIFFERENT real key — count guard catches it.
        let a = json!([{"role":"user","content":[{"type":"text","text":"hi","cache_control":{"type":"ephemeral"}}]}]);
        let b = json!([{"role":"user","content":[{"type":"text","text":"hi","extra":"v"}]}]);
        assert!(!messages_eq_ignoring_cache_control(
            a.as_array().unwrap(),
            b.as_array().unwrap()
        ));
    }

    /// F10 positive: when the ONLY prefix delta is a moved/toggled `cache_control`
    /// marker, `append_only` must hold so the stable branch runs and the cached
    /// summary still splices onto the wire.
    #[test]
    fn moved_cache_control_in_prefix_keeps_summary_splicing() {
        use crate::summarizer::slice::SummaryDecision;
        let cfg = cfg();
        let mut st = PruneState::default();
        let body0 = body_at(3); // 6 messages
        let _ = stable_apply_to_body(&body0, &cfg, &mut st, 8); // checkpoint
        assert!(st.is_initialized());

        // Cache a valid summary over an OLD turn pair [0,2).
        let msgs0 = messages_of(&body0);
        st.set_summary(SummaryDecision::new(&msgs0, 0, 2, "SUMMARY_OF_OLD").unwrap());

        // Next turn: identical content, but Anthropic's cache breakpoint moved —
        // add a cache_control marker to the last prefix block. Metadata only.
        let mut root1: Value = serde_json::from_slice(&body0).unwrap();
        root1["messages"][5]["content"][0]["cache_control"] = json!({"type":"ephemeral"});
        let body1 = serde_json::to_vec(&root1).unwrap();

        let out = stable_apply_to_body(&body1, &cfg, &mut st, 8);
        let s = String::from_utf8(bytes_of(&out, &body1)).unwrap();
        assert!(
            s.contains("SUMMARY_OF_OLD"),
            "summary must splice despite a moved cache_control marker (F10)"
        );
        assert_eq!(
            st.summary_segment_count(),
            1,
            "a cache_control-only delta must NOT clear the cached summary"
        );
    }

    /// F10 negative: a REAL content change inside the prefix must still force a
    /// re-checkpoint (append_only=false) and drop the now-stale summary.
    #[test]
    fn real_content_change_in_prefix_forces_recheckpoint() {
        use crate::summarizer::slice::SummaryDecision;
        let cfg = cfg();
        let mut st = PruneState::default();
        let body0 = body_at(3);
        let _ = stable_apply_to_body(&body0, &cfg, &mut st, 8);

        let msgs0 = messages_of(&body0);
        st.set_summary(SummaryDecision::new(&msgs0, 0, 2, "SUMMARY").unwrap());
        assert_eq!(st.summary_segment_count(), 1);

        // Mutate real tool_result content in the prefix (not cache_control).
        let mut root1: Value = serde_json::from_slice(&body0).unwrap();
        root1["messages"][5]["content"][0]["content"] = json!("MUTATED_RESULT");
        let body1 = serde_json::to_vec(&root1).unwrap();
        let _ = stable_apply_to_body(&body1, &cfg, &mut st, 8);

        assert_eq!(
            st.summary_segment_count(),
            0,
            "a real content change must force re-checkpoint and drop the stale summary"
        );
    }

    /// Six plain-TEXT messages (no tool blocks) → the deterministic strategies prune
    /// nothing, so the warmup is an `Unchanged` checkpoint — the exact "no-deterministic-
    /// prune" geometry that exposed the splice bug.
    fn text_body() -> Vec<u8> {
        serde_json::to_vec(&json!({
            "model":"claude","system":[{"type":"text","text":"sys"}],
            "messages":[
                {"role":"assistant","content":[{"type":"text","text":"a0 ................"}]},
                {"role":"user","content":[{"type":"text","text":"u0 ................"}]},
                {"role":"assistant","content":[{"type":"text","text":"a1 ................"}]},
                {"role":"user","content":[{"type":"text","text":"u1 the boundary turn"}]},
                {"role":"assistant","content":[{"type":"text","text":"a2 ................"}]},
                {"role":"user","content":[{"type":"text","text":"u2 ................"}]},
            ]
        }))
        .unwrap()
    }

    /// REGRESSION (fix/summary-splice-no-op-checkpoint): an async summary installed after
    /// an `Unchanged` (no-deterministic-prune) warmup must still SPLICE on the next
    /// append-only turn when a prefix user turn is re-rendered from a single text block to
    /// a bare string (Claude Code's current→history normalization). Pre-fix this cosmetic
    /// flip made `append_only=false` → `prefix_changed` → the summary was CLEARED every turn
    /// and never reached the wire (`in == sent`).
    #[test]
    fn summary_splices_after_unchanged_warmup_despite_content_shape_flip() {
        use crate::summarizer::slice::SummaryDecision;
        let cfg = cfg();
        let mut st = PruneState::default();
        let body0 = text_body();
        let out0 = stable_apply_to_body(&body0, &cfg, &mut st, 8);
        assert!(
            matches!(out0, BodyOutcome::Unchanged),
            "pure-text warmup prunes nothing (Unchanged) — the bug geometry"
        );
        assert!(
            st.is_initialized(),
            "an Unchanged warmup must still record a checkpoint"
        );

        // Async install: summarize the OLD pair [0,2).
        let msgs0 = messages_of(&body0);
        st.set_summary(SummaryDecision::new(&msgs0, 0, 2, "SUMMARY_OF_OLD").unwrap());
        assert_eq!(st.summary_segment_count(), 1);

        // Next turn: identical history EXCEPT the boundary user turn (idx 3) is now a bare
        // string instead of a single text block. Cosmetic only (same text).
        let mut root1: Value = serde_json::from_slice(&body0).unwrap();
        let txt = root1["messages"][3]["content"][0]["text"]
            .as_str()
            .unwrap()
            .to_string();
        root1["messages"][3]["content"] = json!(txt);
        let body1 = serde_json::to_vec(&root1).unwrap();

        let out = stable_apply_to_body(&body1, &cfg, &mut st, 8);
        assert!(
            matches!(out, BodyOutcome::Mutated { .. }),
            "spliced body must be Mutated (sent < in), not Unchanged"
        );
        let s = String::from_utf8(bytes_of(&out, &body1)).unwrap();
        assert!(
            s.contains("SUMMARY_OF_OLD"),
            "summary must splice despite the content string/block shape flip"
        );
        assert_eq!(
            st.summary_segment_count(),
            1,
            "a cosmetic shape flip must NOT clear the cached summary"
        );
    }

    /// NEGATIVE: the shape-flip leniency must not mask a REAL text change — a different
    /// string in the prefix must still force a re-checkpoint and drop the stale summary.
    #[test]
    fn content_shape_flip_with_different_text_still_busts() {
        use crate::summarizer::slice::SummaryDecision;
        let cfg = cfg();
        let mut st = PruneState::default();
        let body0 = text_body();
        let _ = stable_apply_to_body(&body0, &cfg, &mut st, 8);
        let msgs0 = messages_of(&body0);
        st.set_summary(SummaryDecision::new(&msgs0, 0, 2, "SUMMARY").unwrap());
        assert_eq!(st.summary_segment_count(), 1);

        let mut root1: Value = serde_json::from_slice(&body0).unwrap();
        root1["messages"][3]["content"] = json!("DIFFERENT TEXT ENTIRELY");
        let body1 = serde_json::to_vec(&root1).unwrap();
        let _ = stable_apply_to_body(&body1, &cfg, &mut st, 8);
        assert_eq!(
            st.summary_segment_count(),
            0,
            "a real text change (not just a shape flip) must re-checkpoint and drop the summary"
        );
    }

    /// Direct unit test of the prefix-stability comparator's shape-insensitivity.
    #[test]
    fn prefix_compare_treats_string_and_single_text_block_as_equal() {
        let block = json!([{"role":"user","content":[{"type":"text","text":"hi"}]}]);
        let strg = json!([{"role":"user","content":"hi"}]);
        let block = block.as_array().unwrap();
        let strg = strg.as_array().unwrap();
        assert!(
            messages_eq_ignoring_cache_control(block, strg),
            "block-list and bare-string content with the same text are equal"
        );
        // block WITH cache_control vs bare string (same text) — still equal (cc ignored).
        let block_cc = json!([{"role":"user","content":[{"type":"text","text":"hi","cache_control":{"type":"ephemeral"}}]}]);
        assert!(
            messages_eq_ignoring_cache_control(block_cc.as_array().unwrap(), strg),
            "cache_control on the block must not defeat the shape-insensitive compare"
        );
        // different text still differs.
        let strg2 = json!([{"role":"user","content":"bye"}]);
        assert!(
            !messages_eq_ignoring_cache_control(block, strg2.as_array().unwrap()),
            "different text must still differ"
        );
        // a bare string vs a MULTI-block content is a genuine difference.
        let multi = json!([{"role":"user","content":[{"type":"text","text":"hi"},{"type":"text","text":"x"}]}]);
        assert!(
            !messages_eq_ignoring_cache_control(strg, multi.as_array().unwrap()),
            "string vs multi-block content must differ"
        );
        // SCOPE: the shape-insensitivity is for TOP-LEVEL message content only. A nested
        // object WITHOUT a `role` (e.g. a tool_result block) keeps structural comparison.
        let tr_str = json!([{"type":"tool_result","tool_use_id":"u0","content":"hi"}]);
        let tr_blk = json!([{"type":"tool_result","tool_use_id":"u0","content":[{"type":"text","text":"hi"}]}]);
        assert!(
            !messages_eq_ignoring_cache_control(
                tr_str.as_array().unwrap(),
                tr_blk.as_array().unwrap()
            ),
            "nested (non-message) content must NOT get the message-content shape leniency"
        );
    }

    /// Intersection of the F10 cache_control fix and this shape-flip fix: a checkpoint built
    /// with a cache_control-bearing single text block, then a next turn that presents the SAME
    /// text as a bare string with cache_control gone. Both are cosmetic — the summary must splice.
    #[test]
    fn summary_splices_when_cache_control_drops_and_content_flips_to_string() {
        use crate::summarizer::slice::SummaryDecision;
        let cfg = cfg();
        let mut st = PruneState::default();
        let mut root0: Value = serde_json::from_slice(&text_body()).unwrap();
        root0["messages"][3]["content"][0]["cache_control"] = json!({"type":"ephemeral"});
        let body0 = serde_json::to_vec(&root0).unwrap();
        let _ = stable_apply_to_body(&body0, &cfg, &mut st, 8);
        let msgs0 = messages_of(&body0);
        st.set_summary(SummaryDecision::new(&msgs0, 0, 2, "SUMMARY_CC").unwrap());
        assert_eq!(st.summary_segment_count(), 1);

        // Next turn: same text, but the block collapsed to a bare string AND cache_control gone.
        let mut root1: Value = serde_json::from_slice(&text_body()).unwrap();
        let txt = root1["messages"][3]["content"][0]["text"]
            .as_str()
            .unwrap()
            .to_string();
        root1["messages"][3]["content"] = json!(txt);
        let body1 = serde_json::to_vec(&root1).unwrap();

        let out = stable_apply_to_body(&body1, &cfg, &mut st, 8);
        let s = String::from_utf8(bytes_of(&out, &body1)).unwrap();
        assert!(
            s.contains("SUMMARY_CC"),
            "summary must splice when cache_control drops AND content flips to a bare string"
        );
        assert_eq!(
            st.summary_segment_count(),
            1,
            "the combined cosmetic change must not clear the cached summary"
        );
    }
}
