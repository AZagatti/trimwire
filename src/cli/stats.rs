//! `trimwire stats` — print the savings ledger report.

use anyhow::{Context, Result};

use super::render;
use trimwire::config::Config;
use trimwire::ledger::{self, Ledger, SessionReport};

/// Print the savings ledger report (per-day savings, per-strategy counts, and
/// the cache-prefix stability ratio). With `session`, print a per-session
/// per-model cache/token breakdown (pass `"last"` for the most recent session,
/// or a session-id from `x-claude-code-session-id`). With `since`/`until`
/// (UTC `YYYY-MM-DD`), restrict the all-time report to a date window.
pub fn stats(
    json: bool,
    quiet: bool,
    verbose: bool,
    session: Option<String>,
    since: Option<String>,
    until: Option<String>,
) -> Result<()> {
    let config = Config::load().context("load config")?;
    if !config.ledger.enabled {
        if json {
            println!(
                "{}",
                serde_json::json!({"available": false, "reason": "ledger disabled"})
            );
        } else {
            println!("ledger is disabled in config ([ledger] enabled = false); nothing to report.");
        }
        return Ok(());
    }
    // Common first-run case: the daemon hasn't created the DB yet. Treat it as
    // "nothing to report" (exit 0) rather than a raw SQLite "can't open" error.
    if !ledger::resolve_path(&config.ledger.db_path).exists() {
        if json {
            println!(
                "{}",
                serde_json::json!({"available": false, "reason": "ledger not created"})
            );
        } else {
            println!(
                "ledger not yet created — run `trimwire run claude` (or `trimwire on`), \
                 use Claude Code a bit, then re-run `trimwire stats`."
            );
        }
        return Ok(());
    }
    // Per-session, per-model report (the cache_read-vs-cache_creation view).
    if let Some(sel) = session {
        let report = Ledger::session_report(&config.ledger.db_path, Some(&sel))
            .context("read per-session ledger report")?;
        return print_session_report(report, &sel, json);
    }

    // Optional UTC date window. Half-open [since 00:00, until+1d 00:00) so
    // `--until` is inclusive of that whole day. Bounds match SQLite's UTC `ts`.
    let since_ts = match &since {
        Some(s) => day_start_utc(s).with_context(|| format!("--since {s:?}"))?,
        None => i64::MIN,
    };
    let until_ts = match &until {
        Some(u) => day_end_utc(u).with_context(|| format!("--until {u:?}"))?,
        None => i64::MAX,
    };
    let windowed = since.is_some() || until.is_some();

    let report = Ledger::report_window(&config.ledger.db_path, since_ts, until_ts)
        .context("read ledger (start `trimwire on` first to populate it)")?;

    let saved = report.bytes_saved();
    let reduction = report.reduction_pct();
    // Tokens of content *removed* (~4 bytes/token). NOT a dollar figure — net
    // cost is non-monotonic under prompt caching (benchmark §5).
    let est_tokens_removed = report.est_tokens_removed();

    // One-line headline for scripts / prompts / a quick glance.
    if quiet {
        let win = if windowed {
            format!(
                " [{}→{}]",
                since.as_deref().unwrap_or("start"),
                until.as_deref().unwrap_or("now")
            )
        } else {
            String::new()
        };
        println!(
            "{} {} → {} {} {:.0}% lighter · {} requests{win}",
            render::header(),
            human_bytes(report.total_in_bytes as i64),
            human_bytes(report.total_out_bytes as i64),
            render::gauge(reduction),
            reduction,
            report.total_requests,
        );
        return Ok(());
    }

    if json {
        let mut v = serde_json::to_value(&report).context("serialize report")?;
        // Stable key so scripts branch on one field across both schemas.
        v["available"] = true.into();
        v["bytes_saved"] = saved.into();
        v["reduction_pct"] = ((reduction * 10.0).round() / 10.0).into();
        v["est_tokens_removed"] = est_tokens_removed.into();
        v["since"] = since
            .clone()
            .map(serde_json::Value::from)
            .unwrap_or(serde_json::Value::Null);
        v["until"] = until
            .clone()
            .map(serde_json::Value::from)
            .unwrap_or(serde_json::Value::Null);
        println!(
            "{}",
            serde_json::to_string_pretty(&v).context("render JSON")?
        );
        return Ok(());
    }

    println!(
        "{} trimwire — {}",
        render::header(),
        report.db_path.display()
    );
    if windowed {
        println!(
            "  window (UTC): {} → {}",
            since.as_deref().unwrap_or("start"),
            until.as_deref().unwrap_or("now"),
        );
    }
    println!(
        "  request size: {} → {}   {}  {:.0}% lighter",
        human_bytes(report.total_in_bytes as i64),
        human_bytes(report.total_out_bytes as i64),
        render::gauge(reduction),
        reduction,
    );
    println!(
        "  {} trimmed over {} requests  (~{} tokens of context removed)",
        human_bytes(report.bytes_saved()),
        report.total_requests,
        human_count(est_tokens_removed),
    );
    println!(
        "  → context-window headroom reclaimed — the reliable win (not a $ figure; \
         net cost varies under prompt caching)."
    );

    if !report.per_strategy.is_empty() {
        println!("  strategies (bar = bytes trimmed; count · bytes):");
        let bytes_of = |n: &str| {
            report
                .per_strategy_bytes
                .iter()
                .find(|(m, _)| m == n)
                .map_or(0, |(_, b)| *b)
        };
        let max_b = report
            .per_strategy_bytes
            .iter()
            .map(|(_, b)| *b)
            .max()
            .unwrap_or(0)
            .max(1);
        for (name, count) in &report.per_strategy {
            let b = bytes_of(name);
            let bar =
                render::bar_fill().repeat(((b as f64 / max_b as f64) * 12.0).round() as usize);
            println!("    {name:<20} {bar} {count}× {}", human_bytes(b));
        }
    }

    let cs = &report.cache_stability;
    println!(
        "  cache-prefix stability: {:.1}% ({}/{} no-op requests kept the prefix unchanged)",
        cs.ratio * 100.0,
        cs.no_strategy_stable,
        cs.no_strategy_total,
    );
    if cs.ratio < 1.0 {
        println!(
            "    {} a no-op request changed the prefix (cache may be thrashing) — run \
             `trimwire doctor`, or try the `gentle` profile if it persists.",
            render::warn()
        );
    }

    // Upstream failures (proxy couldn't reach / timed out on Anthropic). These
    // never produce a normal request row, so they'd be invisible otherwise. Shown
    // in the DEFAULT view ONLY when non-zero — they always matter when present, and
    // stay silent on the happy path.
    if report.upstream_errors + report.upstream_timeouts > 0 {
        println!(
            "  {} upstream failures: {} connection error(s), {} timeout(s) — trimwire couldn't \
             reach Anthropic (check your network / status.anthropic.com)",
            render::warn(),
            report.upstream_errors,
            report.upstream_timeouts,
        );
    }

    // Summarizer activity (only when it ran — engine != model-free). Surfaces
    // whether the summary is being installed, losing to model-free, or ERRORING
    // (every engine down/timeout) — otherwise that's invisible at the default log
    // level. errored > 0 with accepted == 0 means the summarizer is broken.
    let summ_total =
        report.summarizer_accepted + report.summarizer_rejected + report.summarizer_errored;
    if summ_total > 0 {
        let err_pct = report.summarizer_errored as f64 / summ_total as f64 * 100.0;
        let marker = if report.summarizer_errored > 0 && report.summarizer_accepted == 0 {
            render::warn()
        } else {
            render::bullet()
        };
        println!(
            "  {marker} summarizer: {} installed, {} model-free won, {} errored ({:.0}% error)",
            report.summarizer_accepted,
            report.summarizer_rejected,
            report.summarizer_errored,
            err_pct,
        );
    }

    // Summarizer chain collapses (accumulator hit max_summary_segments → REPLACE):
    // the long-session "context-pressure" signal. Shown only when non-zero — it's
    // the cue that the oldest detail is aging out and a checkpoint is worth it.
    if report.summarizer_collapses > 0 {
        println!(
            "  {} summarizer: {} chain collapse(s) — very old context was re-summarized; \
             consider Claude Code /compact or a fresh session + handoff (files stay re-readable)",
            render::warn(),
            report.summarizer_collapses,
        );
    } else if config.summarizer.engine == "model-free" {
        // Discoverability: hint the opt-in summarizer to model-free users.
        println!(
            "  {} summarizer is off — `trimwire summarizer setup` adds model-based \
             compression on top of these savings (optional)",
            render::bullet()
        );
    }

    // Response-side metrics (v3 columns). The DEFAULT view shows just the one
    // figure most people act on — the cache-hit % — and tucks the TTFT / billed
    // breakdown / cache-write / native-clear detail behind `--verbose` so the
    // default `trimwire stats` stays scannable.
    let rm = &report.response_metrics;
    let has_response_metrics = rm.requests_with_ttft > 0
        || rm.total_input_tokens > 0
        || rm.requests_with_applied_edits > 0;
    if has_response_metrics && !verbose {
        if rm.total_input_tokens > 0 {
            let cache_ratio =
                rm.total_cache_read_input_tokens as f64 / rm.total_input_tokens as f64 * 100.0;
            println!(
                "  cache-hit: {:.0}% of input tokens served from cache  ({} input tokens · --verbose for more)",
                cache_ratio,
                human_count(rm.total_input_tokens as i64),
            );
        } else {
            println!(
                "  response metrics recorded — `trimwire stats --verbose` for TTFT + token detail."
            );
        }
    }
    if has_response_metrics && verbose {
        println!("  response instrumentation:");
        if rm.requests_with_ttft > 0 {
            println!(
                "    avg TTFT: {:.1} ms  ({} requests measured)",
                rm.avg_ttft_us / 1000.0,
                rm.requests_with_ttft
            );
        }
        if rm.total_input_tokens > 0 || rm.total_output_tokens > 0 {
            let total_billed = rm.total_input_tokens + rm.total_output_tokens;
            let cache_served = rm.total_cache_read_input_tokens;
            let cache_ratio = if rm.total_input_tokens > 0 {
                cache_served as f64 / rm.total_input_tokens as f64 * 100.0
            } else {
                0.0
            };
            println!(
                "    tokens billed: {} input + {} output  (cache-read: {} = {:.0}%)",
                human_count(rm.total_input_tokens as i64),
                human_count(rm.total_output_tokens as i64),
                human_count(cache_served as i64),
                cache_ratio,
            );
            if rm.total_cache_creation_input_tokens > 0 {
                println!(
                    "    cache writes: {} tokens",
                    human_count(rm.total_cache_creation_input_tokens as i64)
                );
            }
            let _ = total_billed; // suppressed; surfaced via input+output breakdown above
        }
        if rm.requests_with_applied_edits > 0 {
            println!(
                "    Anthropic native cleared: {} thinking-turns, {} tool-uses, {} tokens  ({} requests)",
                human_count(rm.total_applied_edits_cleared_thinking_turns as i64),
                human_count(rm.total_applied_edits_cleared_tool_uses as i64),
                human_count(rm.total_applied_edits_cleared_input_tokens as i64),
                rm.requests_with_applied_edits,
            );
        }
    }

    if !report.per_day.is_empty() {
        let days = if verbose { 14 } else { 7 };
        println!("  by day:");
        for d in report.per_day.iter().take(days) {
            println!(
                "    {}  {:>5} req  saved {}",
                d.day,
                d.requests,
                human_bytes(d.in_bytes as i64 - d.out_bytes as i64),
            );
        }
        if !verbose && report.per_day.len() > days {
            println!(
                "    … {} earlier day(s) — --verbose to show",
                report.per_day.len() - days
            );
        }
    }
    Ok(())
}

