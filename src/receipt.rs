//! Install receipt — a small JSON record of HOW trimwire was installed.
//!
//! Written by `scripts/install.sh` (the curl|sh installer) and refreshed by
//! `trimwire install`. `trimwire upgrade` reads it to decide whether
//! self-update is allowed (only a managed `method = "script"` install is
//! self-replaceable) or whether to refuse and redirect the user to their
//! package manager. A **missing or unparseable receipt is non-fatal** and means
//! "manual/unknown" — i.e. NOT self-updatable, the safe default. Nothing here
//! makes network calls or performs any update; it only records facts.
//!
//! Location: `$XDG_DATA_HOME/trimwire/install-receipt.json`, else
//! `$HOME/.local/share/trimwire/install-receipt.json` (mirrors how
//! [`crate::config::global_config_path`] resolves `XDG_CONFIG_HOME`).

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Current receipt schema version. Bump on a breaking field change so a future
/// reader can migrate or ignore older receipts.
pub const SCHEMA_VERSION: u32 = 1;

/// Managed install via `scripts/install.sh` — self-updatable by `trimwire upgrade`.
pub const METHOD_SCRIPT: &str = "script";
/// Anything else (cargo, `cargo binstall`, manual download): not a managed
/// install, so NOT self-updatable. The default when no prior receipt exists.
pub const METHOD_UNKNOWN: &str = "unknown";

/// A record of how the currently-installed trimwire binary got there. Kept
/// deliberately small and content-free.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstallReceipt {
    /// Schema version of this receipt ([`SCHEMA_VERSION`]).
    pub schema_version: u32,
    /// How it was installed: [`METHOD_SCRIPT`] or [`METHOD_UNKNOWN`].
    pub method: String,
    /// Absolute path of the installed binary at write time.
    pub binary_path: String,
    /// Crate version recorded at write time, e.g. `0.3.11`.
    pub version: String,
    /// Build target triple, e.g. `x86_64-unknown-linux-gnu`.
    pub target: String,
    /// Unix epoch seconds when the receipt was written.
    pub installed_at: i64,
}

/// `$XDG_DATA_HOME/trimwire/install-receipt.json`, else
/// `$HOME/.local/share/trimwire/install-receipt.json`, else a bare relative path
/// (last resort when neither env var is set). Mirrors
/// [`crate::config::global_config_path`]'s XDG handling.
pub fn receipt_path() -> PathBuf {
    data_dir().join("install-receipt.json")
}

fn data_dir() -> PathBuf {
    if let Ok(xdg) = std::env::var("XDG_DATA_HOME") {
        if !xdg.is_empty() {
            return PathBuf::from(xdg).join("trimwire");
        }
    }
    if let Ok(home) = std::env::var("HOME") {
        return PathBuf::from(home)
            .join(".local")
            .join("share")
            .join("trimwire");
    }
    PathBuf::from("trimwire")
}

/// Load the receipt if present and parseable. Returns `None` on a missing or
/// corrupt/older-schema file — never an error, because no receipt is the normal
/// state for cargo/manual installs.
pub fn load() -> Option<InstallReceipt> {
    let s = std::fs::read_to_string(receipt_path()).ok()?;
    serde_json::from_str(&s).ok()
}

/// The `method` to record when refreshing: preserve an existing one (so running
/// `trimwire install` right after the curl|sh installer keeps `method="script"`
/// rather than downgrading it), else default to [`METHOD_UNKNOWN`] (a
/// cargo/manual install we did not place and must not assume is self-updatable).
fn refreshed_method(existing: Option<&InstallReceipt>) -> String {
    existing
        .map(|r| r.method.clone())
        .unwrap_or_else(|| METHOD_UNKNOWN.to_owned())
}

