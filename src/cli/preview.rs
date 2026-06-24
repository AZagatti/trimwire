//! `trimwire preview <session.jsonl>` — a read-only what-if: reconstruct the
//! `messages[]` array from an on-disk Claude Code transcript and report what the
//! pruning strategies *would* trim, without touching the file or the network.
//!
//! The transcript shape differs from the wire shape: each line is a record
//! `{type, uuid, message:{role,content}, isSidechain, ...}` (plus non-message
//! records like `summary`, `file-history-snapshot`, `system`). We keep only the
//! `user`/`assistant` records, unwrap their `.message`, and — by default — drop
//! `isSidechain` turns (sub-agent transcripts are interleaved into the same file
//! but never sent as part of the parent's `messages[]`; including them would
//! both over-count and orphan tool pairs). The reconstruction is then validated
//! through the same [`PairingIndex`] gate the gateway uses before we measure.

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use serde_json::Value;

use trimwire::config::{
    Config, DEFAULT_PROFILE, PROFILES, SummarizerProviderConfig, profile_baseline,
};
use trimwire::pairing::PairingIndex;
use trimwire::strategies;
use trimwire::summarizer;

use super::render;
use super::stats::human_bytes;

/// Outcome of reconstructing `messages[]` from a transcript file.
#[derive(Debug)]
struct Reconstructed {
    messages: Vec<Value>,
    /// `user`/`assistant` records seen (before sidechain filtering).
    turn_records: usize,
    /// `isSidechain` turns skipped (0 when `--include-sidechains`).
    skipped_sidechain: usize,
}

/// Print the pre-call safety warning for an API summarizer engine and return
/// whether the caller should proceed (i.e. `yes=true` or `local`/non-API engine).
///
/// Always returns `false` when `yes` is false and the engine is API-backed —
/// the caller must skip the summarizer call. When `yes` is true the warning is
/// still printed so the user can see exactly what is about to be charged.
/// For the `local` engine (ollama, no key, no paid call) this is a no-op and
/// always returns `true`.
fn api_cost_gate(engine: &str, providers: &[SummarizerProviderConfig], yes: bool) -> bool {
    // "model-free" and "local" are never API — no gate needed.
    if engine == "model-free" || engine == "local" {
        return true;
    }
    let Some(provider) = providers.iter().find(|p| p.id == engine) else {
        // Unknown engine id: no provider to charge; let the cascade error naturally.
        return true;
    };
    eprintln!(
        "{w}  API SUMMARIZER PREVIEW — REAL MONEY WARNING\n\
         \x20  This makes ONE real API call to {} using model {:?}.\n\
         \x20  Charged to your {} key (NOT your Anthropic subscription).\n\
         \x20  The preview is directional: a single-slice, one-shot call — the live\n\
         \x20  gateway runs the accumulator (multi-turn replay) which this doesn't model.",
        if provider.base_url.is_empty() {
            "(provider default URL)".to_owned()
        } else {
            provider.base_url.clone()
        },
        provider.model,
        provider.api_key_env,
        w = render::warn(),
    );
    if !yes {
        eprintln!(
            "\n  DRY RUN — no API call made.\n\
             \x20  The deterministic preview above is still shown.\n\
             \x20  To also estimate the summarizer's contribution: trimwire preview --with-summarizer --yes"
        );
    }
    yes
}

