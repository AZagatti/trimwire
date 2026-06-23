//! `trimwire install` — write a starter config (if absent) and add the env
//! exports to the shell rc (idempotent).

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use trimwire::config::{self, Config};

const RC_MARKER_START: &str = "# >>> trimwire >>>";
const RC_MARKER_END: &str = "# <<< trimwire <<<";

/// Write a starter config (if absent) and add the gateway env exports to the
/// shell rc via an idempotent guarded block. `boot` additionally enables
/// start-before-login (systemd lingering); the default is login-scoped.
///
/// Deliberately does **not** touch Claude Code's statusline — that's an
/// explicit, separate step (`trimwire statusline add`). install is idempotent,
/// so you can re-run it freely.
pub fn install(boot: bool) -> Result<()> {
    let cfg_path = config::global_config_path();
    if super::write_config_if_absent(&cfg_path)? {
        println!("wrote starter config: {}", cfg_path.display());
    } else {
        println!(
            "config already exists: {} (left unchanged)",
            cfg_path.display()
        );
    }

    let listen = Config::load()
        .map(|c| c.server.listen)
        .unwrap_or_else(|_| "127.0.0.1:8765".to_owned());
    let base_url = format!("http://{listen}");
    let block = if is_fish_shell() {
        rc_block_fish(&base_url)
    } else {
        rc_block(&base_url)
    };

    match shell_rc_path() {
        Some(rc) => {
            let existing = std::fs::read_to_string(&rc).unwrap_or_default();
            match ensure_rc_block(&existing, &block) {
                Some(updated) => {
                    write_text_atomic(&rc, &updated)
                        .with_context(|| format!("write {}", rc.display()))?;
                    println!("added trimwire env exports to {}", rc.display());
                    println!("→ restart your shell or `source {}`", rc.display());
                }
                None => println!("shell rc already has the trimwire block: {}", rc.display()),
            }
        }
        None => {
            println!("could not detect your shell rc — add these exports manually:");
            println!("{block}");
        }
    }

    // Install + start the always-up service so the global export above is safe:
    // when the OS owns the listening socket, a dead/restarting daemon can't
    // strand Claude Code with a connection error.
    match listen.parse() {
        Ok(addr) => match super::service::install(addr, boot) {
            Ok(installed) => {
                use super::service::Autostart;
                println!("installed service: {}", installed.manager.label());
                match installed.autostart {
                    Autostart::Linger => {
                        println!("→ starts on (re)boot, even before login — forget it's there.")
                    }
                    Autostart::Login => println!(
                        "→ starts automatically when you log in. (Add `--boot` to also start \
                         before login / survive logout.)"
                    ),
                    Autostart::LingerFailed => {
                        let user = std::env::var("USER").unwrap_or_else(|_| "$USER".to_owned());
                        println!(
                            "→ starts at login; couldn't enable pre-login start automatically — \
                             run once: loginctl enable-linger {user}"
                        );
                    }
                    Autostart::Manual => println!(
                        "→ no socket activation here (no systemd/launchd); it won't auto-start — \
                         run `trimwire on` to (re)start it."
                    ),
                }
                if installed.gui_env {
                    println!(
                        "→ also set the env for GUI-launched editors. If your IDE still bypasses \
                         it, set ANTHROPIC_BASE_URL in its settings (see docs/TROUBLESHOOTING.md)."
                    );
                }
                println!(
                    "turn it off any time with `trimwire off`; check it with `trimwire status`."
                );
            }
            Err(e) => {
                println!("could not install the background service ({e}).");
                println!("→ run the gateway yourself with `trimwire run`.");
            }
        },
        Err(_) => println!("could not parse listen address `{listen}`; skipped service install"),
    }

    // We do NOT touch the statusline here. See savings via `trimwire stats`, or
    // add a live bar explicitly with `trimwire statusline add`.
    println!();
    println!("Optional next steps:");
    println!("  see savings anytime:         trimwire stats");
    println!(
        "  model summarizer (long runs): trimwire summarizer setup  \
         (optional — local or API model compresses old context)"
    );
    println!(
        "  live savings bar:            trimwire statusline add  \
         (or `trimwire statusline wrap` if you already have a statusline)"
    );
    println!("  health alert hook:           trimwire hook");
    println!(
        "  shell completions:           trimwire completions {}  \
         (see `trimwire completions --help`)",
        detected_shell_hint()
    );

    // End-state confirmation: probe /healthz so the user gets a clear result
    // instead of guessing whether it worked. Socket-activated managers
    // (systemd/launchd) start the worker on this very connection; give a slow
    // start a couple of retries before deciding it isn't up.
    if let Ok(addr) = listen.parse::<std::net::SocketAddr>() {
        let serving = (0..3).any(|i| {
            if i > 0 {
                std::thread::sleep(std::time::Duration::from_millis(300));
            }
            super::service::healthz_ok(addr)
        });
        println!();
        if serving {
            println!(
                "{} trimwire is active and serving on {addr} — start coding: claude",
                super::render::ok()
            );
        } else {
            println!(
                "{} installed, but the gateway isn't answering on {addr} yet — it may start on \
                 first use (socket activation) or at next login. Check it: trimwire doctor",
                super::render::warn()
            );
        }
    }

    // Record/refresh the install receipt (best-effort, non-fatal). Preserves the
    // curl|sh installer's `method="script"` if it already wrote one; otherwise
    // records `method="unknown"` (cargo/manual install). A future `trimwire
    // update` reads this to decide whether self-update is allowed. No network.
    let _ = trimwire::receipt::refresh_for_current_binary();

    Ok(())
}

