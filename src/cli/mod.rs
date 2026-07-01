//! CLI command implementations (binary-private; `main.rs` stays thin wiring).
//!
//! One file per subcommand. Shared config-writing lives here because both
//! `install` and `config` need it.

mod benchmark;
mod civil;
mod config_edit;
mod dashboard;
mod hook;
mod install;
mod preview;
mod recall;
mod render;
mod report;
mod run;
mod serve;
mod service;
mod share;
mod stats;
mod statusline;
mod summarizer;
mod sweep;
mod update;

pub use benchmark::benchmark;
pub use config_edit::{config_edit, config_show};
pub use dashboard::dashboard;
pub use hook::hook;
pub use install::{install, statusline_add};
pub use preview::preview;
pub use recall::recall;
pub use report::report;
pub use run::run;
pub use serve::serve;
pub use share::share_benchmark;
pub use share::{share_disable, share_enable, share_stats};
pub use stats::stats;
pub use statusline::statusline;
pub use summarizer::{summarizer_probe, summarizer_setup, summarizer_status};
pub use sweep::{sweep_all, sweep_file, sweep_list, sweep_undo};
pub use update::{update, upgrade};

use std::path::Path;

use anyhow::{Context, Result};

/// Async helper shared by `summarizer` and `benchmark`: GET `{endpoint}/api/tags`
/// and return the `models[*].name` list. Reuses the proxy's hyper-rustls client
/// (plain `http://` localhost works fine). Any failure is returned as an `Err`
/// the caller turns into a friendly message.
pub(super) async fn fetch_ollama_tags(endpoint: &str) -> Result<Vec<String>> {
    use http_body_util::{BodyExt, Full};
    use hyper::body::Bytes;

    let url = format!("{}/api/tags", endpoint.trim_end_matches('/'));
    let req = hyper::Request::builder()
        .method(hyper::Method::GET)
        .uri(&url)
        .body(Full::new(Bytes::new()))
        .context("build /api/tags request")?;
    let client = trimwire::proxy::upstream::build_client();
    let resp = client
        .request(req)
        .await
        .with_context(|| format!("GET {url} (is ollama running?)"))?;
    if !resp.status().is_success() {
        anyhow::bail!("ollama /api/tags returned HTTP {}", resp.status());
    }
    let bytes = resp
        .into_body()
        .collect()
        .await
        .context("read /api/tags")?
        .to_bytes();
    let v: serde_json::Value = serde_json::from_slice(&bytes).context("parse /api/tags JSON")?;
    let names = v
        .get("models")
        .and_then(|m| m.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|m| m.get("name").and_then(|n| n.as_str()).map(str::to_owned))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    Ok(names)
}

/// Resolve the gateway listen address from config (for lifecycle commands).
fn listen_addr() -> Result<std::net::SocketAddr> {
    use trimwire::config::Config;
    let listen = Config::load()
        .map(|c| c.server.listen)
        .unwrap_or_else(|_| "127.0.0.1:8765".to_owned());
    listen
        .parse()
        .with_context(|| format!("parse listen address {listen}"))
}

/// `trimwire on` — resume pruning: clear the bypass sentinel and (re)start the
/// always-up gateway service.
pub fn on() -> Result<()> {
    // Clear bypass first so that even if the service is already up (the common
    // case — `on` after `off` never stopped it), pruning resumes immediately.
    // If we CAN'T clear it, don't go on to claim "pruning active": the sentinel
    // would still force the gateway to forward unmodified. Report honestly and
    // stop, so the success line never lies about the pruning state.
    if let Err(e) = trimwire::bypass::disable() {
        println!("couldn't clear bypass: {e}");
        println!(
            "→ pruning is still OFF — the {} sentinel remains. Fix the error above, \
             then re-run `trimwire on`.",
            trimwire::bypass::sentinel_path().display()
        );
        return Ok(());
    }
    match service::on() {
        Ok(()) => println!(
            "trimwire is on — pruning active ({}).",
            service::detect().label()
        ),
        // Degrade gracefully like `status`/`stats` rather than a bare `Error:`.
        Err(e) => {
            println!("couldn't start the service: {e}");
            println!(
                "→ is it installed? run `trimwire install`. Or run the gateway yourself: \
                 `trimwire run`."
            );
        }
    }
    Ok(())
}

