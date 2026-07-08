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

/// Default bound for a synchronous ollama-tags probe (#145). It bounds the WHOLE
/// fetch — DNS + connect + request + response — not just the TCP connect. 5s: a
/// healthy localhost ollama answers in single-digit ms, so this only bites a
/// *hung* endpoint (a filtered host that drops packets) or a genuinely slow
/// remote one (the wizard lets you enter a custom endpoint). On a false timeout
/// the outcome is only cosmetic — an empty model list — never a blocked/failed
/// command, so erring generous is safe.
pub(super) const OLLAMA_PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Run `fut` to completion on a one-shot current-thread runtime, then tear the
/// runtime down with a **bounded** shutdown so an orphaned `getaddrinfo`
/// `spawn_blocking` (hyper's default resolver) can't hang the caller (#145/#150).
///
/// Every CLI site that hand-rolled `Builder::new_current_thread()…block_on(fut)`
/// and let the runtime drop implicitly should use this instead: the implicit
/// `Drop` calls `BlockingPool::shutdown(None)`, which blocks **forever** joining
/// any still-running blocking-pool thread (a stuck DNS lookup), defeating any
/// `tokio::time::timeout` the future itself carries. `shutdown_timeout` waits
/// briefly, then leaks the straggler instead of blocking. The caller is
/// responsible for wrapping `fut` in a `tokio::time::timeout` where the network
/// op needs an *observable* cap (the timer must be built inside `fut`, i.e.
/// within the runtime context, or it panics with "no timer running").
pub(super) fn run_bounded<F: std::future::Future>(fut: F) -> Result<F::Output> {
    let rt = BoundedRuntime::new(
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .context("build one-shot runtime")?,
    );
    Ok(rt.block_on(fut))
    // `rt` drops here → bounded `shutdown_timeout`.
}

/// How long a bounded runtime teardown waits for the blocking pool before it
/// leaks the straggler. Long enough for a healthy pool to drain, short enough
/// that a wedged `getaddrinfo` can't stall process exit.
const RUNTIME_SHUTDOWN_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(100);

/// A tokio runtime that ALWAYS tears down via `shutdown_timeout` on drop, rather
/// than the implicit `Drop` (`BlockingPool::shutdown(None)`) that blocks forever
/// joining a stuck `getaddrinfo` (#145/#150). Wrap any ad-hoc CLI runtime that
/// drives network I/O in this: because teardown is guaranteed on `Drop`, it
/// stays correct across loops, `block_on` reuse, and early `?`-returns — the
/// exact shapes a bare `rt.shutdown_timeout(..)` at the end of a function misses
/// (#152). `Deref`s to the runtime, so `rt.block_on(..)` is unchanged.
pub(super) struct BoundedRuntime(Option<tokio::runtime::Runtime>);

impl BoundedRuntime {
    pub(super) fn new(rt: tokio::runtime::Runtime) -> Self {
        Self(Some(rt))
    }
}

impl std::ops::Deref for BoundedRuntime {
    type Target = tokio::runtime::Runtime;
    fn deref(&self) -> &Self::Target {
        // Only `Drop` takes the runtime, so it is always present while borrowable.
        self.0.as_ref().expect("runtime present until drop")
    }
}

impl Drop for BoundedRuntime {
    fn drop(&mut self) {
        if let Some(rt) = self.0.take() {
            rt.shutdown_timeout(RUNTIME_SHUTDOWN_TIMEOUT);
        }
    }
}

