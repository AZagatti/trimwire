//! Service lifecycle: install/start/stop/status of the always-up gateway.
//!
//! The point is that you can point your whole CLI (and IDE extensions) at
//! trimwire via a global `ANTHROPIC_BASE_URL` and *never* get stranded by a
//! dead daemon — because a service manager owns the listening socket (socket
//! activation), so a connection while the worker is down is queued, not
//! refused. See `src/proxy/listener.rs`.
//!
//! Three tiers, auto-detected:
//!   - **systemd** (Linux incl. WSL2 with `systemd=true`): a `.socket` + a
//!     `Restart=always` `.service`. True fail-open.
//!   - **launchd** (macOS): a LaunchAgent with `Sockets` (socket activation —
//!     no `KeepAlive`, which Apple says is wrong for socket listeners). The
//!     next connection re-launches the worker, same fail-open as systemd.
//!   - **supervisor** (no systemd / no launchd): a detached background daemon
//!     with a pidfile. No socket activation — degraded; documented as such.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::Command;

use anyhow::{Context, Result, bail};

const UNIT_SOCKET: &str = "trimwire.socket";
const UNIT_SERVICE: &str = "trimwire.service";
const PLIST_LABEL: &str = "dev.trimwire.gateway";

/// Which init/service manager we'll use on this machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Manager {
    Systemd,
    Launchd,
    Supervisor,
}

impl Manager {
    pub fn label(self) -> &'static str {
        match self {
            Manager::Systemd => "systemd (user service, socket-activated)",
            Manager::Launchd => "launchd (LaunchAgent, socket-activated)",
            Manager::Supervisor => "background daemon (no socket activation — degraded)",
        }
    }
}

/// Pick the best available manager for this OS/environment.
pub fn detect() -> Manager {
    if cfg!(target_os = "macos") {
        return Manager::Launchd;
    }
    // Linux: use systemd --user only if the user manager is actually running
    // (WSL2 without `systemd=true` has systemctl present but no user bus).
    if systemd_user_available() {
        return Manager::Systemd;
    }
    Manager::Supervisor
}

fn systemd_user_available() -> bool {
    if cfg!(target_os = "macos") {
        return false;
    }
    Command::new("systemctl")
        .args(["--user", "is-system-running"])
        .output()
        .map(|o| {
            // "running" or "degraded" both mean the user manager is up.
            let s = String::from_utf8_lossy(&o.stdout);
            let s = s.trim();
            s == "running" || s == "degraded" || s == "starting"
        })
        .unwrap_or(false)
}

// ---- unit/plist content (pure, unit-tested) ----

/// systemd `.socket` unit — systemd owns the listener (this is what gives
/// fail-open).
pub fn systemd_socket_unit(addr: SocketAddr) -> String {
    format!(
        "[Unit]\n\
         Description=trimwire gateway socket\n\
         \n\
         [Socket]\n\
         ListenStream={addr}\n\
         \n\
         [Install]\n\
         WantedBy=sockets.target\n"
    )
}

/// systemd `.service` unit — started on first connection to the socket,
/// restarted on crash; the socket stays held by systemd across restarts.
pub fn systemd_service_unit(exe: &str) -> String {
    // No [Install]: this is socket-activated, pulled in on demand by the
    // `.socket` (we only `enable` the socket). StartLimit caps restart storms
    // so a worker that crash-loops on a bad config enters `failed` (visible in
    // `status`) instead of pegging a core.
    format!(
        "[Unit]\n\
         Description=trimwire — Claude Code context-pruning gateway\n\
         Requires={UNIT_SOCKET}\n\
         After={UNIT_SOCKET}\n\
         StartLimitIntervalSec=60\n\
         StartLimitBurst=5\n\
         \n\
         [Service]\n\
         ExecStart=\"{exe}\" serve\n\
         Restart=always\n\
         RestartSec=1\n"
    )
}

