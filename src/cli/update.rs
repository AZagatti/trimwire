//! `trimwire update` / `upgrade` — the updater's impure I/O layer.
//!
//! Three modes (the pure decision logic lives in `trimwire::update`):
//!   1. default — READ-ONLY check: resolve + canonicalize the running binary,
//!      confirm it's a managed (`script`) install, and report current/available.
//!   2. `--dry-run` (4b) — download the latest release archive + `.sha256` +
//!      `.minisig`, verify the checksum AND the minisign signature against the
//!      pinned key, and report verified / NOT verified. Changes nothing.
//!   3. `--apply` / `--yes` (4c) — after the same verification, atomically
//!      replace the binary and restart the service, rolling back on any health
//!      failure. Linux + managed installs only; refuses otherwise.
//!
//! Fail-closed everywhere: nothing is replaced unless the download verified
//! against the pinned key. See `docs/UPDATE-COMMAND-SPIKE.md`.

use anyhow::Result;
use std::io::IsTerminal;
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

/// Release-download host base. Like [`api_base`], a localhost override
/// (`TRIMWIRE_UPDATE_DL_BASE`) is honored ONLY for a localhost value, so a
/// hostile env var can't redirect downloads in production. Default: the GitHub
/// web host that serves `releases/<tag>/download/<asset>` (which 302s to the
/// asset CDN — see [`download_bytes`]).
fn dl_base() -> String {
    const DEFAULT: &str = "https://github.com";
    match std::env::var("TRIMWIRE_UPDATE_DL_BASE")
        .ok()
        .filter(|s| !s.is_empty())
    {
        Some(o) if is_localhost_base(&o) => o,
        _ => DEFAULT.to_owned(),
    }
}

/// The minisign public key this build trusts for release artifacts, or `None`
/// when unset (no pinned key ⇒ verification fails closed). Test seam: ONLY when
/// the API base is localhost (i.e. an integration test pointed us at a fake
/// server) do we honor `TRIMWIRE_UPDATE_PUBKEY`, so tests can verify a fake
/// release signed by a throwaway key. In production `api_base()` is never
/// localhost, so the override is ignored and only the embedded key is trusted.
fn pinned_pubkey() -> Option<String> {
    if is_localhost_base(&api_base()) {
        if let Some(k) = std::env::var("TRIMWIRE_UPDATE_PUBKEY")
            .ok()
            .filter(|s| !s.trim().is_empty())
        {
            return Some(k);
        }
    }
    let k = upd::PINNED_PUBKEY.trim();
    (!k.is_empty()).then(|| k.to_owned())
}

/// Hard cap on a downloaded artifact (releases are a few MB; this only guards
/// against a hostile/broken server streaming unbounded data).
const MAX_DOWNLOAD_BYTES: usize = 200 * 1024 * 1024;

/// GET `url`, following up to 5 redirects (GitHub's release-download URL 302s to
/// an asset CDN), returning the body bytes. Redirects must stay on HTTPS (no
/// downgrade), except a localhost test base may redirect to localhost. Any
/// failure is returned as a short string — every caller treats an error as "not
/// verified / do not apply" (fail closed).
async fn download_bytes(
    client: &trimwire::proxy::upstream::UpstreamClient,
    url: &str,
) -> std::result::Result<Vec<u8>, String> {
    use http_body_util::{BodyExt, Full};
    use hyper::Request;
    use hyper::body::Bytes;

    let allow_plain = is_localhost_base(url);
    let mut url = url.to_owned();
    for _ in 0..6 {
        let req = Request::builder()
            .method("GET")
            .uri(&url)
            .header("user-agent", "trimwire-update")
            .header("accept", "application/octet-stream")
            .body(Full::new(Bytes::new()))
            .map_err(|e| format!("bad request: {e}"))?;
        let resp = client
            .request(req)
            .await
            .map_err(|e| format!("request failed: {e}"))?;
        let status = resp.status();
        if status.is_redirection() {
            let loc = resp
                .headers()
                .get(hyper::header::LOCATION)
                .and_then(|v| v.to_str().ok())
                .ok_or("redirect without a Location header")?;
            // Never downgrade to plaintext on a redirect (except localhost tests).
            if !(loc.starts_with("https://") || (allow_plain && is_localhost_base(loc))) {
                return Err("refusing a non-HTTPS redirect".to_owned());
            }
            url = loc.to_owned();
            continue;
        }
        if !status.is_success() {
            return Err(format!("HTTP {status}"));
        }
        let bytes = resp
            .into_body()
            .collect()
            .await
            .map_err(|e| format!("download body error: {e}"))?
            .to_bytes();
        if bytes.len() > MAX_DOWNLOAD_BYTES {
            return Err("download exceeds the size cap".to_owned());
        }
        return Ok(bytes.to_vec());
    }
    Err("too many redirects".to_owned())
}

