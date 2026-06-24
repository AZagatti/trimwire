//! `trimwire statusline` — render the live savings/health bar for Claude Code's
//! statusline. Claude Code invokes this after each message, passing a JSON blob
//! on stdin that includes `session_id`. We look that session up in the ledger
//! and print one line. This is the safe equivalent of opencode-dcp's in-chat
//! savings bar (a gateway can't draw in the chat without polluting the
//! transcript — see the README).
//!
//! Also the ambient health light: if `ANTHROPIC_BASE_URL` points at us but the
//! gateway isn't answering, we show a warning instead of a (misleading) savings
//! number — so a silent "set but not pruning" failure can't go unnoticed.

use std::io::Read;
use std::net::SocketAddr;
use std::path::PathBuf;

use anyhow::Result;

use trimwire::config::Config;
use trimwire::ledger::{Ledger, SessionSavings};

// Subtle ANSI; Claude Code renders color in the statusline.
const DIM: &str = "\x1b[2m";
const GREEN: &str = "\x1b[32m";
const YELLOW: &str = "\x1b[33m";
const RESET: &str = "\x1b[0m";

/// Read Claude Code's statusline JSON from stdin and print the bar.
///
/// If `wrap_file` is set, it holds the user's *original* statusline command:
/// we run it first (feeding it the same stdin), print its output, then add
/// trimwire's row underneath — so trimwire becomes an extra section of an
/// existing statusline (e.g. claude-statusline) instead of replacing it.
pub fn statusline(wrap_file: Option<PathBuf>) -> Result<()> {
    let mut input = String::new();
    let _ = std::io::stdin().read_to_string(&mut input);

    // Wrapped statusline first (its rows render above ours).
    if let Some(wf) = wrap_file {
        if let Ok(cmd) = std::fs::read_to_string(&wf) {
            let cmd = cmd.trim();
            if !cmd.is_empty() {
                if let Some(out) = run_wrapped(cmd, input.as_bytes()) {
                    print!("{out}");
                    if !out.ends_with('\n') {
                        println!();
                    }
                }
            }
        }
    }

    let session_id = serde_json::from_str::<serde_json::Value>(&input)
        .ok()
        .and_then(|v| {
            v.get("session_id")
                .and_then(|s| s.as_str())
                .map(str::to_owned)
        });

    let cfg = Config::load().unwrap_or_default();

    // Health first: if we're the configured upstream but not serving, warn
    // loudly rather than show a stale/zero savings figure.
    if set_but_down(&cfg) {
        println!("{YELLOW}⚠ trimwire not responding — run `trimwire status`{RESET}");
        return Ok(());
    }

    if !cfg.ledger.enabled {
        return Ok(()); // ledger off → nothing to show; stay out of the way.
    }

    let savings = session_id
        .as_deref()
        .map(|id| Ledger::session_savings(&cfg.ledger.db_path, id).unwrap_or_default())
        .unwrap_or_default();

    println!("{}", render(&savings));
    Ok(())
}