/// `trimwire off` — stop pruning. By default this is a **true bypass**: the
/// gateway keeps serving but forwards every request UNMODIFIED to Anthropic, so
/// the shell's `ANTHROPIC_BASE_URL` still resolves and Claude Code keeps working
/// with zero pruning. `--stop` instead hard-stops the gateway process (the old
/// kill-switch behavior — power users / freeing the port).
pub fn off(stop: bool) -> Result<()> {
    if stop {
        let had_bypass = trimwire::bypass::is_active();
        // Hard stop — degrade gracefully rather than a raw anyhow "Error:" blast.
        match service::off() {
            Ok(()) => {
                println!("trimwire gateway stopped.");
                // Now that the gateway is down, clear any bypass sentinel so a
                // later restart-by-other-means (socket activation, `systemctl
                // start`, launchd RunAtLoad) doesn't silently come back bypassed.
                // Only on a SUCCESSFUL stop: if the stop failed we leave the state
                // untouched (still "off" if it was bypassing), which matches the
                // user's intent better than flipping the running gateway back to
                // pruning.
                if had_bypass {
                    if trimwire::bypass::disable().is_ok() {
                        println!("  (also cleared the bypass sentinel.)");
                    } else {
                        println!(
                            "  (note: couldn't clear the bypass sentinel — remove {} by hand.)",
                            trimwire::bypass::sentinel_path().display()
                        );
                    }
                }
                println!(
                    "→ your shell still exports ANTHROPIC_BASE_URL, so Claude Code can't reach \
                     Anthropic until you `trimwire on` (restart it) — or use plain `trimwire off` \
                     (no --stop) next time, which bypasses to Anthropic without stopping."
                );
            }
            Err(e) => {
                println!("couldn't stop the service: {e}");
                println!(
                    "→ is it installed/running? check `trimwire status`. If you never ran \
                     `trimwire install`, there's nothing to stop."
                );
            }
        }
        return Ok(());
    }

    // Default: bypass. Keep the gateway serving (so ANTHROPIC_BASE_URL stays
    // live) and flip the runtime sentinel the gateway reads per request.
    match trimwire::bypass::enable() {
        Ok(()) => {
            println!(
                "trimwire is off — sessions go straight to Anthropic, unmodified (no pruning)."
            );
            // Bypass only takes effect if the gateway is actually serving (it
            // reads the sentinel per request). If it was hard-stopped, the socket
            // is dead and ANTHROPIC_BASE_URL has nothing to connect to — say so
            // rather than imply Claude will keep working. Best-effort probe; we do
            // NOT auto-start (that would surprise `off` and shell the service
            // manager) — `trimwire on` is the one command that both starts it and
            // resumes pruning.
            if let Ok(addr) = listen_addr() {
                if !service::healthz_ok(addr) {
                    println!(
                        "⚠ but the gateway isn't running right now — run `trimwire on` to start it \
                         so requests actually reach Anthropic."
                    );
                }
            }
            println!(
                "→ `trimwire on` to resume pruning. (`trimwire off --stop` stops the gateway entirely.)"
            );
        }
        Err(e) => {
            println!("couldn't switch to bypass: {e}");
            println!("→ stop the gateway instead with `trimwire off --stop`.");
        }
    }
    Ok(())
}

// `trimwire update` / `upgrade` lives in `cli/update.rs` (read-only check).

/// `trimwire status` — is it running / serving?
pub fn status() -> Result<()> {
    service::status(listen_addr()?)
}

