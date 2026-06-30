//! `trimwire report` — print a pre-filled GitHub issue URL for content-free bug reports.
//!
//! Gathers only tool/runtime versions, OS/arch, and a coarse cache-stability signal —
//! never file paths, session content, or any personally identifiable data.

use std::io::{IsTerminal, Read};
use std::path::{Path, PathBuf};

use anyhow::Result;

use trimwire::ledger::SessionReport;

/// Print a pre-filled GitHub issue URL (content-free: versions + OS/arch only).
///
/// With `url_only` set, prints only the bare URL (suitable for scripting or
/// piping into a browser opener). With `auto` set, runs the anomaly-detection
/// flow designed for a Stop hook — silent when nothing to do, never errors.
pub fn report(url_only: bool, auto: bool, session: Option<String>) -> Result<()> {
    if auto {
        // The auto flow must never propagate errors — a hook must not break a session.
        let _ = do_auto_report(session.as_deref());
        return Ok(());
    }

    let tw_ver = env!("TRIMWIRE_VERSION");

    let claude_ver = run_version("claude");
    let rustc_ver = run_version("rustc");

    let os = std::env::consts::OS;
    let arch = std::env::consts::ARCH;

    // Best-effort ledger probe: produce a coarse cache-stability phrase or None.
    let cache_line = best_effort_cache_line();

    let url = build_issue_url(tw_ver, &claude_ver, &rustc_ver, os, arch, cache_line.as_deref());

    if url_only {
        println!("{url}");
    } else {
        println!(
            "Open this link to file a content-free bug report (versions and OS only — \
             no file paths or session content)."
        );
        println!(
            "Fill in what happened and paste reviewed gateway logs before submitting \
             (`TRIMWIRE_LOG=info trimwire serve` — review lines before sharing)."
        );
        println!();
        println!("{url}");
    }
    Ok(())
}

/// Run `<cmd> --version` and return the first line of stdout, trimmed.
/// Returns `"unknown"` on any error (missing binary, non-zero exit, etc.).
fn run_version(cmd: &str) -> String {
    std::process::Command::new(cmd)
        .arg("--version")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| {
            std::str::from_utf8(&o.stdout)
                .ok()
                .and_then(|s| s.lines().next())
                .map(|l| l.trim().to_owned())
        })
        .unwrap_or_else(|| "unknown".to_owned())
}

/// Best-effort: open the ledger and return a coarse cache-stability phrase, or
/// `None` if the ledger is absent, disabled, or has no data.
fn best_effort_cache_line() -> Option<String> {
    use trimwire::config::Config;
    use trimwire::ledger::Ledger;

    let cfg = Config::load().ok()?;
    if !cfg.ledger.enabled {
        return None;
    }
    let path = trimwire::ledger::resolve_path(&cfg.ledger.db_path);
    if !path.exists() {
        return None;
    }
    let report = Ledger::report(&cfg.ledger.db_path).ok()?;
    let cs = &report.cache_stability;
    if cs.no_strategy_total == 0 {
        return None;
    }
    // Bucket into a coarse phrase (rounded to nearest 5%) to avoid leaking
    // precise token counts or exact session cadence.
    let pct = (cs.ratio * 100.0).round() as u64;
    let bucketed = (pct / 5) * 5; // nearest 5%
    if cs.ratio < 1.0 {
        Some(format!("cache stability: ~{bucketed}% (may be thrashing)"))
    } else {
        Some("cache stability: ok".to_owned())
    }
}

/// Build the issue body string (pure function — no I/O — so it can be
/// unit-tested without shelling out or hitting the ledger).
///
/// When `anomaly` is `Some`, an "Auto-detected anomaly" section is appended to
/// the body. Content-free: no file paths or session content.
pub(crate) fn issue_body(
    tw: &str,
    claude: &str,
    rustc: &str,
    os: &str,
    arch: &str,
    cache_line: Option<&str>,
    anomaly: Option<&str>,
) -> String {
    let cache_section = cache_line
        .map(|l| format!("\n- {l}"))
        .unwrap_or_default();
    let anomaly_section = anomaly
        .map(|a| format!("\n\n## Auto-detected anomaly\n{a}"))
        .unwrap_or_default();

    format!(
        "\
## What happened
<!-- describe the problem -->

## Environment
- trimwire version: {tw}
- Claude Code version: {claude}
- Rust toolchain: {rustc}
- OS / arch: {os} {arch}{cache_section}

## Gateway logs
<!-- run `TRIMWIRE_LOG=info trimwire serve` and paste reviewed lines (they may contain file paths) -->

## Additional context
<!-- anything else that might help -->{anomaly_section}
"
    )
}