/// Write `receipt` atomically (temp file in the same dir + rename), creating the
/// data dir if needed. Best-effort: callers treat any error as non-fatal.
pub fn write(receipt: &InstallReceipt) -> std::io::Result<()> {
    let path = receipt_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_string_pretty(receipt)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    let tmp = tmp_path(&path);
    std::fs::write(&tmp, json)?;
    std::fs::rename(&tmp, &path).inspect_err(|_| {
        // Don't leave an orphaned `*.tmp.<pid>` behind on a rename failure
        // (matches `write_text_atomic` in cli/install.rs).
        let _ = std::fs::remove_file(&tmp);
    })
}

fn tmp_path(path: &Path) -> PathBuf {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "install-receipt.json".to_owned());
    path.with_file_name(format!("{name}.tmp.{}", std::process::id()))
}

/// Write/refresh the receipt for the binary running this process. Preserves the
/// recorded `method` if a receipt already exists (so the curl|sh installer's
/// `script` method survives a subsequent `trimwire install`), else records
/// [`METHOD_UNKNOWN`]. Refreshes path/version/target/timestamp. Best-effort —
/// returns the I/O error for the caller to ignore.
///
/// CAVEAT for `trimwire upgrade`: `method` is preserved blindly, so a
/// `method="script"` receipt can go stale if the user later `cargo install`s
/// over the same path. The receipt is therefore NOT a sufficient authority on
/// its own — before self-replacing, the updater MUST also verify the binary it's
/// about to overwrite is the managed one (e.g. `binary_path == current_exe()`
/// and the path is user-writable), not trust `method` alone. The safe default
/// (missing/`unknown` → refuse self-update) still holds regardless.
pub fn refresh_for_current_binary() -> std::io::Result<()> {
    let method = refreshed_method(load().as_ref());
    let binary_path = std::env::current_exe()
        .map(|p| p.display().to_string())
        .unwrap_or_default();
    write(&InstallReceipt {
        schema_version: SCHEMA_VERSION,
        method,
        binary_path,
        version: env!("CARGO_PKG_VERSION").to_owned(),
        target: crate::build_target().to_owned(),
        installed_at: now_secs(),
    })
}

/// Refresh the receipt after a successful self-update, using an EXPLICIT
/// installed path and version instead of reading them from the running process.
///
/// `trimwire upgrade` calls this from the OLD process whose executable has just
/// been atomically replaced on disk. In that process [`refresh_for_current_binary`]
/// is WRONG twice over: on Linux `current_exe()` resolves to `"<path> (deleted)"`
/// (the inode was renamed over), and `env!("CARGO_PKG_VERSION")` is still the OLD
/// binary's version. A receipt written that way poisons the next `trimwire
/// upgrade` (its `binary_path` can't canonicalize → [`crate::update::Eligibility::PathMismatch`]).
/// So the updater passes the known-good canonical install path (the binary it
/// just replaced — already verified not to be `(deleted)`) and the freshly
/// installed version (the release tag, `v`-stripped) directly.
///
/// Preserves the recorded `method` (a self-update only happens for a managed
/// `script` install), and refreshes target + timestamp. Best-effort.
pub fn refresh_after_apply(binary_path: &str, version: &str) -> std::io::Result<()> {
    write(&receipt_after_apply(
        load().as_ref(),
        binary_path,
        version,
        now_secs(),
    ))
}

/// Pure record-builder for [`refresh_after_apply`] (no I/O — unit-testable). The
/// `binary_path` and `version` come from the updater's known-good context (the
/// canonical install path it replaced + the release tag), NOT from the running
/// process — see [`refresh_after_apply`] for why that matters.
fn receipt_after_apply(
    existing: Option<&InstallReceipt>,
    binary_path: &str,
    version: &str,
    now: i64,
) -> InstallReceipt {
    InstallReceipt {
        schema_version: SCHEMA_VERSION,
        method: refreshed_method(existing),
        binary_path: binary_path.to_owned(),
        version: version.to_owned(),
        target: crate::build_target().to_owned(),
        installed_at: now,
    }
}

