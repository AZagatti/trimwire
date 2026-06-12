//! `trimwire sweep …` — clean session JSONL transcripts on disk (atomic,
//! backed up). Subcommands: `list`, `file <path>`, `all`, `undo <path>`.

use std::path::PathBuf;

use anyhow::{Result, bail};

use trimwire::sweep as engine;

/// `trimwire sweep list` — show the session transcripts trimwire can clean, so
/// you never have to hunt for a path.
pub fn sweep_list() -> Result<()> {
    let files = engine::session_files();
    if files.is_empty() {
        match engine::sessions_root() {
            Some(root) => {
                println!("no session transcripts found under {}", root.display());
                println!("→ run `trimwire on`, then `claude` to create a session first.");
            }
            None => println!("could not locate Claude Code's sessions directory ($HOME unset)"),
        }
        return Ok(());
    }
    println!("{} session transcript(s):", files.len());
    for f in &files {
        let size = std::fs::metadata(f).map(|m| m.len()).unwrap_or(0);
        println!("  {:>9}  {}", human(size as i64), f.display());
    }
    println!("\nclean one: `trimwire sweep file <path>`   ·   clean all: `trimwire sweep all`");
    Ok(())
}

/// `trimwire sweep all [--dry-run]` — clean every discovered session. Active
/// sessions safely abort (the file changed mid-sweep) and are reported, not
/// fatal.
pub fn sweep_all(dry_run: bool, yes: bool) -> Result<()> {
    let files = engine::session_files();
    if files.is_empty() {
        println!("no session transcripts found to sweep.");
        return Ok(());
    }
    // A live sweep rewrites every transcript on disk (backups are made, and
    // `sweep undo` restores). Gate it behind a confirmation unless --yes; refuse
    // rather than hang when stdin isn't a terminal (pipes / CI).
    if !dry_run && !yes && !confirm_sweep_all(files.len())? {
        return Ok(());
    }
    let mut total_saved: i64 = 0;
    let mut swept = 0usize;
    let mut skipped = 0usize;
    for f in &files {
        let res = if dry_run {
            engine::dry_run_file(f)
        } else {
            engine::sweep_file(f)
        };
        match res {
            Ok(r) if r.thinking_dropped + r.inputs_purged > 0 => {
                total_saved += r.saved();
                swept += 1;
                println!(
                    "  {} {}: saved {}",
                    verb(dry_run),
                    f.display(),
                    human(r.saved())
                );
            }
            Ok(_) => {} // nothing to do for this file; stay quiet
            Err(e) => {
                skipped += 1;
                println!("  skipped {} ({e})", f.display());
            }
        }
    }
    println!(
        "\n{} {} file(s){}; total saved {}.",
        verb(dry_run),
        swept,
        if skipped > 0 {
            format!(", skipped {skipped}")
        } else {
            String::new()
        },
        human(total_saved),
    );
    if dry_run {
        println!("(dry-run — nothing written)");
    }
    Ok(())
}

/// `trimwire sweep undo <path>` — restore a session from its latest backup.
pub fn sweep_undo(path: PathBuf) -> Result<()> {
    let bak = engine::restore_backup(&path)?;
    // Append a human-readable timestamp by parsing the `.bak.<nanos>` suffix.
    let bak_ts = bak
        .file_name()
        .and_then(|n| n.to_str())
        .and_then(|n| n.rsplit('.').next())
        .and_then(|s| s.parse::<u128>().ok())
        .map(|nanos| {
            let secs = (nanos / 1_000_000_000) as i64;
            format_unix_ts(secs)
        })
        .unwrap_or_default();
    let ts_note = if bak_ts.is_empty() {
        String::new()
    } else {
        format!(" (backed up {bak_ts})")
    };
    println!(
        "restored {} from {}{}",
        path.display(),
        bak.display(),
        ts_note
    );
    Ok(())
}

/// Format a Unix timestamp (seconds) as `YYYY-MM-DD HH:MM UTC` without any
/// external dependency — simple integer arithmetic on the proleptic Gregorian
/// calendar. Accurate for any date in the reasonable past/future range.
fn format_unix_ts(secs: i64) -> String {
    // Days + time-of-day.
    let (day_secs, time_secs) = if secs >= 0 {
        (secs / 86_400, secs % 86_400)
    } else {
        // For negative timestamps (pre-1970) keep the math correct.
        let d = (secs - 86_399) / 86_400;
        let t = secs - d * 86_400;
        (d, t)
    };
    let h = time_secs / 3600;
    let m = (time_secs % 3600) / 60;

    // Gregorian calendar reconstruction from day count (days since 1970-01-01).
    // Algorithm: shift epoch to 1 Mar 2000 (day 10957 from 1970-01-01).
    let z = day_secs + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mo = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if mo <= 2 { y + 1 } else { y };
    format!("{y:04}-{mo:02}-{d:02} {h:02}:{m:02} UTC")
}