/// Build the pre-filled GitHub issue URL from gathered facts.
///
/// Pure function — no I/O — so it can be unit-tested without shelling out.
pub(crate) fn build_issue_url(
    tw: &str,
    claude: &str,
    rustc: &str,
    os: &str,
    arch: &str,
    cache_line: Option<&str>,
) -> String {
    let title = "trimwire: unexpected behaviour";
    let body = issue_body(tw, claude, rustc, os, arch, cache_line, None);

    let base = "https://github.com/AZagatti/trimwire/issues/new";
    format!(
        "{base}?labels={}&template={}&title={}&body={}",
        percent_encode("bug"),
        percent_encode("bug_report.md"),
        percent_encode(title),
        percent_encode(&body),
    )
}

/// Percent-encode a string for use in a URL query value.
///
/// Encodes every byte EXCEPT the RFC 3986 unreserved set `A-Za-z0-9-_.~`
/// as `%XX` (uppercase hex). Does NOT add a new crate dependency.
fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 3);
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            _ => {
                out.push('%');
                out.push(char::from_digit((b >> 4) as u32, 16).unwrap().to_ascii_uppercase());
                out.push(char::from_digit((b & 0xf) as u32, 16).unwrap().to_ascii_uppercase());
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Auto mode (--auto): detect anomalies, dedup, and file a GitHub issue.
// ---------------------------------------------------------------------------

/// Run the anomaly-detection flow. Returns `Ok(())` on all errors so the
/// caller (`report()`) can discard the result without disrupting a Stop hook.
fn do_auto_report(session_arg: Option<&str>) -> Result<()> {
    use trimwire::config::Config;
    use trimwire::ledger::Ledger;

    let cfg = Config::load()?;
    if !cfg.ledger.enabled {
        return Ok(());
    }
    let db_path = &cfg.ledger.db_path;

    // Determine target session id:
    // 1. --session arg if provided
    // 2. session_id from Stop-hook JSON on stdin
    // 3. fall back to None → most-recent ledger session
    let target_session: Option<String> = if let Some(s) = session_arg {
        Some(s.to_owned())
    } else {
        session_from_stdin()
    };

    // Build session report. Returns None when the ledger is absent, empty, or
    // the resolved session has no rows (e.g. trimwire wasn't active for it).
    let report = match Ledger::session_report(db_path, target_session.as_deref())? {
        Some(r) => r,
        None => return Ok(()), // nothing to do → silent
    };

    // Check for anomaly. Only anomaly kind currently: post-prune HTTP errors.
    let note = match anomaly_note(&report) {
        Some(n) => n,
        None => return Ok(()), // happy path → silent
    };

    // Dedup: avoid filing the same issue twice for the same session.
    let dir = dedup_dir(db_path);
    let fingerprint = format!("{}:post_prune_errors", report.session_id);
    if already_filed(&dir, &fingerprint) {
        return Ok(());
    }

    // Build issue content (content-free: only versions + anomaly note).
    let tw_ver = env!("TRIMWIRE_VERSION");
    let claude_ver = run_version("claude");
    let rustc_ver = run_version("rustc");
    let os = std::env::consts::OS;
    let arch = std::env::consts::ARCH;
    let cache_line = best_effort_cache_line();

    let title = "trimwire: post-prune HTTP error (auto-detected)";
    let body = issue_body(
        tw_ver,
        &claude_ver,
        &rustc_ver,
        os,
        arch,
        cache_line.as_deref(),
        Some(&note),
    );

    // Try to file via `gh issue create`.
    let gh_result = std::process::Command::new("gh")
        .args([
            "issue",
            "create",
            "--repo",
            "AZagatti/trimwire",
            "--title",
            title,
            "--body",
            &body,
            "--label",
            "trimwire-anomaly",
        ])
        .output();

    match gh_result {
        Ok(out) if out.status.success() => {
            // Record fingerprint so we don't file again for this session.
            let _ = record_filed(&dir, &fingerprint);
            let url_raw = String::from_utf8_lossy(&out.stdout);
            let url = url_raw.trim();
            println!("trimwire: filed anomaly issue {url}");
        }
        _ => {
            // gh missing, not authenticated, or non-zero exit → fallback URL.
            // Do NOT record the fingerprint; allow a retry / manual filing.
            let fallback =
                build_issue_url(tw_ver, &claude_ver, &rustc_ver, os, arch, cache_line.as_deref());
            println!("trimwire: anomaly this session — file it: {fallback}");
        }
    }

    Ok(())
}

/// Return an anomaly note if `report` has detectable anomalies, else `None`.
///
/// This is the single decision point for "should we file?". Structured to
/// allow new anomaly kinds to be added without changing the auto-flow logic.
pub(crate) fn anomaly_note(report: &SessionReport) -> Option<String> {
    if report.post_prune_errors > 0 {
        Some(format!(
            "post-prune HTTP >=400 on {} request(s) this session",
            report.post_prune_errors
        ))
    } else {
        None
    }
}

/// Resolve the directory for the dedup file (`filed-issues`).
///
/// Uses the parent directory of the resolved ledger db path. Falls back to
/// `~/.trimwire/` if the parent cannot be determined.
fn dedup_dir(db_path: &str) -> PathBuf {
    let ledger_path = trimwire::ledger::resolve_path(db_path);
    if let Some(parent) = ledger_path.parent() {
        if parent != Path::new("") {
            return parent.to_owned();
        }
    }
    // Fallback: ~/.trimwire/
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_owned());
    PathBuf::from(home).join(".trimwire")
}