/// `trimwire doctor` — one-shot setup/health diagnosis. Composes the checks a
/// user otherwise runs by hand: does the config load + which profile, is the
/// gateway serving, does `ANTHROPIC_BASE_URL` point at it, which service manager,
/// is the ledger present. Read-only (only a local `/healthz` probe).
///
/// **Exit contract:**
/// - exit 0 in the normal/recoverable cases — not installed yet, or installed but
///   the gateway just isn't up / `ANTHROPIC_BASE_URL` isn't set in this shell.
///   These are advisory (`warn`): they print what to run next (`trimwire install`
///   / `trimwire on` / the `export` line) but don't fail, so `trimwire doctor &&
///   claude` works even right after install while the service is warming up.
/// - exit 1 (✗ lines) only on a genuine hard failure: a config file that won't
///   load/parse, an unparseable listen address, or a disqualified summarizer model.
/// - with `--strict`: advisory states (gateway not running / `ANTHROPIC_BASE_URL`
///   unset) also exit 1 — for CI / scripted health checks.
pub fn doctor(strict: bool) -> Result<()> {
    use trimwire::config::Config;

    println!("trimwire doctor\n");

    // Build platform — handy in bug reports, and the asset-selection primitive
    // `trimwire upgrade` uses to pick the matching release artifact.
    // Printed before the install-state branch so it always shows.
    println!(
        "{} platform: {}",
        render::bullet(),
        trimwire::build_target()
    );

    // Install receipt — how the binary was installed (written by the curl|sh
    // installer / refreshed by `trimwire install`). Absent is normal for
    // cargo/manual installs and is non-fatal. `trimwire upgrade` reads
    // this to decide whether self-update is allowed.
    match trimwire::receipt::load() {
        Some(r) => println!(
            "{} install: {} · v{} · {}",
            render::bullet(),
            r.method,
            r.version,
            r.binary_path
        ),
        None => println!(
            "{} install: no receipt recorded (manual or cargo install)",
            render::bullet()
        ),
    }

    // Advisory: a newer release exists. Silent when already current or the check
    // can't complete (offline/rate-limited) — never a warning or failure, so it
    // doesn't affect the exit contract (incl. `--strict`).
    if let Some(tag) = update::newer_available() {
        println!(
            "{} update: {} available (you have {}) — run `trimwire upgrade`",
            render::bullet(),
            tag.trim_start_matches('v'),
            env!("CARGO_PKG_VERSION")
        );
    }

    // ── Detect the "not installed yet" state early ─────────────────────────
    // Three simultaneous signals: no config file on disk + gateway not
    // responding on the configured (or default) listen addr + ANTHROPIC_BASE_URL
    // unset. All three together mean the user just downloaded the binary and
    // hasn't run `trimwire install` yet — that's informational, not a failure.
    let config_path = trimwire::config::global_config_path();
    let config_exists = config_path.exists();
    let base_url_set = std::env::var("ANTHROPIC_BASE_URL")
        .map(|v| !v.is_empty())
        .unwrap_or(false);
    let default_addr: std::net::SocketAddr = "127.0.0.1:8765".parse().unwrap();
    let gateway_addr = listen_addr().unwrap_or(default_addr);
    let gateway_up = service::healthz_ok(gateway_addr);

    if !config_exists && !gateway_up && !base_url_set {
        println!(
            "{} trimwire is not installed yet — this is normal if you just downloaded the binary.",
            render::bullet()
        );
        println!(
            "{} config file: not found at {}",
            render::bullet(),
            config_path.display()
        );
        println!(
            "{} gateway: not running (expected before install)",
            render::bullet()
        );
        println!(
            "{} ANTHROPIC_BASE_URL: not set (expected before install)",
            render::bullet()
        );
        println!();
        println!("→ run `trimwire install` to set up the gateway, shell env, and starter config.");
        println!("→ run `trimwire doctor` again after install to verify the setup.");
        if strict {
            // Pre-install is an advisory state (not installed / gateway down / env
            // unset). `--strict` exists for CI / scripted health checks, which must
            // fail when trimwire isn't set up at all — matching the documented
            // contract in docs/CLI.md. Plain `doctor` still returns 0 below so
            // `trimwire doctor && claude` works on a fresh machine.
            std::process::exit(1);
        }
        return Ok(()); // exit 0 — pre-install is not a failure (non-strict default)
    }

    // ── Installed (or partial) — run the full health check ─────────────────
    // Accumulate hard failures (✗) across every check, then exit non-zero at the
    // end so `trimwire doctor && claude` / CI healthchecks work. We must NOT
    // return Err for this — anyhow would print an ugly "Error:" line after the
    // clean diagnostic. Advisory warnings (⚠) never set `failed`; they set
    // `warned` so that `--strict` can exit 1 on them too.
    let mut failed = false;
    let mut warned = false;

    let cfg = match Config::load() {
        Ok(c) => {
            let profile = c
                .profile
                .clone()
                .unwrap_or_else(|| trimwire::config::DEFAULT_PROFILE.to_owned());
            println!("{} config loads — profile: {profile}", render::ok());
            let s = &c.strategies;
            let any = s.cross_turn_dedup.enabled
                || s.failed_input_purge.enabled
                || s.bloat_cap.enabled
                || s.sliding_window.enabled
                || s.image_strip.enabled;
            if !any {
                println!(
                    "{} every strategy is disabled — the gateway is a pass-through (no pruning)",
                    render::warn()
                );
            }
            Some(c)
        }
        Err(e) => {
            println!("{} config failed to load: {e}", render::bad());
            failed = true;
            None
        }
    };

    match listen_addr() {
        Ok(addr) => {
            if service::healthz_ok(addr) {
                println!(
                    "{} gateway is serving on {addr} (/healthz ok)",
                    render::ok()
                );
            } else {
                // Not running yet is recoverable (user just needs `trimwire on`).
                // Don't set failed=true so `trimwire doctor && claude` still works
                // when the gateway hasn't been started yet after install.
                // With --strict, set warned=true so the caller can exit 1.
                println!(
                    "{} gateway not responding on {addr} — start it with `trimwire on` (service) \
                     or `trimwire run`",
                    render::warn()
                );
                warned = true;
            }
            match std::env::var("ANTHROPIC_BASE_URL") {
                Ok(v) if base_url_matches(&v, addr) => {
                    println!(
                        "{} ANTHROPIC_BASE_URL points at the gateway ({v})",
                        render::ok()
                    );
                }
                Ok(v) => {
                    println!(
                        "{} ANTHROPIC_BASE_URL = {v} — does not match the gateway addr {addr}",
                        render::warn()
                    );
                    warned = true;
                }
                Err(_) => {
                    // Not set in the current shell is recoverable (the env var is
                    // written to the shell rc by `trimwire install`; opening a new
                    // terminal or sourcing the rc fixes it). Don't set failed=true.
                    // With --strict, set warned=true so the caller can exit 1.
                    println!(
                        "{} ANTHROPIC_BASE_URL not set in THIS shell — Claude Code launched here \
                         won't route through trimwire (install adds it to new shells; an IDE/app \
                         may need it set separately)\n  → to fix this shell: \
                         export ANTHROPIC_BASE_URL='http://{addr}'",
                        render::warn()
                    );
                    warned = true;
                }
            }
        }
        Err(e) => {
            println!("{} can't parse the listen address: {e}", render::bad());
            failed = true;
        }
    }

    // Bypass state (from `trimwire off`): the gateway is serving but forwarding
    // unmodified. Advisory, not a failure — it's a deliberate user choice — so it
    // never flips the exit code (matching the "all strategies disabled" note).
    if trimwire::bypass::is_active() {
        println!(
            "{} bypass ON (`trimwire off`) — the gateway forwards unmodified; NO pruning. \
             Run `trimwire on` to resume.",
            render::warn()
        );
    }

    println!(
        "{} service manager: {}",
        render::bullet(),
        service::detect().label()
    );

    if let Some(c) = &cfg {
        if c.ledger.enabled {
            let p = trimwire::ledger::resolve_path(&c.ledger.db_path);
            if p.exists() {
                println!("{} ledger present: {}", render::ok(), p.display());
            } else {
                println!(
                    "{} ledger not yet created (it populates once traffic flows)",
                    render::bullet()
                );
            }
        } else {
            println!("{} ledger disabled in config", render::bullet());
        }
    }

    // OPT-IN summarizer diagnostics. Only act when the user configured a
    // non-model-free engine (else stay quiet — model-free is the default).
    if let Some(c) = &cfg {
        let s = &c.summarizer;
        if s.engine != "model-free" {
            // Coupling: reprune carries the cached summary across turns; without it
            // the summarizer is a silent no-op.
            if !c.reprune.enabled {
                println!(
                    "{} summarizer.engine is set but reprune.enabled=false — the summarizer \
                     is a SILENT NO-OP (reprune carries the cached summary across turns). \
                     Enable reprune, or it does nothing.",
                    render::warn()
                );
            }
            // Provider-id case: any engine string that isn't "model-free" or "local" is a
            // provider id that must resolve to an entry in summarizer.providers.
            if s.engine != "local" {
                if let Some(provider) = s.providers.iter().find(|p| p.id == s.engine) {
                    let base_url = if provider.base_url.is_empty() {
                        match provider.style.as_str() {
                            "openai" => "https://api.openai.com",
                            _ => "https://api.anthropic.com",
                        }
                        .to_owned()
                    } else {
                        provider.base_url.clone()
                    };
                    let model = if provider.model.is_empty() {
                        "(not set)".to_owned()
                    } else {
                        provider.model.clone()
                    };
                    let key_env = if provider.api_key_env.is_empty() {
                        "(not set)".to_owned()
                    } else {
                        provider.api_key_env.clone()
                    };
                    let key_file = match provider.api_key_file.as_deref() {
                        Some(f) if !f.is_empty() => format!(", api_key_file={f}"),
                        _ => String::new(),
                    };
                    println!(
                        "{} summarizer provider '{}': style={}, base_url={base_url}, \
                         model={model}, api_key_env={key_env}{key_file}",
                        render::ok(),
                        provider.id,
                        provider.style,
                    );
                    // Resolve the key the SAME way the runtime does (env first, then
                    // api_key_file). A configured key file makes the provider work in a
                    // daemon even when the shell env var is unset, so only warn when
                    // neither source actually yields a key.
                    match trimwire::summarizer::api::resolve_provider_key(provider) {
                        Ok(_) => {
                            let has_file = provider
                                .api_key_file
                                .as_deref()
                                .is_some_and(|f| !f.trim().is_empty());
                            // CRITICAL CAVEAT: doctor runs in YOUR shell, so it sees a
                            // key you exported — but the always-up service that
                            // `trimwire install` registers does NOT inherit shell
                            // exports. If the only source is an env var (no file) and a
                            // managed service is installed, the summarizer will silently
                            // fail there even though doctor looks green. Surface it.
                            if !has_file && service::managed_service_installed() {
                                println!(
                                    "{} provider '{}' key resolves from your shell env (${}), but the \
                                     installed background service can't see shell exports — the \
                                     summarizer will silently fall back to model-free there. \
                                     Set api_key_file = \"~/.{}_key\" to fix it (works as a service).",
                                    render::warn(),
                                    provider.id,
                                    provider.api_key_env,
                                    provider.id,
                                );
                            }
                            // Permission hygiene: a key file readable by other users is a
                            // leak. Warn (don't block) if it isn't owner-only.
                            #[cfg(unix)]
                            if let Some(f) =
                                provider.api_key_file.as_deref().filter(|f| !f.is_empty())
                            {
                                use std::os::unix::fs::PermissionsExt;
                                let path = trimwire::ledger::resolve_path(f);
                                if let Ok(meta) = std::fs::metadata(&path) {
                                    let mode = meta.permissions().mode() & 0o777;
                                    if mode & 0o077 != 0 {
                                        println!(
                                            "{} api_key_file {} is mode {:o} — readable by others; \
                                             run `chmod 600 {}`",
                                            render::warn(),
                                            path.display(),
                                            mode,
                                            path.display(),
                                        );
                                    }
                                }
                            }
                        }
                        Err(reason) => {
                            println!(
                                "{} provider '{}' key unavailable: {reason} — \
                                 the API summarizer will fail at runtime.",
                                render::warn(),
                                provider.id,
                            );
                            // Recommend the key file first: the always-up service that
                            // `trimwire install` sets up can't see shell exports.
                            println!(
                                "  → recommended: set `api_key_file = \"~/.{}_key\"` on this provider \
                                 in trimwire.toml (works as a service — the default install; `chmod 600` it).",
                                provider.id
                            );
                            if !provider.api_key_env.is_empty() {
                                println!(
                                    "  → or, for foreground `trimwire run` only: export {}=\"<your-api-key>\" \
                                     (add to ~/.zshrc/~/.bashrc to persist).",
                                    provider.api_key_env
                                );
                            }
                            println!(
                                "  → then `trimwire off --stop && trimwire on` to restart the gateway with the new key."
                            );
                        }
                    }
                    // Privacy reminder: the prunable slice leaves the machine.
                    println!(
                        "{} privacy: the prunable slice is sent to {base_url} to be summarized \
                         (your key, your provider, your choice)",
                        render::bullet(),
                    );
                } else {
                    println!(
                        "{} summarizer.engine='{}' does not match any provider in \
                         summarizer.providers — run `trimwire summarizer setup` to configure it",
                        render::warn(),
                        s.engine,
                    );
                }
            }
            if s.engine == "local" {
                let m = s.local.model.as_str();
                if trimwire::summarizer::is_disqualified(m) {
                    println!(
                        "{} summarizer.local.model = {m} is DISQUALIFIED (hallucinates / overstates \
                         completed work) — the runtime guard will REFUSE it; set it to qwen3.5:4b",
                        render::bad()
                    );
                    failed = true;
                } else if trimwire::summarizer::WARN_MODELS.contains(&m) {
                    println!(
                        "{} summarizer.local.model = {m} FAILED the harm gate (drops load-bearing \
                         facts) — a RAM opt-down, not an equal to qwen3.5:4b",
                        render::warn()
                    );
                } else if trimwire::summarizer::APPROVED_MODELS.contains(&m) {
                    println!(
                        "{} summarizer.local.model = {m} (cost + harm validated)",
                        render::ok()
                    );
                } else {
                    println!(
                        "{} summarizer.local.model = {m} is unvalidated — summary fidelity is \
                         unverified for it (validated: qwen3.5:4b, qwen3.5:4b-q8_0, qwen3.5:9b; warning-gated opt-down: qwen3.5:2b)",
                        render::warn()
                    );
                }
                match ollama_has_model(&s.local.endpoint, m) {
                    None => println!(
                        "{} ollama not reachable at {} — when it's down the feature silently \
                         falls back to model-free pruning (never load-bearing)",
                        render::warn(),
                        s.local.endpoint
                    ),
                    Some(true) => println!("{} ollama reachable and {m} is pulled", render::ok()),
                    Some(false) => {
                        println!(
                            "{} ollama reachable but {m} is NOT pulled — run `ollama pull {m}`",
                            render::warn()
                        )
                    }
                }
            }
        } else {
            // Discoverability: surface the opt-in summarizer for model-free users.
            println!(
                "{} summarizer is off (engine = model-free) — optional: \
                 `trimwire summarizer setup` compresses old context on long sessions",
                render::bullet()
            );
        }
    }

    // Non-zero exit on any hard failure (✗) so scripts/CI can gate on it. With
    // --strict, also exit 1 on advisory warnings (⚠) so CI health checks fail
    // fast when the gateway is down or ANTHROPIC_BASE_URL is unset. Exit cleanly
    // (no anyhow error blast); flush first so no buffered line is lost when stdout
    // is piped (process::exit skips destructors).
    if failed || (strict && warned) {
        use std::io::Write;
        let _ = std::io::stdout().flush();
        std::process::exit(1);
    }
    Ok(())
}

