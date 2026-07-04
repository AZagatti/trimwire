//! `trimwire dashboard` — write a self-contained local stats dashboard.
//!
//! Runs the ledger report queries and embeds the result (content-free metadata
//! only — byte counts, hashes, timings, model names; never message text) into a
//! single self-contained HTML file. No server, no network, no external assets:
//! open the output via `file://`. A snapshot — re-run to refresh.

use std::path::PathBuf;

use anyhow::{Context, Result};

use trimwire::config::Config;
use trimwire::ledger::{self, Ledger, SessionRow};

/// The dashboard page. The CLI replaces `__TRIMWIRE_DATA__` with the JSON payload.
const TEMPLATE: &str = include_str!("dashboard_template.html");

pub fn dashboard(out: Option<PathBuf>) -> Result<()> {
    use super::render;
    let config = Config::load().context("load config")?;
    // No ledger data yet → nothing to render, so no HTML file is written. Say so
    // explicitly: a bare exit 0 left a `--out` caller wondering why the file never
    // appeared. Exit 0 stays consistent with `stats`/`recall`, which also treat
    // "no data yet" as a non-error.
    if !config.ledger.enabled {
        println!(
            "{} ledger is disabled in config ([ledger] enabled = false); no dashboard file written.",
            render::bullet()
        );
        return Ok(());
    }
    if !ledger::resolve_path(&config.ledger.db_path).exists() {
        println!(
            "{} ledger not yet created — run {}/{} first; no dashboard file written.",
            render::bullet(),
            render::accent("trimwire on"),
            render::accent("trimwire run")
        );
        return Ok(());
    }

    let report = Ledger::report(&config.ledger.db_path).context("read ledger report")?;
    let sessions = Ledger::list_sessions(&config.ledger.db_path, None, 50)
        .context("list sessions for dashboard")?;

    // Derive the headline figures from the typed Report (single source —
    // `Report::reduction_pct` / `est_tokens_removed`), then hand them to the
    // pure splicer so it never recomputes the formula from JSON.
    let derived = Derived {
        bytes_saved: report.bytes_saved(),
        reduction_pct: report.reduction_pct(),
        est_tokens_removed: report.est_tokens_removed(),
    };
    let report_value = serde_json::to_value(&report).context("serialize report")?;
    let html = inject(report_value, &sessions, derived).context("build dashboard html")?;

    let out_path = out.unwrap_or_else(|| PathBuf::from("trimwire-report.html"));
    std::fs::write(&out_path, html)
        .with_context(|| format!("write dashboard to {}", out_path.display()))?;
    println!(
        "{} wrote {} — open it in a browser (file://, no server needed). Content-free; re-run to refresh.",
        render::ok(),
        render::accent(&out_path.display().to_string())
    );
    Ok(())
}

/// The headline figures derived from the typed [`Report`] (so the formulas live
/// in one place — `ledger::Report` — not re-derived from JSON here).
struct Derived {
    bytes_saved: i64,
    reduction_pct: f64,
    est_tokens_removed: i64,
}

/// Splice the serialized `Report` + the derived figures + a generated-at stamp +
/// the sessions list into the template. Pure (no IO/config) and no business
/// logic — the figures arrive precomputed. Content-free: `report` and
/// `SessionRow` carry only ledger metadata — never message text.
fn inject(
    mut report: serde_json::Value,
    sessions: &[SessionRow],
    derived: Derived,
) -> Result<String> {
    let round1 = |x: f64| (x * 10.0).round() / 10.0;

    report["generated_at"] = now_utc_string().into();
    report["bytes_saved"] = derived.bytes_saved.into();
    report["reduction_pct"] = round1(derived.reduction_pct).into();
    report["est_tokens_removed"] = derived.est_tokens_removed.into();
    // Project each SessionRow field EXPLICITLY (not serde's whole-struct
    // serialization): this is the content-free allowlist for the embedded HTML —
    // a future field added to SessionRow must be consciously added here, so it
    // can never auto-leak into the dashboard. The two computed fields
    // (reduction_pct / cache_hit_pct) are methods, not stored, so they'd be
    // absent from a blanket serialization anyway.
    report["sessions"] = serde_json::Value::Array(
        sessions
            .iter()
            .map(|r| {
                serde_json::json!({
                    "session_id": r.session_id,
                    "last_day": r.last_day,
                    "requests": r.requests,
                    "in_bytes": r.in_bytes,
                    "out_bytes": r.out_bytes,
                    "reduction_pct": round1(r.reduction_pct()),
                    "cache_hit_pct": round1(r.cache_hit_pct()),
                    "model": r.model,
                })
            })
            .collect(),
    );

    let json = serde_json::to_string(&report).context("serialize dashboard payload")?;
    // serde_json does NOT escape `<`/`>`, and the JSON is embedded inside a
    // `<script>` block — so a `</script>` inside any string value (session_id and
    // model come straight from request headers/body, unvalidated) would close the
    // block. Neutralize `</` → `<\/`: a valid JSON string escape that JS decodes
    // back to `</` at runtime (data unchanged), but the HTML parser no longer sees
    // a closing tag. JSON syntax itself never contains `</`, so this only touches
    // string contents. (Django's `json_script` uses the same defense.)
    let json = json.replace("</", "<\\/");
    // The token sits inside a JS literal (`const DATA = __TRIMWIRE_DATA__;`), so a
    // JSON object is a valid drop-in. It appears exactly once.
    Ok(TEMPLATE.replacen("__TRIMWIRE_DATA__", &json, 1))
}

