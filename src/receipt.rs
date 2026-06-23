//! Install receipt — a small JSON record of HOW trimwire was installed.
//!
//! Written by `scripts/install.sh` (the curl|sh installer) and refreshed by
//! `trimwire install`. A future `trimwire update` reads it to decide whether
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

/// Managed install via `scripts/install.sh` — self-updatable by a future updater.
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
/// CAVEAT for a future `trimwire update`: `method` is preserved blindly, so a
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

    #[test]
    fn corrupt_or_partial_json_does_not_parse() {
        // A partial file (e.g. a half-written or older-shape receipt) must fail
        // to parse so callers fall back to "no receipt" rather than a wrong one.
        assert!(serde_json::from_str::<InstallReceipt>("{\"method\":\"script\"}").is_err());
        assert!(serde_json::from_str::<InstallReceipt>("not json").is_err());
    }
}