/// Best-effort blocking probe (mirrors `service::healthz_ok`): is `model` present in
/// ollama's `/api/tags` at `endpoint`? `None` = ollama unreachable / non-200;
/// `Some(true|false)` once it answered. Sync TCP so it runs in `doctor` without a
/// tokio runtime; substring match on the tags JSON is enough for a doctor hint.
fn ollama_has_model(endpoint: &str, model: &str) -> Option<bool> {
    use std::io::{Read, Write};
    use std::net::ToSocketAddrs;
    let hostport = endpoint
        .trim_end_matches('/')
        .trim_start_matches("https://")
        .trim_start_matches("http://");
    let hostport = hostport.split('/').next().unwrap_or(hostport);
    let with_port = if hostport.contains(':') {
        hostport.to_owned()
    } else {
        format!("{hostport}:11434")
    };
    let addr = with_port.to_socket_addrs().ok()?.next()?;
    let mut s =
        std::net::TcpStream::connect_timeout(&addr, std::time::Duration::from_millis(700)).ok()?;
    let _ = s.set_read_timeout(Some(std::time::Duration::from_millis(1500)));
    let req = format!("GET /api/tags HTTP/1.1\r\nHost: {with_port}\r\nConnection: close\r\n\r\n");
    s.write_all(req.as_bytes()).ok()?;
    let mut buf = String::new();
    let _ = s.read_to_string(&mut buf);
    if !buf.starts_with("HTTP/1.1 200") {
        return None;
    }
    Some(buf.contains(&format!("\"{model}\"")))
}

