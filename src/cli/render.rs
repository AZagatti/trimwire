//! Terminal rendering helpers: Unicode glyphs + semantic ANSI colour when stdout
//! is an interactive terminal, plain ASCII when output is piped/redirected (or
//! NO_COLOR is set).
//!
//! Rationale (DS-reviewed): the status glyphs (✓ ✗ ⚠ ⊡ •) and the ▰▱ gauge are
//! *Unicode*, not ANSI colour — so the correct trigger for an ASCII fallback is a
//! NON-TTY stdout (piped into a file / CI log / script), where box-drawing can
//! garble and isn't copy-paste-friendly. NO_COLOR (which the spec ties to ANSI
//! colour) is honoured here too as a courtesy. ANSI-colour stripping for the
//! statusline is a *separate* concern keyed on NO_COLOR alone — its stdout is
//! always piped into Claude Code, so a TTY check there would wrongly drop colour.
//!
//! Colour policy (issue #118, research-led): use colour *sparingly and
//! semantically* — green ✓ / yellow ⚠ / red ✗ for status, ONE accent (cyan) for
//! the recommended choice and the value the user must type, dim for secondary
//! chrome. NEVER colour alone: every status still carries its glyph shape and a
//! descriptive word, so colour-blind and NO_COLOR/piped readers lose nothing.
//! Colour is gated by [`use_color`] (TTY + !NO_COLOR + TERM≠dumb) — a stricter
//! trigger than [`ascii_only`], since a `TERM=dumb` terminal is still a TTY that
//! can show glyphs but must not receive ANSI escapes.
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

/// Should we emit ANSI colour? True only when stdout is an interactive terminal,
/// NO_COLOR is unset, and `TERM` isn't `dumb` (a real terminal that can't render
/// escapes). Cached for the same reason as [`ascii_only`]. This is deliberately
/// AND-stricter than `!ascii_only()`: piped/NO_COLOR output has no colour *and*
/// falls back to ASCII glyphs, while `TERM=dumb` keeps Unicode glyphs (it's a
/// TTY) but drops colour.
pub(crate) fn use_color() -> bool {
    static USE_COLOR: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *USE_COLOR.get_or_init(|| {
        std::io::stdout().is_terminal()
            && !no_color()
            && std::env::var_os("TERM").is_none_or(|t| t != "dumb")
    })
}

// Semantic ANSI palette (hand-rolled, matching statusline.rs — zero new deps).
// Reserved meanings: green=success, yellow=warning, red=error, cyan=accent (the
// ONE colour for "type this" / "recommended"), bold=emphasis, dim=secondary.
const GREEN: &str = "\x1b[32m";
const YELLOW: &str = "\x1b[33m";
const RED: &str = "\x1b[31m";
const CYAN: &str = "\x1b[36m";
const BOLD: &str = "\x1b[1m";
const DIM: &str = "\x1b[2m";
const RESET: &str = "\x1b[0m";

/// Wrap `text` in `code`…reset when colour is enabled; return it untouched
/// otherwise. The single choke-point every styled helper below routes through,
/// so `use_color()` is the only gate that matters.
fn paint(code: &str, text: &str) -> String {
    if use_color() {
        format!("{code}{text}{RESET}")
    } else {
        text.to_owned()
    }
}

/// Success sigil — green `✓` / plain `[ok]`.
pub(crate) fn ok() -> String {
    paint(GREEN, if ascii_only() { "[ok]" } else { "✓" })
}
/// Failure sigil — red `✗` / plain `[x]`.
pub(crate) fn bad() -> String {
    paint(RED, if ascii_only() { "[x]" } else { "✗" })
}
/// Warning sigil — yellow `⚠` / plain `[!]`.
pub(crate) fn warn() -> String {
    paint(YELLOW, if ascii_only() { "[!]" } else { "⚠" })
}
/// Neutral bullet (`•` / `-`) — dimmed, never a status colour.
pub(crate) fn bullet() -> String {
    paint(DIM, if ascii_only() { "-" } else { "•" })
}
/// Report header glyph (`⊡` / `::`) — accented.
pub(crate) fn header() -> String {
    paint(CYAN, if ascii_only() { "::" } else { "⊡" })
}

/// Accent a value the user should notice — the recommended choice, or the exact
/// value to type. Cyan; the ONE accent colour (never red/yellow, which are
/// reserved for warn/error). No-op glyph-wise, so it's safe to wrap any text.
pub(crate) fn accent(text: &str) -> String {
    paint(CYAN, text)
}
/// Bold emphasis for section headings and the single load-bearing token in a
/// line. Falls back to plain text under NO_COLOR/non-TTY.
pub(crate) fn strong(text: &str) -> String {
    paint(BOLD, text)
}
/// Dim secondary chrome (hints, "why" asides, separators) so the actionable line
/// stands out. Falls back to plain text.
pub(crate) fn dim(text: &str) -> String {
    paint(DIM, text)
}
/// Colour an inline span of *warning* text (yellow) — for the caution part of a
/// line whose leading glyph is already [`warn`]. Reserved: never decorative.
pub(crate) fn warn_text(text: &str) -> String {
    paint(YELLOW, text)
}
/// Colour an inline span of *error* text (red) — for the load-bearing failure
/// phrase on a line led by [`bad`]. Reserved: never decorative.
pub(crate) fn error_text(text: &str) -> String {
    paint(RED, text)
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
        // The test harness captures stdout (non-TTY) → ASCII fallbacks AND no
        // colour (use_color() is false), so the sigils are bare ASCII with no
        // ANSI escapes. String == &str via PartialEq<&str>.
        assert_eq!(ok(), "[ok]");
        assert_eq!(bad(), "[x]");
        assert_eq!(warn(), "[!]");
        assert_eq!(bullet(), "-");
        assert_eq!(header(), "::");
        assert_eq!(bar_fill(), "#");
    }

    #[test]
    fn no_ansi_escapes_when_colour_is_off() {
        // Under a captured (non-TTY) stdout use_color() is false, so nothing the
        // styled helpers emit may contain an ESC (0x1b) byte — this is what keeps
        // colour out of piped output, CI logs, and the substring-asserting
        // integration tests. Covers every colour choke-point.
        assert!(
            !use_color(),
            "captured stdout must not be treated as a colour TTY"
        );
        for s in [ok(), bad(), warn(), bullet(), header(), gauge(50.0)] {
            assert!(!s.contains('\x1b'), "unexpected ANSI escape in {s:?}");
        }
        for s in [accent("x"), strong("x"), dim("x")] {
            assert_eq!(
                s, "x",
                "styled wrappers are pass-through when colour is off"
            );
        }
    }
}