/// `trimwire statusline add [--wrap]` — wire the savings bar into Claude Code.
pub fn statusline_add(wrap: bool) -> Result<()> {
    match wire_statusline(wrap) {
        StatuslineWire::Added => {
            println!("added trimwire's savings bar to Claude Code's statusline.")
        }
        StatuslineWire::Wrapped => println!(
            "wrapped your existing statusline — trimwire now adds a row beneath it. \
             (`trimwire statusline remove` restores your original.)"
        ),
        StatuslineWire::Exists => println!(
            "you already have a statusline — left it untouched. Run \
             `trimwire statusline wrap` to keep it and add trimwire's row beneath."
        ),
        StatuslineWire::NoChange => println!("statusline already shows trimwire (no change)."),
        StatuslineWire::ParseError => println!(
            "left ~/.claude/settings.json untouched (couldn't parse it — JSON comments?). \
             Add the statusline yourself per CONFIGURATION.md."
        ),
        StatuslineWire::NotObject => println!(
            "~/.claude/settings.json isn't a JSON object — left it untouched. Fix it (it should \
             be `{{ ... }}`) and re-run, or add the statusline yourself per CONFIGURATION.md."
        ),
        StatuslineWire::Skipped => {
            println!("couldn't access Claude Code settings; see savings with `trimwire stats`.")
        }
    }
    Ok(())
}

/// Path to Claude Code's user settings (`~/.claude/settings.json`).
fn claude_settings_path() -> Option<PathBuf> {
    let home = std::env::var("HOME").ok()?;
    Some(Path::new(&home).join(".claude/settings.json"))
}

/// File where we stash the user's original statusline command when wrapping,
/// so `uninstall` can restore it exactly.
fn wrapped_cmd_path() -> Option<PathBuf> {
    let home = std::env::var("HOME").ok()?;
    Some(Path::new(&home).join(".trimwire/statusline-wrapped.cmd"))
}

/// Full original `statusLine` value (JSON), stashed alongside the command-only
/// `.cmd` file so `statusline remove` restores the user's config LOSSLESSLY —
/// e.g. ccstatusline's `padding`/`refreshInterval`, which the command string
/// alone drops. The `.cmd` file stays the source for the render hot path.
fn wrapped_json_path() -> Option<PathBuf> {
    let home = std::env::var("HOME").ok()?;
    Some(Path::new(&home).join(".trimwire/statusline-wrapped.json"))
}

