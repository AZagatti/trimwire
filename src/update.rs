//! Self-update support — PURE logic only (no network, no filesystem, no process
//! exec). The CLI wrapper (`src/cli/update.rs`) does the impure I/O (GitHub
//! query, download, `current_exe()`, writability probe, binary swap) and calls
//! into here.
//!
//! This module holds: version parse/compare + asset selection, the install
//! eligibility predicate, and the artifact-verification gates (SHA-256 checksum
//! + minisign/Ed25519 signature against a pinned key). Keeping every
//! trust-critical decision here — pure and unit-tested against real minisign
//! output — is what the download (`--dry-run`) and apply (`--apply`) paths build
//! on. See `docs/UPDATE-COMMAND-SPIKE.md`.

use crate::receipt::{self, InstallReceipt};

/// Canonical GitHub repo the installer + releases live under.
pub const REPO: &str = "AZagatti/trimwire";

/// A parsed `major.minor.patch` version. Ordering is the derived field order,
/// which gives correct numeric precedence (e.g. `0.10.0 > 0.9.0`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version {
    pub major: u32,
    pub minor: u32,
    pub patch: u32,
}

/// Parse a bare/`v`-prefixed semver, tolerant of trailing build/pre-release
/// noise so it accepts both a release tag (`v0.3.12`) and the binary's own
/// `--version` string (`0.3.12 (sha date)` → first whitespace token). Returns
/// `None` if the three numeric components aren't present.
pub fn parse_version(s: &str) -> Option<Version> {
    let s = s.trim();
    let s = s
        .strip_prefix('v')
        .or_else(|| s.strip_prefix('V'))
        .unwrap_or(s);
    // Drop anything after the first space: "0.3.12 (abc 2026-..)" → "0.3.12".
    let core = s.split_whitespace().next().unwrap_or("");
    let mut parts = core.split('.');
    let major = leading_u32(parts.next()?)?;
    let minor = leading_u32(parts.next()?)?;
    let patch = leading_u32(parts.next()?)?;
    Some(Version {
        major,
        minor,
        patch,
    })
}

/// Leading ASCII digits of `s` as u32 — so a patch like `12` parses, and a
/// pre-release like `0-rc.1` yields its numeric prefix (`0`). `None` if there's
/// no leading digit.
fn leading_u32(s: &str) -> Option<u32> {
    let digits: String = s.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok()
}

/// True only when `latest` is STRICTLY greater than `current` — so an equal or
/// older "latest" (e.g. a downgrade/rollback of the GitHub release) never reports
/// an update available. The actual apply path (a later phase) must also enforce
/// strictly-greater before replacing anything.
pub fn is_newer(latest: Version, current: Version) -> bool {
    latest > current
}

/// True only for an EXACT stable release tag: `vMAJOR.MINOR.PATCH` (a leading
/// `v`/`V` is allowed, each component is non-empty ASCII digits, nothing else).
/// Rejects prereleases (`v1.2.3-rc.1`), build metadata, extra components
/// (`1.2.3.4`), short forms (`1.2`), and any garbage (`v1.2.3@evil`, spaces,
/// newlines). The self-updater requires this before a tag is used to build an
/// asset URL or to health-check a restart, so release-metadata weirdness can't
/// steer the download or version comparison. (Stricter than [`parse_version`],
/// which is deliberately tolerant for reading a binary's own `--version`.)
pub fn is_stable_release_tag(s: &str) -> bool {
    let core = s
        .strip_prefix('v')
        .or_else(|| s.strip_prefix('V'))
        .unwrap_or(s);
    let mut parts = core.split('.');
    let (a, b, c) = (parts.next(), parts.next(), parts.next());
    if parts.next().is_some() {
        return false; // more than three components
    }
    match (a, b, c) {
        (Some(a), Some(b), Some(c)) => [a, b, c]
            .iter()
            .all(|p| !p.is_empty() && p.bytes().all(|x| x.is_ascii_digit())),
        _ => false,
    }
}

