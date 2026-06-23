//! Self-update support — PURE logic only (no network, no filesystem, no process
//! exec). The CLI wrapper (`src/cli/update.rs`) does the impure I/O (GitHub
//! query, `current_exe()`, writability probe) and calls into here.
//!
//! Scope today: a **read-only** check. There is NO download / verification /
//! binary replacement / service restart yet — see `docs/UPDATE-COMMAND-SPIKE.md`
//! for the full phased plan (this is phase 4a). Keeping the decision logic here,
//! pure and unit-tested, is what later phases build the apply path on.

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
}