/// Does `ANTHROPIC_BASE_URL` value `v` actually point at the gateway on `addr`?
/// Compares host:port (not a raw substring, which false-positives on decoy URLs
/// and port substrings, and false-negatives on a `0.0.0.0` listen). The port
/// must match exactly; the host matches if it equals the listen host, or the
/// listen host is a wildcard (`0.0.0.0`/`::` serves every interface), or both
/// are loopback (`127.0.0.1` ≡ `localhost` ≡ `::1`).
fn base_url_matches(v: &str, addr: std::net::SocketAddr) -> bool {
    let authority = v
        .trim()
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .split('/')
        .next()
        .unwrap_or("");
    let Some((host, port_str)) = authority.rsplit_once(':') else {
        return false;
    };
    if port_str.parse::<u16>().ok() != Some(addr.port()) {
        return false;
    }
    let listen = addr.ip();
    if listen.is_unspecified() {
        return true; // 0.0.0.0 / :: serves all interfaces → any host on this port
    }
    let host = host.trim_start_matches('[').trim_end_matches(']'); // [::1]
    let host_is_loopback = host == "localhost"
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback());
    host == listen.to_string() || (listen.is_loopback() && host_is_loopback)
}

/// Print the truthful result of an `unwire_statusline()`.
fn report_unwire(u: install::Unwired) {
    use install::Unwired::*;
    match u {
        Restored => println!("removed trimwire — restored your original statusline."),
        Removed => println!("removed trimwire's statusline bar."),
        StashMissing => {
            println!("removed trimwire's wrapped statusline, but COULD NOT restore your original");
            println!("— the backup (~/.trimwire/statusline-wrapped.cmd) is missing. Re-add your");
            println!("  own statusline in ~/.claude/settings.json if you had one.");
        }
        NotOurs => println!("trimwire isn't in your statusline — nothing to remove."),
    }
}