/// Release asset name for a target triple — `.zip` on Windows, `.tar.gz`
/// elsewhere. Matches `release.yml`'s packaging + the installer. (Pure helper;
/// not used by the read-only check, but the apply path will select the asset
/// with it.)
pub fn asset_name(target: &str) -> String {
    if target.contains("windows") {
        format!("trimwire-{target}.zip")
    } else {
        format!("trimwire-{target}.tar.gz")
    }
}

/// Whether a self-update is permissible for this install, and if not, exactly
/// why (so the CLI can print a precise refusal). Self-update is allowed ONLY for
/// a managed (`method="script"`) install whose recorded binary path matches the
/// running binary, whose target matches this build, and whose location is
/// writable — never trusting `method` alone (see [`crate::receipt`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Eligibility {
    Eligible,
    /// No install receipt on disk (cargo/manual install, or pre-install).
    NoReceipt,
    /// Receipt exists but method isn't `script` (cargo/binstall/manual).
    NotScriptInstall,
    /// Receipt's binary_path doesn't match the running binary.
    PathMismatch,
    /// Receipt's target triple doesn't match this build.
    TargetMismatch,
    /// The binary's directory isn't writable by the current user.
    NotWritable,
}

/// Pure eligibility decision. Callers pass already-resolved, already-canonical
/// values: `current_exe_canon` = canonicalized `current_exe()`, and the
/// `receipt`'s `binary_path` must likewise be canonicalized by the caller before
/// this call so the comparison is symlink-stable.
pub fn eligibility(
    receipt: Option<&InstallReceipt>,
    current_exe_canon: &str,
    target: &str,
    parent_writable: bool,
) -> Eligibility {
    let Some(r) = receipt else {
        return Eligibility::NoReceipt;
    };
    if r.method != receipt::METHOD_SCRIPT {
        return Eligibility::NotScriptInstall;
    }
    if r.binary_path != current_exe_canon {
        return Eligibility::PathMismatch;
    }
    if r.target != target {
        return Eligibility::TargetMismatch;
    }
    if !parent_writable {
        return Eligibility::NotWritable;
    }
    Eligibility::Eligible
}

/// The per-install-method update guidance (re-used verbatim from the previous
/// help-only stub, so cargo/manual users see the same actionable steps they
/// always have). Printed on every refusal.
pub fn manual_update_guidance() -> String {
    "Update with the method you installed with:\n\
     \x20 • curl | sh installer (re-run — it fetches the latest binary and re-runs install):\n\
     \x20     curl -LsSf https://raw.githubusercontent.com/AZagatti/trimwire/main/scripts/install.sh | sh\n\
     \x20 • cargo:\n\
     \x20     cargo binstall trimwire           # prebuilt, or\n\
     \x20     cargo install trimwire --locked   # from source\n\
     \x20 • manual binary — download the latest asset and replace the one on your PATH:\n\
     \x20     https://github.com/AZagatti/trimwire/releases/latest\n\
     \n\
     Then restart the service so the new binary serves:\n\
     \x20     trimwire off && trimwire on"
        .to_owned()
}

// ── 4b: artifact verification (PURE) ─────────────────────────────────────────
//
// Two independent gates, checked in order (checksum THEN signature). The
// `.sha256` only proves transit integrity (it is served from the same origin as
// the asset). The minisign/Ed25519 signature, verified against a key PINNED in
// this binary, is the provenance gate — an attacker who replaces the GitHub
// release asset and its checksum still cannot forge a signature without the
// owner's offline secret key. Both must pass; every error path fails CLOSED.
//
// All functions here are pure (no network, no filesystem) so the trust-critical
// decision is unit-testable end-to-end against real minisign output. The impure
// download/orchestration lives in `src/cli/update.rs`.

