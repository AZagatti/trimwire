//! `trimwire hook` — a Claude Code hook that warns, in-session, when trimwire
//! is configured (`ANTHROPIC_BASE_URL` points at it) but not actually serving.
//!
//! This is the "fail loud" channel for users who keep their own statusline (so
//! the statusline savings bar isn't wired): the scary failure is *set but not
//! pruning* — Claude Code doesn't error, so you'd silently send full-size
//! context and pay full price with no signal. Wire it as a SessionStart (and/or
//! UserPromptSubmit) hook; it stays silent when healthy and emits a visible
//! `systemMessage` only when something's wrong. It never blocks the prompt.

use std::io::{IsTerminal, Read};

use anyhow::Result;

use trimwire::config::Config;

/// Read the hook JSON on stdin (ignored — we only need our own state), and emit
/// a warning iff trimwire is set-but-down. Exit 0 either way.
///
/// TTY guard: if stdin is a terminal (the user ran `trimwire hook` by hand),
/// print a usage note and return immediately instead of blocking forever.
pub fn hook() -> Result<()> {
    if std::io::stdin().is_terminal() {
        println!(
            "trimwire hook reads Claude Code hook JSON on stdin; it's meant to be wired as a \
             hook, not run by hand — see docs/CONFIGURATION.md"
        );
        return Ok(());
    }
    let mut input = String::new();
    let _ = std::io::stdin().read_to_string(&mut input);

    let cfg = Config::load().unwrap_or_default();
    if super::statusline::set_but_down(&cfg) {
        let msg = "⚠ trimwire is configured (ANTHROPIC_BASE_URL) but not responding — \
                   you may be sending full-size context. Run `trimwire status` / `trimwire on`.";
        // `systemMessage` renders as a visible notice without becoming model
        // context. Printed as the hook's JSON result.
        let out = serde_json::json!({ "systemMessage": msg });
        println!("{out}");
    }
    Ok(())
}