/// `trimwire preview` — estimate pruning savings for a recorded session.
///
/// In `--json` mode the error cases (no target, empty/invalid session) are
/// emitted as a JSON object `{"error": "..."}` on stdout with a non-zero exit,
/// so a `--json` consumer always gets parseable output rather than a plain-text
/// anyhow message on stderr.
pub fn preview(
    path: Option<PathBuf>,
    last: bool,
    profile: Option<String>,
    include_sidechains: bool,
    json: bool,
    with_summarizer: bool,
    yes: bool,
) -> Result<()> {
    if !json {
        return preview_inner(
            path,
            last,
            profile,
            include_sidechains,
            json,
            with_summarizer,
            yes,
        );
    }
    match preview_inner(
        path,
        last,
        profile,
        include_sidechains,
        json,
        with_summarizer,
        yes,
    ) {
        Ok(()) => Ok(()),
        Err(e) => {
            // `--json` callers must get a parseable error on stdout, not anyhow's
            // plain-text stderr blast. `process::exit(1)` is the CLI's established
            // way to set an exit code without that blast (see `cli::mod::doctor`,
            // `cli::update`); flush first so the JSON line isn't lost when stdout
            // is piped (`process::exit` skips destructors). `{e:#}` includes the
            // anyhow context chain ("preview <path>: …").
            use std::io::Write;
            let v = serde_json::json!({ "error": format!("{e:#}") });
            println!(
                "{}",
                serde_json::to_string_pretty(&v)
                    .unwrap_or_else(|_| "{\"error\":\"preview failed\"}".to_owned())
            );
            let _ = std::io::stdout().flush();
            std::process::exit(1);
        }
    }
}

