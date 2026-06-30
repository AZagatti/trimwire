//! `BloatCap` strategy — trim each oversized old `tool_result` to head + tail.
//!
//! Catches the large *unique* old result that dedup (only removes duplicates)
//! and sliding_window (only denylisted tools) miss — e.g. a 200 KB `cargo
//! build` dump from 10 turns ago. Replaces the middle with a marker, keeping
//! the first `head_bytes` and last `tail_bytes`.
//!
//! **Safe by construction:** by default only trims results *older* than
//! `keep_recent_turns` (so the model is never deprived of a result it's
//! actively using), only string content (structured/image content is left to
//! `ImageStrip`), and never touches `tool_use.input` or the pairing — so it
//! cannot orphan anything. Deterministic.
//!
//! **POC — `catastrophic_bytes` (opt-in, default 0 = OFF):** also caps a RECENT
//! result if it ALONE exceeds the (much higher) catastrophic threshold. This
//! deliberately lifts the recent-window exemption — justified only because a
//! result that large can't be used in full anyway (it exceeds the context
//! window, so it would otherwise brick the session). Uses a generous head/tail
//! floor (`CATASTROPHIC_KEEP`) and a distinct `catastrophic-cap` marker so it
//! is diagnosable. Off by default → zero effect on existing behaviour.

use serde_json::Value;

use crate::config::{BloatCapConfig, matches_any};
use crate::error::Result;
use crate::pairing::PairingIndex;
use crate::strategies::{Stats, assistant_cutoff, block_mut, elision_marker, is_already_cleared};

/// Trim oversized old string tool_results, and elide oversized old array-content
/// tool_results with a single marker (same "only if it shrinks" guard).
pub fn apply(messages: &mut [Value], cfg: &BloatCapConfig) -> Result<Stats> {
    super::with_stats(messages, |m| apply_counted(m, cfg))
}

