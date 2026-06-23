//! `trimwire update` / `upgrade` — READ-ONLY update check (phase 4a).
//!
//! This command does NOT download, verify, replace, restart, or roll back
//! anything yet (see `docs/UPDATE-COMMAND-SPIKE.md`). It:
//!   1. resolves + canonicalizes the running binary,
//!   2. checks whether this install is self-updatable (managed `script` install,
//!      path/target match, writable) — refusing with guidance if not,
//!   3. queries the latest GitHub release (non-destructively) and reports whether
//!      a newer version exists.
//!
//! All impure I/O lives here; the decision logic is in `trimwire::update`.

use anyhow::Result;
use std::time::Duration;
use trimwire::update as upd;

/// GitHub API base. A test-only override (`TRIMWIRE_UPDATE_API_BASE`) lets the
/// integration tests point at a local mock — but it is honored ONLY for a
/// localhost base, so a stray/hostile env var in production can't redirect the
/// update check to an attacker-controlled server. (Read-only today; this guard
/// also protects 4b, which will derive the download URL from the same base.)
fn api_base() -> String {
    const DEFAULT: &str = "https://api.github.com";
    match std::env::var("TRIMWIRE_UPDATE_API_BASE")
        .ok()
        .filter(|s| !s.is_empty())
    {
        Some(o) if is_localhost_base(&o) => o,
        _ => DEFAULT.to_owned(),
    }
}

fn is_localhost_base(url: &str) -> bool {
    url.starts_with("http://127.0.0.1:")
        || url.starts_with("http://localhost:")
        || url.starts_with("http://[::1]:")
}

/// Fetch the latest release tag from GitHub. Returns `None` on ANY problem
/// (network down, rate-limited, timeout, non-2xx, unparseable) — a failed check
/// is non-destructive and never an error state. One short-timeout GET, reusing
/// the shared hyper HTTPS client (no extra deps; mirrors `cli::share::post`).
///
/// Uses `releases/latest`, which GitHub defines as the latest **non-prerelease,
/// non-draft** release — so the prerelease-suffix tolerance in
/// [`trimwire::update::parse_version`] doesn't surface a `-rc` tag here. (4b's
/// apply gate must still handle prereleases explicitly if that ever changes.)
fn fetch_latest_tag() -> Option<String> {
    use http_body_util::{BodyExt, Full};
    use hyper::Request;
    use hyper::body::Bytes;
    use trimwire::proxy::upstream::build_client;

    let url = format!("{}/repos/{}/releases/latest", api_base(), upd::REPO);
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .ok()?;
    rt.block_on(async {
        tokio::time::timeout(Duration::from_secs(6), async {
            let client = build_client();
            let req = Request::builder()
                .method("GET")
                .uri(&url)
                // GitHub requires a User-Agent; Accept selects the stable API shape.
                .header("user-agent", "trimwire-update-check")
                .header("accept", "application/vnd.github+json")
                .body(Full::new(Bytes::new()))
                .ok()?;
            let resp = client.request(req).await.ok()?;
            if !resp.status().is_success() {
                return None;
            }
            let bytes = resp.into_body().collect().await.ok()?.to_bytes();
            let body: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
            body.get("tag_name")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
        })
        .await
        .ok()
        .flatten()
    })
}

/// If a newer release than the running build exists, return its tag. `None` when
/// already current OR the check couldn't complete. Used by `doctor` for a silent
/// advisory bullet (so a failed-or-current check shows nothing).
pub(crate) fn newer_available() -> Option<String> {
    let current = upd::parse_version(env!("CARGO_PKG_VERSION"))?;
    let tag = fetch_latest_tag()?;
    let latest = upd::parse_version(&tag)?;
    upd::is_newer(latest, current).then_some(tag)
}

/// Canonicalized path of the running binary, or an error string for a clear
/// abort (missing / `(deleted)` / unresolvable).
fn resolved_current_exe() -> std::result::Result<std::path::PathBuf, String> {
    let raw =
        std::env::current_exe().map_err(|e| format!("cannot determine the running binary: {e}"))?;
    // On Linux a removed-while-running exe resolves to "<path> (deleted)".
    if raw.to_string_lossy().ends_with(" (deleted)") {
        return Err(format!(
            "the running binary appears to have been deleted ({}) — reinstall before updating",
            raw.display()
        ));
    }
    raw.canonicalize()
        .map_err(|e| format!("cannot resolve the running binary {}: {e}", raw.display()))
}