/// Run the user's original statusline command via the shell, feeding it the
/// same stdin JSON Claude Code gave us, and capture its stdout. `None` on
/// failure (then we just show trimwire's own row).
fn run_wrapped(cmd: &str, stdin: &[u8]) -> Option<String> {
    use std::io::Write;
    use std::process::{Command, Stdio};
    let (sh, flag) = if cfg!(windows) {
        ("cmd", "/c")
    } else {
        ("sh", "-c")
    };
    let mut child = Command::new(sh)
        .arg(flag)
        .arg(cmd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    if let Some(mut si) = child.stdin.take() {
        let _ = si.write_all(stdin); // dropped here → stdin closed
    }
    let out = child.wait_with_output().ok()?;
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Build the one-line bar from a session's savings.
fn render(s: &SessionSavings) -> String {
    if s.requests == 0 {
        // "ready" = the statusline is wired and live, with no savings to show
        // YET this session (no requests recorded). It does NOT assert the gateway
        // is up: the genuinely dangerous "configured-but-down" case is caught
        // earlier in `statusline()` by `set_but_down()`, which prints a loud
        // YELLOW "not responding" warning and returns before reaching here. When
        // trimwire is NOT the routed upstream, a down gateway isn't our concern,
        // so "ready" (idle) is the correct, non-alarming state.
        return format!("{DIM}⊡ trimwire · ready{RESET}");
    }
    let saved = s.saved_bytes();
    if saved <= 0 {
        return format!("{DIM}⊡ trimwire · {} reqs{RESET}", s.requests);
    }
    // Frame the win as context-window *headroom* (request made lighter), not
    // "saved" — bytes removed reliably buy context headroom, but net $ is
    // non-monotonic under prompt caching (see benchmark/). "lighter/trimmed"
    // is the honest, always-true framing.
    format!(
        "{GREEN}⊡ trimwire{RESET} {:.0}% lighter · {} trimmed · {} reqs",
        s.reduction_pct(),
        human_bytes(saved as u64),
        s.requests,
    )
}

/// True when `ANTHROPIC_BASE_URL` points at our gateway but it isn't serving —
/// the dangerous "set but not pruning, silently paying full price" state.
/// Shared with the `hook` command. `false` if trimwire isn't the configured
/// upstream (then a down gateway isn't our concern).
pub(crate) fn set_but_down(cfg: &Config) -> bool {
    if !env_points_at_us(cfg) {
        return false;
    }
    match cfg.server.listen.parse::<SocketAddr>() {
        Ok(addr) => !healthz_ok(addr),
        Err(_) => false,
    }
}

/// Does `ANTHROPIC_BASE_URL` (inherited from Claude Code's env) point at our
/// gateway? Matches on the configured host:port — crucially on the **port**, so
/// a *different* local proxy on another port doesn't make us cry "down".
fn env_points_at_us(cfg: &Config) -> bool {
    match std::env::var("ANTHROPIC_BASE_URL") {
        Ok(v) => url_points_at(&v, &cfg.server.listen),
        Err(_) => false,
    }
}

/// Does base-URL `v` target our `listen` (host:port)? Exact host:port match, or
/// same port on a localhost host. Pure — unit-tested.
fn url_points_at(v: &str, listen: &str) -> bool {
    if v.contains(listen) {
        return true;
    }
    let port = listen.rsplit(':').next().unwrap_or("");
    !port.is_empty()
        && v.contains(&format!(":{port}"))
        && (v.contains("127.0.0.1") || v.contains("localhost") || v.contains("[::1]"))
}

/// `/healthz` probe, cached briefly so a down gateway doesn't add latency to
/// every statusline repaint (the statusline is a fresh process each render, so
/// the cache lives in a tmp file with a short TTL).
fn healthz_ok(addr: SocketAddr) -> bool {
    const TTL: std::time::Duration = std::time::Duration::from_secs(2);
    // Scope the cache file to the user + gateway port: `temp_dir()` is `/tmp`
    // (world-writable) on Linux, so a bare name would let one user's health
    // result mask another's on a shared machine. Port disambiguates multiple
    // gateways for the same user.
    let user = std::env::var("USER")
        .or_else(|_| std::env::var("LOGNAME"))
        .unwrap_or_default();
    let cache = std::env::temp_dir().join(format!("trimwire-health-{user}-{}", addr.port()));
    if let Ok(meta) = std::fs::metadata(&cache) {
        if let Ok(age) = meta.modified().map(|m| m.elapsed().unwrap_or(TTL)) {
            if age < TTL {
                return std::fs::read(&cache)
                    .map(|b| b.first() == Some(&b'1'))
                    .unwrap_or(false);
            }
        }
    }
    let ok = probe_healthz(addr);
    let _ = std::fs::write(&cache, if ok { b"1" } else { b"0" });
    ok
}

fn probe_healthz(addr: SocketAddr) -> bool {
    use std::io::Write;
    // Localhost connect-refused returns instantly; the short timeouts only bite
    // if a wedged worker holds the socket but never answers.
    let Ok(mut s) =
        std::net::TcpStream::connect_timeout(&addr, std::time::Duration::from_millis(150))
    else {
        return false;
    };
    let _ = s.set_read_timeout(Some(std::time::Duration::from_millis(150)));
    let req = format!("GET /healthz HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n");
    if s.write_all(req.as_bytes()).is_err() {
        return false;
    }
    let mut buf = [0u8; 16];
    match s.read(&mut buf) {
        Ok(n) => buf[..n].starts_with(b"HTTP/1.1 200"),
        Err(_) => false,
    }
}

fn human_bytes(n: u64) -> String {
    const U: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1024.0 && i < U.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{n} B")
    } else {
        format!("{v:.0} {}", U[i])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ready_when_no_requests() {
        let out = render(&SessionSavings::default());
        assert!(out.contains("ready"));
    }

    #[test]
    fn shows_savings_and_reduction() {
        let s = SessionSavings {
            requests: 10,
            in_bytes: 200_000,
            out_bytes: 120_000,
        };
        let out = render(&s);
        assert!(out.contains("trimwire"));
        assert!(out.contains("40% lighter"));
        assert!(out.contains("78 KB trimmed")); // 80000 bytes removed
        assert!(out.contains("10 reqs"));
    }

    #[test]
    fn human_bytes_scales() {
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(2048), "2 KB");
    }

    #[test]
    fn url_matching_is_port_specific() {
        let listen = "127.0.0.1:8765";
        assert!(url_points_at("http://127.0.0.1:8765", listen), "exact");
        assert!(
            url_points_at("http://localhost:8765", listen),
            "same port, localhost"
        );
        // A *different* local proxy must NOT match (the over-match bug).
        assert!(
            !url_points_at("http://127.0.0.1:9999", listen),
            "other port"
        );
        assert!(
            !url_points_at("https://api.anthropic.com", listen),
            "remote"
        );
    }
}