/// What a verification attempt produced. `result` is `Ok(archive bytes)` when
/// the download passed BOTH gates (so the apply path installs exactly the bytes
/// it verified), else the specific [`upd::VerifyError`].
struct DryRunOutcome {
    tag: String,
    asset: String,
    result: std::result::Result<Vec<u8>, upd::VerifyError>,
}

/// Download a release's archive + `.sha256` + `.minisig` for this platform and
/// verify them. Pure decision in [`trimwire::update::verify_artifact`]; this only
/// does the I/O. `pinned_tag` lets a caller (the apply path) pass an
/// already-resolved tag so the version it confirms/installs is the SAME one the
/// rest of the flow used — `None` resolves the latest here (the `--dry-run` use).
/// Returns `Err` for any orchestration failure (no network, no releases,
/// download/timeout) — which the caller treats as NOT verified (fail closed). A
/// present-but-bad artifact comes back as `Ok(outcome)` whose `result` is the
/// specific [`upd::VerifyError`].
fn verify_latest_release(pinned_tag: Option<String>) -> std::result::Result<DryRunOutcome, String> {
    let target = trimwire::build_target();
    let asset = upd::asset_name(target);
    let tag = match pinned_tag {
        Some(t) => t,
        None => fetch_latest_tag().ok_or(
            "couldn't determine the latest release (GitHub unreachable, rate-limited, or no releases)",
        )?,
    };

    // No pinned key ⇒ verification can't succeed regardless; report it without a
    // pointless download.
    let Some(pubkey) = pinned_pubkey() else {
        return Ok(DryRunOutcome {
            tag,
            asset,
            result: Err(upd::VerifyError::NoPinnedKey),
        });
    };

    let base = dl_base();
    let archive_url = format!("{base}/{}/releases/download/{tag}/{asset}", upd::REPO);
    let sha_url = format!("{archive_url}.sha256");
    let sig_url = format!("{archive_url}.minisig");

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("runtime: {e}"))?;
    rt.block_on(async {
        let client = trimwire::proxy::upstream::build_client();
        let dl = |u: String| {
            let c = client.clone();
            async move {
                tokio::time::timeout(Duration::from_secs(60), download_bytes(&c, &u))
                    .await
                    .map_err(|_| format!("download timed out: {u}"))?
            }
        };
        let archive = dl(archive_url.clone()).await?;
        let sha = dl(sha_url.clone()).await?;
        // A MISSING signature must fail closed (download error → propagated), never
        // be silently skipped.
        let sig = dl(sig_url.clone()).await?;
        let sha_text = String::from_utf8_lossy(&sha).into_owned();
        let sig_text = String::from_utf8_lossy(&sig).into_owned();
        let result = match upd::verify_artifact(&archive, &sha_text, &sig_text, &pubkey, &asset) {
            Ok(()) => Ok(archive),
            Err(e) => Err(e),
        };
        Ok::<_, String>(DryRunOutcome {
            tag: tag.clone(),
            asset: asset.clone(),
            result,
        })
    })
}

/// `--dry-run`: verify the latest release without changing anything. Returns the
/// process exit code: 0 = verified, 1 = NOT verified (any failure). Fail-closed.
fn run_dry_run() -> i32 {
    match verify_latest_release(None) {
        Err(e) => {
            eprintln!("trimwire update --dry-run: {e}\n→ treating as NOT verified (fail-closed).");
            1
        }
        Ok(o) => match o.result {
            Ok(_archive) => {
                println!(
                    "verified ✓  {} ({})\n  • SHA-256 checksum matches the published .sha256\n  \
                     • minisign signature is valid for the pinned key",
                    o.asset, o.tag
                );
                0
            }
            Err(ve) => {
                eprintln!(
                    "NOT verified ✗  {} ({})\n  • {ve}\n→ refusing to trust this download (fail-closed).",
                    o.asset, o.tag
                );
                if ve == upd::VerifyError::NoPinnedKey {
                    eprintln!(
                        "  (this build has no pinned update-signing key yet — see \
                         docs/UPDATE-COMMAND-SPIKE.md, \"Release signing — owner setup\".)"
                    );
                }
                1
            }
        },
    }
}