/// The exact suffix the Linux kernel appends to `/proc/self/exe` when the
/// running executable's on-disk path has been replaced (renamed over). The
/// pre-fix `trimwire upgrade` (≤ v0.3.13) wrote this into the receipt's
/// `binary_path` because it refreshed from the replaced process — see
/// [`refresh_after_apply`].
const DELETED_SUFFIX: &str = " (deleted)";

/// Compatibility self-heal for a legacy poisoned receipt written by the v0.3.13
/// updater bug. If `receipt` is a managed (`script`) receipt whose `binary_path`
/// ends in the kernel's `" (deleted)"` marker, AND the stripped path
/// canonicalizes to the SAME inode as `current_exe_canon` (the running binary),
/// rewrite it in place to the canonical current path + current running version
/// (`method`/`target` preserved) and persist it best-effort. Returns `true` when
/// it repaired.
///
/// Deliberately narrow — it is a one-time migration, NOT a general "accept a
/// mismatched path" feature:
/// - only `script` receipts (never `unknown`/manual installs);
/// - only the exact `" (deleted)"` suffix (no other mismatch);
/// - only when the stripped path *resolves to the running binary* (so we never
///   adopt some unrelated path).
///
/// Anything else is left untouched and normal eligibility still refuses.
pub fn heal_legacy_deleted_receipt(receipt: &mut InstallReceipt, current_exe_canon: &str) -> bool {
    match planned_legacy_repair(
        receipt,
        current_exe_canon,
        env!("CARGO_PKG_VERSION"),
        now_secs(),
    ) {
        Some(fixed) => {
            *receipt = fixed;
            // Best-effort persist so the next run doesn't have to re-heal; even
            // if the write fails, THIS run already has the repaired in-memory
            // receipt and proceeds normally.
            let _ = write(receipt);
            true
        }
        None => false,
    }
}