/// The minisign PUBLIC key pinned into this build (the base64 payload — the
/// SECOND line of a `minisign.pub` file, without the `untrusted comment:` line).
/// The matching SECRET key lives only with the release owner / in the signing
/// CI secret (`MINISIGN_SECRET_KEY`); see `docs/UPDATE-COMMAND-SPIKE.md`
/// ("Release signing — owner setup"). Key id `9DD74C076C33E227`. An empty pin
/// would make every verification fail closed ([`VerifyError::NoPinnedKey`]);
/// `pinned_pubkey_is_valid_minisign_key` guards that a pasted key actually
/// parses, so a malformed pin can't ship.
pub const PINNED_PUBKEY: &str = "RWQn4jNsB0zXnSYsszvH8ARk8/wYpp7sVtYxiV6W9dws/WVzJc1Pkm6i";

/// Whether this build has a usable pinned key. `false` ⇒ verification cannot be
/// attempted and the updater must refuse (fail-closed), not fall back to
/// checksum-only.
pub fn has_pinned_key() -> bool {
    !PINNED_PUBKEY.trim().is_empty()
}

/// Why an artifact failed verification. Distinct variants so the CLI can print a
/// precise, non-misleading reason — and so tests can assert the exact gate that
/// tripped. There is deliberately NO "verified despite X" variant: anything
/// other than `Ok(())` from [`verify_artifact`] means "do not trust this file".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VerifyError {
    /// This build has no pinned public key, so provenance can't be checked.
    NoPinnedKey,
    /// The `.sha256` file couldn't be parsed (no `<64-hex>  <name>` line).
    MalformedChecksum,
    /// The download's SHA-256 doesn't match the published `.sha256`.
    ChecksumMismatch { expected: String, got: String },
    /// The pinned public key isn't valid minisign base64 (build misconfigured).
    MalformedPublicKey,
    /// The `.minisig` couldn't be parsed.
    MalformedSignature,
    /// The signature was made by a DIFFERENT key than the pinned one.
    KeyIdMismatch,
    /// A legacy (non-prehashed) signature — rejected; we require `minisign -H`.
    LegacySignature,
    /// The signature does not validate the downloaded bytes.
    SignatureMismatch,
}

impl std::fmt::Display for VerifyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            VerifyError::NoPinnedKey => write!(
                f,
                "this build has no pinned update-signing key, so the download can't be verified"
            ),
            VerifyError::MalformedChecksum => {
                write!(f, "the published .sha256 file is missing or malformed")
            }
            VerifyError::ChecksumMismatch { expected, got } => write!(
                f,
                "checksum mismatch (expected {expected}, got {got}) — the download is corrupt or has been tampered with"
            ),
            VerifyError::MalformedPublicKey => {
                write!(f, "the pinned public key is invalid (build misconfigured)")
            }
            VerifyError::MalformedSignature => {
                write!(f, "the .minisig signature file is missing or malformed")
            }
            VerifyError::KeyIdMismatch => write!(
                f,
                "the signature was made by a different key than the one pinned in this build"
            ),
            VerifyError::LegacySignature => write!(
                f,
                "the signature uses a legacy (non-prehashed) format, which is not accepted"
            ),
            VerifyError::SignatureMismatch => write!(
                f,
                "the signature does not match the downloaded bytes — do not trust this file"
            ),
        }
    }
}

/// The expected lowercase hex SHA-256 for `asset` parsed from a `sha256sum`-style
/// file. Accepts a single bare-hash line, or one-or-more `<hex>  <name>` lines
/// (matching by file name), exactly the two shapes `release.yml` produces on
/// Unix (`sha256sum`) and Windows (`"$hash  $asset"`). Returns `None` if no
/// well-formed 64-hex digest for the asset is present.
pub fn expected_sha256(sha256_file: &str, asset: &str) -> Option<String> {
    let is_hex64 = |s: &str| s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit());
    for line in sha256_file.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let mut parts = line.split_whitespace();
        let hash = parts.next().unwrap_or("");
        if !is_hex64(hash) {
            continue;
        }
        match parts.next() {
            // `<hex>  <name>` — accept only on an EXACT name match (after dropping
            // the sha256sum `*` binary-mode marker and a leading `./`). A suffix
            // match would let a crafted `evil-<asset>` entry in a multi-file
            // manifest satisfy the checksum gate for the wrong file.
            Some(name) => {
                let name = name.trim_start_matches('*').trim_start_matches("./");
                if name == asset {
                    return Some(hash.to_ascii_lowercase());
                }
            }
            // A bare `<hex>` line (no file name) — the checksum for this asset.
            None => return Some(hash.to_ascii_lowercase()),
        }
    }
    None
}