/// Render the per-session, per-model cache/token report.
fn print_session_report(report: Option<SessionReport>, requested: &str, json: bool) -> Result<()> {
    let Some(report) = report else {
        if json {
            println!(
                "{}",
                serde_json::json!({"available": false, "reason": "no rows for session", "requested": requested})
            );
        } else if requested == "last" {
            println!(
                "no sessions recorded yet — run a Claude Code session through trimwire first (`trimwire on`)."
            );
        } else {
            println!(
                "no ledger rows for session {requested:?} (ids come from x-claude-code-session-id)."
            );
        }
        return Ok(());
    };

    if json {
        let mut v = serde_json::to_value(&report).context("serialize session report")?;
        v["available"] = true.into();
        // Derived, per-model cache-hit % (kept out of the struct: it's a view).
        if let Some(arr) = v["per_model"].as_array_mut() {
            for (row, stat) in arr.iter_mut().zip(report.per_model.iter()) {
                row["cache_hit_pct"] = ((stat.cache_hit_pct() * 10.0).round() / 10.0).into();
                row["saved_bytes"] = stat.saved_bytes().into();
            }
        }
        println!(
            "{}",
            serde_json::to_string_pretty(&v).context("render JSON")?
        );
        return Ok(());
    }

    let dur = (report.ended_at - report.started_at).max(0);
    println!(
        "{} trimwire — session {} ({} turns over {})",
        render::header(),
        report.session_id,
        report.per_model.iter().map(|m| m.turns).sum::<u64>(),
        human_duration(dur),
    );
    println!(
        "  per model (cache-hit = cache_read / input; read ~0.1×, creation ~1.25–2× — read both):"
    );
    for m in &report.per_model {
        let name = m.model.as_deref().unwrap_or("(unknown)");
        println!("    {name}");
        println!(
            "      turns {} · pruned {} (of {} sent)",
            m.turns,
            human_bytes(m.saved_bytes()),
            human_bytes(m.in_bytes as i64),
        );
        println!(
            "      cache-hit {:.0}%  ({} read of {} input)  · cache writes {}  · uncached {}  · output {}",
            m.cache_hit_pct(),
            human_count(m.cache_read_input_tokens as i64),
            human_count(m.total_input_tokens() as i64),
            human_count(m.cache_creation_input_tokens as i64),
            human_count(m.input_tokens as i64),
            human_count(m.output_tokens as i64),
        );
        if m.native_cleared_input_tokens > 0 {
            println!(
                "      Anthropic native compaction cleared {} tokens",
                human_count(m.native_cleared_input_tokens as i64),
            );
        }
    }
    // Distinguish "genuinely 0% cache" from "usage never recorded" — otherwise a
    // session whose tokens weren't captured looks identical to a real cache miss.
    let all_zero = report.per_model.iter().all(|m| {
        m.input_tokens == 0
            && m.cache_read_input_tokens == 0
            && m.cache_creation_input_tokens == 0
            && m.output_tokens == 0
    });
    if all_zero {
        println!(
            "  {} all token counts are 0 — usage was NOT recorded for this session (pre-v4 rows,\n\
             \x20   or trimwire didn't observe SSE responses). Run a fresh session through\n\
             \x20   trimwire and re-check before drawing any cache/cost conclusions.",
            render::warn()
        );
    }
    println!(
        "  note: a LOW cache-hit with HIGH cache-writes on a stable session = the prefix is\n\
         \x20   being re-billed (e.g. thinking_strip busting the cache). That's the Track A signal.\n\
         \x20   Claude Code may report one model under two names (e.g. `claude-opus-4-8` and\n\
         \x20   `claude-opus-4-8[1m]` after the 1M auto-bump) — sum both rows for the full picture."
    );
    Ok(())
}