/// Synchronous, BOUNDED ollama-tags fetch — the safe entry point every CLI probe
/// site (`summarizer setup`, `summarizer benchmark`, `benchmark --all-installed`)
/// must use instead of hand-rolling `rt.block_on(fetch_ollama_tags(..))`. Bounds
/// both the observable latency (`tokio::time::timeout`) and the runtime teardown
/// (via [`run_bounded`]) — see #145.
///
/// Returns the model list, or an `Err` (timeout / unreachable / bad response) the
/// caller renders as a friendly "configure now, start ollama later" message.
pub(super) fn fetch_ollama_tags_blocking(
    endpoint: &str,
    timeout: std::time::Duration,
) -> Result<Vec<String>> {
    // Build the `timeout` future INSIDE the async block — as a bare argument it
    // would register its timer before the runtime is entered and panic.
    match run_bounded(async { tokio::time::timeout(timeout, fetch_ollama_tags(endpoint)).await })? {
        Ok(inner) => inner,
        Err(_elapsed) => anyhow::bail!(
            "timed out after {}s connecting to {endpoint} (is it firewalled?)",
            timeout.as_secs()
        ),
    }
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
    use trimwire::config::Config;
    println!("{}\n", render::strong("trimwire on"));

    // (Re-)wire the persistent path so a prior full `off` is undone: the shell-rc
    // export + the GUI/login env hook. Both are idempotent — in the common case
    // (path already wired) they're silent no-ops. New shells/GUI apps route
    // through the gateway again; the CURRENT shell needs a re-source (printed
    // only when we actually add the block).
    let cfg = Config::load().unwrap_or_default();
    let listen = cfg.server.listen.clone();
    let coexist = cfg.server.remote_control;
    let base_url = format!("http://{listen}");
    // A failed rc edit is NON-fatal: the gateway can still come up and pruning can
    // resume — don't block the whole re-engage on a cosmetic rc write. In coexist
    // mode this wires BUN_OPTIONS (+ writes the shim) instead of ANTHROPIC_BASE_URL.
    let wiring = install::wiring_for(&cfg);
    // Capture the coexist shim (written as a side effect only when wiring_for
    // succeeds) so the GUI launcher is written ONLY when coexist wiring actually
    // took — never pointing at a shim a bailed wiring_for never created.
    let coexist_shim = match &wiring {
        Ok(install::Wiring::Coexist(shim)) => Some(shim.clone()),
        _ => None,
    };
    match wiring.and_then(|w| install::wire_rc(&w)) {
        Ok(install::RcWire::Added(rc)) => {
            println!(
                "{} re-added the trimwire env exports to {}",
                render::ok(),
                rc.display()
            );
            println!(
                "  {} restart your shell or {} so this shell routes through trimwire.",
                render::dim("→"),
                render::accent(&format!("source {}", rc.display()))
            );
        }
        // Already wired (the normal case) or no detectable rc — stay quiet.
        Ok(install::RcWire::AlreadyPresent(_)) | Ok(install::RcWire::NoShell(_)) => {}
        Err(e) => {
            println!(
                "{} couldn't update your shell rc ({e}) — add the export by hand if needed:",
                render::warn()
            );
            if coexist {
                // Coexist mode: preload the shim; ANTHROPIC_BASE_URL must stay UNSET.
                println!(
                    "  {} export BUN_OPTIONS={}  (leave ANTHROPIC_BASE_URL unset)",
                    render::dim("→"),
                    install::sh_squote(&format!(
                        "--preload {}",
                        install::coexist_shim_path().display()
                    ))
                );
            } else {
                println!(
                    "  {} export ANTHROPIC_BASE_URL='{base_url}'",
                    render::dim("→")
                );
            }
        }
    }
    if let Ok(addr) = listen.parse() {
        if coexist {
            // Coexist mode leaves ANTHROPIC_BASE_URL unset everywhere so Remote
            // Control works; strip any GUI/login env a prior default install wrote.
            service::remove_gui_env_files();
            // (Re)write the process-local launcher for GUI/editor surfaces (#173) and
            // remind how to point an editor wrapper at it — but only if coexist wiring
            // actually produced the shim (skip if wiring_for bailed, e.g. non-loopback).
            // Best-effort: a launcher write failure must not block re-engaging pruning.
            if let Some(shim) = &coexist_shim {
                match install::write_coexist_launcher(shim) {
                    Ok(launcher) => install::print_gui_coexist_guidance(&launcher),
                    Err(e) => println!(
                        "{} couldn't write the GUI coexistence launcher ({e}).",
                        render::warn()
                    ),
                }
            }
        } else {
            install::remove_coexist_launcher(); // symmetric cleanup on mode-switch
            let _ = service::wire_gui_env(addr); // best-effort GUI/login env
        }
    }

    // Clear bypass so pruning is actually active (not merely passthrough). If we
    // CAN'T clear it, don't claim "pruning active": the sentinel would still force
    // passthrough. Report honestly and stop.
    if let Err(e) = trimwire::bypass::disable() {
        println!("{} couldn't clear the pause sentinel: {e}", render::bad());
        println!(
            "  {} pruning is still paused — the {} sentinel remains. Fix the error above, then re-run {}.",
            render::dim("→"),
            trimwire::bypass::sentinel_path().display(),
            render::accent("trimwire on")
        );
        return Ok(());
    }
    match service::on() {
        Ok(()) => println!(
            "{} trimwire is on — pruning active ({}).",
            render::ok(),
            service::detect().label()
        ),
        // Degrade gracefully like `status`/`stats` rather than a bare `Error:`.
        Err(e) => {
            println!("{} couldn't start the service: {e}", render::warn());
            println!(
                "  {} is it installed? run {}. Or run the gateway yourself: {}.",
                render::dim("→"),
                render::accent("trimwire install"),
                render::accent("trimwire run")
            );
        }
    }
    Ok(())
}

