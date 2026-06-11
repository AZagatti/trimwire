//! `trimwire serve` (hidden) — run the gateway in the foreground until SIGINT.
//!
//! This is the internal command the service manager (systemd/launchd/supervisor)
//! calls. End users start/stop the service with `trimwire on`/`off`; advanced
//! users can run the gateway directly with this command.

use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::{Context, Result};

use trimwire::config::Config;
use trimwire::ledger::Ledger;
use trimwire::proxy::gateway;

/// Run the gateway in the foreground. CLI `--listen`/`--upstream` override the
/// config; config falls back to built-in defaults.
pub fn serve(
    listen: Option<String>,
    upstream: Option<String>,
    audit: Option<String>,
) -> Result<()> {
    let config = Config::load().context("load config")?;
    let listen = listen.unwrap_or_else(|| config.server.listen.clone());
    let upstream = upstream.unwrap_or_else(|| config.server.upstream.clone());
    // --audit flag wins; else the TRIMWIRE_AUDIT env var.
    let audit = audit.or_else(|| std::env::var("TRIMWIRE_AUDIT").ok());
    let addr: SocketAddr = listen
        .parse()
        .with_context(|| format!("parse listen address {listen}"))?;

    let ledger = if config.ledger.enabled {
        Ledger::open(&config.ledger.db_path, config.ledger.retain_days)
    } else {
        Ledger::disabled()
    };
    let config = Arc::new(config);

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("build tokio runtime")?;
    runtime.block_on(async move {
        tokio::select! {
            res = gateway::run(addr, upstream, config, ledger, audit) => res,
            sig = shutdown_signal() => {
                eprintln!("[gateway] received {sig}, shutting down");
                Ok(())
            }
        }
    })
}

/// Await the first shutdown signal and return its name. Handles **both** SIGINT
/// (Ctrl-C) and SIGTERM on Unix — SIGTERM is what `systemctl stop`/`restart` and
/// `trimwire off` send, and an UNHANDLED SIGTERM is a hard kernel kill (no clean
/// exit). Catching it lets the process exit cleanly. (In-flight streams are still
/// cut on exit; a timed graceful drain is a separate, larger follow-up.)
async fn shutdown_signal() -> &'static str {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        // If a handler can't be installed, fall back to a never-resolving future on
        // that arm rather than panicking the gateway.
        let mut term = match signal(SignalKind::terminate()) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("[gateway] could not install SIGTERM handler: {e}");
                std::future::pending::<()>().await;
                unreachable!()
            }
        };
        tokio::select! {
            _ = tokio::signal::ctrl_c() => "SIGINT",
            _ = term.recv() => "SIGTERM",
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
        "Ctrl-C"
    }
}
