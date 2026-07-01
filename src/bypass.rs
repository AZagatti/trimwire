//! Runtime bypass toggle — the state behind `trimwire off` / `trimwire on`.
//!
//! `trimwire off` should mean "stop pruning my sessions", NOT "break every
//! Claude call". Because `trimwire install` bakes `ANTHROPIC_BASE_URL` into the
//! shell rc (and `environment.d` / a launchd env agent), a shell can't be
//! un-pointed at the gateway after the fact — so stopping the gateway would
//! strand every request on a dead socket.
//!
//! Instead we keep the always-up gateway serving and flip a **runtime sentinel**
//! it consults per request: when the sentinel file exists, the gateway forwards
//! `/v1/messages` bodies UNMODIFIED to Anthropic (zero pruning), so `off` is a
//! true bypass — the socket stays live in every shell and GUI app with no env or
//! rc surgery. `on` removes the sentinel and pruning resumes on the next turn.
//!
//! State, not config: bypass is a live on/off flip a running daemon must observe
//! without a restart, so it's a file the gateway `stat`s — not a `Config` field
//! (config is loaded once at startup and never re-read on the hot path). The
//! check is one `stat(2)` on the messages path only; against a buffered-MB body
//! and a 100 ms–10 s upstream round trip it's sub-noise (warm dentry cache ≈
//! 1–2 µs), so no atomic/poll caching is warranted.

use std::path::PathBuf;

use anyhow::{Context, Result};

/// Where the sentinel lives — alongside `ledger.db` / `daemon.pid` in
/// `~/.trimwire/`. Reuses `ledger::resolve_path` so `~/` expands the same way
/// the rest of the tool resolves its paths.
pub fn sentinel_path() -> PathBuf {
    crate::ledger::resolve_path("~/.trimwire/bypass")
}

/// Is bypass currently active? True iff the sentinel file exists. Presence — not
/// contents — is the signal, so this is a single cheap existence check with no
/// parsing. Any stat error other than "exists" reads as inactive (fail toward
/// pruning, the installed default), so a transient FS hiccup never silently
/// disables pruning.
pub fn is_active() -> bool {
    is_active_at(&sentinel_path())
}

/// Turn bypass ON (create the sentinel). Idempotent — re-enabling is a no-op.
/// Creates `~/.trimwire/` if absent.
pub fn enable() -> Result<()> {
    enable_at(&sentinel_path())
}

/// Turn bypass OFF (remove the sentinel). Idempotent — a missing sentinel is
/// success (pruning is already the active state).
pub fn disable() -> Result<()> {
    disable_at(&sentinel_path())
}

// ── path-injectable cores (so tests don't mutate the process-wide `$HOME`) ──

fn is_active_at(path: &std::path::Path) -> bool {
    path.try_exists().unwrap_or(false)
}

fn enable_at(path: &std::path::Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    std::fs::write(path, b"")
        .with_context(|| format!("write bypass sentinel {}", path.display()))?;
    Ok(())
}

fn disable_at(path: &std::path::Path) -> Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).with_context(|| format!("remove bypass sentinel {}", path.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enable_disable_is_active_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        // Nested dir so we also cover enable_at() creating the parent.
        let path = dir.path().join(".trimwire/bypass");

        // Absent → inactive.
        assert!(!is_active_at(&path), "no sentinel yet → not bypassed");

        // enable_at() creates the parent dir + sentinel and flips active.
        enable_at(&path).unwrap();
        assert!(is_active_at(&path), "sentinel present → bypassed");
        assert!(path.exists());

        // enable_at() is idempotent.
        enable_at(&path).unwrap();
        assert!(is_active_at(&path));

        // disable_at() removes it.
        disable_at(&path).unwrap();
        assert!(!is_active_at(&path), "sentinel gone → not bypassed");

        // disable_at() on an already-absent sentinel is success, not an error.
        disable_at(&path).unwrap();
        assert!(!is_active_at(&path));
    }

    #[test]
    fn sentinel_lives_under_dot_trimwire() {
        // The public path resolves under ~/.trimwire/ (same dir as ledger.db),
        // reusing ledger::resolve_path's `~/` expansion.
        assert!(sentinel_path().ends_with(".trimwire/bypass"));
    }
}