/// Worker: returns the number of results trimmed/elided. Byte accounting is
/// threaded by the caller ([`super::run`]); see [`apply`].
pub(crate) fn apply_counted(messages: &mut [Value], cfg: &BloatCapConfig) -> Result<usize> {
    let idx = PairingIndex::build(messages);
    idx.validate()?;

    // `None` ⇒ history too short to have any "old" turn. We DON'T early-return:
    // the opt-in catastrophic cap may still fire on a recent (even first-turn)
    // result that alone exceeds the context window.
    let cutoff = assistant_cutoff(messages, cfg.keep_recent_turns);
    // B-5 age ladder (opt-in, 0 = off): a SECOND, older boundary. A non-recent
    // result older than `stub_age_turns` is FULLY stubbed (marker only) instead of
    // head+tail trimmed — trading the head/tail glimpse + signal lines for bytes.
    // GUARD: only engage when `stub_age_turns > keep_recent_turns`; a value at or
    // below the head+tail window would collapse the middle tier and stub far more
    // aggressively than intended, so we treat that misconfiguration as OFF.
    let stub_cutoff = (cfg.stub_age_turns > cfg.keep_recent_turns)
        .then(|| assistant_cutoff(messages, cfg.stub_age_turns))
        .flatten();

    // Read-only pass: collect edits — either a string trim or an array elision.
    // Each edit is (location, new_content_value).
    let mut edits: Vec<((usize, usize), Value)> = Vec::new();
    for (mi, msg) in messages.iter().enumerate() {
        // Old results use the normal threshold. Recent results are EXEMPT (the
        // model may be actively using them) UNLESS the opt-in catastrophic cap is
        // enabled — then they're trimmed at its (much higher) threshold, with a
        // generous head/tail floor and a distinct marker. `cutoff == None` ⇒ every
        // message is recent.
        let recent = cutoff.is_none_or(|c| mi > c);
        // B-5: a non-recent result older than the stub boundary → full stub tier.
        let very_old = !recent && stub_cutoff.is_some_and(|sc| mi <= sc);
        let (threshold, head, tail, label) = if recent {
            if cfg.catastrophic_bytes == 0 {
                continue; // recent + catastrophic disabled ⇒ leave untouched
            }
            (
                cfg.catastrophic_bytes,
                cfg.head_bytes.max(CATASTROPHIC_KEEP),
                cfg.tail_bytes.max(CATASTROPHIC_KEEP),
                "catastrophic-cap trimmed",
            )
        } else {
            (
                cfg.threshold_bytes,
                cfg.head_bytes,
                cfg.tail_bytes,
                "trimmed",
            )
        };
        let Some(content_arr) = msg.get("content").and_then(Value::as_array) else {
            continue;
        };
        for (ci, block) in content_arr.iter().enumerate() {
            if block.get("type").and_then(Value::as_str) != Some("tool_result") {
                continue;
            }
            // Resolve the paired tool_use once — used for exempt-by-name and the
            // POC protect-by-path check below. (`Option<&Value>` is `Copy`.)
            let paired_use = block
                .get("tool_use_id")
                .and_then(Value::as_str)
                .and_then(|id| idx.uses.get(id))
                .and_then(|&(umi, uci)| messages[umi].get("content")?.as_array()?.get(uci));
            // Exempt check via the paired tool_use's name — two tiers:
            //  - `exempt_tools`: NEVER trimmed at any age (authoring/load-bearing:
            //    Write/Edit/MultiEdit/Task — eliding their results corrupts sessions).
            //  - `exempt_recent_only_tools`: exempt ONLY while RECENT (within
            //    keep_recent_turns). `Read` lives here — recent reads stay protected
            //    (active use), but an OLD large Read result is trimmed (the "Read
            //    coverage gap" fix). An unresolvable name matches neither → eligible.
            if let Some(name) = paired_use
                .and_then(|u| u.get("name"))
                .and_then(Value::as_str)
            {
                if matches_any(&cfg.exempt_tools, name) {
                    continue;
                }
                if recent && matches_any(&cfg.exempt_recent_only_tools, name) {
                    continue;
                }
            }
            // POC protect-by-path: never trim a result whose paired tool_use
            // targets a protected file path (input.file_path / input.path).
            if !cfg.protected_file_patterns.is_empty()
                && paired_use
                    .and_then(|u| u.get("input"))
                    .and_then(|i| i.get("file_path").or_else(|| i.get("path")))
                    .and_then(Value::as_str)
                    .is_some_and(|p| matches_any(&cfg.protected_file_patterns, p))
            {
                continue;
            }

            let result_content = block.get("content").unwrap_or(&Value::Null);

            // --- String content path (existing behaviour) ---
            if let Some(text) = result_content.as_str() {
                // Skip results already carrying ANY trimwire marker (our string
                // trim, the array-elided stub once it's been replaced by a string,
                // the failed-input-purge marker) or CC's own compact marker. The
                // shared `[trimwire: ` namespace makes this idempotent regardless
                // of which marker produced it, even at a tiny custom threshold.
                if text.starts_with("[trimwire: ") || is_already_cleared(result_content) {
                    continue;
                }
                if text.len() <= threshold {
                    continue;
                }
                if very_old {
                    // B-5 stub tier: a full marker instead of head+tail (only when
                    // it actually shrinks the body).
                    // B-5 fully erases the result (no head/tail glimpse), leaving only
                    // a byte count — so, unlike the inline trim, the model has nothing
                    // to work from. A recovery hint (NOT a report nudge: bloat_cap only
                    // reduces, it never fabricates) shortens the model's path back to
                    // the content. Opt-in (stub_age_turns>0); off by default.
                    let marker = elision_marker(
                        "[trimwire: aged-out result — re-run the tool to restore full content",
                        result_content,
                    );
                    if serde_json::to_string(&marker)
                        .map(|m| m.len())
                        .unwrap_or(usize::MAX)
                        < text.len()
                    {
                        edits.push(((mi, ci), marker));
                    }
                } else if let Some(trimmed) = trim(text, head, tail, label) {
                    edits.push(((mi, ci), Value::String(trimmed)));
                }
                continue;
            }

            // --- Array content path ---
            if let Some(arr) = result_content.as_array() {
                let marker_stub = "[trimwire: array content elided";
                let already_our_array_stub =
                    arr.len() == 1 && arr[0].as_str().is_some_and(|s| s.starts_with(marker_stub));
                if already_our_array_stub {
                    continue;
                }
                // Serialise the array to measure its size. Use compact JSON to
                // match how serde_json serialises it on the wire.
                let serialized = serde_json::to_string(result_content).unwrap_or_default();
                if serialized.len() <= threshold {
                    continue;
                }
                // §13C: SALVAGE the bulky TEXT blocks inside the array (head/tail +
                // signal lines, the same `trim` the string path uses) instead of
                // total-erasing the whole array. This keeps structure, small blocks,
                // images, and — crucially — error/warning signal the model may need.
                // Idempotent: `trim` returns None once a block is already minimal.
                let mut new_arr = arr.clone();
                let mut any_trimmed = false;
                let mut has_text = false;
                for blk in new_arr.iter_mut() {
                    let Some(obj) = blk.as_object_mut() else {
                        continue;
                    };
                    // Own the text so the immutable borrow ends before we insert.
                    let Some(text) = obj.get("text").and_then(Value::as_str).map(str::to_owned)
                    else {
                        continue;
                    };
                    has_text = true;
                    // Only trim a block that is itself over threshold (mirrors the
                    // string path). A trimmed block falls below threshold, so a
                    // re-run never re-trims it → idempotent (no digit-count drift).
                    if text.len() <= threshold {
                        continue;
                    }
                    if let Some(trimmed) = trim(&text, head, tail, label) {
                        obj.insert("text".to_owned(), Value::String(trimmed));
                        any_trimmed = true;
                    }
                }
                if any_trimmed {
                    let new_val = Value::Array(new_arr);
                    if serde_json::to_string(&new_val).unwrap_or_default().len() < serialized.len()
                    {
                        edits.push(((mi, ci), new_val));
                    }
                    continue;
                }
                // Array has text blocks but none were trimmable (already minimal /
                // each small) → leave it intact rather than destroy salvageable text.
                if has_text {
                    continue;
                }
                // Pure non-text (image/binary) array over threshold → total-erase
                // fallback (still caps it), only if the marker actually shrinks.
                let marker = elision_marker(marker_stub, result_content);
                let marker_len = serde_json::to_string(&marker).unwrap_or_default().len();
                if marker_len < serialized.len() {
                    edits.push(((mi, ci), marker));
                }
            }
        }
    }

    let mut stubbed = 0usize;
    for (loc, new_content) in edits {
        if let Some(block) = block_mut(messages, loc) {
            block["content"] = new_content;
            stubbed += 1;
        }
    }

    Ok(stubbed)
}

/// Diagnostic keywords salvaged from the dropped middle (lower-cased contains).
const SIGNAL_KEYWORDS: [&str; 5] = ["error", "warning", "fail", "panic", "exception"];
const MAX_SIGNAL_LINES: usize = 12;
const MAX_SIGNAL_BYTES: usize = 2_048;
/// Floor for head/tail kept by the opt-in catastrophic cap on RECENT results:
/// the model may be actively using the result, so keep generously (≥16 KB each
/// end) — still a vast reduction on a window-exceeding (e.g. 580 KB) result.
const CATASTROPHIC_KEEP: usize = 16_384;