/// Result of trying to wire our statusline into Claude Code.
enum StatuslineWire {
    /// We set `statusLine` (it was absent) — the bar shows out of the box.
    Added,
    /// We wrapped an existing statusline so trimwire adds a row beneath it.
    Wrapped,
    /// A statusLine already exists and we left it alone (no `--wrap-statusline`).
    Exists,
    /// `--wrap-statusline` but it was already wrapped, or not wrappable.
    NoChange,
    /// settings.json exists but isn't parseable (e.g. JSONC comments) — we
    /// refuse to overwrite it.
    ParseError,
    /// settings.json's root isn't a JSON object (it's an array/string/number).
    NotObject,
    /// Couldn't read/write settings.json.
    Skipped,
}

/// Outcome of removing trimwire from the statusline.
pub enum Unwired {
    /// Restored the user's original (was wrapped, stash present).
    Restored,
    /// Removed our plain bar.
    Removed,
    /// Was wrapped but the stash is gone — dropped the dangling wrapper; the
    /// original could NOT be restored.
    StashMissing,
    /// statusLine isn't trimwire's (or no settings) — nothing to do.
    NotOurs,
}

/// Atomically write `value` to `path` (temp file in the same dir + rename) so
/// we never race Claude Code's own non-atomic writer (a documented corruption
/// source). Returns false on any I/O/serialize failure.
/// Temp path in the SAME directory as `path` (so the later rename is atomic on
/// one filesystem), preserving the original filename so the `.json` extension
/// isn't dropped (`settings.json` → `settings.json.tmp.<pid>`, not
/// `settings.tmp.<pid>` which other tools might scan).
fn atomic_tmp_path(path: &Path) -> std::path::PathBuf {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "trimwire".to_owned());
    path.with_file_name(format!("{name}.tmp.{}", std::process::id()))
}

/// Write text atomically (temp file + rename) so an interrupted write can never
/// truncate an existing file — critical for the user's shell rc.
fn write_text_atomic(path: &Path, contents: &str) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let tmp = atomic_tmp_path(path);
    if let Err(e) = std::fs::write(&tmp, contents) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    std::fs::rename(&tmp, path)
}

fn write_json_atomic(path: &Path, value: &serde_json::Value) -> bool {
    let Ok(s) = serde_json::to_string_pretty(value) else {
        return false;
    };
    write_text_atomic(path, &(s + "\n")).is_ok()
}

/// Best-guess current shell (from `$SHELL` basename) for the completions hint.
/// Falls back to `zsh` (the macOS default and a common Linux choice).
fn detected_shell_hint() -> &'static str {
    let shell = std::env::var("SHELL").unwrap_or_default();
    match shell.rsplit('/').next() {
        Some("bash") => "bash",
        Some("fish") => "fish",
        _ => "zsh",
    }
}

