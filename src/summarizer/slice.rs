//! The cached summary decision + its **synchronous replay** (range substitution).
//!
//! The local model is non-deterministic and slow, so it is called at most once
//! per batch from the async gateway. The resulting summary is cached here as a
//! [`SummaryDecision`] and replayed VERBATIM on every subsequent turn by reprune
//! (mirroring `reprune::apply_thinking_removals`): the OLD slice is removed from
//! the array and the cached summary message-pair is spliced in its place. The
//! replay is pure + synchronous + deterministic, so the pruned prefix stays
//! byte-identical turn-to-turn and Anthropic's prompt cache holds.
//!
//! Safety: the substitution is anchored to the ORIGINAL slice content by a
//! content hash. If Claude Code rewrites that history (its own compaction), the
//! hash no longer matches and the substitution is skipped — the model-free
//! pruning is forwarded instead. The replacement carries only text blocks (no
//! tool_use / tool_result), so it can never orphan a pair; the caller still
//! re-validates the whole array and drops the summary if it ever would.

use serde_json::{Value, json};
use sha2::{Digest, Sha256};

/// Raw SHA-256 (hex) of bytes — the slice anchor. NOTE: deliberately NOT
/// `ledger::prefix_hash`, which parses the body and strips `messages[]` (it
/// hashes the request *envelope*); we need a content hash of the message slice
/// itself, so any change inside it is detected.
fn content_hash(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

/// Acknowledgement turn inserted after the summary so the alternating
/// user/assistant structure is preserved (the slice always covers whole
/// `[assistant, user]` turn-pairs, so `[assistant(summary), user(ack)]` slots in
/// cleanly between the preceding user turn and the following assistant turn).
const ACK_TEXT: &str = "Understood — continuing from the summarized context above.";

/// A cached, replayable summary of the message range `[start, end)`.
///
/// `slice_hash` pins the ORIGINAL content of that range; replay verifies it
/// before substituting, so a rewritten history can never be mis-spliced.
#[derive(Debug, Clone)]
pub struct SummaryDecision {
    /// First message index of the summarized slice (inclusive).
    pub start: usize,
    /// One past the last summarized message (exclusive).
    pub end: usize,
    /// SHA-256 of the serialized ORIGINAL `messages[start..end]` — the anchor.
    pub slice_hash: String,
    /// The replacement messages: `[assistant(summary text), user(ack)]`.
    pub messages: Vec<Value>,
}

/// The content-free marker prepended to a summary. It is part of the shared
/// `[trimwire: …]` marker family (recognizable + idempotency-skippable) and is
/// model-facing: it tells the agent the turn range was locally compacted, that
/// exact detail is recoverable by re-reading, and — crucially — that the summary
/// may be lossy or wrong (the runtime accept-gate is size-only, no fidelity
/// check), so load-bearing "done"/result claims should be verified before they
/// are relied on. If the summary looks fabricated, the agent surfaces it and
/// offers `trimwire report`.
fn summary_marker(start: usize, end: usize) -> String {
    format!(
        "[trimwire: summarized turns {start}..{end} — a local model compacted these \
         older turns to save context; file/output detail is recoverable by re-reading \
         the files or re-running the tools. This summary may be lossy: treat any \
         \"done\"/result claims in it as unverified and double-check load-bearing ones \
         before relying on them. If it looks wrong or fabricated, tell the user (they \
         can run `trimwire report`).]"
    )
}

impl SummaryDecision {
    /// Build a decision summarizing `original[start..end]` with the given summary
    /// text. Returns `None` for an empty/out-of-range slice. The hash is computed
    /// from the ORIGINAL (un-pruned) slice content — the replay anchor.
    pub fn new(original: &[Value], start: usize, end: usize, summary: &str) -> Option<Self> {
        if start >= end || end > original.len() {
            return None;
        }
        let bytes = serde_json::to_vec(&original[start..end]).ok()?;
        let slice_hash = content_hash(&bytes);
        let text = format!("{}\n\n{}", summary_marker(start, end), summary.trim());
        let messages = vec![
            json!({"role": "assistant", "content": [{"type": "text", "text": text}]}),
            json!({"role": "user", "content": [{"type": "text", "text": ACK_TEXT}]}),
        ];
        Some(Self {
            start,
            end,
            slice_hash,
            messages,
        })
    }
}

/// Replay the summary: splice `out[start..end]` → the cached summary pair.
///
/// `original` is the inbound (un-pruned) messages array and is used ONLY to
/// re-verify the anchor hash; `out` is the array being built (a clone of
/// `original`, possibly with model-free content edits — same length/indexing,
/// since no strategy adds or removes whole messages). Returns `true` iff the
/// substitution was applied. On any anchor mismatch or bounds issue it returns
/// `false` and leaves `out` untouched (the caller then forwards the model-free
/// output — never load-bearing).
pub fn apply_summary(out: &mut Vec<Value>, original: &[Value], d: &SummaryDecision) -> bool {
    apply_summaries(out, original, std::slice::from_ref(d))
}

/// Replay a CHAIN of frozen summary segments (the opt-in accumulator): splice each
/// segment's `out[start..end]` → its summary pair, ALL-OR-NOTHING. The chain must be
/// ordered ascending by `start` and non-overlapping (`seg[i].end <= seg[i+1].start`).
/// Every anchor is verified against `original` BEFORE any mutation, and the splices
/// run back-to-front (highest `start` first) so earlier indices stay valid. Returns
/// `true` iff the WHOLE chain applied; on any anchor mismatch, overlap, ordering, or
/// bounds problem it returns `false` and leaves `out` UNTOUCHED (the caller forwards
/// the model-free output — never load-bearing). A single-segment chain is byte-for-byte
/// identical to the original single-summary splice.
///
/// Cache-stability: because every segment is frozen (fixed `start`/`end`/hash/messages)
/// and the splice is deterministic, the spliced bytes for the old region are identical
/// turn-to-turn while the anchors hold — so appending a new delta segment only changes
/// bytes from that segment onward, preserving the cached prefix up to it.
pub fn apply_summaries(
    out: &mut Vec<Value>,
    original: &[Value],
    chain: &[SummaryDecision],
) -> bool {
    if chain.is_empty() {
        return false;
    }
    // Validate ordering + non-overlap + bounds + EVERY anchor before touching `out`.
    let mut prev_end = 0usize;
    for d in chain {
        let order_ok = d.start >= prev_end;
        let bounds_ok = d.end <= out.len();
        let anchor_ok = anchor_matches(original, d);
        if !order_ok || !bounds_ok || !anchor_ok {
            tracing::debug!(
                start = d.start,
                end = d.end,
                out_len = out.len(),
                order_ok,
                bounds_ok,
                anchor_ok,
                "trimwire: apply_summaries rejected a segment"
            );
            return false;
        }
        prev_end = d.end;
    }
    // Splice back-to-front so each splice leaves lower indices (earlier segments) valid.
    for d in chain.iter().rev() {
        out.splice(d.start..d.end, d.messages.iter().cloned());
    }
    true
}

/// Does the cached summary still anchor to `original` — i.e. is
/// `original[start..end]` byte-identical to what was summarized? This is the
/// hash-check half of [`apply_summary`], exposed so callers (the gateway's
/// re-summarization gate) can tell a still-valid summary from a stale one (CC
/// rewrote history) without mutating anything.
pub fn anchor_matches(original: &[Value], d: &SummaryDecision) -> bool {
    if d.start >= d.end || d.end > original.len() {
        return false;
    }
    match serde_json::to_vec(&original[d.start..d.end]) {
        Ok(bytes) => content_hash(&bytes) == d.slice_hash,
        Err(_) => false,
    }
}

/// Choose the OLD prunable slice to summarize: whole `[assistant, user]` turn
/// pairs from the first assistant turn up to (but not including) the most-recent
/// `keep_recent_turns` assistant turns, and never past `max_end` (the gateway
/// passes the reprune `checkpoint_len`, so the summarized region stays inside the
/// cache-guarded, decision-stable prefix). Returns `(start, end)` message indices
/// or `None` when there isn't enough old history to be worth a model call.
///
/// The boundaries snap to assistant-turn starts so the slice contains whole tool
/// pairs; the caller still re-validates pairing after substitution, so an
/// imperfect snap is safe (it just falls back to model-free pruning).
pub fn select_slice(
    messages: &[Value],
    keep_recent_turns: usize,
    max_end: usize,
) -> Option<(usize, usize)> {
    let keep = keep_recent_turns.max(1);
    let a_idx: Vec<usize> = messages
        .iter()
        .enumerate()
        .filter(|(_, m)| m.get("role").and_then(Value::as_str) == Some("assistant"))
        .map(|(i, _)| i)
        .collect();
    // Need at least the protected recent turns plus a couple to summarize.
    if a_idx.len() < keep + 2 {
        return None;
    }
    let protect_from = a_idx[a_idx.len() - keep];
    let eligible_end = protect_from.min(max_end);
    let slice_start = a_idx[0];
    // The summary is an assistant turn; it must follow a user turn to preserve
    // role alternation (Anthropic rejects a leading-assistant `messages[0]`). So
    // the message before the slice must exist and be a user turn — true for any
    // real Claude Code transcript (it opens with a user message). If not (e.g. a
    // non-standard leading-assistant transcript), don't summarize.
    if slice_start == 0
        || messages
            .get(slice_start - 1)
            .and_then(|m| m.get("role").and_then(Value::as_str))
            != Some("user")
    {
        return None;
    }
    // Largest assistant-turn start strictly after slice_start and within bounds.
    let slice_end = *a_idx
        .iter()
        .rev()
        .find(|&&i| i > slice_start && i <= eligible_end)?;
    // Require a few messages (≈2+ pairs) to be worth a model call.
    if slice_end < slice_start + 4 {
        return None;
    }
    Some((slice_start, slice_end))
}

/// High-precision supersession/correction markers (matched case-insensitively). A slice turn carrying
/// any of these is an authoritative correction the summarizer must NOT compress away — O12 showed both
/// the local (qwen3.5:4b) and provider (GLM-5.2) summaries dropped a one-time override in favor of
/// repeated stale facts. [`cap_slice_at_override`] keeps such turns VERBATIM on the wire.
///
/// Deliberately PHRASE-level coined phrases, not broad single words: bare `obsolete` false-positived on
/// benign prose ("obsolete columns" in a migrations explainer) and over-protected ~28 KB; bare
/// `supersedes` is similarly common ("the new schema supersedes the old"); `correction:` collides with
/// git commit messages ("correction: fix typo"), shell/log `tool_result` lines, and code-review prose
/// ("correction: the index is 0-based"). All three are dropped. Re-add only with a scoped, high-confidence
/// phrase + a benign-usage no-cap test, never a bare common word.
pub const OVERRIDE_MARKERS: &[&str] = &[
    "authoritative override",
    "current ground truth",
    "do not resurrect",
];

/// True if any text or tool_result content of `m` contains an [`OVERRIDE_MARKERS`] phrase
/// (case-insensitive; `to_ascii_lowercase` leaves multibyte UTF-8 untouched, so no false match/panic).
/// `tool_use` INPUT blocks are intentionally NOT scanned: a correction/override is a human/assistant
/// *statement* (user text, or a tool_result the agent read), not a tool-call argument — scanning
/// command/edit bodies would protect turns on incidental matches inside code or commands.
pub fn message_has_override_marker(m: &Value) -> bool {
    fn has(s: &str) -> bool {
        let l = s.to_ascii_lowercase();
        OVERRIDE_MARKERS.iter().any(|mk| l.contains(mk))
    }
    match m.get("content") {
        Some(Value::String(s)) => has(s),
        Some(Value::Array(blocks)) => blocks.iter().any(|b| {
            match b.get("type").and_then(Value::as_str) {
                Some("text") => b.get("text").and_then(Value::as_str).is_some_and(has),
                Some("tool_result") => match b.get("content") {
                    Some(Value::String(s)) => has(s),
                    Some(v) => has(&v.to_string()),
                    None => false,
                },
                // tool_use input (commands / edit bodies) intentionally NOT scanned — see doc above.
                _ => false,
            }
        }),
        _ => false,
    }
}

/// B1 OVERRIDE PROTECTION (O12 safety mitigation): given a chosen slice `[start, end)`, if any
/// message carries a supersession marker, cap `end` to the largest assistant-turn start `i` with
/// `start < i <= j` (j = first marker message), so the override turn AND everything after it stay
/// VERBATIM on the wire — the protected correction then supersedes whatever the summary records.
/// Returns the original `end` when there is no marker; otherwise the capped end.
///
/// CONTRACT: the return is the index to summarize UP TO, but it may be `< start + 4` — including the
/// `start` fallback (an empty-slice sentinel) when no whole `[assistant,user]` pair precedes the marker.
/// Callers MUST treat any value `< start + 4` as "skip summarization → leave the region verbatim" and
/// must NEVER summarize `[start, return)` without that guard (this is why both call sites re-check
/// `end < start + 4`). Snapping to an assistant index keeps whole pairs, matching [`select_slice`].
/// Deterministic and model-independent: it does NOT rely on the summarizer recognizing the override
/// (which O12 proved it does not).
pub fn cap_slice_at_override(messages: &[Value], start: usize, end: usize) -> usize {
    let Some(j) = (start..end).find(|&i| message_has_override_marker(&messages[i])) else {
        return end;
    };
    (start + 1..=j)
        .rev()
        .find(|&i| {
            messages
                .get(i)
                .and_then(|m| m.get("role"))
                .and_then(Value::as_str)
                == Some("assistant")
        })
        .unwrap_or(start)
}

/// Count the eligible assistant-turn START positions a *density-aware* slice
/// selector could choose among. `select_slice` always starts at the first
/// assistant turn (the widest slice); a density-aware variant would instead pick
/// the start that yields the densest (lowest `tool_result_fraction`) sub-slice to
/// the same end. When this count is ≤ 1 the prunable region admits only one viable
/// slice, so density-aware selection is a **no-op** — this is the empirical signal
/// (run over real sessions) for whether the density-aware `select_slice` idea is
/// worth building. Same eligibility rules as `select_slice`
/// (keep / max_end / ≥4-message / role-alternation). Returns 0 when
/// `select_slice` would return `None`.
pub fn eligible_window_count(
    messages: &[Value],
    keep_recent_turns: usize,
    max_end: usize,
) -> usize {
    let keep = keep_recent_turns.max(1);
    let a_idx: Vec<usize> = messages
        .iter()
        .enumerate()
        .filter(|(_, m)| m.get("role").and_then(Value::as_str) == Some("assistant"))
        .map(|(i, _)| i)
        .collect();
    if a_idx.len() < keep + 2 {
        return 0;
    }
    let protect_from = a_idx[a_idx.len() - keep];
    let eligible_end = protect_from.min(max_end);
    // The end every candidate shares (same as select_slice): the largest
    // assistant-turn start within the eligible region, after the earliest start.
    let Some(&slice_end) = a_idx
        .iter()
        .rev()
        .find(|&&i| i > a_idx[0] && i <= eligible_end)
    else {
        return 0;
    };
    eligible_starts_to(messages, slice_end).len()
}

/// The eligible assistant-turn START positions (ascending, message index) for a
/// slice ending at `end`: each yields a ≥4-message slice and is preceded by a
/// user turn (role-alternation) — exactly the rule `select_slice` applies to its
/// single widest start. Shared by [`eligible_window_count`] and the density-aware
/// fallback ([`densify_start`]).
pub(crate) fn eligible_starts_to(messages: &[Value], end: usize) -> Vec<usize> {
    messages
        .iter()
        .enumerate()
        .filter(|(_, m)| m.get("role").and_then(Value::as_str) == Some("assistant"))
        .map(|(i, _)| i)
        .filter(|&s| {
            s < end
                && end - s >= 4
                && s > 0
                && messages
                    .get(s - 1)
                    .and_then(|m| m.get("role").and_then(Value::as_str))
                    == Some("user")
        })
        .collect()
}

/// Density-aware fallback START for the slice `[widest_start, end)` when the
/// widest slice is too tool-result-dominated to summarize. Returns the EARLIEST
/// eligible start `s > widest_start` whose sub-slice `[s, end)` scores
/// `density(&messages[s..end]) <= max_fraction` — i.e. the WIDEST sub-window that
/// is dense enough (lowest tool fraction first becomes acceptable as `start`
/// advances and drops old tool-heavy turns), maximizing recovered reduction while
/// staying inside the safety bound. `None` if no eligible sub-window qualifies
/// (then the caller skips, exactly as before — this is purely additive: it only
/// ever turns a SKIP into a summary, never changes an already-summarizable slice).
///
/// `density` is injected (the gateway passes `tool_result_fraction`) so this stays
/// in the slice module without depending on the compactor's scoring.
pub fn densify_start(
    messages: &[Value],
    widest_start: usize,
    end: usize,
    max_fraction: f64,
    density: impl Fn(&[Value]) -> f64,
) -> Option<usize> {
    eligible_starts_to(messages, end)
        .into_iter()
        .filter(|&s| s > widest_start)
        .find(|&s| density(&messages[s..end]) <= max_fraction)
}

/// Phase 2.5a slice-size CAP: narrow the slice START forward (toward `end`) to a
/// whole assistant-turn boundary so the SERIALIZED slice fits `char_budget` chars —
/// bounding the model prompt to the num_ctx budget. ollama truncates the prompt HEAD
/// once it exceeds num_ctx, silently dropping the OLDEST content of an over-long
/// slice (a summary that then misrepresents the range yet still passes the size
/// gate). Returns a start `>=` the input `start`: it summarizes the most-recent old
/// turns that fit and leaves the very oldest to model-free pruning. Falls back to
/// `start` if even the smallest viable (≥2-pair) slice exceeds the budget (the
/// num_ctx clamp then applies and the debug log fires).
pub fn cap_slice_start(
    messages: &[Value],
    start: usize,
    end: usize,
    reasoning_cap: usize,
    tool_result_cap: usize,
    char_budget: usize,
) -> usize {
    for i in start..end {
        if messages[i].get("role").and_then(Value::as_str) != Some("assistant") {
            continue;
        }
        // Keep at least ~2 [assistant,user] pairs; don't narrow below that.
        if i + 4 > end {
            break;
        }
        if serialize_slice(&messages[i..end], reasoning_cap, tool_result_cap).len() <= char_budget {
            return i;
        }
    }
    start
}

/// Phase 2.5b ACCUMULATOR slice-size cap: with the slice START fixed at the chain tail,
/// narrow the END backward to a whole assistant-turn boundary so the serialized delta
/// `messages[start..end]` fits `char_budget`. This lets the accumulator APPEND a
/// budget-sized frozen segment (marching forward in contiguous chunks) instead of
/// falling back to a full REPLACE — so older segments stay frozen (fidelity preserved)
/// and the cache busts only on the bounded delta. Returns the LARGEST assistant-aligned
/// end in `[start+4, max_end]` whose serialized slice fits (binary search — the
/// serialized length is monotonic in `end`); if even the smallest viable (≥2-pair)
/// slice exceeds the budget it returns that smallest end (the num_ctx clamp then
/// applies, same tension `cap_slice_start` documents). Returns `start` if no ≥2-pair
/// end exists at all (caller then skips — too little to summarize).
pub fn cap_slice_end(
    messages: &[Value],
    start: usize,
    max_end: usize,
    reasoning_cap: usize,
    tool_result_cap: usize,
    char_budget: usize,
) -> usize {
    // Whole-pair candidate ends (each an assistant-turn start so `[start..end]` ends on
    // a complete [assistant,user] pair), ascending.
    let candidates: Vec<usize> = (start + 4..=max_end)
        .filter(|&e| {
            messages
                .get(e)
                .and_then(|m| m.get("role"))
                .and_then(Value::as_str)
                == Some("assistant")
        })
        .collect();
    let Some(&smallest) = candidates.first() else {
        return start;
    };
    // Rightmost candidate whose serialized slice fits the budget (monotonic → bisect).
    let (mut lo, mut hi, mut best) = (0usize, candidates.len(), None);
    while lo < hi {
        let mid = (lo + hi) / 2;
        if serialize_slice(
            &messages[start..candidates[mid]],
            reasoning_cap,
            tool_result_cap,
        )
        .len()
            <= char_budget
        {
            best = Some(candidates[mid]);
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    best.unwrap_or(smallest)
}

/// Per-block-cap ASYMMETRY (Phase 1b): the small model's budget is best spent on
/// hard-to-compress REASONING (assistant text + the model's own tool_use inputs),
/// not on bulky tool_result OUTPUT — which is exactly what the deterministic
/// model-free strategies already prune. So tool_result content is capped hard while
/// reasoning/text is given a large budget. (The verbatim facts in a tool_result
/// sit at its START, so a head+tail trim keeps them.)
pub const REASONING_BLOCK_CAP: usize = 8_000;
pub const TOOL_RESULT_BLOCK_CAP: usize = 400;

/// Render a message slice as readable text for the summarization prompt (the local
/// model handles labelled prose better than raw JSON). Strings longer than the
/// block's cap (CHARACTERS) are head+tail trimmed with an elision note: tool_result
/// content uses `tool_result_cap` (small — bulk is model-free's job), everything
/// else (assistant/user text, tool_use input) uses `reasoning_cap` (large). thinking
/// blocks are skipped (already handled by thinking_strip).
pub fn serialize_slice(slice: &[Value], reasoning_cap: usize, tool_result_cap: usize) -> String {
    let mut out = String::new();
    for m in slice {
        let role = m.get("role").and_then(Value::as_str).unwrap_or("?");
        out.push_str("\n### ");
        out.push_str(role);
        out.push('\n');
        match m.get("content") {
            Some(Value::String(s)) => push_capped(&mut out, s, reasoning_cap),
            Some(Value::Array(blocks)) => {
                for b in blocks {
                    match b.get("type").and_then(Value::as_str) {
                        Some("text") => {
                            if let Some(t) = b.get("text").and_then(Value::as_str) {
                                push_capped(&mut out, t, reasoning_cap);
                            }
                        }
                        Some("tool_use") => {
                            let name = b.get("name").and_then(Value::as_str).unwrap_or("?");
                            out.push_str(&format!("[tool_use {name}] "));
                            if let Some(input) = b.get("input") {
                                push_capped(&mut out, &input.to_string(), reasoning_cap);
                            }
                        }
                        Some("tool_result") => {
                            out.push_str("[tool_result] ");
                            let s = match b.get("content") {
                                Some(Value::String(s)) => s.clone(),
                                Some(v) => v.to_string(),
                                None => String::new(),
                            };
                            push_capped(&mut out, &s, tool_result_cap);
                        }
                        // thinking / redacted_thinking: skip (already stripped).
                        _ => {}
                    }
                    out.push('\n');
                }
            }
            _ => {}
        }
    }
    out
}

/// Append `s` to `out`, head+tail trimming (by char count) if it exceeds `cap`.
fn push_capped(out: &mut String, s: &str, cap: usize) {
    // cap == 0 means UNCAPPED (pass-through), not zero-length.
    if cap == 0 {
        out.push_str(s);
        return;
    }
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= cap {
        out.push_str(s);
        return;
    }
    let head = cap / 2;
    let tail = cap - head;
    out.extend(&chars[..head]);
    out.push_str(&format!("\n…[{} chars elided]…\n", chars.len() - cap));
    out.extend(&chars[chars.len() - tail..]);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pairing::PairingIndex;

    /// `turns` whole [assistant(tool_use), user(tool_result)] pairs, preceded by
    /// an initial user turn (the real-conversation shape).
    fn convo(turns: usize) -> Vec<Value> {
        let mut m = vec![json!({"role":"user","content":[{"type":"text","text":"start"}]})];
        for i in 0..turns {
            let id = format!("t{i}");
            m.push(json!({"role":"assistant","content":[
                {"type":"tool_use","id":id,"name":"Bash","input":{"command":format!("run {i}")}}
            ]}));
            m.push(json!({"role":"user","content":[
                {"type":"tool_result","tool_use_id":id,"content":format!("out {i}")}
            ]}));
        }
        m
    }

    // --- B1 override-protection (O12 safety mitigation) ---
    // convo(10): user@0, assistant@1,3,..,19, user@2,4,..,20. select_slice(_,2,len) = (1, 17).
    #[test]
    fn cap_slice_at_override_excludes_and_protects_override_turn() {
        // repeated stale (benign) + a later authoritative override (a USER turn @14): cap to the last
        // assistant-turn START before it (13) → the override turn is EXCLUDED, stays verbatim.
        let mut m = convo(10);
        m[14] = json!({"role":"user","content":[{"type":"text",
            "text":"AUTHORITATIVE OVERRIDE: store is SQLite, port 9090; do not resurrect 8080."}]});
        assert!(message_has_override_marker(&m[14]));
        let (start, end) = select_slice(&m, 2, m.len()).unwrap();
        assert!(
            start < 14 && 14 < end,
            "override must be inside the chosen slice for the test"
        );
        assert_eq!(
            cap_slice_at_override(&m, start, end),
            13,
            "cap = last assistant turn before override"
        );
    }

    #[test]
    fn cap_slice_at_override_marker_on_assistant_turn_returns_j() {
        // Marker carried by an ASSISTANT turn @13 (agent restates the override). The inclusive upper
        // bound `..=j` must return j itself (an assistant index) so the marker turn is excluded.
        let mut m = convo(10);
        m[13] = json!({"role":"assistant","content":[{"type":"text",
            "text":"Understood — do not resurrect the in-memory store; current backend is SQLite."}]});
        assert!(message_has_override_marker(&m[13]));
        let (start, end) = select_slice(&m, 2, m.len()).unwrap();
        assert_eq!(
            cap_slice_at_override(&m, start, end),
            13,
            "assistant-turn marker → cap == j (..=j)"
        );
    }

    #[test]
    fn cap_slice_at_override_first_marker_wins() {
        // Two markers; the cap is computed from the FIRST (proves .find, not .rfind).
        let mut m = convo(10);
        m[8] = json!({"role":"user","content":[{"type":"text","text":"current ground truth: port is 9090"}]});
        m[16] = json!({"role":"user","content":[{"type":"text","text":"authoritative override: store is SQLite"}]});
        let (start, end) = select_slice(&m, 2, m.len()).unwrap();
        assert!(start < 8 && 16 < end);
        assert_eq!(
            cap_slice_at_override(&m, start, end),
            7,
            "cap from the FIRST marker (7), not the second"
        );
    }

    #[test]
    fn cap_slice_at_override_noop_without_marker() {
        let m = convo(10);
        let (start, end) = select_slice(&m, 2, m.len()).unwrap();
        assert_eq!(
            cap_slice_at_override(&m, start, end),
            end,
            "no marker → slice unchanged"
        );
    }

    #[test]
    fn cap_slice_at_override_no_false_positive_on_dropped_words() {
        // Precision: bare 'obsolete' / 'supersedes' / 'correction:' are NOT markers — they collide
        // with benign prose, git commit messages, and tool/log output. None may cap the slice.
        for benign in [
            "note: the migration deprecates obsolete columns in the legacy table",
            "the new event-sourced schema supersedes the old CRUD design",
            "correction: fix typo in the readme",
            "correction: the index is 0-based, not 1-based",
            "git log: a1b2c3 correction: align table headers (tool output line)",
        ] {
            let mut m = convo(10);
            m[10] = json!({"role":"user","content":[{"type":"text","text":benign}]});
            let (start, end) = select_slice(&m, 2, m.len()).unwrap();
            assert_eq!(
                cap_slice_at_override(&m, start, end),
                end,
                "benign text must NOT cap: {benign}"
            );
        }
    }

    #[test]
    fn cap_slice_at_override_returns_start_sentinel_when_override_is_earliest() {
        // override at the slice head (no whole pair before it) → returns the `start` sentinel → the
        // caller's `< start + 4` guard skips summarization, leaving the region verbatim.
        let mut m = convo(10);
        m[2] = json!({"role":"user","content":[{"type":"text","text":"do not resurrect the old config"}]});
        let (start, end) = select_slice(&m, 2, m.len()).unwrap();
        let capped = cap_slice_at_override(&m, start, end);
        assert_eq!(
            capped, start,
            "no whole pair before head override → start sentinel"
        );
        assert!(
            capped < start + 4,
            "sentinel triggers the caller's skip guard"
        );
    }

    #[test]
    fn splice_replaces_range_and_preserves_pairing() {
        let original = convo(6); // [user, (a,u)x6] = 13 messages
        // Summarize turns 1..=3 → message indices [1, 7).
        let d = SummaryDecision::new(&original, 1, 7, "## Goal\nDo work").unwrap();
        let mut out = original.clone();
        assert!(apply_summary(&mut out, &original, &d));
        // 6 messages removed, 2 inserted → 13 - 6 + 2 = 9.
        assert_eq!(out.len(), 9);
        // Summary is an assistant text turn carrying the marker + text.
        let txt = out[1]["content"][0]["text"].as_str().unwrap();
        assert!(txt.contains("[trimwire: summarized turns"));
        assert!(txt.contains("## Goal"));
        assert_eq!(out[2]["role"], "user", "ack turn follows the summary");
        // Surviving tool pairs are intact; no orphans introduced.
        PairingIndex::build(&out).validate().expect("no orphans");
        // The summarized tool_use ids are gone; the kept ones remain.
        let s = serde_json::to_string(&out).unwrap();
        assert!(!s.contains("\"t1\""), "summarized id t1 must be gone");
        assert!(s.contains("\"t5\""), "kept id t5 must remain");
    }

    #[test]
    fn role_alternation_holds_after_splice() {
        let original = convo(5);
        let d = SummaryDecision::new(&original, 1, 5, "summary").unwrap();
        let mut out = original.clone();
        assert!(apply_summary(&mut out, &original, &d));
        // Walk: roles must strictly alternate user/assistant from the top.
        let roles: Vec<&str> = out.iter().map(|m| m["role"].as_str().unwrap()).collect();
        for w in roles.windows(2) {
            assert_ne!(w[0], w[1], "adjacent roles must differ: {roles:?}");
        }
    }

    #[test]
    fn anchor_mismatch_skips_substitution() {
        let original = convo(6);
        let d = SummaryDecision::new(&original, 1, 7, "summary").unwrap();
        // Simulate a rewritten history: same indices, different content.
        let mut rewritten = original.clone();
        rewritten[3]["content"][0]["content"] = json!("REWRITTEN");
        let mut out = rewritten.clone();
        assert!(
            !apply_summary(&mut out, &rewritten, &d),
            "anchor hash must not match rewritten history"
        );
        assert_eq!(out, rewritten, "out untouched on mismatch");
    }

    #[test]
    fn out_of_range_or_empty_is_a_noop() {
        let original = convo(3);
        assert!(
            SummaryDecision::new(&original, 2, 2, "x").is_none(),
            "empty"
        );
        assert!(
            SummaryDecision::new(&original, 5, 3, "x").is_none(),
            "inverted"
        );
        assert!(
            SummaryDecision::new(&original, 0, 999, "x").is_none(),
            "end past array"
        );
        // A decision whose end exceeds a (later, shorter) out is skipped safely.
        let d = SummaryDecision::new(&original, 1, 7, "x").unwrap();
        let mut shorter = convo(1); // only 3 messages
        let snapshot = shorter.clone();
        assert!(!apply_summary(&mut shorter, &snapshot, &d));
        assert_eq!(shorter, snapshot);
    }

    #[test]
    fn apply_summaries_chains_two_frozen_segments() {
        let original = convo(8); // [user, (a,u)x8] = 17 messages
        // Two contiguous frozen segments: [1..5) then [5..9).
        let seg0 = SummaryDecision::new(&original, 1, 5, "## Seg0").unwrap();
        let seg1 = SummaryDecision::new(&original, 5, 9, "## Seg1").unwrap();
        let mut out = original.clone();
        assert!(apply_summaries(&mut out, &original, &[seg0, seg1]));
        // 17 - (5-1) - (9-5) + 2 + 2 = 13.
        assert_eq!(out.len(), 13);
        // Both summaries present, in order, each followed by its ack.
        assert!(
            out[1]["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("## Seg0")
        );
        assert_eq!(out[2]["role"], "user");
        assert!(
            out[3]["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("## Seg1")
        );
        assert_eq!(out[4]["role"], "user");
        // Strict role alternation across the whole array (no Anthropic 400).
        let roles: Vec<&str> = out.iter().map(|m| m["role"].as_str().unwrap()).collect();
        for w in roles.windows(2) {
            assert_ne!(w[0], w[1], "adjacent roles must differ: {roles:?}");
        }
        // No orphaned tool pairs; summarized ids gone, later ids kept.
        PairingIndex::build(&out).validate().expect("no orphans");
        let s = serde_json::to_string(&out).unwrap();
        for gone in ["\"t0\"", "\"t1\"", "\"t2\"", "\"t3\""] {
            assert!(!s.contains(gone), "summarized id {gone} must be gone");
        }
        assert!(s.contains("\"t7\""), "kept id t7 must remain");
    }

    #[test]
    fn apply_summaries_is_all_or_nothing_on_a_stale_segment() {
        let original = convo(8);
        let seg0 = SummaryDecision::new(&original, 1, 5, "## Seg0").unwrap();
        let seg1 = SummaryDecision::new(&original, 5, 9, "## Seg1").unwrap();
        // Rewrite history inside seg1's range only → seg1 anchor goes stale.
        let mut rewritten = original.clone();
        rewritten[6]["content"][0]["content"] = json!("REWRITTEN");
        let mut out = rewritten.clone();
        assert!(
            !apply_summaries(&mut out, &rewritten, &[seg0, seg1]),
            "one stale segment invalidates the whole chain"
        );
        assert_eq!(out, rewritten, "out left UNTOUCHED — no partial splice");
    }

    #[test]
    fn apply_summaries_rejects_overlapping_or_misordered_segments() {
        let original = convo(8);
        let a = SummaryDecision::new(&original, 1, 7, "A").unwrap();
        let b = SummaryDecision::new(&original, 5, 9, "B").unwrap(); // overlaps a
        let mut out = original.clone();
        assert!(
            !apply_summaries(&mut out, &original, &[a, b]),
            "overlap rejected"
        );
        assert_eq!(out, original, "untouched on overlap");
        // Misordered (descending start) is also rejected by the prev_end guard.
        let s0 = SummaryDecision::new(&original, 5, 9, "S0").unwrap();
        let s1 = SummaryDecision::new(&original, 1, 5, "S1").unwrap();
        let mut out2 = original.clone();
        assert!(
            !apply_summaries(&mut out2, &original, &[s0, s1]),
            "misorder rejected"
        );
        assert_eq!(out2, original, "untouched on misorder");
    }

    #[test]
    fn appending_a_segment_keeps_the_earlier_segments_prefix_byte_stable() {
        // The cache-stability invariant: growing the chain from [seg0] to
        // [seg0, seg1] must not disturb seg0's spliced region — the leading run is
        // byte-identical, so the prompt cache survives up to the new segment.
        let original = convo(8);
        let seg0 = SummaryDecision::new(&original, 1, 5, "## Seg0").unwrap();
        let seg1 = SummaryDecision::new(&original, 5, 9, "## Seg1").unwrap();

        let mut one = original.clone();
        assert!(apply_summaries(
            &mut one,
            &original,
            std::slice::from_ref(&seg0)
        ));
        let mut two = original.clone();
        assert!(apply_summaries(&mut two, &original, &[seg0, seg1]));

        // The frozen seg0 pair (indices 0..=2: user, summary, ack) is identical in
        // both; divergence begins only at seg1.
        assert_eq!(
            one[..3],
            two[..3],
            "seg0 region must be byte-stable across growth"
        );
        assert_ne!(one[3], two[3], "divergence starts at the appended segment");
    }

    #[test]
    fn select_slice_snaps_to_whole_pairs_and_protects_recent() {
        let messages = convo(10); // [user, (a,u)x10] = 21 messages
        let (start, end) = select_slice(&messages, 2, usize::MAX).unwrap();
        // Starts at the first assistant (index 1, after the leading user turn).
        assert_eq!(start, 1);
        assert_eq!(messages[start]["role"], "assistant");
        // Ends on an assistant-turn start → whole pairs; the preceding msg is a
        // user (tool_result), so no pair is split.
        assert_eq!(messages[end]["role"], "assistant");
        assert_eq!(messages[end - 1]["role"], "user");
        // The last 2 assistant turns are protected: end is at/below their start.
        // 11 assistant indices are 1,3,5,...; keep=2 protects the last two
        // (indices 17, 19) → end must be ≤ 17.
        assert!(end <= 17, "recent turns must be protected, got end={end}");
        // The chosen slice substitutes cleanly (whole pairs, no orphans).
        let d = SummaryDecision::new(&messages, start, end, "s").unwrap();
        let mut out = messages.clone();
        assert!(apply_summary(&mut out, &messages, &d));
        PairingIndex::build(&out).validate().expect("no orphans");
    }

    #[test]
    fn select_slice_caps_at_max_end() {
        let messages = convo(10);
        let (start, end) = select_slice(&messages, 2, 8).unwrap();
        assert_eq!(start, 1);
        assert!(
            end <= 8,
            "must not summarize past max_end (checkpoint), got {end}"
        );
        assert_eq!(messages[end]["role"], "assistant");
    }

    #[test]
    fn select_slice_refuses_leading_assistant_transcript() {
        // A non-standard transcript whose messages[0] is an assistant turn: a
        // summary at index 0 would be a leading-assistant message (Anthropic 400).
        // select_slice must decline rather than produce it.
        let mut m = Vec::new();
        for i in 0..8 {
            let id = format!("t{i}");
            m.push(json!({"role":"assistant","content":[
                {"type":"tool_use","id":id,"name":"Bash","input":{"command":"x"}}
            ]}));
            m.push(json!({"role":"user","content":[
                {"type":"tool_result","tool_use_id":id,"content":"ok"}
            ]}));
        }
        assert!(
            select_slice(&m, 2, usize::MAX).is_none(),
            "must not summarize from index 0 of a leading-assistant transcript"
        );
    }

    #[test]
    fn anchor_matches_detects_rewrite() {
        let original = convo(6);
        let d = SummaryDecision::new(&original, 1, 7, "s").unwrap();
        assert!(anchor_matches(&original, &d), "unchanged history anchors");
        let mut rewritten = original.clone();
        rewritten[3]["content"][0]["content"] = json!("CHANGED");
        assert!(
            !anchor_matches(&rewritten, &d),
            "rewritten history must not anchor"
        );
    }

    #[test]
    fn select_slice_none_when_too_short() {
        // Only 2 turns, keep_recent 2 → nothing old enough to summarize.
        assert!(select_slice(&convo(2), 2, usize::MAX).is_none());
        assert!(select_slice(&convo(3), 2, usize::MAX).is_none());
    }

    #[test]
    fn eligible_window_count_signals_density_aware_choice() {
        // A long session has many viable start positions → density-aware selection
        // has a real choice (NOT a no-op).
        assert!(
            eligible_window_count(&convo(10), 2, usize::MAX) >= 2,
            "a long session should offer multiple eligible slice windows"
        );
    }

    #[test]
    fn eligible_window_count_is_one_when_only_one_viable_slice() {
        // Just enough old history for a single ≥4-message slice: select_slice returns
        // Some, but there's only ONE viable start → density-aware would be a no-op.
        assert!(select_slice(&convo(4), 2, usize::MAX).is_some());
        assert_eq!(
            eligible_window_count(&convo(4), 2, usize::MAX),
            1,
            "a minimal prunable region admits exactly one slice (density-aware = no-op)"
        );
    }

    #[test]
    fn eligible_window_count_is_zero_when_nothing_to_summarize() {
        // Mirrors select_slice returning None.
        assert_eq!(eligible_window_count(&convo(3), 2, usize::MAX), 0);
        assert!(select_slice(&convo(3), 2, usize::MAX).is_none());
    }

    // --- density-aware fallback (densify_start) ---

    /// Test-local density: fraction of slice bytes that live in `tool_result`
    /// blocks (mirrors the gateway's `tool_result_fraction`).
    fn tool_frac(slice: &[Value]) -> f64 {
        let (mut total, mut tool) = (0usize, 0usize);
        for m in slice {
            if let Some(blocks) = m.get("content").and_then(Value::as_array) {
                for b in blocks {
                    let len = b
                        .get("text")
                        .and_then(Value::as_str)
                        .map(str::len)
                        .or_else(|| b.get("content").and_then(Value::as_str).map(str::len))
                        .or_else(|| b.get("input").map(|v| v.to_string().len()))
                        .unwrap_or(0);
                    total += len;
                    if b.get("type").and_then(Value::as_str) == Some("tool_result") {
                        tool += len;
                    }
                }
            }
        }
        if total == 0 {
            0.0
        } else {
            tool as f64 / total as f64
        }
    }

    /// 8 turns: turns 0-3 are tool-heavy (huge tool_result), turns 4-7 are
    /// reasoning-dense (big assistant text, tiny tool_result). Assistant turns at
    /// odd indices 1,3,5,7 (tool-heavy) and 9,11,13,15 (reasoning).
    fn mixed_density_convo() -> Vec<Value> {
        let mut m = vec![json!({"role":"user","content":[{"type":"text","text":"start"}]})];
        for i in 0..8 {
            let id = format!("t{i}");
            if i < 4 {
                m.push(json!({"role":"assistant","content":[
                    {"type":"tool_use","id":id,"name":"Bash","input":{"command":"x"}}
                ]}));
                m.push(json!({"role":"user","content":[
                    {"type":"tool_result","tool_use_id":id,"content":"T".repeat(2000)}
                ]}));
            } else {
                m.push(json!({"role":"assistant","content":[
                    {"type":"text","text":"R".repeat(2000)},
                    {"type":"tool_use","id":id,"name":"Bash","input":{"command":"x"}}
                ]}));
                m.push(json!({"role":"user","content":[
                    {"type":"tool_result","tool_use_id":id,"content":"ok"}
                ]}));
            }
        }
        m
    }

    #[test]
    fn densify_start_rescues_a_tool_heavy_widest_slice() {
        let m = mixed_density_convo();
        let (start, end) = select_slice(&m, 2, usize::MAX).unwrap();
        // The widest slice is tool-dominated (would be SKIPPED by the 0.6 gate).
        assert!(
            tool_frac(&m[start..end]) > 0.6,
            "widest slice should be tool-heavy in this fixture"
        );
        // Density-aware fallback finds a denser sub-window that passes.
        let s = densify_start(&m, start, end, 0.6, tool_frac)
            .expect("a reasoning-dense sub-window should be rescuable");
        assert!(
            s > start,
            "must advance the start forward, got {s} <= {start}"
        );
        assert!(
            tool_frac(&m[s..end]) <= 0.6,
            "the rescued sub-window must pass the density bound"
        );
        // It must remain a valid, orphan-free summarizable slice.
        let d = SummaryDecision::new(&m, s, end, "## Goal\nrescued").unwrap();
        let mut out = m.clone();
        assert!(apply_summary(&mut out, &m, &d));
        PairingIndex::build(&out)
            .validate()
            .expect("no orphans after rescue");
    }

    #[test]
    fn densify_start_picks_the_widest_passing_window() {
        // The EARLIEST eligible start that passes (widest passing window), so the
        // start immediately before it must still FAIL the bound.
        let m = mixed_density_convo();
        let (start, end) = select_slice(&m, 2, usize::MAX).unwrap();
        let s = densify_start(&m, start, end, 0.6, tool_frac).unwrap();
        let earlier: Vec<usize> = eligible_starts_to(&m, end)
            .into_iter()
            .filter(|&x| x > start && x < s)
            .collect();
        for e in earlier {
            assert!(
                tool_frac(&m[e..end]) > 0.6,
                "every start earlier than the chosen one must fail the bound (got {e})"
            );
        }
    }

    #[test]
    fn densify_start_none_when_uniformly_tool_heavy() {
        // Every sub-window is tool-dominated → nothing to rescue.
        let m = convo(8); // all tiny, but make tool_results dominate
        let mut heavy = m.clone();
        for msg in heavy.iter_mut() {
            if let Some(c) = msg.get_mut("content").and_then(Value::as_array_mut) {
                for b in c {
                    if b.get("type").and_then(Value::as_str) == Some("tool_result") {
                        b["content"] = json!("T".repeat(3000));
                    }
                }
            }
        }
        let (start, end) = select_slice(&heavy, 2, usize::MAX).unwrap();
        assert!(tool_frac(&heavy[start..end]) > 0.6);
        assert!(
            densify_start(&heavy, start, end, 0.6, tool_frac).is_none(),
            "no eligible sub-window passes → None (caller skips, as before)"
        );
    }

    #[test]
    fn serialize_slice_labels_and_caps() {
        let mut m = vec![json!({"role":"assistant","content":[
            {"type":"text","text":"thinking out loud"},
            {"type":"tool_use","id":"t0","name":"Bash","input":{"command":"ls"}}
        ]})];
        m.push(json!({"role":"user","content":[
            {"type":"tool_result","tool_use_id":"t0","content":"X".repeat(500)}
        ]}));
        let text = serialize_slice(&m, 8_000, 100);
        assert!(text.contains("### assistant"));
        assert!(text.contains("[tool_use Bash]"));
        assert!(text.contains("thinking out loud"));
        assert!(text.contains("[tool_result]"));
        assert!(text.contains("chars elided"), "long result must be capped");
        // The 500-char result is reduced well below its original size.
        assert!(
            text.len() < 600,
            "capped output stays small: {}",
            text.len()
        );
    }

    #[test]
    fn serialize_slice_caps_tool_result_harder_than_reasoning() {
        // ~1.4K chars of reasoning + a 4K-char tool_result. With the production
        // asymmetry, reasoning survives in full but the tool_result is trimmed hard.
        let reasoning = "decided on approach B because the trait bound failed. ".repeat(26);
        let mut m = vec![json!({"role":"assistant","content":[
            {"type":"text","text": reasoning.clone()}
        ]})];
        m.push(json!({"role":"user","content":[
            {"type":"tool_result","tool_use_id":"t0","content":"X".repeat(4_000)}
        ]}));
        let text = serialize_slice(&m, REASONING_BLOCK_CAP, TOOL_RESULT_BLOCK_CAP);
        assert!(
            text.contains(&reasoning),
            "reasoning under the reasoning cap must survive verbatim"
        );
        assert!(
            text.contains("chars elided"),
            "the 4K tool_result must be trimmed by the small tool_result cap"
        );
        // tool_result trimmed to ~TOOL_RESULT_BLOCK_CAP, far below its 4K original.
        let result_section = text.split("[tool_result]").nth(1).unwrap_or("");
        // Tight bound: < 2× the tool_result cap (≈ head+tail+elision), which a
        // reasoning_cap/tool_result_cap arg-swap (8000) would blow past.
        assert!(
            result_section.len() < TOOL_RESULT_BLOCK_CAP * 2,
            "tool_result section stays near its cap (arg-swap guard): {}",
            result_section.len()
        );
    }

    #[test]
    fn cap_slice_start_narrows_oversized_reasoning_slice() {
        // 12 reasoning-dense pairs (~6.3K-char text each, under the 8K cap so they
        // survive) → the full slice far exceeds a 30K budget and must be narrowed.
        let mut m = vec![json!({"role":"user","content":[{"type":"text","text":"go"}]})];
        for i in 0..12 {
            let prose =
                format!("analysed failure {i} and chose approach B for case {i}. ").repeat(110);
            m.push(json!({"role":"assistant","content":[{"type":"text","text": prose.clone()}]}));
            m.push(json!({"role":"user","content":[{"type":"text","text": prose}]}));
        }
        let (start, end) = (1, m.len());
        let budget = 30_000;
        let full =
            serialize_slice(&m[start..end], REASONING_BLOCK_CAP, TOOL_RESULT_BLOCK_CAP).len();
        assert!(
            full > budget,
            "precondition: full slice exceeds the budget ({full})"
        );
        let narrowed = cap_slice_start(
            &m,
            start,
            end,
            REASONING_BLOCK_CAP,
            TOOL_RESULT_BLOCK_CAP,
            budget,
        );
        assert!(
            narrowed > start,
            "must advance the start to shrink the slice"
        );
        assert!(
            (narrowed - start) % 2 == 0,
            "narrowed start stays on an assistant-turn boundary"
        );
        let capped = serialize_slice(
            &m[narrowed..end],
            REASONING_BLOCK_CAP,
            TOOL_RESULT_BLOCK_CAP,
        )
        .len();
        assert!(
            capped <= budget,
            "narrowed slice must fit the budget ({capped} <= {budget})"
        );
        assert!(end >= narrowed + 4, "keeps at least ~2 pairs");
    }

    #[test]
    fn cap_slice_start_keeps_start_when_slice_fits() {
        let mut m = vec![json!({"role":"user","content":[{"type":"text","text":"go"}]})];
        for i in 0..3 {
            let id = format!("t{i}");
            m.push(json!({"role":"assistant","content":[{"type":"tool_use","id":id,"name":"Bash","input":{"command":"x"}}]}));
            m.push(json!({"role":"user","content":[{"type":"tool_result","tool_use_id":id,"content":"small"}]}));
        }
        let end = m.len();
        assert_eq!(
            cap_slice_start(
                &m,
                1,
                end,
                REASONING_BLOCK_CAP,
                TOOL_RESULT_BLOCK_CAP,
                40_000
            ),
            1,
            "a slice already under budget keeps its original start"
        );
    }

    #[test]
    fn cap_slice_end_narrows_an_oversized_delta_to_a_fitting_whole_pair_chunk() {
        // 12 reasoning-dense pairs: the full delta [1..end] far exceeds a 30K budget,
        // so the accumulator must cap the END to a budget-sized contiguous chunk.
        let mut m = vec![json!({"role":"user","content":[{"type":"text","text":"go"}]})];
        for i in 0..12 {
            let prose =
                format!("analysed failure {i} and chose approach B for case {i}. ").repeat(110);
            m.push(json!({"role":"assistant","content":[{"type":"text","text": prose.clone()}]}));
            m.push(json!({"role":"user","content":[{"type":"text","text": prose}]}));
        }
        let (start, max_end) = (1, m.len());
        let budget = 30_000;
        let full = serialize_slice(
            &m[start..max_end],
            REASONING_BLOCK_CAP,
            TOOL_RESULT_BLOCK_CAP,
        )
        .len();
        assert!(
            full > budget,
            "precondition: full delta exceeds the budget ({full})"
        );
        let capped_end = cap_slice_end(
            &m,
            start,
            max_end,
            REASONING_BLOCK_CAP,
            TOOL_RESULT_BLOCK_CAP,
            budget,
        );
        assert!(capped_end > start + 3, "must keep at least ~2 pairs");
        assert!(
            capped_end < max_end,
            "must narrow the end below the full delta"
        );
        assert!(
            (capped_end - start) % 2 == 0,
            "capped end stays on an assistant-turn boundary"
        );
        let chunk = serialize_slice(
            &m[start..capped_end],
            REASONING_BLOCK_CAP,
            TOOL_RESULT_BLOCK_CAP,
        )
        .len();
        assert!(
            chunk <= budget,
            "capped chunk must fit the budget ({chunk} <= {budget})"
        );
        // The next end up would overflow (it returned the LARGEST fitting end).
        if let Some(next) = (capped_end + 1..=max_end).find(|&e| {
            m.get(e).and_then(|x| x.get("role")).and_then(Value::as_str) == Some("assistant")
        }) {
            let bigger =
                serialize_slice(&m[start..next], REASONING_BLOCK_CAP, TOOL_RESULT_BLOCK_CAP).len();
            assert!(
                bigger > budget,
                "cap_slice_end returns the LARGEST fitting chunk"
            );
        }
    }

    #[test]
    fn cap_slice_end_returns_max_end_when_the_whole_delta_fits() {
        let mut m = vec![json!({"role":"user","content":[{"type":"text","text":"go"}]})];
        for i in 0..4 {
            let id = format!("t{i}");
            m.push(json!({"role":"assistant","content":[{"type":"tool_use","id":id,"name":"Bash","input":{"command":"x"}}]}));
            m.push(json!({"role":"user","content":[{"type":"tool_result","tool_use_id":id,"content":"small"}]}));
        }
        // max_end must be an assistant-turn index; use the start of the last pair.
        let max_end = m.len() - 2;
        assert_eq!(
            cap_slice_end(
                &m,
                1,
                max_end,
                REASONING_BLOCK_CAP,
                TOOL_RESULT_BLOCK_CAP,
                40_000
            ),
            max_end,
            "a delta already under budget summarizes the whole thing"
        );
    }

    #[test]
    fn cap_slice_end_returns_start_when_no_two_pair_end_fits() {
        // Documented edge: when the available range can't hold even ~2 pairs (no
        // assistant-aligned end ≥ start+4 exists ≤ max_end), there are no candidates →
        // return `start`, so the caller's `end >= start + 4` guard skips (too little to
        // summarize) rather than building a degenerate 1-pair segment.
        let m = convo(8); // assistant indices 1,3,5,...
        // start=1, max_end=3 → the candidate range start+4..=max_end (5..=3) is empty.
        assert_eq!(
            cap_slice_end(&m, 1, 3, REASONING_BLOCK_CAP, TOOL_RESULT_BLOCK_CAP, 40_000),
            1,
            "no ≥2-pair end in range → returns start (caller then skips)"
        );
    }
}