/// Current UTC time as `YYYY-MM-DD HH:MM UTC`, dependency-free (no chrono):
/// Howard Hinnant's civil-from-days algorithm on the Unix epoch-day.
fn now_utc_string() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0) as i64;
    format!("{} UTC", super::civil::fmt_date_time(secs))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inject_fills_data_and_removes_token() {
        let report = serde_json::json!({
            "total_requests": 2, "total_in_bytes": 1000, "total_out_bytes": 600,
            "per_strategy": [["bloat_cap", 1]], "per_strategy_bytes": [["bloat_cap", 400]],
            "cache_stability": {"ratio": 1.0, "no_strategy_stable": 1, "no_strategy_total": 1},
            "response_metrics": {}, "per_day": [], "db_path": "x"
        });
        let sessions = vec![SessionRow {
            session_id: "abc-123-session".to_owned(),
            last_day: "2026-06-06".to_owned(),
            requests: 2,
            in_bytes: 1000,
            out_bytes: 600,
            ..Default::default()
        }];
        let html = inject(
            report,
            &sessions,
            Derived {
                bytes_saved: 400,
                reduction_pct: 40.0,
                est_tokens_removed: 100,
            },
        )
        .unwrap();
        assert!(
            !html.contains("__TRIMWIRE_DATA__"),
            "data token must be replaced"
        );
        assert!(
            html.contains("\"total_requests\":2"),
            "report data embedded"
        );
        assert!(
            html.contains("\"bytes_saved\":400"),
            "derived bytes_saved injected"
        );
        assert!(
            html.contains("\"reduction_pct\":40"),
            "derived reduction_pct injected"
        );
        assert!(html.contains("abc-123-session"), "session row embedded");
        assert!(
            html.contains("const DATA ="),
            "renders into the DATA literal"
        );
    }

    #[test]
    fn embedded_data_cannot_break_out_of_the_script_block() {
        // A forged session-id / model containing `</script>` must not close the
        // <script> block — it should be escaped to `<\/script>` in the DATA literal.
        let report = serde_json::json!({
            "total_requests": 1, "total_in_bytes": 100, "total_out_bytes": 50,
            "per_strategy": [], "per_strategy_bytes": [], "per_day": [],
            "cache_stability": {"ratio": 1.0, "no_strategy_stable": 0, "no_strategy_total": 0},
            "response_metrics": {}, "db_path": "x"
        });
        let sessions = vec![SessionRow {
            session_id: "s".to_owned(),
            last_day: "2026-06-06".to_owned(),
            model: Some("evil</script><img>".to_owned()),
            ..Default::default()
        }];
        let html = inject(
            report,
            &sessions,
            Derived {
                bytes_saved: 50,
                reduction_pct: 50.0,
                est_tokens_removed: 12,
            },
        )
        .unwrap();
        assert!(
            html.contains("evil<\\/script>"),
            "the injected </script> must be escaped"
        );
        assert!(
            !html.contains("evil</script>"),
            "raw </script> must NOT survive in the data"
        );
        // The only literal closing tag left is the template's own.
        assert_eq!(
            html.matches("</script>").count(),
            1,
            "exactly one (template) </script>"
        );
    }
}