/// Compact human-readable duration (s / m / h).
fn human_duration(secs: i64) -> String {
    let s = secs.max(0);
    if s >= 3600 {
        format!("{}h {}m", s / 3600, (s % 3600) / 60)
    } else if s >= 60 {
        format!("{}m {}s", s / 60, s % 60)
    } else {
        format!("{s}s")
    }
}

/// Compact human-readable count (e.g. 12.3k, 4.5M) for token estimates.
pub(crate) fn human_count(n: i64) -> String {
    let neg = n < 0;
    let v = n.unsigned_abs() as f64;
    let sign = if neg { "-" } else { "" };
    if v >= 1_000_000.0 {
        format!("{sign}{:.1}M", v / 1_000_000.0)
    } else if v >= 1_000.0 {
        format!("{sign}{:.1}k", v / 1_000.0)
    } else {
        format!("{sign}{}", v as u64)
    }
}

/// Compact human-readable byte count (handles negative deltas). Kept in step
/// with the `fmtBytes` JS in `src/cli/dashboard_template.html` so `trimwire
/// stats` and the local HTML dashboard show the same number for the same ledger.
pub(crate) fn human_bytes(n: i64) -> String {
    let neg = n < 0;
    let mut v = n.unsigned_abs() as f64;
    let units = ["B", "KB", "MB", "GB", "TB"];
    let mut u = 0;
    while v >= 1024.0 && u < units.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    let sign = if neg { "-" } else { "" };
    if u == 0 {
        format!("{sign}{} {}", v as u64, units[u])
    } else {
        format!("{sign}{v:.1} {}", units[u])
    }
}