/// Resolve the running binary + eligibility (shared by the read-only check and
/// the apply path). Returns `(canonical exe, exe string, eligibility, receipt
/// path)`, or an `Err(message)` if the binary can't be resolved at all.
fn resolve_eligibility()
-> std::result::Result<(std::path::PathBuf, String, upd::Eligibility, String), String> {
    let receipt_path = trimwire::receipt::receipt_path().display().to_string();
    let exe = resolved_current_exe()?;
    let exe_str = exe.display().to_string();
    let mut receipt = trimwire::receipt::load();
    if let Some(r) = receipt.as_mut() {
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
    Ok((exe, exe_str, elig, receipt_path))
}

/// The read-only check (default `trimwire update` / `upgrade`). Refuses with
/// guidance for non-managed installs; otherwise reports current/available.
/// Network failure is non-fatal (exit 0).
fn run_check() -> ! {
    let (_, exe_str, elig, receipt_path) = match resolve_eligibility() {
        Ok(v) => v,
        Err(msg) => {
            eprintln!(
                "trimwire update: {msg}\n\n{}",
                upd::manual_update_guidance()
            );
            std::process::exit(2);
        }
    };
    if elig != upd::Eligibility::Eligible {
        eprintln!(
            "{}\n\n{}\n\nTo verify the next release before updating manually:\n\
             \x20     trimwire update --dry-run",
            upd::refusal_reason(&elig, &exe_str, &receipt_path),
            upd::manual_update_guidance()
        );
        std::process::exit(2);
    }

    let current = env!("CARGO_PKG_VERSION");
    match fetch_latest_tag().and_then(|t| upd::parse_version(&t).map(|v| (t, v))) {
        None => {
            eprintln!(
                "trimwire update: couldn't check for updates (GitHub unreachable or rate-limited). \
                 See https://github.com/{}/releases",
                upd::REPO
            );
            std::process::exit(0);
        }
        Some((tag, latest)) => {
            let cur = upd::parse_version(current).expect("own version parses");
            if upd::is_newer(latest, cur) {
                println!(
                    "trimwire {} is available (you have {current}).\n  • verify it:  trimwire update --dry-run\n  \
                     • apply it:   trimwire update --apply",
                    tag.trim_start_matches('v')
                );
            } else {
                println!("trimwire is already up to date ({current}).");
            }
            std::process::exit(0);
        }
    }
}

/// `trimwire update [--dry-run | --apply [--yes] | --yes]` (alias `upgrade`).
/// Default = read-only check. `--dry-run` = verify the latest release.
/// `--apply`/`--yes` = self-update (Linux managed installs only).
pub fn update(dry_run: bool, apply: bool, yes: bool) -> Result<()> {
    if dry_run {
        std::process::exit(run_dry_run());
    }
    if apply || yes {
        std::process::exit(run_apply(yes));
    }
    run_check()
}

// ── 4c: apply (self-update) ───────────────────────────────────────────────────
//
// Linux + managed (`script`) install only. The contract is fail-closed at every
// gate: we NEVER touch the on-disk binary unless the download verified against
// the pinned key, and on ANY post-swap health failure we roll back to the saved
// `.bak`. The actual replace/restart is Linux-gated at compile time; other
// platforms refuse before this runs.

/// Resolve the gateway listen address from config (for the post-restart health
/// probe). Falls back to the documented default. Linux-only (only the apply path
/// uses it).
#[cfg(target_os = "linux")]
fn listen_addr() -> std::net::SocketAddr {
    use trimwire::config::Config;
    Config::load()
        .map(|c| c.server.listen)
        .unwrap_or_else(|_| "127.0.0.1:8765".to_owned())
        .parse()
        .unwrap_or_else(|_| "127.0.0.1:8765".parse().expect("default addr parses"))
}

/// Test seam (localhost mode only): when set, the apply path verifies + confirms
/// as normal but stops BEFORE replacing the binary or restarting — so the full
/// gate sequence is integration-testable without ever overwriting the running
/// test binary. Ignored in production (api_base is never localhost there).
#[cfg(target_os = "linux")]
fn apply_is_dry_in_test() -> bool {
    is_localhost_base(&api_base())
        && std::env::var("TRIMWIRE_UPDATE_DRYRUN_APPLY").is_ok_and(|v| !v.is_empty())
}

/// Print an apply refusal and return exit code 2.
fn apply_refuse(msg: &str) -> i32 {
    eprintln!("{msg}");
    2
}

/// `--apply` / `--yes`. Returns the process exit code.
fn run_apply(yes: bool) -> i32 {
    // D2: self-replace is Linux-only in v1 (macOS Gatekeeper/notarization and the
    // Windows running-exe lock are out of scope — refuse, don't half-do it).
    if !cfg!(target_os = "linux") {
        return apply_refuse(&format!(
            "trimwire update --apply: self-update is only supported on Linux right now. \
             Update manually:\n\n{}",
            upd::manual_update_guidance()
        ));
    }

    // Eligibility (managed install, path/target match, writable) — path +
    // writability are re-checked here, never trusting the receipt method alone.
    let (exe, exe_str, elig, receipt_path) = match resolve_eligibility() {
        Ok(v) => v,
        Err(msg) => {
            return apply_refuse(&format!(
                "trimwire update --apply: {msg}\n\n{}",
                upd::manual_update_guidance()
            ));
        }
    };
    if elig != upd::Eligibility::Eligible {
        // Lead with the command so the user sees `--apply` was understood and
        // deliberately refused (not silently ignored), then the precise reason.
        return apply_refuse(&format!(
            "trimwire update --apply: cannot self-update this install.\n{}\n\n{}",
            upd::refusal_reason(&elig, &exe_str, &receipt_path),
            upd::manual_update_guidance()
        ));
    }

    // Anti-downgrade FIRST: a current install is a no-op regardless of key/TTY,
    // and only a STRICTLY newer release proceeds.
    let current = env!("CARGO_PKG_VERSION");
    let tag = match fetch_latest_tag() {
        Some(t) => t,
        None => {
            eprintln!(
                "trimwire update --apply: couldn't reach GitHub to check the latest release \
                 (fail-closed)."
            );
            return 1;
        }
    };
    match upd::parse_version(&tag) {
        Some(latest)
            if upd::is_newer(
                latest,
                upd::parse_version(current).expect("own version parses"),
            ) => {}
        Some(_) => {
            println!("trimwire is already up to date ({current}). Nothing to apply.");
            return 0;
        }
        None => {
            eprintln!(
                "trimwire update --apply: couldn't parse the latest tag '{tag}' (fail-closed)."
            );
            return 1;
        }
    }

    // A pinned key is mandatory — no key ⇒ can't verify ⇒ won't apply.
    if pinned_pubkey().is_none() {
        return apply_refuse(
            "trimwire update --apply: this build has no pinned update-signing key, so a download \
             can't be verified — refusing to self-update (fail-closed). See \
             docs/UPDATE-COMMAND-SPIKE.md.",
        );
    }

    // Confirmation: a non-interactive shell requires --yes; never apply unattended
    // without it. Checked before the (multi-MB) archive download so the refusal
    // avoids a pointless transfer — the version check above is a single small GET.
    let interactive = std::io::stdin().is_terminal();
    if !yes && !interactive {
        return apply_refuse(
            "trimwire update --apply: refusing to self-update without confirmation in a \
             non-interactive shell. Re-run with --yes to apply unattended.",
        );
    }

    // Download + verify (checksum THEN pinned-key signature). The verified bytes
    // are exactly what we install. Pass the tag we already resolved so the version
    // we install + health-check is the same one checked above (no second fetch).
    let archive = match verify_latest_release(Some(tag.clone())) {
        Ok(o) => match o.result {
            Ok(bytes) => bytes,
            Err(ve) => {
                eprintln!(
                    "trimwire update --apply: download did NOT verify — {ve}\n→ refusing to apply (fail-closed)."
                );
                return 1;
            }
        },
        Err(e) => {
            eprintln!("trimwire update --apply: {e}\n→ refusing to apply (fail-closed).");
            return 1;
        }
    };

    // Interactive confirmation (only reached on a TTY without --yes).
    if !yes
        && !confirm(&format!(
            "Update trimwire {current} → {tag} and restart the service?"
        ))
    {
        println!("Cancelled. Nothing was changed.");
        return 0;
    }

    apply_verified(&exe, &archive, &tag, current)
}

/// Read a `[y/N]` confirmation from stdin (default No).
fn confirm(prompt: &str) -> bool {
    use std::io::Write;
    print!("{prompt} [y/N] ");
    let _ = std::io::stdout().flush();
    let mut line = String::new();
    if std::io::stdin().read_line(&mut line).is_err() {
        return false;
    }
    matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes")
}

/// Install the already-verified `archive` over `exe`, then restart + health-check;
/// roll back on any failure. Linux-only body (callers gate non-Linux).
#[cfg(target_os = "linux")]
fn apply_verified(exe: &std::path::Path, archive: &[u8], tag: &str, old_version: &str) -> i32 {
    // Test seam: stop before mutating anything (keeps the apply path fully
    // exercisable in integration tests without overwriting the test binary).
    if apply_is_dry_in_test() {
        println!(
            "[test] verified — would replace {} and restart (test seam active, no changes made).",
            exe.display()
        );
        return 0;
    }

    let new_bytes = match extract_trimwire(archive) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("trimwire update --apply: {e}\n→ nothing was changed (fail-closed).");
            return 1;
        }
    };

    let addr = listen_addr();
    let bak = match atomic_replace(exe, &new_bytes) {
        Ok(b) => b,
        Err(e) => {
            eprintln!(
                "trimwire update --apply: failed to replace the binary: {e}\n→ nothing was changed (fail-closed)."
            );
            return 1;
        }
    };

    // Restart and confirm the new version is actually serving.
    match restart_and_verify(addr, tag) {
        Ok(()) => {
            // Refresh the receipt's recorded version, best-effort.
            let _ = trimwire::receipt::refresh_for_current_binary();
            let _ = std::fs::remove_file(&bak);
            println!(
                "Updated trimwire {old_version} → {} and restarted the service ✓",
                tag.trim_start_matches('v')
            );
            0
        }
        Err(restart_err) => {
            eprintln!(
                "trimwire update --apply: the updated gateway did not come up healthy ({restart_err})."
            );
            eprintln!("→ rolling back to the previous binary…");
            match rollback(exe, &bak, addr) {
                Ok(()) => {
                    eprintln!(
                        "Rolled back to {old_version} and the service is healthy again. The update was NOT applied."
                    );
                    1
                }
                Err(rb) => {
                    // Never swallow a rollback failure — the user must act. Exit 3
                    // (distinct from the clean-rollback 1) so wrapping automation can
                    // tell "safe failure, old version running" from "indeterminate
                    // state, manual restore needed".
                    eprintln!(
                        "CRITICAL: rollback FAILED ({rb}). The previous binary is saved at {}. \
                         Restore it manually and run `trimwire on`:\n    cp {} {} && trimwire on",
                        bak.display(),
                        bak.display(),
                        exe.display()
                    );
                    3
                }
            }
        }
    }
}