/// Gate 1: the download's SHA-256 must equal the published checksum. Fails closed
/// on a malformed checksum file or any mismatch.
pub fn verify_sha256(data: &[u8], sha256_file: &str, asset: &str) -> Result<(), VerifyError> {
    use sha2::{Digest, Sha256};
    let expected = expected_sha256(sha256_file, asset).ok_or(VerifyError::MalformedChecksum)?;
    let got = hex::encode(Sha256::digest(data));
    if got == expected {
        Ok(())
    } else {
        Err(VerifyError::ChecksumMismatch { expected, got })
    }
}

/// Gate 2: the `.minisig` must be a valid signature over `data` made by the key
/// whose base64 is `pubkey_b64`. Requires a prehashed signature (`minisign -H`);
/// legacy signatures are rejected (`allow_legacy = false`). Fails closed on a
/// malformed key/signature, a key-id mismatch, or a bad signature.
pub fn verify_minisig(data: &[u8], minisig: &str, pubkey_b64: &str) -> Result<(), VerifyError> {
    use minisign_verify::{Error as MvError, PublicKey, Signature};
    let pk =
        PublicKey::from_base64(pubkey_b64.trim()).map_err(|_| VerifyError::MalformedPublicKey)?;
    let sig = Signature::decode(minisig).map_err(|_| VerifyError::MalformedSignature)?;
    // allow_legacy = false → only prehashed (`-H`) signatures pass; a legacy
    // signature returns UnexpectedAlgorithm, which we surface as LegacySignature.
    match pk.verify(data, &sig, false) {
        Ok(()) => Ok(()),
        Err(MvError::UnexpectedKeyId) => Err(VerifyError::KeyIdMismatch),
        Err(MvError::UnexpectedAlgorithm) => Err(VerifyError::LegacySignature),
        Err(_) => Err(VerifyError::SignatureMismatch),
    }
}

/// Verify a downloaded artifact end-to-end: checksum FIRST (cheap, catches
/// corruption), then the pinned-key signature (the real provenance gate). Both
/// must pass. `pubkey_b64` empty ⇒ [`VerifyError::NoPinnedKey`] (never falls back
/// to checksum-only). This is the single entry point the updater trusts.
pub fn verify_artifact(
    data: &[u8],
    sha256_file: &str,
    minisig: &str,
    pubkey_b64: &str,
    asset: &str,
) -> Result<(), VerifyError> {
    if pubkey_b64.trim().is_empty() {
        return Err(VerifyError::NoPinnedKey);
    }
    verify_sha256(data, sha256_file, asset)?;
    verify_minisig(data, minisig, pubkey_b64)?;
    Ok(())
}