/// `trimwire statusline remove` — take trimwire out of the statusline.
pub fn statusline_remove() -> Result<()> {
    report_unwire(install::unwire_statusline());
    Ok(())
}

/// `trimwire uninstall` — remove the service, env hooks, and lingering.
pub fn uninstall() -> Result<()> {
    service::uninstall()?;
    let u = install::unwire_statusline(); // only touches the statusLine if it's ours
    println!("removed trimwire's service + env hooks.");
    report_unwire(u);
    println!("→ the shell-rc export block is left in place; delete the `# >>> trimwire >>>` block");
    println!("  from your rc if you want it gone, then restart your shell.");
    Ok(())
}

/// Commented starter config written by `install`/`config`. It selects the
/// `"default"` pruning profile (aggressive, the shipped default) and
/// documents the `"gentle"` alternative. The profile sets every strategy
/// knob; the file shows how to override individual values.
const CONFIG_TEMPLATE: &str = r#"# trimwire configuration. See https://github.com/AZagatti/trimwire
# A per-project ./.trimwire.toml overrides these; TRIMWIRE_* env vars win last.

# Pruning profile — the one knob most people need:
#   "default"  aggressive (the shipped default), cleanest context:
#              all eight cache-safe strategies with tight knobs. Sliding window denylists
#              throwaway browser-automation verbs (*screenshot*, *navigate*,
#              *click*, *browser_act*, Grep) while preserving reference-data MCP results.
#   "gentle"   lightest touch (least pruning): de-duplication
#              of repeated calls + dropping old failed inputs + conservative
#              bloat-cap (>32 KB) + conservative thinking_strip (keep 8).
#              No sliding window, stale_reads, stale_input_cap, or image stripping.
profile = "default"

