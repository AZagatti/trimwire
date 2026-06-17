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

/// Record what the checkpoint prune did, keyed by STABLE identifiers (not
/// position): tool-block overwrites by `tool_use_id`, and removed thinking blocks
/// by `signature`/`data`. Id/signature keys are position-independent, so this is
/// correct even though `thinking_strip` shortens a message's content array
/// (a positional zip would misalign after a removal — that's why this is id-keyed).
fn record_decisions(orig: &[Value], pruned: &[Value], state: &mut PruneState) {
    state.result_decisions.clear();
    state.input_decisions.clear();
    state.stripped_thinking.clear();

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
                            state.result_decisions.insert(id.to_owned(), c.clone());
                        }
                    }
                }
                Some("tool_use") => {
                    if let (Some(id), Some(inp)) =
                        (b.get("id").and_then(Value::as_str), b.get("input"))
                    {
                        if orig_inputs.get(id).copied() != Some(inp) {
                            state.input_decisions.insert(id.to_owned(), inp.clone());
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
            state.stripped_thinking.insert(k);
        }
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

/// Serialize `root` (with re-applied decisions) into a `Mutated` outcome. Tags a
/// synthetic `stable_reprune` "fired" entry so the ledger records it as a pruning
/// turn — not a no-op-that-changed-the-prefix (which is its cache-bust tripwire).
fn finish(root: Value, original_bytes: usize, final_bytes: usize) -> BodyOutcome {
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
        Err(_) => BodyOutcome::Unchanged,
    }
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

    tracing::debug!(
        initialized = state.initialized,
        len,
        checkpoint_len = state.checkpoint_len,
        grew,
        threshold,
        append_only,
        summary_segments = state.summary.len(),
        "trimwire: reprune branch decision"
    );

    if append_only && grew <= threshold {
        // STABLE: replay the checkpoint's decisions + thinking removals (+ the
        // cached local-model summary, when the feature is on); newer messages are
        // untouched (their ids/signatures aren't in the recorded sets). Paranoia:
        // the replay can't normally orphan, but `replay_decisions` re-validates
        // and yields `None` so we forward the original body if it ever did.
        let Some((mut out, changed)) = replay_decisions(messages, state) else {
            return BodyOutcome::Unchanged;
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
        finish(root, orig_len, out_len)
    } else {
        // CHECKPOINT: full prune (== stateless apply_to_body), then record.
        // Detect a history rewrite (CC's own compaction) up front: initialized
        // but the prefix no longer matches → any cached summary anchor is stale.
        let prefix_changed = state.initialized && !append_only;

        let mut pruned = messages.clone();
        let fired = match strategies::run(&mut pruned, cfg) {
            Ok(f) => f,
            Err(_) => return BodyOutcome::Unchanged, // orphan pre/post → forward original
        };
        // Telemetry for the (deferred) minimum-savings gate (IMPROVEMENTS P0 #2): the
        // decision metric is the MARGINAL savings of re-checkpointing vs just replaying
        // the OLD decisions on the longer array — NOT the total-vs-raw savings. Compute
        // it here, while the old decisions are still intact (before record_decisions),
        // and only on the append-only rebase case (the only one the gate would govern).
        // Guarded so it's free unless DEBUG tracing is on.
        if append_only && tracing::enabled!(tracing::Level::DEBUG) {
            if let Some((replay_out, _)) = replay_decisions(messages, state) {
                let replay_len = serde_json::to_vec(&replay_out)
                    .map(|v| v.len())
                    .unwrap_or(0);
                let pruned_len = serde_json::to_vec(&pruned).map(|v| v.len()).unwrap_or(0);
                tracing::debug!(
                    grew,
                    checkpoint_len = len,
                    marginal_saved = replay_len.saturating_sub(pruned_len),
                    "trimwire: re-checkpoint marginal-vs-replay"
                );
            }
        }
        record_decisions(messages, &pruned, state);
        state.checkpoint_len = len;
        state.checkpoint_prefix = messages.to_vec();
        state.initialized = true;

        // Apply the cached summary (if any) to the freshly-pruned checkpoint so
        // its prefix matches the stable turns'. Reverted if it would orphan;
        // cleared on a history rewrite. Never load-bearing.
        let (mut pruned, fired, summary_applied) =
            apply_checkpoint_summary(pruned, fired, messages, state, prefix_changed);

        // Always-on correctness sanitize (see strategies::apply_to_body): drop
        // empty-thinking blocks Anthropic rejects on resume. Run AFTER
        // record_decisions so it is not recorded as a replayable decision — the
        // stable branch re-applies the same deterministic pass, so the pruned
        // prefix stays byte-identical across turns (cache-safe).
        let empty_removed = strategies::thinking_strip::strip_empty(&mut pruned);

        if fired.iter().all(|(_, s)| s.stubbed == 0)
            && !summary_applied
            && empty_removed == 0
            && !normalized
        {
            return BodyOutcome::Unchanged; // exact-original bytes preserved
        }
        // Telemetry: a re-checkpoint MUTATES the prefix → busts the Anthropic prompt
        // cache. Logging `grew` + bytes saved reveals, on real sessions, how often
        // re-checkpoints fire for SMALL savings — the data that would justify (and set
        // the threshold for) a minimum-savings bust gate (IMPROVEMENTS-RESEARCH.md
        // P0 #2; design ready, deferred pending this telemetry). Guarded so the extra
        // serialization only runs when DEBUG tracing is enabled.
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
        root["messages"] = Value::Array(pruned);
        match serde_json::to_vec(&root) {
            Ok(bytes) => BodyOutcome::Mutated { bytes, fired },
            Err(_) => BodyOutcome::Unchanged,
        }
    }
}

/// Splice the cached summary into a freshly-pruned CHECKPOINT array (if a valid
/// summary is cached), so the checkpoint's prefix matches the stable turns'. The
/// summary is anchored to the ORIGINAL slice and spliced into `pruned` (same
/// indexing — no strategy adds or removes whole messages); reverted if it would
/// orphan a pair. Clears a stale summary first when CC rewrote history. Returns
/// the (possibly updated) `pruned` + `fired` and whether the summary applied.
/// When `[summarizer] engine = "model-free"` (the default), `summary` is always empty
/// and this is a cheap no-op.
fn apply_checkpoint_summary(
    mut pruned: Vec<Value>,
    mut fired: Vec<(&'static str, strategies::Stats)>,
    messages: &[Value],
    state: &mut PruneState,
    prefix_changed: bool,
) -> (Vec<Value>, Vec<(&'static str, strategies::Stats)>, bool) {
    if prefix_changed {
        state.summary.clear();
    }
    let applied = if state.summary.is_empty() {
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
            BodyOutcome::Unchanged => original.to_vec(),
            BodyOutcome::Mutated { bytes, .. } => bytes.clone(),
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
