//! `trimwire config` — ensure the config exists, then open it in `$EDITOR`.

use std::process::Command;

use anyhow::{Context, Result, bail};

use trimwire::config;

/// Ensure the config file exists (writing the starter template if not), then open
/// it in an editor. Tries `$EDITOR`, then `$VISUAL`, then the first of nano/vim/vi
/// actually on PATH; if none launches, prints the path to edit by hand rather than
/// a raw error. Each candidate may carry args (e.g. `EDITOR="code --wait"`).
pub fn config_edit() -> Result<()> {
    let path = config::global_config_path();
    super::write_config_if_absent(&path)?;

    let mut candidates: Vec<String> = Vec::new();
    for var in ["EDITOR", "VISUAL"] {
        if let Ok(e) = std::env::var(var) {
            if !e.trim().is_empty() {
                candidates.push(e);
            }
        }
    }
    candidates.extend(["nano", "vim", "vi"].map(String::from));

    for editor in &candidates {
        let mut parts = editor.split_whitespace();
        let Some(prog) = parts.next() else { continue };
        let args: Vec<&str> = parts.collect();
        match Command::new(prog).args(&args).arg(&path).status() {
            Ok(status) if status.success() => return Ok(()),
            Ok(_) => bail!("editor `{editor}` exited with failure"),
            // Not installed → try the next candidate rather than erroring out.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e).with_context(|| format!("launch editor `{editor}`")),
        }
    }

    // Nothing launchable — degrade gracefully instead of a raw "No such file".
    println!("couldn't open an editor ($EDITOR/$VISUAL unset and no nano/vim/vi on PATH).");
    println!("→ edit the config directly: {}", path.display());
    println!("  or set one, e.g. `EDITOR=code trimwire config`.");
    Ok(())
}

/// `trimwire config show [--json]` — print the *resolved* effective config: the
/// active profile expanded, with global/project/env merged on top. Answers
/// "what is trimwire actually running?", which the layered merge otherwise hides.
pub fn config_show(json: bool) -> Result<()> {
    let cfg = config::Config::load().context("load config")?;
    let profile = cfg
        .profile
        .clone()
        .unwrap_or_else(|| config::DEFAULT_PROFILE.to_owned());

    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&cfg).context("serialize config")?
        );
        return Ok(());
    }

    println!("# trimwire — resolved effective config");
    println!("# active profile: {profile}");
    println!("# `upstream` is resolved from the global config / TRIMWIRE_* env only,");
    println!("# never a project ./.trimwire.toml (credential-routing is global-only).");
    println!(
        "# [share] — telemetry is opt-in and off by default. An empty [share] section is normal."
    );
    println!(
        "# Run `trimwire share enable` to opt in (the collector URL is a built-in default \
         pointing at the live api.trimwire.dev; override [share] endpoint to self-host)."
    );
    println!(
        "# `trimwire share stats` does a dry-run and shows what would be sent until you enable."
    );
    println!();
    print!("{}", toml::to_string_pretty(&cfg).context("render TOML")?);
    Ok(())
}