/// Wire trimwire into Claude Code's `statusLine`:
/// - no statusLine yet → set ours (the bar shows out of the box);
/// - statusLine exists + `wrap` → wrap it so trimwire renders as an extra row;
/// - statusLine exists + no `wrap` → leave it untouched.
fn wire_statusline(wrap: bool) -> StatuslineWire {
    let Some(path) = claude_settings_path() else {
        return StatuslineWire::Skipped;
    };
    // Distinguish "no file yet" (start fresh) from "file exists but won't
    // parse" — in the latter case we must NOT overwrite (it'd nuke the user's
    // settings); abort instead.
    let mut root = match std::fs::read_to_string(&path) {
        Ok(s) if s.trim().is_empty() => serde_json::json!({}),
        Ok(s) => match serde_json::from_str::<serde_json::Value>(&s) {
            Ok(v) => v,
            Err(_) => return StatuslineWire::ParseError,
        },
        Err(_) => serde_json::json!({}),
    };
    let exe = std::env::current_exe()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| "trimwire".to_owned());

    // A non-object root (settings.json is `[]`, `"x"`, `42`, …) can't take a
    // statusLine key — refuse rather than mislabel it "already trimwire".
    if !root.is_object() {
        return StatuslineWire::NotObject;
    }

    let outcome = if root.get("statusLine").is_none() {
        match set_statusline_if_absent(&mut root, &format!("{exe} statusline render")) {
            Some(()) => StatuslineWire::Added,
            None => StatuslineWire::NoChange,
        }
    } else if statusline_is_ours(&root) {
        // Already trimwire's bar (plain OR wrapped) → both `add` and `wrap` are
        // no-ops; never wrap our own bar (that double-rendered the row).
        return StatuslineWire::NoChange;
    } else if wrap {
        let Some(wf) = wrapped_cmd_path() else {
            return StatuslineWire::Skipped;
        };
        let wrapper = format!("{exe} statusline render --wrap-file {}", wf.display());
        // Capture the FULL original statusLine value before rewrap mutates it, so
        // remove can restore it losslessly (extra fields like padding survive).
        let orig_full = root.get("statusLine").cloned();
        match rewrap_statusline(&mut root, &wrapper) {
            Some(original) => {
                if let Some(parent) = wf.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                if std::fs::write(&wf, original).is_err() {
                    return StatuslineWire::Skipped;
                }
                // Best-effort lossless stash (the .cmd above is the render source
                // of truth; this is only consulted on restore).
                if let (Some(jf), Some(full)) = (wrapped_json_path(), orig_full) {
                    if let Ok(s) = serde_json::to_string(&full) {
                        let _ = std::fs::write(&jf, s);
                    }
                }
                StatuslineWire::Wrapped
            }
            None => StatuslineWire::NoChange, // not a wrappable statusline shape
        }
    } else {
        return StatuslineWire::Exists; // respect the user's bar.
    };

    if matches!(outcome, StatuslineWire::Added | StatuslineWire::Wrapped)
        && !write_json_atomic(&path, &root)
    {
        return StatuslineWire::Skipped;
    }
    outcome
}

/// The command string of a `statusLine` value, whether it's our object form
/// (`{type,command}`) or Claude Code's bare-string form (`"my-bar"`).
fn statusline_command(root: &serde_json::Value) -> Option<String> {
    match root.get("statusLine")? {
        serde_json::Value::String(s) => Some(s.clone()),
        v => v.get("command").and_then(|c| c.as_str()).map(str::to_owned),
    }
}

/// Is the configured `statusLine` already trimwire's (plain or wrapped)? Pure.
fn statusline_is_ours(root: &serde_json::Value) -> bool {
    // Require BOTH markers: a third-party tool whose command merely contains
    // "statusline render" must not be mistaken for ours (and removed by
    // uninstall). Our commands are always `<trimwire-exe> statusline render…`.
    statusline_command(root)
        .is_some_and(|c| c.contains("trimwire") && c.contains("statusline render"))
}

/// Set `root.statusLine` to a command iff it's absent and `root` is an object.
/// Returns `Some(())` if it mutated `root`, `None` if a statusLine already
/// exists (leave it). Pure — unit-tested.
fn set_statusline_if_absent(root: &mut serde_json::Value, command: &str) -> Option<()> {
    let obj = root.as_object_mut()?;
    if obj.contains_key("statusLine") {
        return None;
    }
    obj.insert(
        "statusLine".to_owned(),
        serde_json::json!({ "type": "command", "command": command }),
    );
    Some(())
}

/// Replace `root.statusLine.command` with our `wrapper`, returning the original
/// command (to persist so we can restore it). `None` if there's no command
/// string or it's already our wrapper. Pure — unit-tested.
fn rewrap_statusline(root: &mut serde_json::Value, wrapper: &str) -> Option<String> {
    // Works for both the object form and Claude Code's bare-string statusLine.
    let original = statusline_command(root)?;
    if original.contains("statusline render --wrap-file") {
        return None; // already wrapped by us
    }
    let obj = root.as_object_mut()?;
    obj.insert(
        "statusLine".to_owned(),
        serde_json::json!({ "type": "command", "command": wrapper }),
    );
    Some(original)
}

