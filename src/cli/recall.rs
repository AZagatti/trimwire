//! `trimwire recall` — list recent sessions from the ledger (content-free
//! metadata only) so you can find one to inspect with `stats --session <id>`.
//! The ledger stores NO prompt content, so the optional filter matches session
//! id / model substrings only (ToS-clean, never your conversation text).

use anyhow::{Context, Result};

use super::stats::human_bytes;
use trimwire::config::Config;
use trimwire::ledger::{self, Ledger};

/// Validate + normalize a `YYYY-MM-DD` to zero-padded form so a lexical compare
/// against the ledger's stored `last_day` is also a chronological one (`2026-6-7`
/// would otherwise mis-sort against `2026-06-07`).
fn norm_date(s: &str) -> Result<String> {
    let (y, m, d) = super::stats::parse_ymd(s)?;
    Ok(format!("{y:04}-{m:02}-{d:02}"))
}

pub fn recall(
    query: Option<String>,
    json: bool,
    limit: usize,
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
            println!("ledger is disabled in config ([ledger] enabled = false); nothing to recall.");
        }
        return Ok(());
    }
    if !ledger::resolve_path(&config.ledger.db_path).exists() {
        if json {
            println!(
                "{}",
                serde_json::json!({"available": false, "reason": "ledger not created"})
            );
        } else {
            println!("ledger not yet created — run `trimwire on`/`trimwire run` first.");
        }
        return Ok(());
    }

    let q = query.as_deref().map(str::trim).filter(|s| !s.is_empty());
    let since = since
        .as_deref()
        .map(norm_date)
        .transpose()
        .context("--since")?;
    let until = until
        .as_deref()
        .map(norm_date)
        .transpose()
        .context("--until")?;

    // list_sessions limits by recency at the SQL layer; when a date window is set,
    // pull a generous slice and filter the (content-free) metadata in Rust, then
    // re-apply the limit — `last_day` is zero-padded YYYY-MM-DD, so a string compare
    // is chronological.
    let windowed = since.is_some() || until.is_some();
    let fetch = if windowed {
        limit.max(1).max(5000)
    } else {
        limit.max(1)
    };
    let mut rows = Ledger::list_sessions(&config.ledger.db_path, q, fetch)
        .context("read sessions from ledger")?;
    if let Some(s) = &since {
        rows.retain(|r| r.last_day.as_str() >= s.as_str());
    }
    if let Some(u) = &until {
        rows.retain(|r| r.last_day.as_str() <= u.as_str());
    }
    rows.truncate(limit.max(1));

    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(
                &serde_json::json!({"available": true, "sessions": rows})
            )?
        );
        return Ok(());
    }

    if rows.is_empty() {
        match q {
            Some(s) => println!("no sessions match \"{s}\"."),
            None => println!("no sessions recorded yet."),
        }
        return Ok(());
    }

    println!(
        "recent sessions{}  (inspect one: trimwire stats --session <id>)",
        q.map(|s| format!(" matching \"{s}\"")).unwrap_or_default()
    );
    for r in &rows {
        // Strip the "claude-" prefix for width; the full id is shown verbatim so
        // it copy-pastes straight into `stats --session`.
        let model = r
            .model
            .as_deref()
            .map(|m| m.strip_prefix("claude-").unwrap_or(m))
            .unwrap_or("?");
        println!(
            "  {date}  {sid}  {req:>4} req  {into}→{outof} ({red:.0}%)  cache-hit {hit:.0}%  {model}",
            date = r.last_day,
            sid = r.session_id,
            req = r.requests,
            into = human_bytes(r.in_bytes as i64),
            outof = human_bytes(r.out_bytes as i64),
            red = r.reduction_pct(),
            hit = r.cache_hit_pct(),
        );
    }
    Ok(())
}
