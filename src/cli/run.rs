//! `trimwire run [claude args...]` — start the gateway in the background, launch
//! `claude` pointed at it, wait, and propagate the exit code.

use std::net::{SocketAddr, TcpStream};
use std::process::Command;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

use trimwire::config::Config;
use trimwire::ledger::Ledger;
use trimwire::proxy::gateway;

/// Start the gateway (unless one is already listening), launch `claude` with
/// `ANTHROPIC_BASE_URL` pointed at it, wait, and exit with claude's status.
///
/// We deliberately do NOT `exec` claude: by keeping our process as the parent
/// and the gateway on a background (daemon) thread, the gateway is torn down
/// when this process exits, so no orphaned daemon is left behind.
///
/// `bypass = true` runs this ONE session WITHOUT trimwire: it skips the gateway
/// entirely and points `claude` straight at the upstream (`api.anthropic.com`),
/// overriding the shell's gateway `ANTHROPIC_BASE_URL` for the child only. The
/// always-up gateway and everyone else's sessions are untouched.
pub fn run(claude_args: &[String], audit: Option<String>, bypass: bool) -> Result<()> {
    let config = Config::load().context("load config")?;

    if bypass {
        return run_bypass(&config, claude_args, audit);
    }

    let listen = config.server.listen.clone();
    let addr: SocketAddr = listen
        .parse()
        .with_context(|| format!("parse listen address {listen}"))?;
    let base_url = format!("http://{listen}");
    // --audit flag wins; else the TRIMWIRE_AUDIT env var.
    let audit = audit.or_else(|| std::env::var("TRIMWIRE_AUDIT").ok());

    // Start our own gateway only if nothing is already serving that address.
    if TcpStream::connect(addr).is_err() {
        spawn_gateway(addr, config, audit);
        wait_until_listening(addr, Duration::from_secs(5))
            .context("gateway did not start listening")?;
        eprintln!("[trimwire] gateway listening on {base_url}");
    } else if super::service::healthz_ok(addr) {
        // Confirm it's actually trimwire (answers /healthz) before reusing —
        // otherwise we'd hand the real ANTHROPIC_* auth headers to whatever
        // stranger is squatting on the port.
        eprintln!("[trimwire] reusing gateway already listening on {base_url}");
        if audit.is_some() {
            // We didn't start that gateway, so we can't turn its audit on — say
            // so rather than let the user think `--audit` took effect.
            eprintln!(
                "[trimwire] note: --audit/TRIMWIRE_AUDIT ignored — the already-running gateway \
                 controls its own audit; stop it first (`trimwire off`) then \
                 `trimwire run --audit …` to change that"
            );
        }
    } else {
        bail!(
            "something is listening on {addr} but isn't trimwire (no /healthz) — \
             refusing to point `claude` (and your API token) at it. Free the port, \
             or set a different `[server] listen`."
        );
    }

    let status = Command::new("claude")
        .args(claude_args)
        .env("ANTHROPIC_BASE_URL", &base_url)
        .env("ENABLE_TOOL_SEARCH", "true")
        .status()
        .context("failed to launch `claude` (is it installed and on PATH?)")?;

    std::process::exit(status.code().unwrap_or(1));
}

/// `trimwire run --bypass` — launch `claude` for ONE session with trimwire out
/// of the loop, pointed straight at the upstream. No gateway is started or
/// reused; the child's `ANTHROPIC_BASE_URL` is overridden to the configured
/// upstream (`api.anthropic.com` by default), so nothing prunes this session
/// while the global gateway keeps serving everyone else.
fn run_bypass(config: &Config, claude_args: &[String], audit: Option<String>) -> Result<()> {
    if audit.is_some() || std::env::var("TRIMWIRE_AUDIT").is_ok() {
        // No gateway is in the loop, so there's nothing to audit — say so rather
        // than let the user think --audit took effect.
        eprintln!(
            "[trimwire] note: --audit/TRIMWIRE_AUDIT ignored with --bypass (no gateway in the loop)"
        );
    }
    let upstream = config.server.upstream.trim_end_matches('/').to_owned();
    eprintln!("[trimwire] bypass — this session goes straight to {upstream} (no pruning)");
    let status = Command::new("claude")
        .args(claude_args)
        // Override the shell's gateway URL for the CHILD only: straight to Anthropic.
        .env("ANTHROPIC_BASE_URL", &upstream)
        // Keep web search working (Claude Code disables it whenever a custom base
        // URL is set); matches the rc block install writes.
        .env("ENABLE_TOOL_SEARCH", "true")
        .status()
        .context("failed to launch `claude` (is it installed and on PATH?)")?;
    std::process::exit(status.code().unwrap_or(1));
}

/// Spawn the gateway on a background daemon thread with its own runtime.
fn spawn_gateway(addr: SocketAddr, config: Config, audit: Option<String>) {
    let upstream = config.server.upstream.clone();
    let ledger = if config.ledger.enabled {
        Ledger::open(&config.ledger.db_path, config.ledger.retain_days)
    } else {
        Ledger::disabled()
    };
    let config = Arc::new(config);
    thread::spawn(move || {
        let rt = match tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
        {
            Ok(rt) => rt,
            Err(e) => {
                eprintln!("[trimwire] failed to build gateway runtime: {e}");
                return;
            }
        };
        if let Err(e) = rt.block_on(gateway::run(addr, upstream, config, ledger, audit)) {
            eprintln!("[trimwire] gateway exited: {e}");
        }
    });
}

/// Poll-connect until the gateway accepts connections or the deadline passes.
fn wait_until_listening(addr: SocketAddr, timeout: Duration) -> Result<()> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if TcpStream::connect(addr).is_ok() {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(25));
    }
    bail!("timed out waiting for {addr}")
}