/// macOS launchd LaunchAgent plist with a `Sockets` key (socket activation).
/// **No `KeepAlive`** — Apple's docs say it's inappropriate for socket
/// listeners (it would defeat on-demand launch). Socket activation already
/// gives crash recovery: the next connection re-launches the worker, matching
/// the systemd fail-open behavior. Loads at login.
pub fn launchd_plist(exe: &str, addr: SocketAddr) -> String {
    let port = addr.port();
    let ip = addr.ip();
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{PLIST_LABEL}</string>
    <key>ProgramArguments</key>
    <array>
        <string>{exe}</string>
        <string>serve</string>
    </array>
    <key>Sockets</key>
    <dict>
        <key>Listeners</key>
        <dict>
            <key>SockNodeName</key>
            <string>{ip}</string>
            <key>SockServiceName</key>
            <string>{port}</string>
        </dict>
    </dict>
    <key>RunAtLoad</key>
    <false/>
</dict>
</plist>
"#
    )
}

// ---- install / lifecycle ----

fn systemd_unit_dir() -> Result<PathBuf> {
    let home = std::env::var("HOME").context("HOME not set")?;
    Ok(PathBuf::from(home).join(".config/systemd/user"))
}

fn launchd_plist_path() -> Result<PathBuf> {
    let home = std::env::var("HOME").context("HOME not set")?;
    Ok(PathBuf::from(home).join(format!("Library/LaunchAgents/{PLIST_LABEL}.plist")))
}

fn supervisor_pidfile() -> Result<PathBuf> {
    let home = std::env::var("HOME").context("HOME not set")?;
    Ok(PathBuf::from(home).join(".trimwire/daemon.pid"))
}

fn current_exe() -> Result<String> {
    Ok(std::env::current_exe()
        .context("locate current executable")?
        .to_string_lossy()
        .into_owned())
}

/// When the gateway comes back automatically.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Autostart {
    /// Survives logout / starts pre-login (systemd lingering enabled).
    Linger,
    /// `--boot` was asked for but enabling lingering failed (needs manual step).
    LingerFailed,
    /// Starts at login (default; systemd user instance / launchd LaunchAgent).
    Login,
    /// Only runs when started manually (supervisor fallback — no autostart).
    Manual,
}

/// Outcome of `install` — which manager, and when it auto-starts.
pub struct Installed {
    pub manager: Manager,
    pub autostart: Autostart,
    /// True if the GUI-environment hook was written (so Dock/Spotlight-launched
    /// editors also see `ANTHROPIC_BASE_URL`).
    pub gui_env: bool,
}

/// Write the service definition, enable/start it, set it to come back
/// automatically, and (best-effort) make the env var reach GUI-launched apps.
///
/// `boot = true` additionally enables systemd lingering (start pre-login /
/// survive logout); the default is login-scoped, matching ollama / Caddy-on-mac
/// / Docker Desktop.
pub fn install(addr: SocketAddr, boot: bool) -> Result<Installed> {
    let mgr = detect();
    let exe = current_exe()?;
    let mut autostart = Autostart::Manual;
    let mut gui_env = false;
    match mgr {
        Manager::Systemd => {
            let dir = systemd_unit_dir()?;
            std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
            std::fs::write(dir.join(UNIT_SOCKET), systemd_socket_unit(addr))?;
            std::fs::write(dir.join(UNIT_SERVICE), systemd_service_unit(&exe))?;
            run("systemctl", &["--user", "daemon-reload"])?;
            // Enable only the socket — it pulls in the service on demand.
            run("systemctl", &["--user", "enable", "--now", UNIT_SOCKET])?;
            autostart = if boot {
                // Lingering = start pre-login + survive logout. Opt-in because
                // it runs a service 24/7 even when you're logged out.
                if enable_linger() {
                    Autostart::Linger
                } else {
                    Autostart::LingerFailed
                }
            } else {
                Autostart::Login
            };
            gui_env = write_env_d(addr).unwrap_or(false);
        }
        Manager::Launchd => {
            let path = launchd_plist_path()?;
            if let Some(p) = path.parent() {
                std::fs::create_dir_all(p)?;
            }
            std::fs::write(&path, launchd_plist(&exe, addr))?;
            let uid = libc_getuid().to_string();
            // bootout first so re-install replaces cleanly (ignore failure).
            let _ = run(
                "launchctl",
                &["bootout", &format!("gui/{uid}/{PLIST_LABEL}")],
            );
            run(
                "launchctl",
                &["bootstrap", &format!("gui/{uid}"), &path.to_string_lossy()],
            )?;
            // A LaunchAgent loads at every login. (macOS agents can't start
            // pre-login; `--boot` is a no-op here.)
            autostart = Autostart::Login;
            gui_env = install_macos_env_agent(addr).unwrap_or(false);
        }
        Manager::Supervisor => {
            // No socket activation available; on() spawns the detached daemon.
            on()?;
        }
    }
    Ok(Installed {
        manager: mgr,
        autostart,
        gui_env,
    })
}

