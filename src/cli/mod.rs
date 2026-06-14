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
mod run;
mod serve;
mod service;
mod share;
mod stats;
mod statusline;
mod summarizer;
mod sweep;

pub use benchmark::benchmark;
pub use config_edit::{config_edit, config_show};
pub use dashboard::dashboard;
pub use hook::hook;
pub use install::{install, statusline_add};
pub use preview::preview;
pub use recall::recall;
pub use run::run;
pub use serve::serve;
pub use share::share_benchmark;
pub use share::{share_disable, share_enable, share_stats};
pub use stats::stats;
pub use statusline::statusline;
pub use summarizer::{summarizer_probe, summarizer_setup, summarizer_status};
pub use sweep::{sweep_all, sweep_file, sweep_list, sweep_undo};

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

/// `trimwire on` — start the always-up gateway service.
pub fn on() -> Result<()> {
    match service::on() {
        Ok(()) => println!("trimwire is on ({})", service::detect().label()),
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

/// `trimwire off` — stop the gateway service (explicit kill switch).
pub fn off() -> Result<()> {
    // Degrade gracefully like `on()` rather than a raw anyhow "Error:" blast — a
    // user running `off` on a never-installed / already-stopped setup shouldn't see
    // a `systemctl ... failed` stack.
    match service::off() {
        Ok(()) => {
            println!(
                "trimwire is off. (The gateway is stopped — no longer accepting connections.)"
            );
            println!(
                "→ Your shell rc still exports ANTHROPIC_BASE_URL (from `trimwire install`), so \
                 Claude Code will try the stopped gateway and fail until you either:"
            );
            println!("    • `trimwire on` to start it again, or");
            println!(
                "    • `unset ANTHROPIC_BASE_URL` to send Claude Code straight to Anthropic in this shell"
            );
            println!(
                "  (`trimwire uninstall` removes the service but leaves the rc block — it prints the exact lines to delete by hand.)"
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
    Ok(())
}

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
pub fn doctor() -> Result<()> {
    use trimwire::config::Config;

    println!("trimwire doctor\n");

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
        return Ok(()); // exit 0 — pre-install is not a failure
    }

    // ── Installed (or partial) — run the full health check ─────────────────
    // Accumulate hard failures (✗) across every check, then exit non-zero at the
    // end so `trimwire doctor && claude` / CI healthchecks work. We must NOT
    // return Err for this — anyhow would print an ugly "Error:" line after the
    // clean diagnostic. Advisory warnings (⚠) never set this.
    let mut failed = false;

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
                println!(
                    "{} gateway not responding on {addr} — start it with `trimwire on` (service) \
                     or `trimwire run`",
                    render::warn()
                );
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
                }
                Err(_) => {
                    // Not set in the current shell is recoverable (the env var is
                    // written to the shell rc by `trimwire install`; opening a new
                    // terminal or sourcing the rc fixes it). Don't set failed=true.
                    println!(
                        "{} ANTHROPIC_BASE_URL not set in THIS shell — Claude Code launched here \
                         won't route through trimwire (install adds it to new shells; an IDE/app \
                         may need it set separately)\n  → to fix this shell: \
                         export ANTHROPIC_BASE_URL=http://{addr}",
                        render::warn()
                    );
                }
            }
        }
        Err(e) => {
            println!("{} can't parse the listen address: {e}", render::bad());
            failed = true;
        }
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
                    println!(
                        "{} summarizer provider '{}': style={}, base_url={base_url}, \
                         model={model}, api_key_env={key_env}",
                        render::ok(),
                        provider.id,
                        provider.style,
                    );
                    // Warn if the key env var is configured but currently unset in the environment.
                    if !provider.api_key_env.is_empty()
                        && std::env::var(&provider.api_key_env)
                            .unwrap_or_default()
                            .is_empty()
                    {
                        println!(
                            "{} ${} is not set in the current environment — \
                             the API summarizer will fail at runtime.",
                            render::warn(),
                            provider.api_key_env,
                        );
                        println!("  → export {}=\"<your-api-key>\"", provider.api_key_env,);
                        println!(
                            "  → to persist across shells, add that export to your ~/.zshrc or ~/.bashrc."
                        );
                        println!("  → then run `trimwire on` to start the gateway.");
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
                         unverified for it (approved: qwen3.5:4b, qwen3.5:2b)",
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

    // Non-zero exit on any hard failure (✗) so scripts/CI can gate on it. Exit
    // cleanly (no anyhow error blast); flush first so no buffered line is lost
    // when stdout is piped (process::exit skips destructors).
    if failed {
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
#   "default"  aggressive (the shipped default, good for Max / quota-rich):
#              all eight cache-safe strategies with tight knobs. Sliding window denylists
#              throwaway browser-automation verbs (*screenshot*, *navigate*,
#              *click*, *browser_act*, Grep) while preserving reference-data MCP results.
#   "gentle"   lightest touch (good for Pro / pay-per-token): de-duplication
#              of repeated calls + dropping old failed inputs + conservative
#              bloat-cap (>32 KB). No sliding window or image stripping.
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
# model          = "qwen3.5:4b"             # only harm-validated model; don't downgrade
#
# # Cloud API backend — define one or more named providers:
# # PRIVACY: the prunable slice is sent to your chosen provider to be summarized.
# # This is your key and your provider — trimwire never has a default endpoint.
# [[summarizer.providers]]
# id          = "anthropic"                 # any unique label; use as engine = "anthropic"
# style       = "anthropic"                 # "anthropic" | "openai" (OpenAI-compatible)
# base_url    = "https://api.anthropic.com" # REQUIRED: the API root URL (e.g. OpenAI: https://api.openai.com)
# model       = ""                          # e.g. "claude-haiku-4-5" or "gpt-4o-mini"
# api_key_env = "ANTHROPIC_API_KEY"         # name of the env var that holds your key
#                                           # (security: trimwire stores ONLY the name,
#                                           # never the key itself — keys must not live
#                                           # in a committed config file)
#                                           # set it before starting the gateway:
#                                           #   export ANTHROPIC_API_KEY="sk-ant-..."
#                                           # to persist, add that export to ~/.zshrc or ~/.bashrc
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