/// Replace the middle of `s` with a marker, keeping `head`/`tail` bytes (clamped
/// to UTF-8 char boundaries) **plus** any diagnostically-important lines from the
/// dropped middle (errors/warnings/failures) — so the model keeps the *signal*,
/// not just arbitrary head bytes. Deterministic (fixed keyword set, fixed caps,
/// original line order). Returns `None` when the trim wouldn't shrink the string
/// (head/tail overlap, or the marker + salvage exceed the bytes removed) — the
/// caller then leaves the result untouched (the safe head/tail-or-nothing fallback).
fn trim(s: &str, head: usize, tail: usize, label: &str) -> Option<String> {
    let head_end = floor_boundary(s, head);
    let tail_start = ceil_boundary(s, s.len().saturating_sub(tail));
    // Overlap (head+tail >= len): nothing to remove.
    if tail_start <= head_end {
        return None;
    }
    let removed = tail_start - head_end;

    // Salvage bounded signal lines from the middle that's about to be dropped.
    let mut salvaged = String::new();
    let mut kept = 0usize;
    for line in s[head_end..tail_start].lines() {
        if kept >= MAX_SIGNAL_LINES || salvaged.len() >= MAX_SIGNAL_BYTES {
            break;
        }
        let lower = line.to_ascii_lowercase();
        if SIGNAL_KEYWORDS.iter().any(|kw| lower.contains(kw)) {
            salvaged.push_str(line);
            salvaged.push('\n');
            kept += 1;
        }
    }

    let out = if kept > 0 {
        format!(
            "{}\n[trimwire: {label} {removed} bytes, kept {kept} signal line(s)]\n{salvaged}{}",
            &s[..head_end],
            &s[tail_start..]
        )
    } else {
        format!(
            "{}\n[trimwire: {label} {removed} bytes]\n{}",
            &s[..head_end],
            &s[tail_start..]
        )
    };
    // Never emit a result larger than the original — on small inputs (or a
    // middle full of signal lines) the overhead can outweigh the bytes removed.
    (out.len() < s.len()).then_some(out)
}