/// Remove everything `install` created: stop + disable the service, delete unit
/// files / plists / env hooks, and turn off lingering. Idempotent and
/// best-effort (a missing piece is fine).
pub fn uninstall() -> Result<()> {
    // systemd
    if !cfg!(target_os = "macos") {
        let _ = run("systemctl", &["--user", "disable", "--now", UNIT_SOCKET]);
        let _ = run("systemctl", &["--user", "stop", UNIT_SERVICE]);
        if let Ok(dir) = systemd_unit_dir() {
            let _ = std::fs::remove_file(dir.join(UNIT_SOCKET));
            let _ = std::fs::remove_file(dir.join(UNIT_SERVICE));
        }
        let _ = run("systemctl", &["--user", "daemon-reload"]);
        // Turn off lingering only if we likely enabled it (harmless if not).
        if let Ok(user) = std::env::var("USER") {
            let _ = Command::new("loginctl")
                .args(["disable-linger", &user])
                .status();
        }
        if let Ok(p) = env_d_path() {
            let _ = std::fs::remove_file(p);
        }
    }
    // launchd
    #[cfg(target_os = "macos")]
    {
        let uid = libc_getuid().to_string();
        let _ = run(
            "launchctl",
            &["bootout", &format!("gui/{uid}/{PLIST_LABEL}")],
        );
        let _ = run(
            "launchctl",
            &["bootout", &format!("gui/{uid}/{PLIST_LABEL}.env")],
        );
        if let Ok(p) = launchd_plist_path() {
            let _ = std::fs::remove_file(p);
        }
        if let Ok(p) = macos_env_plist_path() {
            let _ = std::fs::remove_file(p);
        }
    }
    // supervisor
    let _ = supervisor_stop();
    Ok(())
}

// ---- GUI environment propagation ----
//
// Shell-rc exports don't reach editors launched from Dock/Spotlight/Start menu
// (they don't source .zshrc/.bashrc), so their Claude Code extensions wouldn't
// see ANTHROPIC_BASE_URL. These hooks set it for the graphical session.

fn env_d_path() -> Result<PathBuf> {
    let home = std::env::var("HOME").context("HOME not set")?;
    Ok(PathBuf::from(home).join(".config/environment.d/trimwire.conf"))
}

/// Linux: systemd user `environment.d` — inherited by the graphical session
/// (coverage depends on the display manager; documented as best-effort).
fn write_env_d(addr: SocketAddr) -> Result<bool> {
    let p = env_d_path()?;
    if let Some(dir) = p.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(&p, format!("ANTHROPIC_BASE_URL=http://{addr}\n"))?;
    Ok(true)
}

#[cfg(target_os = "macos")]
fn macos_env_plist_path() -> Result<PathBuf> {
    let home = std::env::var("HOME").context("HOME not set")?;
    Ok(PathBuf::from(home).join(format!("Library/LaunchAgents/{PLIST_LABEL}.env.plist")))
}

/// macOS: a tiny RunAtLoad LaunchAgent that runs `launchctl setenv` at login so
/// the var reaches every GUI app (plain `launchctl setenv` is reboot-volatile).
#[cfg(target_os = "macos")]
fn install_macos_env_agent(addr: SocketAddr) -> Result<bool> {
    let plist = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{PLIST_LABEL}.env</string>
    <key>ProgramArguments</key>
    <array>
        <string>launchctl</string>
        <string>setenv</string>
        <string>ANTHROPIC_BASE_URL</string>
        <string>http://{addr}</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
</dict>
</plist>
"#
    );
    let path = macos_env_plist_path()?;
    if let Some(p) = path.parent() {
        std::fs::create_dir_all(p)?;
    }
    std::fs::write(&path, plist)?;
    let uid = libc_getuid().to_string();
    let _ = run(
        "launchctl",
        &["bootout", &format!("gui/{uid}/{PLIST_LABEL}.env")],
    );
    run(
        "launchctl",
        &["bootstrap", &format!("gui/{uid}"), &path.to_string_lossy()],
    )?;
    Ok(true)
}