/// `trimwire sweep file <path>` — clean (or validate / dry-run) one transcript.
pub fn sweep_file(path: PathBuf, validate_only: bool, dry_run: bool) -> Result<()> {
    if validate_only {
        if engine::validate_file(&path)? {
            println!("{}: valid", path.display());
            return Ok(());
        }
        bail!(
            "{}: validation failed — run `trimwire sweep file {} --dry-run` \
             to see what would change, or set TRIMWIRE_LOG=warn for trace-level details",
            path.display(),
            path.display()
        );
    }

    if dry_run {
        let r = engine::dry_run_file(&path)?;
        println!(
            "dry-run {}: would save {} bytes ({} → {}); drop {} empty-thinking block(s), \
             purge {} failed input(s) across {} line(s) — nothing written",
            path.display(),
            r.saved(),
            r.orig_bytes,
            r.final_bytes,
            r.thinking_dropped,
            r.inputs_purged,
            r.lines,
        );
        return Ok(());
    }

    // Capture pre-sweep validity so we don't blame sweep for problems that were
    // already in the file; only bail if sweep turned a valid file invalid.
    let was_valid = engine::validate_file(&path).unwrap_or(false);

    let r = engine::sweep_file(&path)?;
    println!(
        "swept {}: {} → {} bytes (saved {}); dropped {} empty-thinking block(s), \
         purged {} failed input(s); backup {}",
        path.display(),
        r.orig_bytes,
        r.final_bytes,
        r.saved(),
        r.thinking_dropped,
        r.inputs_purged,
        r.backup.as_deref().unwrap_or("- (no changes)"),
    );

    let now_valid = engine::validate_file(&path)?;
    if was_valid && !now_valid {
        bail!(
            "post-sweep validation failed — restore with `trimwire sweep undo {}`",
            path.display()
        );
    }
    if !now_valid {
        eprintln!(
            "note: {} still has pre-existing issues sweep does not fix (it was already \
             invalid before sweeping); the swept content was preserved as-is",
            path.display()
        );
    }
    Ok(())
}

fn verb(dry_run: bool) -> &'static str {
    if dry_run { "would sweep" } else { "swept" }
}

/// Confirm a live `sweep all`. Returns `Ok(true)` to proceed. Refuses (with
/// guidance) when stdin isn't a terminal, so it never hangs in a pipe / CI.
fn confirm_sweep_all(count: usize) -> Result<bool> {
    use std::io::{IsTerminal, Write};
    if !std::io::stdin().is_terminal() {
        println!(
            "refusing to sweep {count} transcript(s) without confirmation — re-run with \
             --yes (or --dry-run to preview)."
        );
        return Ok(false);
    }
    print!(
        "About to clean {count} session transcript(s) on disk (backups are made; \
         `trimwire sweep undo <file>` restores). Continue? [y/N] "
    );
    std::io::stdout().flush().ok();
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    let confirmed = matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes");
    if !confirmed {
        println!("aborted — nothing changed.");
    }
    Ok(confirmed)
}

/// Compact human bytes (handles negative deltas).
fn human(n: i64) -> String {
    let neg = n < 0;
    let mut v = n.unsigned_abs() as f64;
    let u = ["B", "KB", "MB", "GB"];
    let mut i = 0;
    while v >= 1024.0 && i < u.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    let sign = if neg { "-" } else { "" };
    if i == 0 {
        format!("{sign}{} {}", v as u64, u[i])
    } else {
        format!("{sign}{v:.1} {}", u[i])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_unix_ts_known_dates() {
        // 2026-06-09 14:30 UTC = 1781015400.
        assert_eq!(format_unix_ts(1_781_015_400), "2026-06-09 14:30 UTC");
        // Unix epoch.
        assert_eq!(format_unix_ts(0), "1970-01-01 00:00 UTC");
        // A leap-year date: 2000-02-29 12:00 UTC = 951825600.
        assert_eq!(format_unix_ts(951_825_600), "2000-02-29 12:00 UTC");
        // 2025-06-09 14:30 UTC = 1749479400.
        assert_eq!(format_unix_ts(1_749_479_400), "2025-06-09 14:30 UTC");
    }
}