fn preview_inner(
    path: Option<PathBuf>,
    last: bool,
    profile: Option<String>,
    include_sidechains: bool,
    json: bool,
    with_summarizer: bool,
    yes: bool,
) -> Result<()> {
    let path = resolve_target(path, last)?;
    let raw =
        std::fs::read(&path).with_context(|| format!("read transcript {}", path.display()))?;

    // Sub-agent transcripts live in a sibling `<uuid>/subagents/*.jsonl` dir, not
    // in this file. They are NOT merged into the preview (they would break
    // messages[] pairing), but we surface their count + on-disk size so the
    // report never implies a session has no sub-agent data when it does.
    let sidechains = trimwire::sweep::sidechain_files_for(&path);
    let sidechain_bytes: u64 = sidechains
        .iter()
        .filter_map(|p| std::fs::metadata(p).ok().map(|m| m.len()))
        .sum();

    let rc = reconstruct_validated(&raw, include_sidechains)
        .with_context(|| format!("preview {}", path.display()))?;

    let profile = resolve_profile(profile.as_deref());
    // `profile_baseline` gives the profile's deterministic strategies but the
    // DEFAULT (model-free) summarizer. For --with-summarizer we need the user's
    // REAL summarizer config (engine / providers / local) from the loaded Config
    // (global file + project + env) — overlay it onto the profile baseline so the
    // deterministic measurement still reflects --profile.
    let mut cfg = profile_baseline(&profile);
    if with_summarizer {
        if let Ok(loaded) = Config::load() {
            cfg.summarizer = loaded.summarizer;
        }
    }

    let in_bytes = serde_json::to_vec(&rc.messages)
        .map(|v| v.len())
        .unwrap_or(0);
    let mut pruned = rc.messages.clone();
    let fired = strategies::run(&mut pruned, &cfg).context("run strategies")?;
    let out_bytes = serde_json::to_vec(&pruned).map(|v| v.len()).unwrap_or(0);
    let saved = in_bytes as i64 - out_bytes as i64;
    let reduction = if in_bytes == 0 {
        0.0
    } else {
        saved as f64 / in_bytes as f64 * 100.0
    };

    // Bytes each strategy actually elided + how many blocks it touched (≥0;
    // ignore strategies that fired but stubbed nothing). Ordered most-trimmed
    // first for the human report. The item count is the transparency half: not
    // just "how many bytes" but "how many results" the strategy acted on.
    let mut per_strategy: Vec<(&str, i64, usize)> = fired
        .iter()
        .filter(|(_, s)| s.stubbed > 0)
        .map(|(name, s)| (*name, s.elided_bytes().max(0), s.stubbed))
        .collect();
    per_strategy.sort_by_key(|(_, b, _)| std::cmp::Reverse(*b));

    // Optionally run the summarizer preview (async; builds a local runtime).
    // We compute this BEFORE rendering so JSON output can include it in one shot.
    let summarizer_result: Option<summarizer::PreviewSummary> = if with_summarizer {
        let engine = cfg.summarizer.engine.as_str();
        if engine == "model-free" {
            // Nothing to add — print a note after the deterministic section.
            None
        } else {
            // API cost gate: if the primary engine resolves to a paid provider,
            // require --yes before making the call (mirrors benchmark.rs).
            let proceed = api_cost_gate(engine, &cfg.summarizer.providers, yes);
            if proceed {
                let rt = tokio::runtime::Builder::new_multi_thread()
                    .enable_all()
                    .build()
                    .context("build tokio runtime for summarizer preview")?;
                rt.block_on(summarizer::preview_summary(&rc.messages, &cfg))
                    .unwrap_or(None)
            } else {
                // Cost gate blocked without --yes: skip the call; deterministic
                // section still renders normally below.
                None
            }
        }
    } else {
        None
    };

    if json {
        let summ_json = summarizer_result.as_ref().map(|ps| {
            let additional = ps.slice_before as i64 - ps.slice_after as i64;
            let reduction_pct = if ps.slice_before == 0 {
                0.0_f64
            } else {
                additional as f64 / ps.slice_before as f64 * 100.0
            };
            let slice_msgs = ps.end.saturating_sub(ps.start);
            let coverage_pct = if rc.messages.is_empty() {
                0.0
            } else {
                slice_msgs as f64 / rc.messages.len() as f64 * 100.0
            };
            serde_json::json!({
                "slice_start": ps.start,
                "slice_end": ps.end,
                "slice_message_count": slice_msgs,
                "total_messages": rc.messages.len(),
                "slice_coverage_pct": (coverage_pct * 10.0).round() / 10.0,
                "slice_before_bytes": ps.slice_before,
                "slice_after_bytes": ps.slice_after,
                "additional_bytes_saved": additional,
                "slice_reduction_pct": (reduction_pct * 10.0).round() / 10.0,
                "engine_kind": ps.engine_kind,
                "directional": true,
            })
        });
        let v = serde_json::json!({
            "path": path.display().to_string(),
            "profile": profile,
            "messages": rc.messages.len(),
            "turn_records": rc.turn_records,
            "skipped_sidechain": rc.skipped_sidechain,
            "subagent_transcripts": sidechains.len(),
            "subagent_transcript_bytes": sidechain_bytes,
            "in_bytes": in_bytes,
            "out_bytes": out_bytes,
            "bytes_saved": saved,
            "reduction_pct": (reduction * 10.0).round() / 10.0,
            "per_strategy": per_strategy
                .iter()
                .map(|(n, b, items)| serde_json::json!({
                    "strategy": n,
                    "bytes": b,
                    "items": items,
                }))
                .collect::<Vec<_>>(),
            "estimate": true,
            "summarizer": summ_json,
        });
        println!(
            "{}",
            serde_json::to_string_pretty(&v).context("render JSON")?
        );
        return Ok(());
    }

    println!("{} trimwire preview — {}", render::header(), path.display());
    print!(
        "  reconstructed {} message{} under the {profile} profile",
        rc.messages.len(),
        if rc.messages.len() == 1 { "" } else { "s" },
    );
    if rc.skipped_sidechain > 0 {
        println!(
            "  ({} sub-agent turn{} skipped; --include-sidechains to count them)",
            rc.skipped_sidechain,
            if rc.skipped_sidechain == 1 { "" } else { "s" },
        );
    } else {
        println!();
    }
    println!(
        "  messages[] size: {} → {}   {}  {:.0}% lighter",
        human_bytes(in_bytes as i64),
        human_bytes(out_bytes as i64),
        render::gauge(reduction),
        reduction,
    );

    if per_strategy.is_empty() {
        println!("  nothing to trim — this session is already lean for this profile.");
    } else {
        let max_b = per_strategy
            .iter()
            .map(|(_, b, _)| *b)
            .max()
            .unwrap_or(0)
            .max(1);
        println!("  would trim (bar = bytes):");
        for (name, b, items) in &per_strategy {
            let bar =
                render::bar_fill().repeat(((*b as f64 / max_b as f64) * 12.0).round() as usize);
            println!(
                "    {name:<20} {bar} {} ({items} result{})",
                human_bytes(*b),
                if *items == 1 { "" } else { "s" },
            );
        }
    }

    println!(
        "  → estimate: messages[] bytes are exact, but the live request also carries a\n\
         \x20   system prompt + tool schemas (not in the transcript), so the % is of\n\
         \x20   messages[] only. Read-only: this never writes the session."
    );

    if !sidechains.is_empty() {
        println!(
            "  → {} sub-agent transcript{} ({}) on disk under this session's subagents/\n\
             \x20   dir — separate files, not part of this preview; clean them with `trimwire sweep all`.",
            sidechains.len(),
            if sidechains.len() == 1 { "" } else { "s" },
            human_bytes(sidechain_bytes as i64),
        );
    }

    if with_summarizer {
        let engine = cfg.summarizer.engine.as_str();
        if engine == "model-free" {
            println!(
                "  → summarizer is model-free / off — nothing to add;\n\
                 \x20   `trimwire summarizer setup` to configure one."
            );
        } else if let Some(ps) = &summarizer_result {
            let additional = ps.slice_before as i64 - ps.slice_after as i64;
            let combined_saved = saved + additional;
            let combined_reduction = if in_bytes == 0 {
                0.0
            } else {
                combined_saved as f64 / in_bytes as f64 * 100.0
            };
            let slice_reduction = if ps.slice_before == 0 {
                0.0_f64
            } else {
                additional as f64 / ps.slice_before as f64 * 100.0
            };
            let slice_msgs = ps.end.saturating_sub(ps.start);
            let coverage_pct = if rc.messages.is_empty() {
                0.0
            } else {
                slice_msgs as f64 / rc.messages.len() as f64 * 100.0
            };
            println!(
                "\n  summarizer ({} engine, turns {}–{} — {} of {} messages, ~{:.0}% of the session):",
                ps.engine_kind,
                ps.start,
                ps.end,
                slice_msgs,
                rc.messages.len(),
                coverage_pct,
            );
            println!(
                "    slice {} → {}   {}  {:.0}% on this slice only",
                human_bytes(ps.slice_before as i64),
                human_bytes(ps.slice_after as i64),
                render::gauge(slice_reduction),
                slice_reduction,
            );
            println!(
                "    combined (deterministic + summarizer): {} total saved   {:.0}% of messages[]",
                human_bytes(combined_saved),
                combined_reduction,
            );
            println!(
                "  → directional UPPER BOUND: this previews ONE summary over the WIDEST old\n\
                 \x20   slice (the ~{:.0}% above — recent turns are never summarized). The live\n\
                 \x20   gateway summarizes INCREMENTALLY in small (~38 KB) accumulator segments,\n\
                 \x20   so per-turn compaction is much smaller and spreads across the session;\n\
                 \x20   non-deterministic (model output varies).",
                coverage_pct,
            );
        } else {
            println!(
                "  → summarizer ({engine} engine): no eligible slice for this session,\n\
                 \x20   or the summary was rejected (model-free pruning was already smaller)."
            );
        }
    } else {
        println!(
            "  → deterministic strategies only — pass --with-summarizer to also estimate\n\
             \x20   the optional summarizer's contribution (requires a configured engine)."
        );
    }
    Ok(())
}