#[cfg(not(target_os = "macos"))]
fn install_macos_env_agent(_addr: SocketAddr) -> Result<bool> {
    Ok(false)
}

/// Enable systemd lingering for the current user so the user instance (and our
/// socket) starts at boot and survives logout. Non-bailing: returns whether it
/// worked (it usually does without sudo for one's own user).
fn enable_linger() -> bool {
    let user = std::env::var("USER").unwrap_or_default();
    let mut cmd = Command::new("loginctl");
    cmd.arg("enable-linger");
    if !user.is_empty() {
        cmd.arg(&user);
    }
    cmd.status().map(|s| s.success()).unwrap_or(false)
}

/// Start the gateway.
pub fn on() -> Result<()> {
    match detect() {
        Manager::Systemd => run("systemctl", &["--user", "start", UNIT_SOCKET]),
        Manager::Launchd => {
            // `off` does `bootout` (fully unloads the agent), so `on` must
            // `bootstrap` it back — `enable` alone only clears a disabled flag
            // and won't reload a booted-out agent (it'd silently stay dead).
            let path = launchd_plist_path()?;
            if !path.is_file() {
                bail!("not installed yet — run `trimwire install` first");
            }
            let uid = libc_getuid().to_string();
            let _ = run(
                "launchctl",
                &["enable", &format!("gui/{uid}/{PLIST_LABEL}")],
            );
            // bootstrap fails harmlessly if it's already loaded; that's fine.
            let _ = run(
                "launchctl",
                &["bootstrap", &format!("gui/{uid}"), &path.to_string_lossy()],
            );
            // Symmetric with `off`: re-load the env LaunchAgent (RunAtLoad → re-runs
            // `launchctl setenv ANTHROPIC_BASE_URL`) so GUI apps get routed again
            // after an `off` had `unsetenv`'d it. Best-effort; macOS-only — validate
            // on macOS. Path mirrors `macos_env_plist_path()` (constructed inline so
            // this arm still compiles on non-macOS, where it never executes).
            if let Ok(home) = std::env::var("HOME") {
                let env_plist = format!("{home}/Library/LaunchAgents/{PLIST_LABEL}.env.plist");
                if std::path::Path::new(&env_plist).is_file() {
                    let _ = run(
                        "launchctl",
                        &["enable", &format!("gui/{uid}/{PLIST_LABEL}.env")],
                    );
                    let _ = run(
                        "launchctl",
                        &["bootstrap", &format!("gui/{uid}"), &env_plist],
                    );
                }
            }
            Ok(())
        }
        Manager::Supervisor => supervisor_start(),
    }
}

/// Stop the gateway. With socket activation this also stops accepting — `off`
/// is the explicit kill switch (pair with unsetting `ANTHROPIC_BASE_URL`).
pub fn off() -> Result<()> {
    match detect() {
        Manager::Systemd => run("systemctl", &["--user", "stop", UNIT_SOCKET, UNIT_SERVICE]),
        Manager::Launchd => {
            let uid = libc_getuid().to_string();
            // Stop the daemon (the kill switch). ALSO stop the env LaunchAgent and
            // clear the session var, so GUI-launched apps (editors) go straight to
            // Anthropic after `off`: the `.env` RunAtLoad agent ran `launchctl setenv`
            // at login, which booting out the daemon alone does NOT undo. Best-effort
            // (never fails the stop). macOS-only paths — validate on macOS (dev/CI is
            // Linux/WSL); `on` re-loads the env agent symmetrically.
            let stopped = run(
                "launchctl",
                &["bootout", &format!("gui/{uid}/{PLIST_LABEL}")],
            );
            let _ = run(
                "launchctl",
                &["bootout", &format!("gui/{uid}/{PLIST_LABEL}.env")],
            );
            let _ = run("launchctl", &["unsetenv", "ANTHROPIC_BASE_URL"]);
            stopped
        }
        Manager::Supervisor => supervisor_stop(),
    }
}