/// Parse `YYYY-MM-DD` → UTC unix seconds at 00:00:00 that day.
fn day_start_utc(s: &str) -> Result<i64> {
    let (y, m, d) = parse_ymd(s)?;
    Ok(super::civil::days_from_civil(y, m, d) * 86_400)
}

/// Parse `YYYY-MM-DD` → the EXCLUSIVE end bound: 00:00:00 UTC of the *next* day,
/// so a `--until` of that date includes the whole day.
fn day_end_utc(s: &str) -> Result<i64> {
    let (y, m, d) = parse_ymd(s)?;
    Ok((super::civil::days_from_civil(y, m, d) + 1) * 86_400)
}

pub(crate) fn parse_ymd(s: &str) -> Result<(i64, i64, i64)> {
    let p: Vec<&str> = s.split('-').collect();
    if p.len() != 3 {
        anyhow::bail!("expected a UTC date as YYYY-MM-DD");
    }
    let y: i64 = p[0].parse().context("year")?;
    let m: i64 = p[1].parse().context("month")?;
    let d: i64 = p[2].parse().context("day")?;
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        anyhow::bail!("month must be 01-12 and day 01-31");
    }
    Ok((y, m, d))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn day_bounds_are_utc_and_half_open() {
        // 2026-06-07 00:00:00 UTC = 1780790400 (verified against the epoch).
        assert_eq!(day_start_utc("2026-06-07").unwrap(), 1_780_790_400);
        // end bound is the next midnight (start + 86400), making --until inclusive
        assert_eq!(
            day_end_utc("2026-06-07").unwrap(),
            day_start_utc("2026-06-07").unwrap() + 86_400
        );
        assert_eq!(day_start_utc("1970-01-01").unwrap(), 0);
    }

    #[test]
    fn bad_dates_are_rejected() {
        assert!(day_start_utc("2026/06/07").is_err());
        assert!(day_start_utc("not-a-date").is_err());
        assert!(day_start_utc("2026-13-01").is_err());
        assert!(day_start_utc("2026-06-32").is_err());
        assert!(day_start_utc("2026-06").is_err());
    }

    #[test]
    fn human_count_formats() {
        assert_eq!(human_count(0), "0");
        assert_eq!(human_count(999), "999");
        assert_eq!(human_count(1_500), "1.5k");
        assert_eq!(human_count(2_500_000), "2.5M");
        assert_eq!(human_count(-1_500), "-1.5k");
    }

    #[test]
    fn human_bytes_formats() {
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(1023), "1023 B");
        assert_eq!(human_bytes(1024), "1.0 KB");
        assert_eq!(human_bytes(-2048), "-2.0 KB");
        assert_eq!(human_bytes(1024 * 1024), "1.0 MB");
    }
}