#[cfg(not(target_os = "linux"))]
fn apply_verified(_exe: &std::path::Path, _archive: &[u8], _tag: &str, _old_version: &str) -> i32 {
    // Unreachable: run_apply refuses non-Linux before this. Present so the crate
    // compiles on every target.
    apply_refuse("trimwire update --apply: self-update is only supported on Linux right now.")
}

/// Extract the single `trimwire` member from a `.tar.gz` to memory via `tar`.
#[cfg(target_os = "linux")]
fn extract_trimwire(archive: &[u8]) -> std::result::Result<Vec<u8>, String> {
    use std::io::Write;
    let tmp = std::env::temp_dir().join(format!("trimwire-update-{}.tar.gz", std::process::id()));
    // create_new (O_EXCL) so a pre-planted symlink at this path can't redirect the
    // write to an attacker-chosen file (matches `atomic_replace`'s discipline).
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&tmp)
        .map_err(|e| format!("create temp archive: {e}"))?;
    f.write_all(archive)
        .map_err(|e| format!("write temp archive: {e}"))?;
    drop(f);
    let out = std::process::Command::new("tar")
        .arg("-xzOf")
        .arg(&tmp)
        .arg("trimwire")
        .output();
    let _ = std::fs::remove_file(&tmp);
    let out = out.map_err(|e| format!("run tar: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "tar extraction failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    if out.stdout.is_empty() {
        return Err("extracted binary is empty".to_owned());
    }
    Ok(out.stdout)
}

/// Atomic-ish replace on Linux: write `new_bytes` to a temp file in the SAME dir
/// (so `rename` is on one filesystem — no EXDEV), `fchmod 0755`, fsync the file,
/// back up the current binary to `<exe>.bak`, `rename` over the exe, fsync the
/// dir. Returns the backup path. The running process keeps its open inode.
#[cfg(target_os = "linux")]
fn atomic_replace(
    exe: &std::path::Path,
    new_bytes: &[u8],
) -> std::result::Result<std::path::PathBuf, String> {
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;

    let dir = exe
        .parent()
        .ok_or_else(|| "the binary has no parent directory".to_owned())?;
    let tmp = dir.join(format!(".trimwire-update.{}", std::process::id()));

    {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)
            .map_err(|e| format!("create temp file: {e}"))?;
        f.write_all(new_bytes)
            .map_err(|e| format!("write temp file: {e}"))?;
        f.set_permissions(std::fs::Permissions::from_mode(0o755))
            .map_err(|e| format!("chmod temp file: {e}"))?;
        f.sync_all().map_err(|e| format!("fsync temp file: {e}"))?;
    }

    let bak = backup_path(exe);
    if let Err(e) = std::fs::copy(exe, &bak) {
        let _ = std::fs::remove_file(&tmp);
        return Err(format!("back up current binary: {e}"));
    }
    if let Err(e) = std::fs::rename(&tmp, exe) {
        let _ = std::fs::remove_file(&tmp);
        return Err(format!("rename new binary into place: {e}"));
    }
    if let Ok(d) = std::fs::File::open(dir) {
        let _ = d.sync_all(); // durability of the rename; best-effort
    }
    Ok(bak)
}