/// Pure decision for [`heal_legacy_deleted_receipt`]: returns the repaired
/// receipt iff `receipt` is a `script` receipt with a `" (deleted)"`-suffixed
/// `binary_path` whose stripped form canonicalizes to `current_exe_canon`. The
/// only I/O is the `canonicalize` needed to confirm the stripped path IS the
/// running binary (identity check); it never writes. Returns `None` (no repair)
/// for unknown/manual installs, a non-suffixed path, an unresolvable stripped
/// path, or a stripped path that resolves to something other than the running
/// binary.
fn planned_legacy_repair(
    receipt: &InstallReceipt,
    current_exe_canon: &str,
    version: &str,
    now: i64,
) -> Option<InstallReceipt> {
    // Rule 4: never repair a non-managed install.
    if receipt.method != METHOD_SCRIPT {
        return None;
    }
    // Rule 1/3: only the exact "(deleted)" suffix — not arbitrary mismatches.
    let stripped = receipt.binary_path.strip_suffix(DELETED_SUFFIX)?;
    // Rule 2: the stripped path must canonicalize AND match the running binary,
    // else we do NOT repair (keep refusing).
    let canon = std::fs::canonicalize(stripped).ok()?;
    if canon.to_string_lossy() != current_exe_canon {
        return None;
    }
    Some(InstallReceipt {
        schema_version: SCHEMA_VERSION,
        method: receipt.method.clone(), // preserved (script)
        binary_path: current_exe_canon.to_owned(),
        version: version.to_owned(),    // current running version
        target: receipt.target.clone(), // unchanged (a target mismatch is handled by eligibility)
        installed_at: now,
    })
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> InstallReceipt {
        InstallReceipt {
            schema_version: SCHEMA_VERSION,
            method: METHOD_SCRIPT.to_owned(),
            binary_path: "/home/u/.local/bin/trimwire".to_owned(),
            version: "0.3.11".to_owned(),
            target: "x86_64-unknown-linux-gnu".to_owned(),
            installed_at: 1_750_000_000,
        }
    }

    #[test]
    fn json_round_trips() {
        let r = sample();
        let s = serde_json::to_string_pretty(&r).unwrap();
        let back: InstallReceipt = serde_json::from_str(&s).unwrap();
        assert_eq!(r, back);
    }

    #[test]
    fn refreshed_method_preserves_existing_else_unknown() {
        // No prior receipt → unknown (cargo/manual — not self-updatable).
        assert_eq!(refreshed_method(None), METHOD_UNKNOWN);
        // Prior script install → preserved across a later `trimwire install`.
        let existing = sample();
        assert_eq!(refreshed_method(Some(&existing)), METHOD_SCRIPT);
    }

    /// The post-apply receipt is built from the EXPLICIT install path + version,
    /// never from the running process. The regression this guards: the updater
    /// calls this from the OLD (replaced) process where `current_exe()` is
    /// `"<path> (deleted)"` and `CARGO_PKG_VERSION` is stale — so it must NOT read
    /// either. Feed it a previously-poisoned receipt and confirm the new record is
    /// clean, the version advances, and `method` is preserved.
    #[test]
    fn receipt_after_apply_uses_explicit_path_and_version_preserving_method() {
        // Simulate the bug's own poisoned receipt as the "existing" one.
        let poisoned = InstallReceipt {
            schema_version: SCHEMA_VERSION,
            method: METHOD_SCRIPT.to_owned(),
            binary_path: "/home/u/.local/bin/trimwire (deleted)".to_owned(),
            version: "0.3.12".to_owned(),
            target: "x86_64-unknown-linux-gnu".to_owned(),
            installed_at: 1_750_000_000,
        };
        let fresh = receipt_after_apply(
            Some(&poisoned),
            "/home/u/.local/bin/trimwire",
            "0.3.13",
            1_750_000_999,
        );
        assert_eq!(
            fresh.binary_path, "/home/u/.local/bin/trimwire",
            "must record the canonical install path, never the ' (deleted)' one"
        );
        assert!(
            !fresh.binary_path.ends_with(" (deleted)"),
            "no (deleted) suffix can survive"
        );
        assert_eq!(
            fresh.version, "0.3.13",
            "version advances to the installed one"
        );
        assert_eq!(
            fresh.method, METHOD_SCRIPT,
            "managed method is preserved across self-update"
        );
        assert_eq!(fresh.installed_at, 1_750_000_999);
    }

    /// With no prior receipt the method falls back to `unknown` (the safe,
    /// not-self-updatable default) — mirrors [`refresh_for_current_binary`].
    #[test]
    fn receipt_after_apply_without_prior_is_unknown() {
        let r = receipt_after_apply(None, "/opt/trimwire", "1.0.0", 7);
        assert_eq!(r.method, METHOD_UNKNOWN);
        assert_eq!(r.binary_path, "/opt/trimwire");
        assert_eq!(r.version, "1.0.0");
    }

    // ── legacy "(deleted)" receipt self-heal (v0.3.13 → v0.3.14 migration) ──────

    /// A real on-disk file whose canonical path we can match against. Returns
    /// (tempdir, canonical_path_string). Keep the dir alive for the test.
    fn real_binary() -> (tempfile::TempDir, String) {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("trimwire");
        std::fs::write(&p, b"#!/bin/true\n").unwrap();
        let canon = std::fs::canonicalize(&p)
            .unwrap()
            .to_string_lossy()
            .into_owned();
        (d, canon)
    }

    fn script_receipt(binary_path: &str) -> InstallReceipt {
        InstallReceipt {
            schema_version: SCHEMA_VERSION,
            method: METHOD_SCRIPT.to_owned(),
            binary_path: binary_path.to_owned(),
            version: "0.3.13".to_owned(),
            target: "x86_64-unknown-linux-gnu".to_owned(),
            installed_at: 1_750_000_000,
        }
    }

    /// (1) A poisoned `script` receipt (`<canon> (deleted)`) repairs to the
    /// canonical current path + current running version, method preserved.
    #[test]
    fn planned_repair_heals_deleted_script_receipt() {
        let (_d, canon) = real_binary();
        let poisoned = script_receipt(&format!("{canon} (deleted)"));
        let fixed = planned_legacy_repair(&poisoned, &canon, "0.3.14", 99)
            .expect("a (deleted) script receipt matching the running exe must repair");
        assert_eq!(fixed.binary_path, canon, "repaired to the canonical path");
        assert!(!fixed.binary_path.ends_with(" (deleted)"));
        assert_eq!(
            fixed.version, "0.3.14",
            "version set to the running version"
        );
        assert_eq!(fixed.method, METHOD_SCRIPT, "method preserved");
        assert_eq!(fixed.target, "x86_64-unknown-linux-gnu", "target preserved");
        assert_eq!(fixed.installed_at, 99);
    }

    /// (2) After repair, eligibility is Eligible (not PathMismatch).
    #[test]
    fn healed_receipt_is_eligible() {
        let (_d, canon) = real_binary();
        let poisoned = script_receipt(&format!("{canon} (deleted)"));
        let fixed = planned_legacy_repair(&poisoned, &canon, "0.3.14", 1).unwrap();
        assert_eq!(
            crate::update::eligibility(Some(&fixed), &canon, "x86_64-unknown-linux-gnu", true),
            crate::update::Eligibility::Eligible,
            "a healed receipt must let the next upgrade proceed"
        );
    }

    /// (3) A poisoned path whose stripped form does NOT match the running exe is
    /// NOT repaired (we never adopt an unrelated path) — and an unresolvable
    /// stripped path is likewise refused.
    #[test]
    fn planned_repair_refuses_non_matching_or_unresolvable() {
        let (_d, canon) = real_binary();
        // Stripped path resolves, but to a DIFFERENT binary than the running one.
        let other = script_receipt(&format!("{canon} (deleted)"));
        let different_exe = format!("{canon}-not-me");
        assert!(
            planned_legacy_repair(&other, &different_exe, "0.3.14", 1).is_none(),
            "must not repair when the stripped path isn't the running binary"
        );
        // Stripped path doesn't exist at all → cannot confirm identity → refuse.
        let ghost = script_receipt("/nope/does/not/exist/trimwire (deleted)");
        assert!(
            planned_legacy_repair(&ghost, &canon, "0.3.14", 1).is_none(),
            "must not repair an unresolvable stripped path"
        );
    }

    /// (4) An unknown/manual receipt is never silently repaired, even with the
    /// "(deleted)" suffix and a matching stripped path.
    #[test]
    fn planned_repair_never_touches_unknown_install() {
        let (_d, canon) = real_binary();
        let mut manual = script_receipt(&format!("{canon} (deleted)"));
        manual.method = METHOD_UNKNOWN.to_owned();
        assert!(
            planned_legacy_repair(&manual, &canon, "0.3.14", 1).is_none(),
            "unknown/manual installs are not self-updatable and must not be healed"
        );
    }

    /// (5) A normal clean receipt is untouched (no suffix → no repair); existing
    /// behavior is unchanged.
    #[test]
    fn planned_repair_leaves_clean_receipt_alone() {
        let (_d, canon) = real_binary();
        let clean = script_receipt(&canon);
        assert!(
            planned_legacy_repair(&clean, &canon, "0.3.14", 1).is_none(),
            "a clean receipt has nothing to heal"
        );
    }

    #[test]
    fn corrupt_or_partial_json_does_not_parse() {
        // A partial file (e.g. a half-written or older-shape receipt) must fail
        // to parse so callers fall back to "no receipt" rather than a wrong one.
        assert!(serde_json::from_str::<InstallReceipt>("{\"method\":\"script\"}").is_err());
        assert!(serde_json::from_str::<InstallReceipt>("not json").is_err());
    }
}