/// Undo our statusLine wiring, but only if it's *ours*:
/// - plain `trimwire statusline render` → remove the key;
/// - our `--wrap-file` wrapper → restore the user's original command.
///
/// A user's own statusLine is never touched.
pub fn unwire_statusline() -> Unwired {
    let Some(path) = claude_settings_path() else {
        return Unwired::NotOurs;
    };
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Unwired::NotOurs;
    };
    let Ok(mut root) = serde_json::from_str::<serde_json::Value>(&text) else {
        return Unwired::NotOurs;
    };
    let cmd = statusline_command(&root).unwrap_or_default();

    let (outcome, changed) = if cmd.contains("statusline render --wrap-file") {
        // Wrapped by us. Restore the stashed original if we still have it; if
        // the stash is gone we can't recover the original, so at least drop the
        // dangling wrapper (which would otherwise keep rendering trimwire's row
        // against a missing file) and report honestly — never claim success.
        // Prefer the FULL original statusLine value (lossless: keeps fields like
        // ccstatusline's padding/refreshInterval). Fall back to reconstructing
        // from the command-only `.cmd` stash for setups wrapped before this stash
        // existed.
        let full = wrapped_json_path()
            .and_then(|jf| std::fs::read_to_string(&jf).ok())
            .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok());
        let original = wrapped_cmd_path()
            .and_then(|wf| std::fs::read_to_string(&wf).ok())
            .map(|s| s.trim().to_owned());
        match (root.as_object_mut(), full, original) {
            (Some(obj), Some(full_val), _) => {
                obj.insert("statusLine".to_owned(), full_val);
                (Unwired::Restored, true)
            }
            (Some(obj), None, Some(orig)) => {
                obj.insert(
                    "statusLine".to_owned(),
                    serde_json::json!({ "type": "command", "command": orig }),
                );
                (Unwired::Restored, true)
            }
            (Some(obj), None, None) => {
                obj.remove("statusLine");
                (Unwired::StashMissing, true)
            }
            _ => (Unwired::NotOurs, false),
        }
    } else if cmd.contains("statusline render") {
        // Plain trimwire bar (we added it) → remove the key.
        let removed = root
            .as_object_mut()
            .map(|o| o.remove("statusLine").is_some())
            .unwrap_or(false);
        (
            if removed {
                Unwired::Removed
            } else {
                Unwired::NotOurs
            },
            removed,
        )
    } else {
        (Unwired::NotOurs, false)
    };

    if changed {
        write_json_atomic(&path, &root);
    }
    outcome
}

/// The guarded shell-rc block exporting the gateway env vars.
fn rc_block(base_url: &str) -> String {
    format!(
        "{RC_MARKER_START}\n\
         # ANTHROPIC_BASE_URL routes Claude Code through the local trimwire gateway.\n\
         # ENABLE_TOOL_SEARCH re-enables Claude Code's web search, which it disables\n\
         # whenever ANTHROPIC_BASE_URL is set (see docs/FAQ.md).\n\
         export ANTHROPIC_BASE_URL='{base_url}'\nexport ENABLE_TOOL_SEARCH=true\n{RC_MARKER_END}\n"
    )
}

/// Append `block` to `existing` rc content unless our marker is already
/// present. Returns the new content, or `None` if no change is needed.
/// Only ever appends — never touches the user's existing lines.
fn ensure_rc_block(existing: &str, block: &str) -> Option<String> {
    if existing.contains(RC_MARKER_START) {
        return None;
    }
    let mut out = existing.to_owned();
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    out.push('\n');
    out.push_str(block);
    Some(out)
}