/// Best-effort test: is `dir` writable by us? Create+drop a unique probe file.
/// (std-only; the dev-dep `tempfile` isn't available to the binary.)
fn dir_writable(dir: &std::path::Path) -> bool {
    let probe = dir.join(format!(".trimwire-write-test-{}", std::process::id()));
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&probe)
    {
        Ok(_) => {
            let _ = std::fs::remove_file(&probe);
            true
        }
        Err(_) => false,
    }
}

/// `trimwire update [--yes]` (alias `upgrade`). Read-only check; `--yes` is not
/// an apply path yet (a later phase) — it explains that and points to the manual
/// update, exiting non-zero, so the advertised flag never dead-ends in a clap
/// error.
pub fn update(yes: bool) -> Result<()> {
    let receipt_path = trimwire::receipt::receipt_path().display().to_string();

    // 1. Resolve the running binary (canonical).
    let exe = match resolved_current_exe() {
        Ok(p) => p,
        Err(msg) => {
            eprintln!(
                "trimwire update: {msg}\n\n{}",
                upd::manual_update_guidance()
            );
            std::process::exit(2);
        }
    };
    let exe_str = exe.display().to_string();

    // 2. Eligibility. Canonicalize the receipt's recorded path too so the compare
    //    is symlink-stable, then run the pure predicate.
    let mut receipt = trimwire::receipt::load();
    if let Some(r) = receipt.as_mut() {
        // If the recorded path no longer exists, canonicalize fails and we keep
        // the raw string — which then reliably diverges from the canonical
        // current_exe, yielding PathMismatch (refuse). Safe by construction.
        if let Ok(canon) = std::fs::canonicalize(&r.binary_path) {
            r.binary_path = canon.display().to_string();
        }
    }
    let parent_writable = exe.parent().map(dir_writable).unwrap_or(false);
    let elig = upd::eligibility(
        receipt.as_ref(),
        &exe_str,
        trimwire::build_target(),
        parent_writable,
    );

    if elig != upd::Eligibility::Eligible {
        eprintln!(
            "{}\n\n{}",
            upd::refusal_reason(&elig, &exe_str, &receipt_path),
            upd::manual_update_guidance()
        );
        std::process::exit(2);
    }

    // 3. `--yes` is reserved for the real apply path (download + verify + atomic
    //    swap + restart), which isn't implemented yet. Be honest, don't pretend.
    if yes {
        eprintln!(
            "trimwire update: self-update (`--yes`) isn't implemented yet — this build only \
             checks for updates. Update now with:\n\n{}",
            upd::manual_update_guidance()
        );
        std::process::exit(2);
    }

    // 4. Read-only version check.
    let current = env!("CARGO_PKG_VERSION");
    match fetch_latest_tag() {
        None => {
            eprintln!(
                "trimwire update: couldn't check for updates (GitHub unreachable or rate-limited). \
                 See https://github.com/{}/releases",
                upd::REPO
            );
            // Non-destructive: a failed check is not an error of the user's request.
            Ok(())
        }
        Some(tag) => match upd::parse_version(&tag) {
            None => {
                eprintln!(
                    "trimwire update: couldn't parse the latest release tag '{tag}'. \
                     See https://github.com/{}/releases",
                    upd::REPO
                );
                Ok(())
            }
            Some(latest) => {
                let cur = upd::parse_version(current).expect("own version parses");
                if upd::is_newer(latest, cur) {
                    // Report the available version, but don't dead-end users at a
                    // `--yes` that isn't implemented yet — give the actionable
                    // manual path now (self-update lands in a later phase).
                    println!(
                        "trimwire {} is available (you have {current}). Self-update (`--yes`) \
                         isn't implemented yet — update now with:\n\n{}",
                        tag.trim_start_matches('v'),
                        upd::manual_update_guidance()
                    );
                } else {
                    println!("trimwire is already up to date ({current}).");
                }
                Ok(())
            }
        },
    }
}