fn floor_boundary(s: &str, mut i: usize) -> usize {
    if i >= s.len() {
        return s.len();
    }
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

fn ceil_boundary(s: &str, mut i: usize) -> usize {
    while i < s.len() && !s.is_char_boundary(i) {
        i += 1;
    }
    i
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn cfg(threshold: usize, keep: usize, exempt: &[&str]) -> BloatCapConfig {
        BloatCapConfig {
            enabled: true,
            threshold_bytes: threshold,
            head_bytes: 8,
            tail_bytes: 8,
            keep_recent_turns: keep,
            exempt_tools: exempt.iter().map(|s| (*s).to_owned()).collect(),
            exempt_recent_only_tools: Vec::new(), // off unless a test opts in
            catastrophic_bytes: 0,                // off unless a test opts in
            stub_age_turns: 0,                    // off unless a test opts in
            protected_file_patterns: Vec::new(),  // off unless a test opts in
        }
    }

    /// N Bash turns; each result is `size` bytes.
    fn session(turns: usize, size: usize) -> Vec<Value> {
        let mut msgs = Vec::new();
        for i in 0..turns {
            let uid = format!("u{i}");
            msgs.push(json!({"role": "user", "content": [{"type": "text", "text": "go"}]}));
            msgs.push(json!({"role": "assistant", "content": [
                {"type": "tool_use", "id": uid, "name": "Bash", "input": {"command": format!("c{i}")}}
            ]}));
            msgs.push(json!({"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": uid, "content": "z".repeat(size)}
            ]}));
        }
        msgs
    }

    #[test]
    fn trims_old_oversized_results_keeps_recent() {
        let mut msgs = session(10, 100); // 10 turns, 100-byte results
        let stats = apply(&mut msgs, &cfg(50, 4, &[])).unwrap();
        // 10 turns, keep 4. A tool_result sits one message AFTER its tool_use,
        // so the cutoff-turn's result is just outside the window → 5 trimmed
        // (turns 0–4), not 6.
        assert_eq!(stats.stubbed, 5);
        assert!(stats.elided_bytes() > 0);
        // Oldest trimmed (head 8 + marker + tail 8); most recent untouched.
        let trimmed = msgs[2]["content"][0]["content"].as_str().unwrap();
        assert!(trimmed.contains("[trimwire: trimmed"));
        assert_eq!(
            msgs[29]["content"][0]["content"].as_str().unwrap().len(),
            100
        );
        PairingIndex::build(&msgs).validate().unwrap();
    }

    #[test]
    fn catastrophic_cap_trims_recent_only_when_enabled() {
        // A 2-turn session with keep_recent_turns=8 → EVERY turn is "recent", so
        // the normal path never trims. turn 0 = small (5 KB) recent result; turn 1
        // (most recent) = catastrophic (600 KB) result.
        let big = "z".repeat(600_000);
        let small = "z".repeat(5_000);
        let build = || {
            json!([
                {"role":"user","content":[{"type":"text","text":"go"}]},
                {"role":"assistant","content":[{"type":"tool_use","id":"a","name":"Bash","input":{"command":"c0"}}]},
                {"role":"user","content":[{"type":"tool_result","tool_use_id":"a","content": small}]},
                {"role":"user","content":[{"type":"text","text":"go"}]},
                {"role":"assistant","content":[{"type":"tool_use","id":"b","name":"Bash","input":{"command":"c1"}}]},
                {"role":"user","content":[{"type":"tool_result","tool_use_id":"b","content": big}]},
            ])
            .as_array()
            .unwrap()
            .clone()
        };

        // DISABLED (catastrophic_bytes = 0): recent results untouched even at 600 KB.
        let mut off = build();
        let stats_off = apply(&mut off, &cfg(50, 8, &[])).unwrap();
        assert_eq!(stats_off.stubbed, 0, "disabled → recent results untouched");
        assert_eq!(
            off[5]["content"][0]["content"].as_str().unwrap().len(),
            600_000
        );

        // ENABLED at 400 KB: only the 600 KB recent result is capped; the 5 KB one
        // (above the normal 50-byte threshold, but recent) is NOT — recent results
        // are only ever touched at the catastrophic threshold.
        let mut on = build();
        let mut c = cfg(50, 8, &[]);
        c.catastrophic_bytes = 400_000;
        let stats_on = apply(&mut on, &c).unwrap();
        assert_eq!(
            stats_on.stubbed, 1,
            "only the catastrophic recent result is capped"
        );
        let capped = on[5]["content"][0]["content"].as_str().unwrap();
        assert!(
            capped.contains("[trimwire: catastrophic-cap trimmed"),
            "distinct catastrophic marker for diagnosis"
        );
        assert!(capped.len() < 600_000, "the catastrophic result shrank");
        assert!(
            capped.len() >= CATASTROPHIC_KEEP,
            "generous head/tail floor preserved (model may be using it)"
        );
        assert_eq!(
            on[2]["content"][0]["content"].as_str().unwrap().len(),
            5_000,
            "the small recent result is left untouched"
        );
        PairingIndex::build(&on).validate().unwrap();

        // Idempotent: re-running caps nothing further (marker prefix is recognised).
        let again = apply(&mut on, &c).unwrap();
        assert_eq!(again.stubbed, 0, "idempotent — already capped");
    }

    #[test]
    fn catastrophic_cap_handles_recent_array_content() {
        // A RECENT tool_result whose content is an ARRAY with a huge text block
        // (e.g. an MCP result) — the catastrophic cap must salvage it via the
        // array path, not just the string path.
        let big = "z".repeat(600_000);
        let mut m = vec![
            json!({"role":"user","content":[{"type":"text","text":"go"}]}),
            json!({"role":"assistant","content":[{"type":"tool_use","id":"a","name":"Bash","input":{"command":"c"}}]}),
            json!({"role":"user","content":[{"type":"tool_result","tool_use_id":"a","content":[
                {"type":"text","text": big}
            ]}]}),
        ];
        let mut c = cfg(50, 8, &[]); // keep_recent 8 → the only turn is recent
        c.catastrophic_bytes = 400_000;
        let stats = apply(&mut m, &c).unwrap();
        assert_eq!(
            stats.stubbed, 1,
            "recent array-content catastrophic result is capped"
        );
        let inner = m[2]["content"][0]["content"][0]["text"].as_str().unwrap();
        assert!(inner.len() < 600_000, "the huge text block shrank");
        assert!(
            inner.contains("[trimwire: catastrophic-cap trimmed"),
            "distinct marker"
        );
        PairingIndex::build(&m).validate().unwrap();
    }

    #[test]
    fn b5_age_ladder_benchmark() {
        // BENCHMARK: 20 turns of 20 KB results, head/tail=2048 (prod defaults),
        // threshold 4096, keep_recent 2, stub_age_turns 6.
        // Baseline = current bloat_cap (head+tail for old). B-5 = very-old (older
        // than `stub_age_turns`=6) fully stubbed. Reports the extra bytes B-5 saves
        // over baseline, and the fidelity it gives up (head/tail glimpse).
        let serialized = |m: &[Value]| serde_json::to_string(m).unwrap().len();
        // Production-like: 20 KB results, head/tail = 2048 (the real defaults), so
        // baseline keeps ~4 KB/old result and B-5's full-stub saves the difference.
        let prod_cfg = |stub_age: usize| BloatCapConfig {
            enabled: true,
            threshold_bytes: 4_096,
            head_bytes: 2_048,
            tail_bytes: 2_048,
            keep_recent_turns: 2,
            exempt_tools: vec![],
            exempt_recent_only_tools: vec![],
            catastrophic_bytes: 0,
            stub_age_turns: stub_age,
            protected_file_patterns: Vec::new(),
        };
        let base_in = session(20, 20_000);
        let raw = serialized(&base_in);

        let mut base = base_in.clone();
        apply(&mut base, &prod_cfg(0)).unwrap();
        let base_bytes = serialized(&base);

        let mut b5 = base_in.clone();
        apply(&mut b5, &prod_cfg(6)).unwrap();
        let b5_bytes = serialized(&b5);

        let extra = base_bytes.saturating_sub(b5_bytes);
        eprintln!(
            "B-5 benchmark (20 turns × 20KB): raw={raw} baseline(bloat_cap head+tail)={base_bytes} \
             b5(age-ladder full-stub)={b5_bytes} → b5 saves {extra} MORE bytes than baseline \
             ({:.1}% of raw), at the cost of the head/tail+signal glimpse on very-old results",
            extra as f64 / raw as f64 * 100.0
        );
        assert!(
            b5_bytes < base_bytes,
            "B-5 full-stub of very-old saves more than the head+tail baseline"
        );
        PairingIndex::build(&b5).validate().unwrap();
    }

    #[test]
    fn b5_age_ladder_three_tiers() {
        // stub_age_turns=6, keep_recent=2 over 20 turns of 20KB results →
        // recent (last 2) untouched, mid-age head+tail, very-old fully stubbed.
        let mut m = session(20, 20_000);
        // head+tail well below threshold so a trimmed result falls under it (clean
        // idempotency — the threshold≈head+tail edge is a separate bloat_cap matter).
        let mut c = cfg(4_096, 2, &[]);
        c.head_bytes = 512;
        c.tail_bytes = 512;
        c.stub_age_turns = 6;
        apply(&mut m, &c).unwrap();

        let results: Vec<&str> = m
            .iter()
            .filter_map(|x| x.get("content").and_then(Value::as_array))
            .flatten()
            .filter(|b| b.get("type").and_then(Value::as_str) == Some("tool_result"))
            .filter_map(|b| b.get("content").and_then(Value::as_str))
            .collect();
        let aged = results
            .iter()
            .filter(|r| r.contains("aged-out result"))
            .count();
        let trimmed = results
            .iter()
            .filter(|r| r.contains("[trimwire: trimmed"))
            .count();
        let untouched = results.iter().filter(|r| r.len() == 20_000).count();
        assert!(
            aged > 0,
            "very-old results are fully stubbed (aged-out tier)"
        );
        assert!(
            trimmed > 0,
            "mid-age results are head+tail trimmed (middle tier)"
        );
        assert!(
            untouched >= 1,
            "the most-recent results are untouched (full tier)"
        );
        PairingIndex::build(&m).validate().unwrap();

        // Idempotent: markers carry the `[trimwire: ` prefix → skipped on re-run.
        let again = apply(&mut m, &c).unwrap();
        assert_eq!(again.stubbed, 0, "idempotent — already laddered");
    }

    #[test]
    fn b5_misconfig_below_keep_recent_is_off() {
        // stub_age_turns <= keep_recent_turns must be treated as OFF (no full-stub),
        // not silently collapse the middle tier.
        let mut m = session(20, 20_000);
        let mut c = cfg(4_096, 6, &[]); // keep_recent = 6
        c.head_bytes = 512;
        c.tail_bytes = 512;
        c.stub_age_turns = 3; // 3 <= 6 → misconfigured → off
        apply(&mut m, &c).unwrap();
        let any_aged = m
            .iter()
            .filter_map(|x| x.get("content").and_then(Value::as_array))
            .flatten()
            .filter_map(|b| b.get("content").and_then(Value::as_str))
            .any(|r| r.contains("aged-out result"));
        assert!(
            !any_aged,
            "stub_age_turns <= keep_recent_turns → no full-stub tier"
        );
    }

    #[test]
    fn protected_file_patterns_skips_matching_paths() {
        // Two OLD oversized Read results (200 B > threshold 50): one for a
        // protected path (AGENTS.md), one not. Read is NOT in this test's
        // exempt_tools, so without protection both would trim.
        let big = "x".repeat(200);
        let mk = |id: &str, path: &str| {
            vec![
                json!({"role":"user","content":[{"type":"text","text":"go"}]}),
                json!({"role":"assistant","content":[{"type":"tool_use","id":id,"name":"Read","input":{"file_path":path}}]}),
                json!({"role":"user","content":[{"type":"tool_result","tool_use_id":id,"content": big.clone()}]}),
            ]
        };
        let mut m = Vec::new();
        m.extend(mk("a", "/repo/AGENTS.md"));
        m.extend(mk("b", "/repo/src/foo.rs"));
        // Pad with recent Bash turns so the two Reads are OLD (outside keep=2).
        for i in 0..6 {
            let id = format!("p{i}");
            m.push(json!({"role":"user","content":[{"type":"text","text":"go"}]}));
            m.push(json!({"role":"assistant","content":[{"type":"tool_use","id":id,"name":"Bash","input":{"command":"c"}}]}));
            m.push(json!({"role":"user","content":[{"type":"tool_result","tool_use_id":id,"content":"ok"}]}));
        }
        let mut c = cfg(50, 2, &[]);
        c.protected_file_patterns = vec!["*AGENTS.md".to_owned()];
        apply(&mut m, &c).unwrap();
        assert_eq!(
            m[2]["content"][0]["content"].as_str().unwrap().len(),
            200,
            "protected AGENTS.md result is left untouched"
        );
        assert!(
            m[5]["content"][0]["content"]
                .as_str()
                .unwrap()
                .contains("[trimwire: trimmed"),
            "non-protected foo.rs result is still trimmed"
        );
        PairingIndex::build(&m).validate().unwrap();
    }

    #[test]
    fn under_threshold_untouched() {
        let mut msgs = session(10, 20);
        let stats = apply(&mut msgs, &cfg(1000, 4, &[])).unwrap();
        assert_eq!(stats.stubbed, 0);
    }

    #[test]
    fn exempt_tool_not_trimmed() {
        let mut msgs = session(10, 100);
        // Rename the tool to Read (exempt by default).
        for m in msgs.iter_mut() {
            if let Some(c) = m.get_mut("content").and_then(Value::as_array_mut) {
                for b in c {
                    if b.get("type").and_then(Value::as_str) == Some("tool_use") {
                        b["name"] = json!("Read");
                    }
                }
            }
        }
        let stats = apply(&mut msgs, &cfg(50, 4, &["Read"])).unwrap();
        assert_eq!(stats.stubbed, 0);
    }

    #[test]
    fn subagent_agent_and_task_exempt_by_default() {
        // Drift fix: the PRODUCTION default exempt_tools must cover BOTH subagent tool
        // names — `Task` AND the drifted `Agent` — so subagent results (findings/blocker
        // lists) are not middle-trimmed; ordinary large `Bash` and unrelated tools still
        // trim (no accidental broad exemption). Uses the real BloatCapConfig::default()
        // exempt list; only thresholds are tweaked for test-sized payloads.
        let base = BloatCapConfig {
            enabled: true,
            threshold_bytes: 50,
            head_bytes: 8,
            tail_bytes: 8,
            keep_recent_turns: 4,
            ..BloatCapConfig::default()
        };
        let renamed = |name: &str| {
            let mut msgs = session(10, 100);
            for m in msgs.iter_mut() {
                if let Some(c) = m.get_mut("content").and_then(Value::as_array_mut) {
                    for b in c {
                        if b.get("type").and_then(Value::as_str) == Some("tool_use") {
                            b["name"] = json!(name);
                        }
                    }
                }
            }
            msgs
        };
        for exempt in ["Agent", "Task", "Edit", "Write", "MultiEdit"] {
            let mut m = renamed(exempt);
            let s = apply(&mut m, &base).unwrap();
            assert_eq!(
                s.stubbed, 0,
                "{exempt} must be exempt from bloat_cap by default"
            );
        }
        for trimmed in ["Bash", "Glob", "WebFetch"] {
            let mut m = renamed(trimmed);
            let s = apply(&mut m, &base).unwrap();
            assert!(
                s.stubbed > 0,
                "{trimmed} must still be trimmed (not exempt)"
            );
        }
    }

    #[test]
    fn deterministic() {
        let mut a = session(10, 100);
        let mut b = session(10, 100);
        apply(&mut a, &cfg(50, 4, &[])).unwrap();
        apply(&mut b, &cfg(50, 4, &[])).unwrap();
        assert_eq!(
            serde_json::to_string(&a).unwrap(),
            serde_json::to_string(&b).unwrap()
        );
    }

    #[test]
    fn trim_respects_utf8_boundaries() {
        // Multi-byte chars around the cut points must not panic or split.
        let s = "é".repeat(100); // 200 bytes
        let out = trim(&s, 9, 9, "trimmed").expect("200-byte string trims smaller"); // 9 lands mid-char → clamps
        assert!(out.contains("[trimwire: trimmed"));
        // Round-trips as valid UTF-8 (String is always valid; just assert non-empty halves).
        assert!(out.starts_with('é'));
        assert!(out.len() < s.len(), "trim must shrink");
    }

    #[test]
    fn trim_never_grows_the_body() {
        // Result just over threshold but smaller than head+tail+marker: the
        // trim would grow it, so it must be skipped (None / not counted).
        assert_eq!(
            trim(&"z".repeat(40), 8, 8, "trimmed"),
            None,
            "trim must not grow"
        );
        // And end-to-end: the gateway must never emit a larger messages body.
        let mut msgs = session(10, 40); // 40-byte results, threshold 30
        let stats = apply(&mut msgs, &cfg(30, 4, &[])).unwrap();
        assert_eq!(stats.stubbed, 0, "no result trimmed (would grow)");
        assert!(
            stats.final_bytes <= stats.original_bytes,
            "body never grows"
        );
        assert!(stats.elided_bytes() >= 0, "savings never negative");
    }

    #[test]
    fn trim_salvages_signal_lines_from_the_middle() {
        // A big log whose error/warning lines sit in the dropped middle: trim
        // keeps them (the signal), not just arbitrary head bytes.
        let mut s = String::new();
        s.push_str(&"head ".repeat(30));
        s.push('\n');
        s.push_str(&"noise\n".repeat(800));
        s.push_str("error[E0001]: the load-bearing error\n");
        s.push_str(&"noise\n".repeat(800));
        s.push_str("warning: a salient warning\n");
        s.push_str(&"noise\n".repeat(800));
        s.push_str(&"tail ".repeat(30));

        let out = trim(&s, 64, 64, "trimmed").expect("a big string should trim");
        assert!(out.len() < s.len(), "still shrinks");
        assert!(
            out.contains("error[E0001]: the load-bearing error"),
            "kept the error"
        );
        assert!(
            out.contains("warning: a salient warning"),
            "kept the warning"
        );
        assert!(out.contains("signal line"), "marker reports the salvage");
    }

    #[test]
    fn exactly_at_threshold_is_untouched() {
        // `<=` threshold: a result exactly at the threshold is left alone;
        // one byte over is trimmed.
        let mut at = session(10, 50);
        assert_eq!(apply(&mut at, &cfg(50, 4, &[])).unwrap().stubbed, 0);
        let mut over = session(10, 51);
        assert_eq!(apply(&mut over, &cfg(50, 4, &[])).unwrap().stubbed, 5);
    }

    /// Build a session with array-content tool_results of a given total serialized size.
    fn array_session(turns: usize, items: usize, item_size: usize) -> Vec<Value> {
        let mut msgs = Vec::new();
        for i in 0..turns {
            let uid = format!("u{i}");
            msgs.push(json!({"role": "user", "content": [{"type": "text", "text": "go"}]}));
            msgs.push(json!({"role": "assistant", "content": [
                {"type": "tool_use", "id": uid, "name": "Bash", "input": {"command": format!("c{i}")}}
            ]}));
            // Array content: a list of text blocks.
            let arr: Vec<Value> = (0..items)
                .map(|j| json!({"type": "text", "text": "z".repeat(item_size + j)}))
                .collect();
            msgs.push(json!({"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": uid, "content": arr}
            ]}));
        }
        msgs
    }

    #[test]
    fn array_content_trimmed_when_over_threshold() {
        // 10 turns, each array result > threshold_bytes.
        let mut msgs = array_session(10, 3, 200); // ~600+ bytes per array result
        let stats = apply(&mut msgs, &cfg(100, 4, &[])).unwrap();
        // 5 old turns should be salvaged (same cutoff logic as string content).
        assert_eq!(stats.stubbed, 5, "5 old array results should be salvaged");
        assert!(stats.elided_bytes() > 0);
        // §13C: the result STAYS AN ARRAY (structure + small blocks + images kept);
        // its bulky text blocks are trimmed in place (signal-preserving), NOT
        // total-erased to a single string marker.
        let result_content = &msgs[2]["content"][0]["content"];
        let arr = result_content
            .as_array()
            .expect("array structure preserved, not erased to a string");
        assert_eq!(arr.len(), 3, "all blocks kept");
        assert!(
            arr[0]["text"]
                .as_str()
                .unwrap()
                .contains("[trimwire: trimmed"),
            "bulky text block trimmed in place: {:?}",
            arr[0]["text"]
        );
        PairingIndex::build(&msgs).validate().unwrap();
    }

    #[test]
    fn array_pure_nontext_falls_back_to_total_erase() {
        // An array with NO text blocks (e.g. a big base64 image) over threshold has
        // nothing to salvage → it is total-erased to the array marker (still caps it).
        let big_b64 = "A".repeat(2000);
        let mut msgs = vec![
            json!({"role": "user", "content": [{"type": "text", "text": "go"}]}),
            json!({"role": "assistant", "content": [
                {"type": "tool_use", "id": "img", "name": "Bash", "input": {"command": "shot"}}
            ]}),
            json!({"role": "user", "content": [{"type": "tool_result", "tool_use_id": "img",
                "content": [{"type": "image", "source": {"type": "base64",
                    "media_type": "image/png", "data": big_b64}}]}]}),
        ];
        for i in 0..6 {
            let uid = format!("t{i}");
            msgs.push(json!({"role": "assistant", "content": [
                {"type": "tool_use", "id": uid, "name": "Bash", "input": {"command": format!("e{i}")}}
            ]}));
            msgs.push(json!({"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": uid, "content": "ok"}
            ]}));
        }
        let stats = apply(&mut msgs, &cfg(100, 4, &[])).unwrap();
        assert!(
            stats.stubbed >= 1,
            "old pure-image array should be total-erased"
        );
        let marker = msgs[2]["content"][0]["content"]
            .as_str()
            .expect("pure non-text array erased to a string marker");
        assert!(marker.contains("[trimwire: array content elided"));
        PairingIndex::build(&msgs).validate().unwrap();
    }

    #[test]
    fn array_content_under_threshold_untouched() {
        let mut msgs = array_session(10, 1, 10); // tiny arrays, well under threshold
        let stats = apply(&mut msgs, &cfg(10_000, 4, &[])).unwrap();
        assert_eq!(stats.stubbed, 0);
    }

    #[test]
    fn array_content_idempotent() {
        // Re-applying to an already-elided array must count zero changes.
        let mut msgs = array_session(10, 3, 200);
        let first = apply(&mut msgs, &cfg(100, 4, &[])).unwrap();
        assert!(first.stubbed > 0, "first pass must elide");
        let second = apply(&mut msgs, &cfg(100, 4, &[])).unwrap();
        assert_eq!(second.stubbed, 0, "second pass must be a no-op");
    }

    // ---- Fix #1: age-gated Read exemption (the "Read coverage gap") ----

    /// A session of `tools.len()` turns; turn `i` runs `tools[i]` with a `size`-byte
    /// result. Each tool_use carries an `input.file_path` so the paired-name lookup
    /// resolves (mirrors a real Read/Write call shape).
    fn named_session(tools: &[&str], size: usize) -> Vec<Value> {
        let mut msgs = Vec::new();
        for (i, tool) in tools.iter().enumerate() {
            let uid = format!("u{i}");
            msgs.push(json!({"role": "user", "content": [{"type": "text", "text": "go"}]}));
            msgs.push(json!({"role": "assistant", "content": [
                {"type": "tool_use", "id": uid, "name": *tool, "input": {"file_path": format!("/f{i}")}}
            ]}));
            msgs.push(json!({"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": uid, "content": "z".repeat(size)}
            ]}));
        }
        msgs
    }

    /// The shipped-default age-gate shape: authoring tools NEVER trimmed,
    /// `Read` exempt only while recent.
    fn age_gate_cfg(threshold: usize, keep: usize) -> BloatCapConfig {
        let mut c = cfg(threshold, keep, &["Edit", "Write", "MultiEdit", "Task"]);
        c.exempt_recent_only_tools = vec!["Read".to_owned()];
        c
    }

    #[test]
    fn age_gated_read_trims_old_keeps_recent() {
        // 10 Read turns, keep_recent 2 → old Read results trimmed, the most-recent
        // ones left fully intact (a just-read file may be in active use).
        let mut msgs = named_session(&["Read"; 10], 100);
        let stats = apply(&mut msgs, &age_gate_cfg(50, 2)).unwrap();
        // 10 turns, keep 2: the result lags its tool_use by one message, so turns
        // 0..=6 are old (7 trimmed) and turns 7,8,9 stay recent (same cutoff math
        // as `trims_old_oversized_results_keeps_recent`: N - keep - 1 trimmed).
        assert_eq!(
            stats.stubbed, 7,
            "exactly the 7 old Read results are trimmed"
        );
        assert!(
            msgs[2]["content"][0]["content"]
                .as_str()
                .unwrap()
                .contains("[trimwire: trimmed"),
            "the oldest Read result is trimmed"
        );
        // BOTH of the last two reads (turns 8 and 9) are recent → fully intact.
        assert_eq!(
            msgs[26]["content"][0]["content"].as_str().unwrap().len(),
            100,
            "the second-most-recent Read result is untouched"
        );
        assert_eq!(
            msgs[29]["content"][0]["content"].as_str().unwrap().len(),
            100,
            "the most-recent Read result is untouched (recent-only exemption)"
        );
        PairingIndex::build(&msgs).validate().unwrap();
    }

    #[test]
    fn age_gated_read_recent_within_keep_is_intact() {
        // keep_recent large enough that EVERY Read turn is recent → none trimmed,
        // exactly as before the fix (recent reads stay protected).
        let mut msgs = named_session(&["Read"; 4], 100);
        let stats = apply(&mut msgs, &age_gate_cfg(50, 8)).unwrap();
        assert_eq!(stats.stubbed, 0, "all-recent Reads must remain intact");
        for m in &msgs {
            if let Some(c) = m["content"][0].get("content").and_then(Value::as_str) {
                if m["content"][0]["type"] == "tool_result" {
                    assert_eq!(c.len(), 100, "recent Read result untouched");
                }
            }
        }
    }

    #[test]
    fn authoring_tools_exempt_at_every_age() {
        // Write/Edit/MultiEdit/Task results are load-bearing → NEVER trimmed, even
        // when old and oversized (eliding them corrupts real sessions, §13A).
        for tool in ["Write", "Edit", "MultiEdit", "Task"] {
            let mut msgs = named_session(&[tool; 10], 100);
            let stats = apply(&mut msgs, &age_gate_cfg(50, 2)).unwrap();
            assert_eq!(stats.stubbed, 0, "{tool} results must never be trimmed");
        }
    }

    #[test]
    fn age_gated_read_array_content_trimmed_when_old() {
        // An OLD Read whose tool_result content is an ARRAY with a bulky text block
        // (e.g. structured output) is salvaged via the array path; a RECENT one is
        // left intact.
        let big = "z".repeat(2000);
        let mk = |id: &str| {
            vec![
                json!({"role":"user","content":[{"type":"text","text":"go"}]}),
                json!({"role":"assistant","content":[{"type":"tool_use","id":id,"name":"Read","input":{"file_path":"/f"}}]}),
                json!({"role":"user","content":[{"type":"tool_result","tool_use_id":id,"content":[
                    {"type":"text","text": big.clone()}
                ]}]}),
            ]
        };
        let mut msgs = Vec::new();
        msgs.extend(mk("old")); // turn 0 — will age out
        // Pad with recent Bash turns so the array Read is OLD (outside keep=2).
        for i in 0..6 {
            let id = format!("b{i}");
            msgs.push(json!({"role":"user","content":[{"type":"text","text":"go"}]}));
            msgs.push(json!({"role":"assistant","content":[{"type":"tool_use","id":id,"name":"Bash","input":{"command":"c"}}]}));
            msgs.push(json!({"role":"user","content":[{"type":"tool_result","tool_use_id":id,"content":"ok"}]}));
        }
        let stats = apply(&mut msgs, &age_gate_cfg(100, 2)).unwrap();
        assert!(stats.stubbed >= 1, "old array-content Read must be trimmed");
        let inner = msgs[2]["content"][0]["content"][0]["text"]
            .as_str()
            .unwrap();
        assert!(inner.len() < 2000, "the bulky text block shrank");
        assert!(
            inner.contains("[trimwire: trimmed"),
            "array path trimmed in place"
        );
        PairingIndex::build(&msgs).validate().unwrap();
    }

    #[test]
    fn age_gated_read_idempotent_no_orphans() {
        // Trimming OLD Reads is idempotent (re-run changes nothing) and never
        // orphans a tool_use/tool_result pair (string + array shapes).
        let mut msgs = named_session(&["Read"; 10], 100);
        let first = apply(&mut msgs, &age_gate_cfg(50, 2)).unwrap();
        assert!(first.stubbed > 0, "first pass trims old Reads");
        let second = apply(&mut msgs, &age_gate_cfg(50, 2)).unwrap();
        assert_eq!(second.stubbed, 0, "idempotent — already trimmed");
        PairingIndex::build(&msgs).validate().unwrap();
    }

    #[test]
    fn is_already_cleared_skips_cc_compact_marker() {
        // A result with CC's own compact marker must not be re-elided by bloat_cap.
        let mut msgs = Vec::new();
        let uid = "u0";
        msgs.push(json!({"role": "user", "content": [{"type": "text", "text": "go"}]}));
        msgs.push(json!({"role": "assistant", "content": [
            {"type": "tool_use", "id": uid, "name": "Bash", "input": {"command": "c"}}
        ]}));
        msgs.push(json!({"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": uid,
             "content": "[Old tool result content cleared]"}
        ]}));
        // Add recent turns to make the CC-cleared one "old".
        for i in 1..8 {
            let id = format!("u{i}");
            msgs.push(json!({"role": "user", "content": [{"type": "text", "text": "go"}]}));
            msgs.push(json!({"role": "assistant", "content": [
                {"type": "tool_use", "id": id, "name": "Bash", "input": {"command": "c"}}
            ]}));
            msgs.push(json!({"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": id, "content": "short"}
            ]}));
        }
        // Threshold of 10 bytes — the CC marker (35 bytes) is over the threshold,
        // but bloat_cap must skip it.
        let stats = apply(&mut msgs, &cfg(10, 4, &[])).unwrap();
        assert_eq!(stats.stubbed, 0, "CC's own marker must not be re-elided");
        assert_eq!(
            msgs[2]["content"][0]["content"],
            json!("[Old tool result content cleared]"),
            "CC marker must be preserved verbatim"
        );
    }
}