/// Resolve which transcript to preview: an explicit path, or — with `--last` —
/// the most recently modified session under the sessions root. `clap` already
/// guarantees the two are mutually exclusive; this just turns "neither given"
/// and "no sessions found" into friendly errors.
fn resolve_target(path: Option<PathBuf>, last: bool) -> Result<PathBuf> {
    if let Some(p) = path {
        return Ok(p);
    }
    if !last {
        bail!(
            "give a transcript path, or pass --last to preview the most recent session \
             (see `trimwire recall` for recent sessions)"
        );
    }
    most_recent_session().context("--last: could not find a recent session")
}

/// The session transcript with the newest mtime under the sessions root. `--last`
/// reuses the same discovery as `sweep` so users never hunt for a path.
///
/// Sub-agent sidechain transcripts (a `subagents/` subdirectory) would fail
/// reconstruction with confusing pairing errors — `session_files()` already
/// excludes them, so `--last` only ever picks a real top-level session.
fn most_recent_session() -> Result<PathBuf> {
    let newest = trimwire::sweep::session_files()
        .into_iter()
        .filter_map(|p| {
            let mtime = std::fs::metadata(&p).and_then(|m| m.modified()).ok()?;
            Some((mtime, p))
        })
        .max_by_key(|(mtime, _)| *mtime)
        .map(|(_, p)| p);
    match newest {
        Some(p) => {
            eprintln!("{} --last → {}", render::bullet(), p.display());
            Ok(p)
        }
        None => bail!(
            "no session transcripts found under ~/.claude/projects (run Claude Code through \
             trimwire first, or pass an explicit path)"
        ),
    }
}