/// Print on/off + a liveness probe.
pub fn status(addr: SocketAddr) -> Result<()> {
    let mgr = detect();
    println!("manager: {}", mgr.label());
    let listening = tcp_open(addr);
    let serving = listening && healthz_ok(addr);
    println!("listening on {addr}: {}", yesno(listening));
    println!("serving (/healthz): {}", yesno(serving));
    if !listening {
        // First-time users: `trimwire on` fails (with a soft exit) until `install`
        // has set up the service — point them there first, and at `doctor` to tell
        // which state they're in.
        println!("→ not running.");
        println!("    First time?       run `trimwire install`.");
        println!("    Already installed? run `trimwire on`  (or `trimwire doctor` to diagnose).");
    } else if !serving {
        println!(
            "→ something holds the port but isn't answering /healthz — another process is on it. \
             Free it, or set a different `[server] listen` in config."
        );
    }
    Ok(())
}

fn yesno(b: bool) -> &'static str {
    if b { "yes" } else { "no" }
}

// ---- supervisor (fallback) ----

fn supervisor_start() -> Result<()> {
    let pidfile = supervisor_pidfile()?;
    if let Some(p) = pidfile.parent() {
        std::fs::create_dir_all(p)?;
    }
    if let Ok(pid) = std::fs::read_to_string(&pidfile) {
        if let Ok(pid) = pid.trim().parse::<i32>() {
            if proc_alive(pid) {
                println!("already running (pid {pid})");
                return Ok(());
            }
        }
    }
    let exe = current_exe()?;
    let child = Command::new(exe)
        .arg("serve")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .context("spawn detached daemon")?;
    std::fs::write(&pidfile, child.id().to_string())?;
    println!("started background daemon (pid {})", child.id());
    Ok(())
}

fn supervisor_stop() -> Result<()> {
    let pidfile = supervisor_pidfile()?;
    let Ok(pid) = std::fs::read_to_string(&pidfile) else {
        println!("not running (no pidfile)");
        return Ok(());
    };
    if let Ok(pid) = pid.trim().parse::<i32>() {
        // Only signal a LIVE process — a crashed daemon's PID may have been
        // recycled by the kernel to an unrelated process; don't kill that.
        if proc_alive(pid) {
            let _ = run("kill", &[&pid.to_string()]);
        } else {
            println!("not running (stale pidfile)");
        }
    }
    let _ = std::fs::remove_file(&pidfile);
    Ok(())
}

#[cfg(unix)]
fn proc_alive(pid: i32) -> bool {
    // signal 0 = existence check.
    Command::new("kill")
        .args(["-0", &pid.to_string()])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}
#[cfg(not(unix))]
fn proc_alive(_pid: i32) -> bool {
    false
}

// ---- helpers ----

fn run(cmd: &str, args: &[&str]) -> Result<()> {
    // Capture stderr (don't inherit) so an init-system's raw error
    // (e.g. systemd's "Failed to enable unit: ... does not exist") never leaks
    // to the user's terminal — we fold a trimmed copy into our own message.
    let out = Command::new(cmd)
        .args(args)
        .output()
        .with_context(|| format!("run {cmd} {}", args.join(" ")))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        let err = err.trim();
        if err.is_empty() {
            bail!("`{cmd} {}` failed ({})", args.join(" "), out.status);
        }
        bail!("`{cmd} {}` failed ({}): {err}", args.join(" "), out.status);
    }
    Ok(())
}

fn tcp_open(addr: SocketAddr) -> bool {
    std::net::TcpStream::connect_timeout(&addr, std::time::Duration::from_millis(300)).is_ok()
}