/// Return `true` iff the dedup file in `dir` already contains `fingerprint`.
pub(crate) fn already_filed(dir: &Path, fingerprint: &str) -> bool {
    let path = dir.join("filed-issues");
    match std::fs::read_to_string(&path) {
        Ok(content) => content.lines().any(|l| l.trim() == fingerprint),
        Err(_) => false,
    }
}

/// Append `fingerprint` as a new line to the dedup file, creating dirs and the
/// file if needed.
pub(crate) fn record_filed(dir: &Path, fingerprint: &str) -> Result<()> {
    use std::io::Write;
    std::fs::create_dir_all(dir)?;
    let path = dir.join("filed-issues");
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)?;
    writeln!(f, "{fingerprint}")?;
    Ok(())
}

/// Try to extract a `session_id` from the Stop-hook JSON on stdin.
///
/// Non-fatal: returns `None` when stdin is a TTY, empty, or not valid JSON.
fn session_from_stdin() -> Option<String> {
    if std::io::stdin().is_terminal() {
        return None;
    }
    let mut input = String::new();
    std::io::stdin().read_to_string(&mut input).ok()?;
    let v: serde_json::Value = serde_json::from_str(input.trim()).ok()?;
    v.get("session_id")?.as_str().map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---------------------------------------------------------------------------
    // percent_encode
    // ---------------------------------------------------------------------------

    #[test]
    fn percent_encode_leaves_unreserved_chars() {
        assert_eq!(percent_encode("hello"), "hello");
        assert_eq!(percent_encode("Hello-World_1.2~"), "Hello-World_1.2~");
        assert_eq!(percent_encode("AZaz09-_.~"), "AZaz09-_.~");
    }

    #[test]
    fn percent_encode_encodes_special_chars() {
        assert_eq!(percent_encode(" "), "%20");
        assert_eq!(percent_encode("#"), "%23");
        assert_eq!(percent_encode("\n"), "%0A");
        assert_eq!(percent_encode("a b#c"), "a%20b%23c");
        assert_eq!(percent_encode("&=+"), "%26%3D%2B");
    }

    #[test]
    fn percent_encode_round_trip_known_string() {
        // Encode then check that known chars survive and special ones are escaped.
        let input = "trimwire: version 1.2.3 (linux x86_64)";
        let encoded = percent_encode(input);
        // Unreserved chars survive.
        assert!(encoded.contains("trimwire"));
        assert!(encoded.contains("1.2.3"));
        // Spaces and colons are encoded.
        assert!(encoded.contains("%20"));
        assert!(encoded.contains("%3A"));
        assert!(!encoded.contains(' '));
        assert!(!encoded.contains(':'));
    }

    // ---------------------------------------------------------------------------
    // build_issue_url (unchanged contract)
    // ---------------------------------------------------------------------------

    #[test]
    fn build_issue_url_starts_with_github_base() {
        let url = build_issue_url("0.3.16", "1.0.0", "rustc 1.85", "linux", "x86_64", None);
        assert!(
            url.starts_with("https://github.com/AZagatti/trimwire/issues/new?"),
            "URL must start with the GitHub base; got: {url}"
        );
    }

    #[test]
    fn build_issue_url_contains_required_params() {
        let url = build_issue_url("0.3.16", "1.0.0", "rustc 1.85", "linux", "x86_64", None);
        assert!(
            url.contains("template=bug_report.md"),
            "URL must contain template param; got: {url}"
        );
        assert!(
            url.contains("labels=bug"),
            "URL must contain labels=bug; got: {url}"
        );
    }

    #[test]
    fn build_issue_url_decoded_body_contains_trimwire_version() {
        let url =
            build_issue_url("0.3.16", "claude 1.0", "rustc 1.85", "linux", "x86_64", None);
        // The body is percent-encoded in the URL; check the decoded body directly.
        let body_marker = percent_encode("trimwire version: 0.3.16");
        assert!(
            url.contains(&body_marker),
            "encoded body must include trimwire version; got: {url}"
        );
    }

    #[test]
    fn build_issue_url_with_cache_line() {
        let url = build_issue_url(
            "0.3.16",
            "1.0.0",
            "rustc 1.85",
            "linux",
            "x86_64",
            Some("cache stability: ~75% (may be thrashing)"),
        );
        let encoded_cache = percent_encode("cache stability: ~75% (may be thrashing)");
        assert!(
            url.contains(&encoded_cache),
            "URL must encode the cache stability line; got: {url}"
        );
    }

    #[test]
    fn build_issue_url_without_cache_line_omits_stability() {
        let url = build_issue_url("0.3.16", "1.0.0", "rustc 1.85", "linux", "x86_64", None);
        assert!(
            !url.contains("cache%20stability"),
            "URL without a cache_line must not mention cache stability; got: {url}"
        );
    }

    /// Content-free guard: the URL must not embed `.rs` file extensions or
    /// encoded multi-segment filesystem paths (e.g. `/src/foo.rs`). A single
    /// `/` in human labels like `OS / arch` is acceptable; what we're guarding
    /// against is a sequence like `%2Fsrc%2F` that would indicate a real
    /// filesystem path leaked into the URL.
    #[test]
    fn build_issue_url_is_content_free() {
        let url = build_issue_url(
            "0.3.16",
            "claude 1.0",
            "rustc 1.85",
            "linux",
            "x86_64",
            Some("cache stability: ok"),
        );
        // No `.rs` extensions (encoded or plain) — would indicate a source file path.
        assert!(
            !url.contains(".rs"),
            "URL must not contain .rs file extensions; got: {url}"
        );
        assert!(
            !url.contains("%2Ers"),
            "URL must not contain encoded .rs; got: {url}"
        );
        // No multi-segment encoded paths (a `%2F` followed immediately by another
        // `%2F` or a `%2F` followed shortly by another `%2F` within 40 chars —
        // characteristic of absolute paths like `/home/user/` or `/src/lib.rs`).
        // The `OS / arch` label produces at most ONE `%2F` in isolation, which
        // is fine; a real filesystem path produces several.
        let query_start = url.find('?').unwrap_or(url.len());
        let query = &url[query_start..];
        let slash_count = query.matches("%2F").count();
        assert!(
            slash_count <= 1,
            "encoded query must not contain multi-segment filesystem paths \
             (found {slash_count} encoded slashes); got: {query}"
        );
    }

    // ---------------------------------------------------------------------------
    // issue_body
    // ---------------------------------------------------------------------------

    #[test]
    fn issue_body_contains_env_fields() {
        let body = issue_body("0.4.0", "claude 2.0", "rustc 1.85", "linux", "x86_64", None, None);
        assert!(body.contains("trimwire version: 0.4.0"));
        assert!(body.contains("Claude Code version: claude 2.0"));
        assert!(body.contains("Rust toolchain: rustc 1.85"));
        assert!(body.contains("OS / arch: linux x86_64"));
    }

    #[test]
    fn issue_body_with_anomaly_contains_note() {
        let body = issue_body(
            "0.4.0",
            "claude 2.0",
            "rustc 1.85",
            "linux",
            "x86_64",
            None,
            Some("post-prune HTTP >=400 on 2 request(s) this session"),
        );
        assert!(body.contains("Auto-detected anomaly"));
        assert!(body.contains("post-prune HTTP >=400 on 2 request(s) this session"));
    }

    #[test]
    fn issue_body_without_anomaly_omits_section() {
        let body = issue_body("0.4.0", "claude 2.0", "rustc 1.85", "linux", "x86_64", None, None);
        assert!(!body.contains("Auto-detected anomaly"));
    }

    #[test]
    fn issue_body_is_content_free() {
        let body = issue_body(
            "0.4.0",
            "claude 2.0",
            "rustc 1.85",
            "linux",
            "x86_64",
            Some("cache stability: ok"),
            Some("post-prune HTTP >=400 on 1 request(s) this session"),
        );
        // No file system paths — no `.rs` extensions, no absolute path segments.
        assert!(!body.contains(".rs"), "body must not contain .rs extensions");
        assert!(
            !body.contains("/home/"),
            "body must not contain filesystem paths"
        );
        assert!(
            !body.contains("/src/"),
            "body must not contain source tree paths"
        );
    }

    #[test]
    fn issue_body_no_anomaly_matches_original_body_format() {
        // The body produced by issue_body with anomaly=None must be identical to
        // what build_issue_url embeds (backwards-compat with existing URL tests).
        let body = issue_body("0.3.16", "1.0.0", "rustc 1.85", "linux", "x86_64", None, None);
        let url = build_issue_url("0.3.16", "1.0.0", "rustc 1.85", "linux", "x86_64", None);
        let encoded_trimwire_ver = percent_encode("trimwire version: 0.3.16");
        assert!(
            url.contains(&encoded_trimwire_ver),
            "URL must still encode the version field"
        );
        // The encoded body in the URL must equal the percent-encoded issue_body output.
        assert!(url.contains(&percent_encode(&body)));
    }

    // ---------------------------------------------------------------------------
    // anomaly_note
    // ---------------------------------------------------------------------------

    #[test]
    fn anomaly_note_none_when_no_errors() {
        let rep = make_session_report("sess1", 0);
        assert_eq!(anomaly_note(&rep), None);
    }

    #[test]
    fn anomaly_note_some_when_errors() {
        let rep = make_session_report("sess2", 3);
        let note = anomaly_note(&rep);
        assert!(note.is_some());
        let note = note.unwrap();
        assert!(note.contains("3"), "note must include the count");
        assert!(note.contains("post-prune"));
    }

    // ---------------------------------------------------------------------------
    // already_filed / record_filed
    // ---------------------------------------------------------------------------

    #[test]
    fn already_filed_false_when_file_absent() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!already_filed(dir.path(), "sess:post_prune_errors"));
    }

    #[test]
    fn already_filed_false_when_fingerprint_not_present() {
        let dir = tempfile::tempdir().unwrap();
        record_filed(dir.path(), "other_session:post_prune_errors").unwrap();
        assert!(!already_filed(dir.path(), "target_session:post_prune_errors"));
    }

    #[test]
    fn record_then_already_filed_true() {
        let dir = tempfile::tempdir().unwrap();
        let fp = "abc123:post_prune_errors";
        assert!(!already_filed(dir.path(), fp));
        record_filed(dir.path(), fp).unwrap();
        assert!(already_filed(dir.path(), fp));
    }

    #[test]
    fn record_filed_is_idempotent_and_both_present() {
        let dir = tempfile::tempdir().unwrap();
        let fp1 = "sess1:post_prune_errors";
        let fp2 = "sess2:post_prune_errors";
        record_filed(dir.path(), fp1).unwrap();
        record_filed(dir.path(), fp2).unwrap();
        record_filed(dir.path(), fp1).unwrap(); // write fp1 a second time
        assert!(already_filed(dir.path(), fp1));
        assert!(already_filed(dir.path(), fp2));
    }

    #[test]
    fn record_filed_creates_parent_dirs() {
        let base = tempfile::tempdir().unwrap();
        let nested = base.path().join("a").join("b").join("c");
        // Dir does not exist yet.
        assert!(!nested.exists());
        record_filed(&nested, "fp").unwrap();
        assert!(nested.join("filed-issues").exists());
    }

    // ---------------------------------------------------------------------------
    // Helpers for tests
    // ---------------------------------------------------------------------------

    fn make_session_report(session_id: &str, post_prune_errors: u64) -> SessionReport {
        SessionReport {
            session_id: session_id.to_owned(),
            started_at: 0,
            ended_at: 0,
            per_model: vec![],
            post_prune_errors,
        }
    }
}