/// Reconstruct `messages[]` and run it through the same [`PairingIndex`] gate
/// the gateway applies before mutating — refusing (rather than reporting a bogus
/// number) on an empty reconstruction or an orphaned tool pair.
fn reconstruct_validated(raw: &[u8], include_sidechains: bool) -> Result<Reconstructed> {
    let rc = reconstruct(raw, include_sidechains);
    if rc.messages.is_empty() {
        bail!("no user/assistant turns found — is this a Claude Code session .jsonl?");
    }
    // The usual cause of an orphaned tool_result is a sub-agent turn pulled in
    // out of context; suggest dropping sidechains when they're the likely cause.
    if let Err(e) = PairingIndex::build(&rc.messages).validate() {
        bail!(
            "reconstructed messages[] failed pairing validation ({e}); the transcript may \
             interleave sub-agent turns out of order{}",
            if include_sidechains {
                " — try again without --include-sidechains"
            } else {
                ""
            }
        );
    }
    Ok(rc)
}

/// Reconstruct `messages[]` from a transcript byte buffer. Skips non-message
/// records (`summary`, `file-history-snapshot`, `system`, …), unwraps `.message`
/// from `user`/`assistant` records, and — unless `include_sidechains` — drops
/// `isSidechain` turns. Malformed lines are skipped, never fatal.
fn reconstruct(raw: &[u8], include_sidechains: bool) -> Reconstructed {
    let mut messages = Vec::new();
    let mut turn_records = 0;
    let mut skipped_sidechain = 0;

    for line in raw.split(|&b| b == b'\n') {
        if line.is_empty() {
            continue;
        }
        let Ok(rec) = serde_json::from_slice::<Value>(line) else {
            continue;
        };
        match rec.get("type").and_then(Value::as_str) {
            Some("user") | Some("assistant") => {}
            _ => continue,
        }
        // A real turn must carry a `.message` with both role and content;
        // anything else (e.g. a metadata record typed user/assistant) is not a
        // turn and is not counted as one.
        let Some(msg) = rec.get("message") else {
            continue;
        };
        if msg.get("role").is_none() || msg.get("content").is_none() {
            continue;
        }
        turn_records += 1;
        if !include_sidechains && rec.get("isSidechain").and_then(Value::as_bool) == Some(true) {
            skipped_sidechain += 1;
            continue;
        }
        messages.push(msg.clone());
    }

    Reconstructed {
        messages,
        turn_records,
        skipped_sidechain,
    }
}