/// Best-effort shell rc path from `$SHELL` (`~/.zshrc` / `~/.bashrc` /
/// `~/.config/fish/config.fish`).
fn shell_rc_path() -> Option<PathBuf> {
    let home = std::env::var("HOME").ok()?;
    let shell = std::env::var("SHELL").unwrap_or_default();
    let file = if shell.ends_with("zsh") {
        ".zshrc"
    } else if shell.ends_with("bash") {
        ".bashrc"
    } else if shell.ends_with("fish") {
        ".config/fish/config.fish"
    } else {
        return None;
    };
    Some(Path::new(&home).join(file))
}

/// Returns `true` when the current shell is fish (detected via `$SHELL`),
/// mirroring `detected_shell_hint()`.
fn is_fish_shell() -> bool {
    std::env::var("SHELL").unwrap_or_default().ends_with("fish")
}

/// The guarded shell-rc block for fish: uses `set -gx` syntax.
fn rc_block_fish(base_url: &str) -> String {
    format!(
        "{RC_MARKER_START}\n\
         # ANTHROPIC_BASE_URL routes Claude Code through the local trimwire gateway.\n\
         # ENABLE_TOOL_SEARCH re-enables Claude Code's web search, which it disables\n\
         # whenever ANTHROPIC_BASE_URL is set (see docs/FAQ.md).\n\
         set -gx ANTHROPIC_BASE_URL '{base_url}'\nset -gx ENABLE_TOOL_SEARCH true\n{RC_MARKER_END}\n"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fish_rc_block_uses_set_gx_syntax() {
        let block = rc_block_fish("http://127.0.0.1:8765");
        // Fish uses `set -gx`, not `export`.
        assert!(
            block.contains("set -gx ANTHROPIC_BASE_URL 'http://127.0.0.1:8765'"),
            "fish block must use set -gx for ANTHROPIC_BASE_URL"
        );
        assert!(
            block.contains("set -gx ENABLE_TOOL_SEARCH true"),
            "fish block must use set -gx for ENABLE_TOOL_SEARCH"
        );
        assert!(
            !block.contains("export "),
            "fish block must not contain bash-style export"
        );
        // Still wrapped in the idempotent markers.
        assert!(block.contains(RC_MARKER_START) && block.contains(RC_MARKER_END));
        // Idempotent when re-applied.
        let updated = ensure_rc_block("# existing\n", &block).expect("should add");
        assert!(ensure_rc_block(&updated, &block).is_none());
    }

    #[test]
    fn bash_zsh_rc_block_uses_export_syntax() {
        let block = rc_block("http://127.0.0.1:8765");
        assert!(block.contains("export ANTHROPIC_BASE_URL='http://127.0.0.1:8765'"));
        assert!(!block.contains("set -gx"));
    }

    #[test]
    fn rc_block_added_once_then_idempotent() {
        let block = rc_block("http://127.0.0.1:8765");
        let updated = ensure_rc_block("export FOO=1\n", &block).expect("should add");
        assert!(
            updated.starts_with("export FOO=1\n"),
            "preserves existing lines"
        );
        assert!(updated.contains(RC_MARKER_START) && updated.contains(RC_MARKER_END));
        // Value is single-quoted so a hostile (but already charset-sanitized) value
        // can never be reinterpreted by the shell (defense-in-depth for P1-1).
        assert!(updated.contains("ANTHROPIC_BASE_URL='http://127.0.0.1:8765'"));
        // Re-running is a no-op.
        assert!(ensure_rc_block(&updated, &block).is_none());
    }

    #[test]
    fn ensure_rc_block_handles_empty_and_missing_newline() {
        let block = rc_block("http://x");
        assert!(ensure_rc_block("", &block).unwrap().ends_with(&block));
        // No trailing newline on existing content → we insert a separator.
        let updated = ensure_rc_block("noeol", &block).unwrap();
        assert!(updated.starts_with("noeol\n"));
    }

    #[test]
    fn statusline_set_only_when_absent() {
        // Absent → we add it, preserving other keys.
        let mut root = serde_json::json!({ "theme": "dark" });
        assert!(set_statusline_if_absent(&mut root, "trimwire statusline").is_some());
        assert_eq!(root["statusLine"]["command"], "trimwire statusline");
        assert_eq!(root["theme"], "dark", "other settings untouched");
    }

    #[test]
    fn statusline_never_clobbers_existing() {
        // Present (e.g. claude-statusline) → we must not touch it.
        let mut root = serde_json::json!({
            "statusLine": { "type": "command", "command": "claude-statusline" }
        });
        assert!(set_statusline_if_absent(&mut root, "trimwire statusline").is_none());
        assert_eq!(root["statusLine"]["command"], "claude-statusline");
    }

    #[test]
    fn detects_our_own_statusline() {
        let ours = serde_json::json!({"statusLine":{"type":"command","command":"/x/trimwire statusline render"}});
        let wrapped = serde_json::json!({"statusLine":{"type":"command","command":"/x/trimwire statusline render --wrap-file /y"}});
        let theirs =
            serde_json::json!({"statusLine":{"type":"command","command":"claude-statusline"}});
        assert!(statusline_is_ours(&ours));
        assert!(statusline_is_ours(&wrapped));
        assert!(!statusline_is_ours(&theirs));
        assert!(!statusline_is_ours(&serde_json::json!({})));
        // A third-party tool whose command merely contains "statusline render"
        // must NOT be claimed as ours (else uninstall would remove the user's).
        let lookalike = serde_json::json!(
            {"statusLine":{"type":"command","command":"my-bar statusline render -v"}});
        assert!(!statusline_is_ours(&lookalike));
    }

    #[test]
    fn write_text_atomic_writes_and_overwrites() {
        let dir = std::env::temp_dir().join(format!("tw-atomic-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join("rc");
        write_text_atomic(&target, "first\n").unwrap();
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "first\n");
        // overwrite is atomic + leaves no temp file behind
        write_text_atomic(&target, "second\n").unwrap();
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "second\n");
        let leftover: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp."))
            .collect();
        assert!(leftover.is_empty(), "no temp file should remain");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn statusline_command_reads_object_and_string_forms() {
        let obj = serde_json::json!({"statusLine":{"type":"command","command":"a"}});
        let string = serde_json::json!({"statusLine":"b"});
        assert_eq!(statusline_command(&obj).as_deref(), Some("a"));
        assert_eq!(statusline_command(&string).as_deref(), Some("b"));
        assert_eq!(statusline_command(&serde_json::json!({})), None);
    }

    #[test]
    fn wrap_handles_bare_string_statusline() {
        // Claude Code allows `statusLine: "cmd"`; wrap must stash it, not bail.
        let mut root = serde_json::json!({"statusLine": "my-bar.sh"});
        let original = rewrap_statusline(&mut root, "/x/trimwire statusline render --wrap-file /y")
            .expect("should wrap a string statusLine");
        assert_eq!(original, "my-bar.sh");
        assert_eq!(
            root["statusLine"]["command"],
            "/x/trimwire statusline render --wrap-file /y"
        );
    }

    #[test]
    fn wrap_preserves_original_and_is_idempotent() {
        let mut root = serde_json::json!({
            "statusLine": { "type": "command", "command": "~/.claude/statusline.sh" }
        });
        let wrapper = "/usr/bin/trimwire statusline render --wrap-file /home/u/.trimwire/statusline-wrapped.cmd";
        let original = rewrap_statusline(&mut root, wrapper).expect("should wrap");
        assert_eq!(
            original, "~/.claude/statusline.sh",
            "returns the original to stash"
        );
        assert_eq!(root["statusLine"]["command"], wrapper);
        // Wrapping again is a no-op (already our wrapper).
        assert!(rewrap_statusline(&mut root, wrapper).is_none());
    }
}