[server]
listen = "127.0.0.1:8765"
upstream = "https://api.anthropic.com"

# The profile above sets every strategy knob. Override individual values only if
# you want to deviate — anything set here wins over the profile. Examples:
#
# [strategies.sliding_window]
# denylist_tools = ["mcp__playwright__*", "Bash"]   # also stub old Bash output
#
# [strategies.bloat_cap]
# threshold_bytes = 8192                            # trim smaller old results

[ledger]
# Per-request savings + cache-prefix telemetry. Pruned to `retain_days` at
# startup. Set enabled = false to record nothing.
enabled = true
db_path = "~/.trimwire/ledger.db"
retain_days = 365

# ── Optional summarizer (OFF by default; engine = "model-free" is a no-op) ────
#
# The easy way to configure this is `trimwire summarizer setup` (interactive wizard).
#
# Two built-in engine tokens:
#   model-free  the default — no summarizer, no model calls
#   local       ollama / llama.cpp on your own machine (no API key needed)
#
# Any other engine string is treated as a provider id that must match an entry
# in [[summarizer.providers]] (see below). Providers let you use Anthropic,
# OpenAI, or any OpenAI-compatible endpoint such as OpenRouter.
#
# Never load-bearing: any failure silently falls back to model-free pruning.
#
# [summarizer]
# engine = "model-free"          # "model-free" | "local" | <provider-id>
# # fallback = ["local"]         # tokens to try in order if the primary fails
#
# # Local ollama backend (engine = "local"):
# [summarizer.local]
# endpoint       = "http://localhost:11434"
# model          = "qwen3.5:4b"             # default harm-validated model (others approved: qwen3.5:4b-q8_0, qwen3.5:9b); don't downgrade
#
# # Cloud API backend — define one or more named providers:
# # PRIVACY: the prunable slice is sent to your chosen provider to be summarized.
# # This is your key and your provider — trimwire ships no default summarizer endpoint (you set base_url).
# [[summarizer.providers]]
# id          = "anthropic"                 # any unique label; use as engine = "anthropic"
# style       = "anthropic"                 # "anthropic" | "openai" (OpenAI-compatible)
# base_url    = "https://api.anthropic.com" # REQUIRED: the API root URL (e.g. OpenAI: https://api.openai.com)
# model       = ""                          # e.g. "claude-haiku-4-5" or "gpt-4o-mini"
# # Key source — pick ONE (trimwire stores the NAME/PATH, never the key itself):
# #
# # RECOMMENDED — api_key_file. `trimwire install` runs an always-up systemd/launchd
# # service, which does NOT inherit your ~/.zshrc exports, so an env var is invisible
# # to it. A key file is read at runtime and works as a service AND in `trimwire run`.
# # A leading ~/ expands to $HOME. Create it, then `chmod 600`:
# #   printf '%s' "sk-ant-..." > ~/.config/trimwire/anthropic.key && chmod 600 ~/.config/trimwire/anthropic.key
# api_key_file = "~/.config/trimwire/anthropic.key"
# #
# # OR api_key_env — the NAME of an env var holding the key. Works for foreground
# # `trimwire run`; a background service won't see it unless you import it into the
# # service environment. Set it before starting: export ANTHROPIC_API_KEY="sk-ant-..."
# # (add to ~/.zshrc or ~/.bashrc to persist).
# api_key_env = "ANTHROPIC_API_KEY"
"#;