/// Validate a requested profile name, warning + falling back to the default for
/// an unknown one (mirrors `Config::load`'s behaviour).
fn resolve_profile(requested: Option<&str>) -> String {
    match requested {
        None => DEFAULT_PROFILE.to_owned(),
        Some(p) if PROFILES.contains(&p) => p.to_owned(),
        Some(p) => {
            let valid = PROFILES.join(", ");
            eprintln!(
                "{} unknown profile {p:?} — using {DEFAULT_PROFILE}. Valid values: {valid}",
                render::warn()
            );
            DEFAULT_PROFILE.to_owned()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = include_str!("../../tests/fixtures/transcript_basic.jsonl");

    #[test]
    fn reconstructs_main_thread_dropping_sidechains() {
        let rc = reconstruct(FIXTURE.as_bytes(), false);
        // summary + file-history-snapshot + system are skipped; the two
        // isSidechain turns (u3/a2) are dropped → [u1, a1, u2, a3].
        assert_eq!(rc.messages.len(), 4, "main thread = u1,a1,u2,a3");
        assert_eq!(rc.skipped_sidechain, 2, "u3 + a2 are sub-agent turns");
        // turn_records counts every well-formed user/assistant turn (incl.
        // sidechains); the invariant turn_records == messages + skipped holds.
        assert_eq!(rc.turn_records, 6);
        assert_eq!(rc.turn_records, rc.messages.len() + rc.skipped_sidechain);

        let roles: Vec<&str> = rc
            .messages
            .iter()
            .map(|m| m["role"].as_str().unwrap())
            .collect();
        assert_eq!(roles, ["user", "assistant", "user", "assistant"]);

        // The reconstruction must pass the same gate the gateway uses.
        assert!(
            PairingIndex::build(&rc.messages).validate().is_ok(),
            "tool pairs (toolu_1/toolu_2) are intact in the main thread"
        );
    }

    #[test]
    fn including_sidechains_keeps_all_turns() {
        let rc = reconstruct(FIXTURE.as_bytes(), true);
        assert_eq!(rc.messages.len(), 6, "u1,a1,u2,u3,a2,a3");
        assert_eq!(rc.skipped_sidechain, 0);
    }

    #[test]
    fn malformed_and_empty_lines_are_skipped() {
        let raw =
            b"not json\n\n{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"hi\"}}\n";
        let rc = reconstruct(raw, false);
        assert_eq!(rc.messages.len(), 1);
        assert_eq!(rc.messages[0]["content"], "hi");
    }

    #[test]
    fn metadata_typed_turn_is_not_counted_or_reconstructed() {
        // A record typed `user` but lacking a `.message` (a metadata stub) must
        // neither reach messages[] nor inflate turn_records.
        let raw = concat!(
            r#"{"type":"user","uuid":"meta","toolUseResult":{"x":1}}"#,
            "\n",
            r#"{"type":"user","message":{"role":"user","content":"hi"}}"#,
            "\n",
        );
        let rc = reconstruct(raw.as_bytes(), false);
        assert_eq!(rc.messages.len(), 1);
        assert_eq!(rc.turn_records, 1, "the metadata stub is not a turn");
    }

    #[test]
    fn empty_reconstruction_refuses_to_measure() {
        // Only non-turn records → nothing to measure → error, not a bogus 0%.
        let raw = br#"{"type":"summary","summary":"x"}"#;
        let err = reconstruct_validated(raw, false).unwrap_err();
        assert!(err.to_string().contains("no user/assistant turns"));
    }

    #[test]
    fn orphan_tool_result_refuses_to_measure() {
        // A tool_result whose tool_use is absent: the gateway's pairing gate
        // would reject it, so preview must too (the feature's safety net) rather
        // than report savings the real gateway would never produce.
        let raw = concat!(
            r#"{"type":"user","message":{"role":"user","content":"go"}}"#,
            "\n",
            r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_ghost","content":"x"}]}}"#,
            "\n",
        );
        let err = reconstruct_validated(raw.as_bytes(), false).unwrap_err();
        assert!(
            err.to_string().contains("pairing validation"),
            "orphaned tool_result must fail the gate: {err}"
        );
    }

    #[test]
    fn fixture_reconstructs_validated_to_main_thread() {
        let rc = reconstruct_validated(FIXTURE.as_bytes(), false).unwrap();
        assert_eq!(rc.messages.len(), 4);
    }

    #[test]
    fn resolve_profile_falls_back_for_unknown() {
        // Valid profiles pass through unchanged.
        assert_eq!(resolve_profile(Some("default")), "default");
        assert_eq!(resolve_profile(Some("gentle")), "gentle");
        // None → DEFAULT_PROFILE.
        assert_eq!(resolve_profile(None), DEFAULT_PROFILE);
        // Any unknown name falls back to the default.
        assert_eq!(resolve_profile(Some("bogus")), DEFAULT_PROFILE);
    }
}
