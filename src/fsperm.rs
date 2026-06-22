//! Tighten on-disk permissions of trimwire's local files to owner-only.
//!
//! trimwire's local files — the savings ledger (byte counts + hashes, no
//! message content), the opt-in `--audit` log (shape metadata only), and the
//! telemetry install-id (an HMAC key that is never transmitted) — hold no
//! request/response content. But on a shared host the user's default umask
//! (often `0022`) creates them world-readable (`0644`), which still leaks
//! session ids and usage patterns to other local users. Restrict them to the
//! owner (`0600`).
//!
//! On non-Unix targets (Windows) newly-created files already inherit the
//! creating user's ACL, so this is a deliberate no-op there.
//!
//! Caveat: on filesystems where POSIX modes are advisory rather than enforced
//! (a WSL `/mnt/c` NTFS mount, an SMB/CIFS share, FAT), the `chmod` succeeds but
//! the underlying ACL governs real access. trimwire's data files live under the
//! native data dir (`~/.local/share/trimwire`, `~/.config`), so this is only a
//! concern if a user points `--ledger`/`--audit` at such a mount.

use std::path::Path;

/// Best-effort: restrict `path` to owner read/write only (`0600`) on Unix.
/// No-op (returns `Ok`) on non-Unix platforms.
#[cfg(unix)]
pub fn restrict_to_owner(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
}

/// No-op on non-Unix: Windows files inherit the creating user's ACL.
#[cfg(not(unix))]
pub fn restrict_to_owner(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn restricts_to_0600_on_unix() {
        let dir = std::env::temp_dir().join(format!("tw-fsperm-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("secret");
        std::fs::write(&p, b"x").unwrap();
        // Make it world-readable first to prove restrict_to_owner tightens it.
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o644)).unwrap();
        restrict_to_owner(&p).unwrap();
        let mode = std::fs::metadata(&p).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            mode, 0o600,
            "file must be owner-only after restrict_to_owner"
        );
        std::fs::remove_dir_all(&dir).ok();
    }
}