/// `trimwire off` — full disengage. Stops the gateway AND strips trimwire from
/// the request path everywhere new processes read it (the shell-rc export + the
/// GUI/login env hook), so Claude Code talks **straight to `api.anthropic.com`**
/// — re-enabling host-gated features like Remote Control (#159/#160). A running
/// shell already exported `ANTHROPIC_BASE_URL` and a child process can't unset it
/// in the parent, so we print the one line that fixes the current shell.
///
/// To only pause pruning while keeping the proxy in the path, use `trimwire
/// pause` / `resume`. To fully re-engage, `trimwire on`.
pub fn off() -> Result<()> {
    println!("{}\n", render::strong("trimwire off"));

    // 1. Stop the gateway. Non-fatal on failure — we still strip the wiring so
    //    the path ends up clean either way.
    match service::off() {
        Ok(()) => println!("{} stopped the trimwire gateway.", render::ok()),
        Err(e) => println!(
            "{} couldn't stop the gateway ({e}) — continuing to unwire the path.",
            render::warn()
        ),
    }

    // 2. Remove the persistent base-url wiring new processes read. A failed rc
    //    edit is NON-fatal: keep going so we still clear the sentinel and tell the
    //    user how to finish by hand (the gateway is already down, so leaving the
    //    export pointing at it would otherwise strand them silently).
    service::remove_gui_env_files();
    if install::remove_coexist_launcher() {
        println!(
            "  {} removed the GUI coexistence launcher; clear any editor {} setting pointing at it.",
            render::dim("→"),
            render::accent("claudeCode.claudeProcessWrapper")
        );
    }
    match install::unwire_rc() {
        Ok(install::RcUnwire::Removed(rc)) => println!(
            "{} removed the trimwire env exports from {}",
            render::ok(),
            rc.display()
        ),
        Ok(install::RcUnwire::NotPresent(rc)) => println!(
            "{} no trimwire block in {} — already clear.",
            render::bullet(),
            rc.display()
        ),
        Ok(install::RcUnwire::NoShell) => {}
        Err(e) => {
            println!(
                "{} couldn't edit your shell rc ({e}) — the trimwire export is still there.",
                render::warn()
            );
            println!(
                "  {} delete the {} block by hand so new shells go straight to Anthropic.",
                render::dim("→"),
                render::accent("# >>> trimwire >>>")
            );
        }
    }

    // 3. Clear the pause sentinel too — moot with the gateway down, but it keeps
    //    the state clean so a later `trimwire on` starts pruning, not paused.
    let _ = trimwire::bypass::disable();

    // 4. The current shell still has the trimwire env exported; we can't unset a
    //    parent's env from here — hand the user the exact line. Detect what's
    //    ACTUALLY exported in THIS shell (default mode → ANTHROPIC_BASE_URL; coexist
    //    → BUN_OPTIONS) rather than trusting config, which may have changed since
    //    the shell started — telling the user to unset the wrong var would leave the
    //    shell still routed. Emit an unset for each that's actually present.
    let fish = install::is_fish_shell();
    let shim = install::coexist_shim_path().display().to_string();
    let base_set = std::env::var("ANTHROPIC_BASE_URL").is_ok();
    let bun_set = std::env::var("BUN_OPTIONS").is_ok_and(|b| b.contains(&shim));
    let unsets = shell_unset_commands(base_set, bun_set, fish);
    println!();
    println!(
        "{} new shells now talk straight to api.anthropic.com — Remote Control works again.",
        render::ok()
    );
    if unsets.is_empty() {
        println!("  {} this shell is already clean.", render::dim("→"));
    } else {
        println!(
            "  {} to fix THIS shell now, run: {}",
            render::dim("→"),
            render::accent(&unsets.join(" ; "))
        );
    }
    println!(
        "  {} re-engage any time with {}.",
        render::dim("→"),
        render::accent("trimwire on")
    );
    println!(
        "  {} only wanted to pause pruning? {} keeps the gateway in the path (no rc changes).",
        render::dim("→"),
        render::accent("trimwire pause")
    );
    Ok(())
}