/// One-line reason for a non-eligible refusal (the guidance block follows it).
pub fn refusal_reason(e: &Eligibility, current_exe: &str, receipt_path: &str) -> String {
    match e {
        Eligibility::Eligible => String::new(),
        Eligibility::NoReceipt => format!(
            "trimwire: no install receipt at {receipt_path} — can't confirm this is a managed install, so self-update is not available."
        ),
        Eligibility::NotScriptInstall => {
            "trimwire: this binary wasn't installed by the curl|sh installer (cargo/manual install) — self-update is not available.".to_owned()
        }
        Eligibility::PathMismatch => format!(
            "trimwire: the running binary ({current_exe}) doesn't match the recorded install path — self-update would touch the wrong binary, so it's refused."
        ),
        Eligibility::TargetMismatch => {
            "trimwire: the install receipt's target triple doesn't match this build — self-update is refused.".to_owned()
        }
        Eligibility::NotWritable => format!(
            "trimwire: {current_exe} is not writable by the current user — self-update is refused (re-run via your install method, or with sufficient privileges)."
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(major: u32, minor: u32, patch: u32) -> Version {
        Version {
            major,
            minor,
            patch,
        }
    }

    #[test]
    fn parses_plain_and_v_prefixed() {
        assert_eq!(parse_version("0.3.12"), Some(v(0, 3, 12)));
        assert_eq!(parse_version("v0.3.12"), Some(v(0, 3, 12)));
        assert_eq!(parse_version("  v1.20.300 "), Some(v(1, 20, 300)));
    }

    #[test]
    fn parses_binary_version_string_with_build_metadata() {
        // The `--version` form embedded by build.rs.
        assert_eq!(
            parse_version("0.3.12 (abc1234 2026-06-23)"),
            Some(v(0, 3, 12))
        );
    }

    #[test]
    fn tolerates_prerelease_suffix_on_patch() {
        assert_eq!(parse_version("v0.4.0-rc.1"), Some(v(0, 4, 0)));
    }

    #[test]
    fn rejects_garbage() {
        assert_eq!(parse_version("not-a-version"), None);
        assert_eq!(parse_version("1.2"), None);
        assert_eq!(parse_version(""), None);
    }

    #[test]
    fn newer_is_strictly_greater() {
        assert!(is_newer(v(0, 3, 13), v(0, 3, 12)));
        assert!(is_newer(v(0, 10, 0), v(0, 9, 9))); // numeric, not lexicographic
        assert!(is_newer(v(1, 0, 0), v(0, 99, 99)));
        // equal and older are NOT newer (no spurious update; no downgrade).
        assert!(!is_newer(v(0, 3, 12), v(0, 3, 12)));
        assert!(!is_newer(v(0, 3, 11), v(0, 3, 12)));
    }

    #[test]
    fn stable_release_tag_is_strict() {
        // Accepted: exact vMAJOR.MINOR.PATCH (v/V optional).
        for ok in ["v0.3.13", "0.3.13", "V1.0.0", "v10.20.30", "1.2.3"] {
            assert!(is_stable_release_tag(ok), "should accept {ok}");
        }
        // Rejected: prerelease, build metadata, wrong arity, garbage, whitespace.
        for bad in [
            "v0.3.13-rc.1",
            "0.4.0-beta",
            "v1.2",
            "1.2.3.4",
            "v1.2.3@evil",
            "v1.2.x",
            "v1.2.3 ",
            " v1.2.3",
            "v1.2.3\n",
            "latest",
            "",
            "v..",
            "v1.2.",
        ] {
            assert!(!is_stable_release_tag(bad), "should reject {bad:?}");
        }
    }

    #[test]
    fn asset_name_per_platform() {
        assert_eq!(
            asset_name("x86_64-unknown-linux-gnu"),
            "trimwire-x86_64-unknown-linux-gnu.tar.gz"
        );
        assert_eq!(
            asset_name("aarch64-apple-darwin"),
            "trimwire-aarch64-apple-darwin.tar.gz"
        );
        assert_eq!(
            asset_name("x86_64-pc-windows-msvc"),
            "trimwire-x86_64-pc-windows-msvc.zip"
        );
    }

    fn receipt(method: &str, path: &str, target: &str) -> InstallReceipt {
        InstallReceipt {
            schema_version: receipt::SCHEMA_VERSION,
            method: method.to_owned(),
            binary_path: path.to_owned(),
            version: "0.3.12".to_owned(),
            target: target.to_owned(),
            installed_at: 0,
        }
    }

    #[test]
    fn eligibility_branches() {
        let exe = "/home/u/.local/bin/trimwire";
        let tgt = "x86_64-unknown-linux-gnu";

        // Happy path.
        let ok = receipt(receipt::METHOD_SCRIPT, exe, tgt);
        assert_eq!(
            eligibility(Some(&ok), exe, tgt, true),
            Eligibility::Eligible
        );

        // No receipt.
        assert_eq!(eligibility(None, exe, tgt, true), Eligibility::NoReceipt);

        // Not a script install.
        let cargo = receipt(receipt::METHOD_UNKNOWN, exe, tgt);
        assert_eq!(
            eligibility(Some(&cargo), exe, tgt, true),
            Eligibility::NotScriptInstall
        );

        // Path mismatch.
        let moved = receipt(receipt::METHOD_SCRIPT, "/usr/local/bin/trimwire", tgt);
        assert_eq!(
            eligibility(Some(&moved), exe, tgt, true),
            Eligibility::PathMismatch
        );

        // Target mismatch.
        let cross = receipt(receipt::METHOD_SCRIPT, exe, "aarch64-apple-darwin");
        assert_eq!(
            eligibility(Some(&cross), exe, tgt, true),
            Eligibility::TargetMismatch
        );

        // Not writable.
        assert_eq!(
            eligibility(Some(&ok), exe, tgt, false),
            Eligibility::NotWritable
        );
    }

    /// Regression for the post-apply receipt bug: the OLD updater wrote the
    /// receipt from the replaced process, so `binary_path` became
    /// `"<path> (deleted)"` → the NEXT `upgrade` check resolved `current_exe` to
    /// the clean path, mismatched, and refused with `PathMismatch`. The fix writes
    /// the canonical path post-apply, so a second check is `Eligible` again.
    #[test]
    fn deleted_suffix_path_mismatches_but_canonical_is_eligible() {
        let exe = "/home/u/.local/bin/trimwire";
        let tgt = "x86_64-unknown-linux-gnu";

        // What the bug produced: a receipt whose path carries the "(deleted)"
        // suffix the kernel reports for a replaced-while-running exe. The next
        // check (running the NEW binary, clean `current_exe`) can't match it.
        let poisoned = receipt(receipt::METHOD_SCRIPT, &format!("{exe} (deleted)"), tgt);
        assert_eq!(
            eligibility(Some(&poisoned), exe, tgt, true),
            Eligibility::PathMismatch,
            "the poisoned (deleted) path is exactly what refused the 2nd upgrade"
        );

        // What the fix writes: the canonical install path → second check passes.
        let healed = receipt(receipt::METHOD_SCRIPT, exe, tgt);
        assert_eq!(
            eligibility(Some(&healed), exe, tgt, true),
            Eligibility::Eligible,
            "after a correct post-apply refresh the next upgrade is eligible"
        );
    }

    #[test]
    fn guidance_and_reasons_are_actionable() {
        let g = manual_update_guidance();
        assert!(g.contains("install.sh"));
        assert!(g.contains("cargo install trimwire"));
        assert!(g.contains("releases/latest"));
        // Each refusal reason is non-empty and distinct-ish.
        for e in [
            Eligibility::NoReceipt,
            Eligibility::NotScriptInstall,
            Eligibility::PathMismatch,
            Eligibility::TargetMismatch,
            Eligibility::NotWritable,
        ] {
            assert!(!refusal_reason(&e, "/x", "/y").is_empty());
        }
        assert!(refusal_reason(&Eligibility::Eligible, "/x", "/y").is_empty());
    }

    // ── 4b verification fixtures ──────────────────────────────────────────────
    //
    // Sign with the `minisign` crate (dev-dep) and verify with the SAME runtime
    // path (`minisign-verify`) the updater uses, so these exercise the exact
    // bytes a real `minisign -H` release signature produces — not hand-rolled
    // fixtures that could drift from the format.

    struct Fixture {
        data: Vec<u8>,
        sha256_file: String,
        minisig: String,
        pubkey: String,
        asset: &'static str,
    }

    fn make_fixture() -> Fixture {
        use minisign::{KeyPair, sign};
        use sha2::{Digest, Sha256};
        let asset = "trimwire-x86_64-unknown-linux-gnu.tar.gz";
        let data = b"fake release archive bytes \x00\x01\x02 trimwire".to_vec();
        let kp = KeyPair::generate_unencrypted_keypair().expect("keypair");
        // `minisign::sign` always produces a PREHASHED signature (the `-H` form),
        // which is what the release workflow emits and what we require.
        let sig_box = sign(
            Some(&kp.pk),
            &kp.sk,
            std::io::Cursor::new(&data),
            Some("trusted comment: trimwire test fixture"),
            None,
        )
        .expect("sign");
        let minisig: String = sig_box.into();
        let sha = hex::encode(Sha256::digest(&data));
        Fixture {
            data,
            sha256_file: format!("{sha}  {asset}\n"),
            minisig,
            pubkey: kp.pk.to_base64(),
            asset,
        }
    }

    /// Flip one character on a given (0-based) line of a minisig, keeping length
    /// so it still parses — used to corrupt a specific field.
    fn corrupt_line(minisig: &str, line_idx: usize) -> String {
        let mut out = String::new();
        for (i, l) in minisig.lines().enumerate() {
            if i == line_idx && !l.is_empty() {
                let mut chars: Vec<char> = l.chars().collect();
                let mid = chars.len() / 2;
                chars[mid] = if chars[mid] == 'A' { 'B' } else { 'A' };
                out.push_str(&chars.into_iter().collect::<String>());
            } else {
                out.push_str(l);
            }
            out.push('\n');
        }
        out
    }

    #[test]
    fn verify_artifact_accepts_a_valid_signed_download() {
        let f = make_fixture();
        assert_eq!(
            verify_artifact(&f.data, &f.sha256_file, &f.minisig, &f.pubkey, f.asset),
            Ok(())
        );
    }

    #[test]
    fn verify_refuses_when_no_key_is_pinned_no_checksum_only_fallback() {
        let f = make_fixture();
        // An empty pinned key ⇒ NoPinnedKey, NEVER a checksum-only "ok". (This is
        // the pure fail-closed guarantee regardless of what this build embeds.)
        assert_eq!(
            verify_artifact(&f.data, &f.sha256_file, &f.minisig, "", f.asset),
            Err(VerifyError::NoPinnedKey)
        );
    }

    #[test]
    fn pinned_pubkey_is_valid_minisign_key() {
        // The shipped build embeds a real key, and it must be a parseable minisign
        // public key — guards against a malformed/truncated paste shipping.
        assert!(has_pinned_key(), "release builds must embed a pinned key");
        assert!(
            minisign_verify::PublicKey::from_base64(PINNED_PUBKEY).is_ok(),
            "PINNED_PUBKEY must be a valid minisign public-key payload"
        );
    }

    #[test]
    fn verify_rejects_tampered_artifact_at_checksum() {
        let mut f = make_fixture();
        f.data[0] ^= 0xff; // tamper AFTER signing + checksumming
        assert!(matches!(
            verify_artifact(&f.data, &f.sha256_file, &f.minisig, &f.pubkey, f.asset),
            Err(VerifyError::ChecksumMismatch { .. })
        ));
    }

    #[test]
    fn verify_rejects_tampered_artifact_at_signature_even_if_checksum_recomputed() {
        // Same-origin attacker swaps the artifact AND its .sha256 — the signature
        // gate (pinned key) still fails closed.
        use sha2::{Digest, Sha256};
        let mut f = make_fixture();
        f.data[0] ^= 0xff;
        let sha = hex::encode(Sha256::digest(&f.data));
        f.sha256_file = format!("{sha}  {}\n", f.asset);
        assert_eq!(
            verify_artifact(&f.data, &f.sha256_file, &f.minisig, &f.pubkey, f.asset),
            Err(VerifyError::SignatureMismatch)
        );
    }

    #[test]
    fn verify_rejects_tampered_signature() {
        let f = make_fixture();
        // Corrupt the global-signature line (index 3): still parses, fails verify.
        let bad = corrupt_line(&f.minisig, 3);
        assert_eq!(
            verify_minisig(&f.data, &bad, &f.pubkey),
            Err(VerifyError::SignatureMismatch)
        );
    }

    #[test]
    fn verify_rejects_legacy_non_prehashed_signature() {
        use ct_codecs::{Base64, Decoder, Encoder};
        let f = make_fixture();
        // The fixture is prehashed (`minisign -H`): line 1 decodes to
        // [sig_alg(2), key_id(8), sig(64)] with sig_alg = "ED" (0x45,0x44). Flip
        // the second byte to 0x64 ("Ed") → a legacy, non-prehashed signature that
        // still parses (key_id unchanged, so no KeyIdMismatch) — it must be
        // rejected by the `allow_legacy = false` gate.
        let mut lines: Vec<String> = f.minisig.lines().map(str::to_owned).collect();
        let mut bin1 = Base64::decode_to_vec(lines[1].as_bytes(), None).expect("decode sig line");
        assert_eq!(bin1[0], 0x45);
        assert_eq!(bin1[1], 0x44, "fixture must be prehashed (ED)");
        bin1[1] = 0x64; // "Ed" = legacy / non-prehashed
        lines[1] = Base64::encode_to_string(&bin1).expect("re-encode sig line");
        let legacy = format!("{}\n", lines.join("\n"));
        assert_eq!(
            verify_minisig(&f.data, &legacy, &f.pubkey),
            Err(VerifyError::LegacySignature)
        );
    }

    #[test]
    fn verify_rejects_wrong_key() {
        let f = make_fixture();
        let other = make_fixture(); // a DIFFERENT keypair
        assert_eq!(
            verify_minisig(&f.data, &f.minisig, &other.pubkey),
            Err(VerifyError::KeyIdMismatch)
        );
    }

    #[test]
    fn verify_rejects_missing_or_malformed_signature() {
        let f = make_fixture();
        assert_eq!(
            verify_minisig(&f.data, "", &f.pubkey),
            Err(VerifyError::MalformedSignature)
        );
        assert_eq!(
            verify_minisig(&f.data, "not a minisig file", &f.pubkey),
            Err(VerifyError::MalformedSignature)
        );
    }

    #[test]
    fn verify_rejects_malformed_pinned_key() {
        let f = make_fixture();
        assert_eq!(
            verify_minisig(&f.data, &f.minisig, "@@@ not base64 @@@"),
            Err(VerifyError::MalformedPublicKey)
        );
    }

    #[test]
    fn checksum_parsing_handles_real_release_shapes() {
        let h = "a".repeat(64);
        let asset = "trimwire-x86_64-unknown-linux-gnu.tar.gz";
        // Unix `sha256sum` form: "<hex>  <name>".
        assert_eq!(
            expected_sha256(&format!("{h}  {asset}"), asset),
            Some(h.clone())
        );
        // Windows binary-mode "*" prefix + uppercase hex → normalized to lower.
        assert_eq!(
            expected_sha256(&format!("{}  *{asset}", h.to_uppercase()), asset),
            Some(h.clone())
        );
        // Bare hash with no file name.
        assert_eq!(expected_sha256(&h, asset), Some(h.clone()));
        // Malformed (not 64 hex, or wrong asset) → None.
        assert_eq!(expected_sha256("deadbeef  other.tar.gz", asset), None);
        assert_eq!(expected_sha256(&format!("{h}  other.tar.gz"), asset), None);
        assert_eq!(expected_sha256("not a checksum line", asset), None);
    }

    #[test]
    fn verify_sha256_reports_malformed_vs_mismatch() {
        let f = make_fixture();
        // Malformed checksum file.
        assert_eq!(
            verify_sha256(&f.data, "garbage", f.asset),
            Err(VerifyError::MalformedChecksum)
        );
        // Well-formed but wrong digest.
        let wrong = format!("{}  {}\n", "b".repeat(64), f.asset);
        assert!(matches!(
            verify_sha256(&f.data, &wrong, f.asset),
            Err(VerifyError::ChecksumMismatch { .. })
        ));
        // Correct digest passes.
        assert_eq!(verify_sha256(&f.data, &f.sha256_file, f.asset), Ok(()));
    }
}
