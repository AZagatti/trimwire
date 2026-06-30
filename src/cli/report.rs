//! `trimwire report` — print a pre-filled GitHub issue URL for content-free bug reports.
//!
//! Gathers only tool/runtime versions, OS/arch, and a coarse cache-stability signal —
//! never file paths, session content, or any personally identifiable data.

use anyhow::Result;

/// Print a pre-filled GitHub issue URL (content-free: versions + OS/arch only).
///
/// With `url_only` set, prints only the bare URL (suitable for scripting or
/// piping into a browser opener). Otherwise prints a short explanation first.
pub fn report(url_only: bool) -> Result<()> {
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

    let cache_section = cache_line
        .map(|l| format!("\n- {l}"))
        .unwrap_or_default();

    let body = format!(
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
<!-- anything else that might help -->
"
    );

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

#[cfg(test)]
mod tests {
    use super::*;

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
}
