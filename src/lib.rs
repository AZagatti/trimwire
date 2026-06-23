//! trimwire — Claude Code Dynamic Context Pruning.
//!
//! A local HTTP gateway that mutates Claude Code's outbound `/v1/messages`
//! requests to strip image payloads and elide stale tool calls, then forwards
//! to `api.anthropic.com`. Anthropic-sanctioned mechanism via
//! `ANTHROPIC_BASE_URL`. No CA cert install, no restart.
//!
//! See `ARCHITECTURE.md` for the design, `SPIKE.md` for the rationale.

pub mod config;
pub mod error;
pub mod fsperm;
pub mod ledger;
pub mod pairing;
pub mod proxy;
/// Install receipt: a small JSON record of how trimwire was installed, used by a
/// future `trimwire update` to decide whether self-update is allowed. See
/// `src/receipt.rs`.
pub mod receipt;
pub mod reprune;
pub mod strategies;
/// Opt-in summarizer context compaction. Always compiled; disabled unless
/// `[summarizer] engine` is not `"model-free"` in config. See `docs/SUMMARIZER.md`.
pub mod summarizer;
pub mod sweep;

/// The Rust target triple this binary was built for, e.g.
/// `x86_64-unknown-linux-gnu` — embedded at build time by `build.rs` from
/// Cargo's `TARGET`.
///
/// This is the asset-selection primitive a future `trimwire update` will use to
/// pick the right `trimwire-<triple>.<ext>` GitHub release artifact (the asset
/// names produced by `.github/workflows/release.yml`). Today it is surfaced in
/// `trimwire doctor` for bug reports. Returns `"unknown"` only if `TARGET` was
/// absent at build time, which does not happen under a normal cargo build.
pub fn build_target() -> &'static str {
    env!("TRIMWIRE_TARGET")
}

#[cfg(test)]
mod build_target_tests {
    /// The embedded triple must be present and consistent with the arch/OS this
    /// test binary is actually running on — so the future updater downloads the
    /// matching asset rather than a wrong-platform one.
    #[test]
    fn build_target_present_and_consistent_with_cfg() {
        let t = super::build_target();
        assert!(!t.is_empty(), "TRIMWIRE_TARGET is empty");
        assert_ne!(t, "unknown", "TARGET should be set under cargo");

        // Arch token matches verbatim (std uses the same spelling as the triple:
        // `x86_64`, `aarch64`, …).
        let arch = std::env::consts::ARCH;
        assert!(
            t.contains(arch),
            "triple `{t}` should contain arch `{arch}`"
        );

        // OS token: the triple uses the OS name verbatim except macOS, whose
        // triples say `darwin` (e.g. `aarch64-apple-darwin`).
        let os_token = match std::env::consts::OS {
            "macos" => "darwin",
            os => os, // linux→linux, windows→windows, and any future platform
        };
        assert!(
            t.contains(os_token),
            "triple `{t}` should contain OS token `{os_token}` (from `{}`)",
            std::env::consts::OS
        );
    }
}