/// `<exe>.bak` next to the binary.
#[cfg(target_os = "linux")]
fn backup_path(exe: &std::path::Path) -> std::path::PathBuf {
    let name = exe
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "trimwire".to_owned());
    exe.with_file_name(format!("{name}.bak"))
}

/// Restart the service and confirm the freshly-started gateway reports the target
/// version on `/healthz` (proves the new binary is actually serving).
#[cfg(target_os = "linux")]
fn restart_and_verify(
    addr: std::net::SocketAddr,
    want_tag: &str,
) -> std::result::Result<(), String> {
    super::service::off().map_err(|e| format!("stop service: {e}"))?;
    super::service::on().map_err(|e| format!("start service: {e}"))?;
    let want = upd::parse_version(want_tag);
    for _ in 0..40 {
        if let Some(v) = super::service::healthz_version(addr) {
            if upd::parse_version(&v) == want && want.is_some() {
                return Ok(());
            }
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    Err(format!(
        "the restarted gateway did not report version {} on /healthz within ~10s",
        want_tag.trim_start_matches('v')
    ))
}

/// Roll back to the saved `.bak`: stop, restore, restart, re-check health. Any
/// failure here is surfaced loudly (never swallowed).
#[cfg(target_os = "linux")]
fn rollback(
    exe: &std::path::Path,
    bak: &std::path::Path,
    addr: std::net::SocketAddr,
) -> std::result::Result<(), String> {
    super::service::off().map_err(|e| format!("stop service: {e}"))?;
    std::fs::rename(bak, exe).map_err(|e| format!("restore backup: {e}"))?;
    if let Some(d) = exe.parent() {
        if let Ok(f) = std::fs::File::open(d) {
            let _ = f.sync_all();
        }
    }
    super::service::on().map_err(|e| format!("start service: {e}"))?;
    for _ in 0..40 {
        if super::service::healthz_ok(addr) {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    Err("restored the previous binary but the gateway did not come back healthy".to_owned())
}
