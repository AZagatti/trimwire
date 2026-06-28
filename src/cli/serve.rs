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
/// config; config falls back to built-in defaults. The control API + web cockpit
/// is spawned alongside it only when `[admin] enabled` is set.
pub fn serve(
    listen: Option<String>,
    upstream: Option<String>,
    audit: Option<String>,
) -> Result<()> {
    run_serve(listen, upstream, audit, false)
}

/// `trimwire cockpit` (POC) — run the gateway AND the local control API + web
/// cockpit, forcing the admin listener on regardless of `[admin] enabled`, then
/// print the cockpit URL. See `docs/cockpit/`.
pub fn cockpit() -> Result<()> {
    run_serve(None, None, None, true)
}

/// Shared body for `serve`/`cockpit`. `force_admin` turns the control listener on
/// even when `[admin] enabled` is false (used by `trimwire cockpit`).
fn run_serve(
    listen: Option<String>,
    upstream: Option<String>,
    audit: Option<String>,
    force_admin: bool,
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

    // Resolve the (optional) admin listener before moving `config` into the Arc.
    let admin_addr: Option<SocketAddr> = if force_admin || config.admin.enabled {
        let a = config.admin.listen.clone();
        Some(
            a.parse()
                .with_context(|| format!("parse admin listen address {a}"))?,
        )
    } else {
        None
    };
    let gateway_listen = addr.to_string();
    if force_admin {
        if let Some(a) = admin_addr {
            eprintln!("[cockpit] open  http://{a}  in your browser");
        }
    }

    let config = Arc::new(config);

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("build tokio runtime")?;
    runtime.block_on(async move {
        let admin = {
            let config = config.clone();
            let gateway_listen = gateway_listen.clone();
            async move {
                match admin_addr {
                    Some(a) => trimwire::admin::run(a, config, gateway_listen).await,
                    // No admin listener: never resolve, so it never wins the select.
                    None => std::future::pending::<Result<()>>().await,
                }
            }
        };
        tokio::select! {
            res = gateway::run(addr, upstream, config.clone(), ledger, audit) => res,
            res = admin => res,
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