/// Write the starter config to `path` if it does not already exist. Returns
/// `true` if it wrote a new file. Never overwrites an existing config.
/// (Accessible to the subcommand submodules as `super::write_config_if_absent`.)
fn write_config_if_absent(path: &Path) -> Result<bool> {
    if path.exists() {
        return Ok(false);
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create config dir {}", parent.display()))?;
    }
    std::fs::write(path, CONFIG_TEMPLATE).with_context(|| format!("write {}", path.display()))?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use trimwire::config::Config;

    #[test]
    fn base_url_matches_compares_host_port_not_substring() {
        let addr: std::net::SocketAddr = "127.0.0.1:8765".parse().unwrap();
        // Exact + scheme/loopback variants match.
        assert!(base_url_matches("http://127.0.0.1:8765", addr));
        assert!(base_url_matches("http://localhost:8765", addr));
        assert!(base_url_matches("http://127.0.0.1:8765/", addr));
        // Decoy host and port-substring must NOT match (the substring-bug cases).
        assert!(!base_url_matches(
            "http://127.0.0.1:8765-decoy.evil.com",
            addr
        ));
        assert!(!base_url_matches("http://127.0.0.1:87650", addr));
        assert!(!base_url_matches("http://10.0.0.1:8765", addr));
        // A wildcard listen serves every interface → any host on the port matches.
        let any: std::net::SocketAddr = "0.0.0.0:8765".parse().unwrap();
        assert!(base_url_matches("http://127.0.0.1:8765", any));
        assert!(base_url_matches("http://192.168.1.5:8765", any));
        assert!(!base_url_matches("http://192.168.1.5:9999", any));
    }

    #[test]
    fn write_config_if_absent_does_not_clobber() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested/trimwire.toml");
        assert!(
            write_config_if_absent(&path).unwrap(),
            "first write creates it"
        );
        assert!(path.exists());
        std::fs::write(&path, "user edits").unwrap();
        assert!(
            !write_config_if_absent(&path).unwrap(),
            "second call is a no-op"
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "user edits");
    }

    #[test]
    fn config_template_selects_the_default_profile() {
        // The shipped template must round-trip and select the default profile.
        let cfg: Config = toml::from_str(CONFIG_TEMPLATE).expect("template is valid TOML/Config");
        assert_eq!(
            cfg.profile.as_deref(),
            Some(trimwire::config::DEFAULT_PROFILE)
        );
        assert_eq!(cfg.server.listen, "127.0.0.1:8765");
        // The template states no strategy knobs — the profile drives them.
        // The "default" baseline turns all eight cache-safe strategies on with aggressive knobs
        // (every KNOWN_STRATEGIES entry except the opt-in simhash_dedup):
        let default = trimwire::config::profile_baseline("default");
        assert!(default.strategies.cross_turn_dedup.enabled);
        assert!(default.strategies.failed_input_purge.enabled);
        assert!(default.strategies.bloat_cap.enabled);
        assert!(default.strategies.sliding_window.enabled);
        assert!(default.strategies.image_strip.enabled);
        // Verb-based denylist (not the blanket mcp__*).
        assert!(
            default
                .strategies
                .sliding_window
                .denylist_tools
                .contains(&"*screenshot*".to_owned())
        );
    }
}
