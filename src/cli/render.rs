//! Terminal rendering helpers: Unicode glyphs when stdout is an interactive
//! terminal, ASCII fallbacks when output is piped/redirected (or NO_COLOR is set).
//!
//! Rationale (DS-reviewed): the status glyphs (✓ ✗ ⚠ ⊡ •) and the ▰▱ gauge are
//! *Unicode*, not ANSI colour — so the correct trigger for an ASCII fallback is a
//! NON-TTY stdout (piped into a file / CI log / script), where box-drawing can
//! garble and isn't copy-paste-friendly. NO_COLOR (which the spec ties to ANSI
//! colour) is honoured here too as a courtesy. ANSI-colour stripping for the
//! statusline is a *separate* concern keyed on NO_COLOR alone — its stdout is
//! always piped into Claude Code, so a TTY check there would wrongly drop colour.
//!
//! Binary-private (CLI presentation only — never in the library crate).

use std::io::IsTerminal;

/// Should we avoid Unicode glyphs? True when stdout isn't an interactive
/// terminal (piped/redirected), or NO_COLOR is set. Cached: stdout doesn't
/// change mid-run, and this avoids re-`ioctl`-ing on every glyph call (and makes
/// mixed ✓/[ok] output structurally impossible within one invocation).
pub(crate) fn ascii_only() -> bool {
    static ASCII_ONLY: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ASCII_ONLY.get_or_init(|| !std::io::stdout().is_terminal() || no_color())
}

/// True when NO_COLOR is set to anything (including empty) — presence is enough,
/// per https://no-color.org. Gates ANSI-colour output (e.g. the statusline).
pub(crate) fn no_color() -> bool {
    std::env::var_os("NO_COLOR").is_some()
}

/// Success sigil (`✓` / `[ok]`).
pub(crate) fn ok() -> &'static str {
    if ascii_only() { "[ok]" } else { "✓" }
}
/// Failure sigil (`✗` / `[x]`).
pub(crate) fn bad() -> &'static str {
    if ascii_only() { "[x]" } else { "✗" }
}
/// Warning sigil (`⚠` / `[!]`).
pub(crate) fn warn() -> &'static str {
    if ascii_only() { "[!]" } else { "⚠" }
}
/// Neutral bullet (`•` / `-`).
pub(crate) fn bullet() -> &'static str {
    if ascii_only() { "-" } else { "•" }
}
/// Report header glyph (`⊡` / `::`).
pub(crate) fn header() -> &'static str {
    if ascii_only() { "::" } else { "⊡" }
}

/// A 12-cell reduction gauge (0–100%), `▰▱` in a terminal, `#-` when plain.
pub(crate) fn gauge(pct: f64) -> String {
    let cells = 12;
    let filled = ((pct.clamp(0.0, 100.0) / 100.0) * cells as f64).round() as usize;
    let (f, e) = if ascii_only() {
        ("#", "-")
    } else {
        ("▰", "▱")
    };
    format!("[{}{}]", f.repeat(filled), e.repeat(cells - filled))
}

/// The single fill character for inline magnitude bars (`▰` / `#`).
pub(crate) fn bar_fill() -> &'static str {
    if ascii_only() { "#" } else { "▰" }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gauge_is_twelve_cells_and_clamped() {
        // Under `cargo test` stdout isn't a TTY, so the ASCII branch is taken.
        assert_eq!(gauge(0.0), "[------------]");
        assert_eq!(gauge(50.0), "[######------]");
        assert_eq!(gauge(100.0), "[############]");
        assert_eq!(gauge(150.0), "[############]"); // clamped
        assert_eq!(gauge(-5.0), "[------------]"); // clamped
    }

    #[test]
    fn glyphs_fall_back_to_ascii_when_not_a_tty() {
        // The test harness captures stdout (non-TTY) → ASCII fallbacks.
        assert_eq!(ok(), "[ok]");
        assert_eq!(bad(), "[x]");
        assert_eq!(warn(), "[!]");
        assert_eq!(bar_fill(), "#");
    }
}