/// `trimwire pause` — stop pruning but keep the gateway in the request path,
/// forwarding every request UNMODIFIED to Anthropic. Fast, no shell/rc changes;
/// `trimwire resume` flips it back. (This is the old default `trimwire off`
/// behavior; `off` now fully disengages.)
pub fn pause() -> Result<()> {
    match trimwire::bypass::enable() {
        Ok(()) => {
            println!(
                "{} trimwire paused — requests pass through unmodified (no pruning).",
                render::ok()
            );
            // Passthrough only works if the gateway is actually serving (it reads
            // the sentinel per request). Best-effort probe; don't auto-start.
            if let Ok(addr) = listen_addr() {
                if !service::healthz_ok(addr) {
                    println!(
                        "{} but the gateway isn't running — {} to start it, or {} to leave the path entirely.",
                        render::warn(),
                        render::accent("trimwire on"),
                        render::accent("trimwire off")
                    );
                }
            }
            println!(
                "  {} {} to resume pruning.",
                render::dim("→"),
                render::accent("trimwire resume")
            );
        }
        Err(e) => {
            println!("{} couldn't pause: {e}", render::warn());
            println!(
                "  {} to leave the path entirely instead, run {}.",
                render::dim("→"),
                render::accent("trimwire off")
            );
        }
    }
    Ok(())
}