/// Minimal blocking `GET /healthz` — confirms the gateway is actually serving.
pub(crate) fn healthz_ok(addr: SocketAddr) -> bool {
    use std::io::{Read, Write};
    let Ok(mut s) =
        std::net::TcpStream::connect_timeout(&addr, std::time::Duration::from_millis(500))
    else {
        return false;
    };
    let _ = s.set_read_timeout(Some(std::time::Duration::from_millis(500)));
    let req = format!("GET /healthz HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n");
    if s.write_all(req.as_bytes()).is_err() {
        return false;
    }
    let mut buf = String::new();
    let _ = s.read_to_string(&mut buf);
    buf.starts_with("HTTP/1.1 200")
}

/// Blocking `GET /healthz` that returns the served `version` field, or `None` if
/// the gateway isn't answering or the body has no version. Used by the updater
/// (4c) to confirm the freshly-restarted service is actually running the new
/// build before declaring success. Parses the JSON body's `"version"` value with
/// a tiny dependency-free scan (the body is the gateway's own small response).
pub(crate) fn healthz_version(addr: SocketAddr) -> Option<String> {
    use std::io::{Read, Write};
    let mut s =
        std::net::TcpStream::connect_timeout(&addr, std::time::Duration::from_millis(500)).ok()?;
    let _ = s.set_read_timeout(Some(std::time::Duration::from_millis(500)));
    let req = format!("GET /healthz HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n");
    s.write_all(req.as_bytes()).ok()?;
    let mut buf = String::new();
    let _ = s.read_to_string(&mut buf);
    if !buf.starts_with("HTTP/1.1 200") {
        return None;
    }
    // Body is `{"ok":true,"version":"X.Y.Z"}` — find the version value.
    let body = buf.split("\r\n\r\n").nth(1).unwrap_or("");
    let key = "\"version\":\"";
    let start = body.find(key)? + key.len();
    let rest = &body[start..];
    let end = rest.find('"')?;
    Some(rest[..end].to_owned())
}

// `getuid()` has no caller preconditions and can't fail or cause UB, so the
// wrapper is a *safe* fn; the `unsafe` (and its opt-in) is confined to the FFI.
#[cfg(target_os = "macos")]
#[allow(unsafe_code)] // libc::getuid() FFI — always safe to call
fn libc_getuid() -> u32 {
    unsafe { libc::getuid() }
}
#[cfg(not(target_os = "macos"))]
fn libc_getuid() -> u32 {
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr() -> SocketAddr {
        "127.0.0.1:8765".parse().unwrap()
    }

    #[test]
    fn systemd_socket_unit_listens_on_addr() {
        let u = systemd_socket_unit(addr());
        assert!(u.contains("ListenStream=127.0.0.1:8765"));
        assert!(u.contains("WantedBy=sockets.target"));
    }

    #[test]
    fn systemd_service_runs_serve_with_restart() {
        let u = systemd_service_unit("/usr/local/bin/trimwire");
        // exe path is double-quoted so a path with spaces doesn't break parsing
        assert!(u.contains("ExecStart=\"/usr/local/bin/trimwire\" serve"));
        assert!(u.contains("Restart=always"));
        assert!(u.contains(&format!("Requires={UNIT_SOCKET}")));
    }

    #[test]
    fn systemd_execstart_quotes_path_with_spaces() {
        let u = systemd_service_unit("/home/John Doe/.cargo/bin/trimwire");
        assert!(u.contains("ExecStart=\"/home/John Doe/.cargo/bin/trimwire\" serve"));
    }

    #[test]
    fn launchd_plist_is_socket_activated_without_keepalive() {
        let p = launchd_plist("/usr/local/bin/trimwire", addr());
        assert!(p.contains("<key>Sockets</key>"));
        assert!(p.contains("<string>8765</string>"));
        assert!(p.contains("<string>127.0.0.1</string>"));
        // KeepAlive must NOT be present — Apple says it's wrong for socket
        // listeners and would defeat on-demand activation.
        assert!(!p.contains("<key>KeepAlive</key>"));
        assert!(p.contains(PLIST_LABEL));
    }

    #[test]
    fn systemd_service_has_no_install_section_and_rate_limit() {
        // Socket-activated: must not carry [Install] (we only enable the socket).
        let u = systemd_service_unit("/usr/local/bin/trimwire");
        assert!(!u.contains("[Install]"));
        assert!(u.contains("StartLimitBurst="));
    }
}