/// `trimwire resume` — resume pruning after a `pause` (clears the pause
/// sentinel). Does not touch the service or the wiring; use `trimwire on` for a
/// full (re-)engage.
pub fn resume() -> Result<()> {
    match trimwire::bypass::disable() {
        Ok(()) => {
            println!("{} trimwire resumed — pruning active.", render::ok());
            if let Ok(addr) = listen_addr() {
                if !service::healthz_ok(addr) {
                    println!(
                        "{} but the gateway isn't running — run {} to start it.",
                        render::warn(),
                        render::accent("trimwire on")
                    );
                }
            }
        }
        Err(e) => {
            println!("{} couldn't resume: {e}", render::warn());
            println!(
                "  {} the {} sentinel remains — remove it by hand, or run {}.",
                render::dim("→"),
                trimwire::bypass::sentinel_path().display(),
                render::accent("trimwire on")
            );
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

    println!("{}\n", render::strong("trimwire doctor"));

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
        println!(
            "  {} run {} to set up the gateway, shell env, and starter config.",
            render::dim("→"),
            render::accent("trimwire install")
        );
        println!(
            "  {} run {} again after install to verify the setup.",
            render::dim("→"),
            render::accent("trimwire doctor")
        );
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
            let coexist = cfg.as_ref().is_some_and(|c| c.server.remote_control);
            if coexist {
                coexist_wiring_check(addr, &mut warned);
            } else {
                match std::env::var("ANTHROPIC_BASE_URL") {
                    Ok(v) if base_url_matches(&v, addr) => {
                        println!(
                            "{} ANTHROPIC_BASE_URL points at the gateway ({v})",
                            render::ok()
                        );
                        // #159: Claude Code's Remote Control only runs when the base
                        // URL is literally api.anthropic.com. Note the trade-off + the
                        // one command that steps fully out of the path.
                        println!(
                            "  {} Remote Control is disabled while routed through trimwire — run {} to fully disengage (direct to Anthropic).",
                            render::dim("→"),
                            render::accent("trimwire off")
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
        }
        Err(e) => {
            println!("{} can't parse the listen address: {e}", render::bad());
            failed = true;
        }
    }

    // Bypass state (from `trimwire pause`): the gateway is serving but forwarding
    // unmodified. Advisory, not a failure — it's a deliberate user choice — so it
    // never flips the exit code (matching the "all strategies disabled" note).
    if trimwire::bypass::is_active() {
        println!(
            "{} bypass ON (`trimwire pause`) — the gateway forwards unmodified; NO pruning. \
             Run `trimwire resume` to prune again.",
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
                                "  {} recommended: set `api_key_file = \"~/.{}_key\"` on this provider \
                                 in trimwire.toml (works as a service — the default install; `chmod 600` it).",
                                render::dim("→"),
                                provider.id
                            );
                            if !provider.api_key_env.is_empty() {
                                println!(
                                    "  {} or, for foreground `trimwire run` only: export {}=\"<your-api-key>\" \
                                     (add to ~/.zshrc/~/.bashrc to persist).",
                                    render::dim("→"),
                                    provider.api_key_env
                                );
                            }
                            println!(
                                "  {} then bounce the gateway so it picks up the new key: `trimwire off && trimwire on`.",
                                render::dim("→")
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

/// Resolve `hostport` to its first socket address, bounded by `timeout`.
///
/// `ToSocketAddrs::to_socket_addrs` is a synchronous, uncancellable
/// `getaddrinfo` with no built-in cap; a hostname on a wedged resolver blocks
/// the calling thread indefinitely. We run it on a detached helper thread and
/// give up after `timeout`, returning `None` (the caller treats that as
/// "unreachable"). If the resolver never returns, the helper thread leaks —
/// the same bounded tradeoff as a runtime's `shutdown_timeout` straggler. See #153.
fn resolve_first_addr(
    hostport: &str,
    timeout: std::time::Duration,
) -> Option<std::net::SocketAddr> {
    use std::net::ToSocketAddrs;
    let (tx, rx) = std::sync::mpsc::channel();
    let hostport = hostport.to_owned();
    std::thread::spawn(move || {
        // `send` fails only if the receiver already timed out and hung up; ignore.
        let _ = tx.send(hostport.to_socket_addrs().ok().and_then(|mut it| it.next()));
    });
    rx.recv_timeout(timeout).ok().flatten()
}

/// Best-effort blocking probe (mirrors `service::healthz_ok`): is `model` present in
/// ollama's `/api/tags` at `endpoint`? `None` = ollama unreachable / non-200;
/// `Some(true|false)` once it answered. Sync TCP so it runs in `doctor` without a
/// tokio runtime; substring match on the tags JSON is enough for a doctor hint.
fn ollama_has_model(endpoint: &str, model: &str) -> Option<bool> {
    use std::io::{Read, Write};
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
    // Bound the DNS step (#153): `to_socket_addrs` is a synchronous, unbounded
    // `getaddrinfo`. For the default `localhost` literal it's instant, but a
    // custom endpoint on a broken resolver would hang `doctor` here — before the
    // bounded `connect_timeout` below ever runs. Cap it; a timeout reads as
    // "unreachable" (`None`), same as a refused connect.
    let addr = resolve_first_addr(&with_port, std::time::Duration::from_millis(700))?;
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
/// Doctor check for Remote-Control coexistence mode (`[server] remote_control`).
/// Unlike the default path, coexist mode wants `ANTHROPIC_BASE_URL` UNSET (so
/// Remote Control's gate is satisfied) and `BUN_OPTIONS` preloading the shim (so
/// `/v1/messages` still routes through the gateway). Sets `*warned` on any gap.
/// The shell command(s) that unset whatever trimwire env is ACTUALLY exported in
/// the current shell after `off`. Driven by the real environment (`base_url_set`
/// = ANTHROPIC_BASE_URL present; `bun_set` = BUN_OPTIONS preloads our shim), NOT
/// config — config may have changed since the shell started, and telling the user
/// to unset the wrong var would leave the shell still routed. `fish` selects
/// `set -e` vs `unset`. Empty when the shell carries neither (already clean).
fn shell_unset_commands(base_url_set: bool, bun_set: bool, fish: bool) -> Vec<&'static str> {
    let mut cmds = Vec::new();
    if base_url_set {
        cmds.push(if fish {
            "set -e ANTHROPIC_BASE_URL"
        } else {
            "unset ANTHROPIC_BASE_URL"
        });
    }
    if bun_set {
        cmds.push(if fish {
            "set -e BUN_OPTIONS"
        } else {
            "unset BUN_OPTIONS"
        });
    }
    cmds
}

/// The four states coexist-mode wiring can be in, for `doctor`. Pure decision
/// (no env / FS access) so it's unit-testable; `coexist_wiring_check` maps the
/// live env/FS onto it and prints.
#[derive(Debug, PartialEq)]
enum CoexistState {
    /// Wired correctly: base URL unset, shim present + preloaded.
    Ok,
    /// ANTHROPIC_BASE_URL is set — Remote Control will be blocked.
    BaseUrlSet(String),
    /// The shim file is missing.
    ShimMissing,
    /// BUN_OPTIONS doesn't preload the shim in this shell.
    NotPreloaded,
}

/// Pure classifier for [`coexist_wiring_check`]. `base_url` = value of
/// ANTHROPIC_BASE_URL if set; `bun_options` = value of BUN_OPTIONS if set.
fn coexist_state(
    base_url: Option<&str>,
    shim_exists: bool,
    bun_options: Option<&str>,
    shim_path: &str,
) -> CoexistState {
    if let Some(v) = base_url {
        return CoexistState::BaseUrlSet(v.to_owned());
    }
    if !shim_exists {
        return CoexistState::ShimMissing;
    }
    if bun_options.is_some_and(|b| b.contains(shim_path)) {
        CoexistState::Ok
    } else {
        CoexistState::NotPreloaded
    }
}

fn coexist_wiring_check(addr: std::net::SocketAddr, warned: &mut bool) {
    let shim = install::coexist_shim_path();
    let shim_s = shim.display().to_string();
    match coexist_state(
        std::env::var("ANTHROPIC_BASE_URL").ok().as_deref(),
        shim.exists(),
        std::env::var("BUN_OPTIONS").ok().as_deref(),
        &shim_s,
    ) {
        CoexistState::Ok => println!(
            "{} Remote-Control coexistence active — /v1/messages routes through the gateway ({addr}); Remote Control works.",
            render::ok()
        ),
        CoexistState::BaseUrlSet(v) => {
            println!(
                "{} Remote-Control coexistence, but ANTHROPIC_BASE_URL is set ({v}) — Remote Control will be blocked. Open a new shell after {}.",
                render::warn(),
                render::accent("trimwire on")
            );
            *warned = true;
        }
        CoexistState::ShimMissing => {
            println!(
                "{} Remote-Control coexistence on, but the shim is missing ({shim_s}) — run {} to (re)write it.",
                render::warn(),
                render::accent("trimwire on")
            );
            *warned = true;
        }
        CoexistState::NotPreloaded => {
            println!(
                "{} Remote-Control coexistence on, but BUN_OPTIONS isn't preloading the shim in THIS shell — open a new shell after {}.",
                render::warn(),
                render::accent("trimwire on")
            );
            *warned = true;
        }
    }
    // GUI/editor surfaces (#173): the launcher is the opt-in injection point for
    // Claude Code launched outside a shell (which never sees the rc block above).
    // Informational — not a failure, since terminal-launched Claude Code prunes via
    // the rc block regardless.
    let launcher = install::coexist_launcher_path();
    if launcher.exists() {
        println!(
            "  {} GUI/editor launcher ready ({}); point VS Code {} at it to prune GUI-launched Claude Code too.",
            render::dim("→"),
            render::accent(&launcher.display().to_string()),
            render::accent("claudeCode.claudeProcessWrapper")
        );
    } else {
        println!(
            "  {} no GUI/editor launcher yet — run {} to write it (lets the VS Code panel prune too).",
            render::dim("→"),
            render::accent("trimwire on")
        );
    }
}

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
        Restored => println!(
            "  {} removed trimwire — restored your original statusline.",
            render::ok()
        ),
        Removed => println!("  {} removed trimwire's statusline bar.", render::ok()),
        StashMissing => {
            println!(
                "  {} removed trimwire's wrapped statusline, but COULD NOT restore your original",
                render::warn()
            );
            println!(
                "  {}",
                render::dim(
                    "— the backup (~/.trimwire/statusline-wrapped.cmd) is missing. Re-add your own \
                     statusline in ~/.claude/settings.json if you had one."
                )
            );
        }
        NotOurs => println!(
            "  {} trimwire isn't in your statusline — nothing to remove.",
            render::bullet()
        ),
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
    println!("{}\n", render::strong("trimwire uninstall"));
    println!("  {} removed trimwire's service + env hooks.", render::ok());
    report_unwire(u);
    println!(
        "  {} the shell-rc export block is left in place; delete the {} block from your rc if you \
         want it gone, then restart your shell.",
        render::dim("→"),
        render::accent("# >>> trimwire >>>")
    );
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
# Remote-Control coexistence. Claude Code refuses to start Remote Control when
# ANTHROPIC_BASE_URL is a custom host, so trimwire's normal wiring blocks it. With
# this on, `install`/`on` wire via BUN_OPTIONS (a preload shim that reroutes only
# /v1/messages through the gateway) and leave ANTHROPIC_BASE_URL unset, so pruning
# AND Remote Control both work. Opt-in: relies on Claude Code's Bun runtime (can
# break on a CC update) and works around a deliberate restriction. Re-run
# `trimwire on` after changing this. All traffic still goes only to Anthropic.
# remote_control = false

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
    fn shell_unset_commands_follow_the_actual_env_not_config() {
        // Only ANTHROPIC_BASE_URL exported (default mode).
        assert_eq!(
            shell_unset_commands(true, false, false),
            vec!["unset ANTHROPIC_BASE_URL"]
        );
        // Only BUN_OPTIONS exported (coexist) — even if config now says otherwise.
        assert_eq!(
            shell_unset_commands(false, true, false),
            vec!["unset BUN_OPTIONS"]
        );
        // Both present (config switched mid-shell) → unset both, nothing left routing.
        assert_eq!(
            shell_unset_commands(true, true, false),
            vec!["unset ANTHROPIC_BASE_URL", "unset BUN_OPTIONS"]
        );
        // fish syntax.
        assert_eq!(
            shell_unset_commands(false, true, true),
            vec!["set -e BUN_OPTIONS"]
        );
        // Neither → already clean.
        assert!(shell_unset_commands(false, false, false).is_empty());
    }

    #[test]
    fn coexist_state_classifies_all_four_branches() {
        let shim = "/home/u/.trimwire/coexist-shim.js";
        // Happy path: base URL unset, shim present + preloaded.
        assert_eq!(
            coexist_state(None, true, Some(&format!("--preload {shim}")), shim),
            CoexistState::Ok
        );
        // ANTHROPIC_BASE_URL set → Remote Control blocked (wins even if the rest is fine).
        assert_eq!(
            coexist_state(Some("http://127.0.0.1:8765"), true, Some(shim), shim),
            CoexistState::BaseUrlSet("http://127.0.0.1:8765".to_owned())
        );
        // Shim file missing.
        assert_eq!(
            coexist_state(None, false, Some(shim), shim),
            CoexistState::ShimMissing
        );
        // Shim present but BUN_OPTIONS doesn't preload it (this shell not re-sourced).
        assert_eq!(
            coexist_state(None, true, None, shim),
            CoexistState::NotPreloaded
        );
        assert_eq!(
            coexist_state(None, true, Some("--preload /other/thing.js"), shim),
            CoexistState::NotPreloaded
        );
    }

    /// #145: the shared bounded fetch used by `summarizer setup` AND
    /// `summarizer benchmark` must return an error FAST on a black-hole endpoint
    /// (accepts the connection, never responds) rather than hang. Uses a short
    /// timeout so the test is quick; asserts it returns `Err("timed out …")` well
    /// within a bounded window. A literal `127.0.0.1` skips DNS, so this exercises
    /// the connect/response-hang + timeout + `shutdown_timeout` teardown path.
    #[test]
    fn fetch_ollama_tags_blocking_is_bounded_on_a_hung_endpoint() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::time::{Duration, Instant};

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        listener.set_nonblocking(true).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let stop2 = stop.clone();
        let handle = std::thread::spawn(move || {
            // Accept + hold connections open, never responding; poll `stop` so the
            // join at the end returns promptly.
            let mut held: Vec<std::net::TcpStream> = Vec::new();
            while !stop2.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((s, _)) => held.push(s),
                    Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(_) => break,
                }
            }
        });

        let start = Instant::now();
        let res = fetch_ollama_tags_blocking(
            &format!("http://127.0.0.1:{port}"),
            Duration::from_millis(300),
        );
        let elapsed = start.elapsed();
        stop.store(true, Ordering::Relaxed);
        let _ = handle.join();

        assert!(res.is_err(), "black-hole endpoint must error, got {res:?}");
        assert!(
            res.unwrap_err().to_string().contains("timed out"),
            "expected a timeout error"
        );
        // Bounded: the 300ms timeout + runtime teardown must resolve well under 2s.
        assert!(
            elapsed < Duration::from_secs(2),
            "fetch was not bounded ({elapsed:?}) — timeout/shutdown regressed"
        );
    }

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

    /// #153: `resolve_first_addr` bounds the DNS step. A literal loopback address
    /// resolves instantly and returns `Some`; the whole call must complete well
    /// inside the timeout (proves it isn't blocking on a real `getaddrinfo`).
    #[test]
    fn resolve_first_addr_resolves_a_loopback_literal_fast() {
        use std::time::{Duration, Instant};
        let start = Instant::now();
        let addr = resolve_first_addr("127.0.0.1:11434", Duration::from_secs(2));
        assert!(
            start.elapsed() < Duration::from_secs(1),
            "literal is instant"
        );
        let addr = addr.expect("loopback literal resolves");
        assert_eq!(addr.port(), 11434);
        assert!(addr.ip().is_loopback());
    }

    /// #153: a near-zero timeout must return `None` rather than block. Even if the
    /// helper thread hasn't produced an answer yet, the caller gives up promptly —
    /// this is the doctor-doesn't-hang guarantee for a wedged resolver.
    #[test]
    fn resolve_first_addr_gives_up_on_a_tiny_timeout() {
        use std::time::{Duration, Instant};
        let start = Instant::now();
        // Zero timeout: recv_timeout returns immediately with no value.
        let addr = resolve_first_addr("127.0.0.1:11434", Duration::from_millis(0));
        assert!(
            start.elapsed() < Duration::from_millis(500),
            "a tiny timeout must not block"
        );
        assert!(addr.is_none(), "no answer within the timeout → None");
    }

    /// #152: `BoundedRuntime` `Deref`s to the runtime (so `block_on` is unchanged)
    /// and its `Drop` runs the BOUNDED `shutdown_timeout`, not the implicit `Drop`
    /// that blocks forever joining the blocking pool. Fire a `spawn_blocking`
    /// straggler that far outlives the 100ms teardown budget, then drop the guard
    /// on an early `?`-return: teardown must return in well under the straggler's
    /// sleep. A bare `Runtime` would block ~5s joining it — so reverting the guard
    /// makes this test hang past its 2s bound and fail. That's the regression edge.
    #[test]
    fn bounded_runtime_teardown_is_bounded_despite_a_straggler() {
        use std::time::{Duration, Instant};
        fn inner() -> Result<()> {
            let rt = BoundedRuntime::new(
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()?,
            );
            // Value path works via Deref.
            assert_eq!(rt.block_on(async { 21 + 21 }), 42);
            // Fire-and-forget a blocking-pool straggler; do NOT await it. Dropping
            // the handle detaches the task, which keeps running. The implicit
            // runtime Drop would block ~5s joining it — the guard's
            // `shutdown_timeout(100ms)` must leak it and return promptly instead.
            let _straggler = rt.spawn_blocking(|| std::thread::sleep(Duration::from_secs(5)));
            // Early return: `rt` drops here → bounded teardown, straggler or not.
            anyhow::bail!("early out");
        }
        let start = Instant::now();
        assert!(inner().is_err());
        let elapsed = start.elapsed();
        // 100ms budget + generous CI slack; a bare-Runtime regression blocks ~5s.
        assert!(
            elapsed < Duration::from_secs(2),
            "teardown not bounded ({elapsed:?}) — BoundedRuntime::Drop regressed"
        );
    }
}
