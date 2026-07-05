//! Cross-platform binary smoke — runs the built `trimwire` binary as a
//! subprocess on EVERY platform.
//!
//! `env!("CARGO_BIN_EXE_trimwire")` is a compile-time guarantee that cargo builds
//! the binary before these tests run, so nextest produces it on macOS + Windows
//! too (`tests/cli.rs`, the richer shell-shim suite, is `#![cfg(unix)]`). That
//! lets CI's cross-platform job run its daemon smoke HERE — as part of the test
//! build it already pays for — instead of a second full `cargo build` that can't
//! reuse nextest's artifacts (feature unification differs; on Windows that was
//! ~104s of pure duplication). See #147.

use std::net::TcpStream;
use std::process::Command;
use std::time::Duration;

/// Path to the binary cargo built for this test run.
fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_trimwire")
}

/// An OS-assigned free port (bound, read, released) — avoids fixed-port
/// collisions with other tests running in parallel.
fn free_port() -> u16 {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    l.local_addr().unwrap().port()
}

/// The binary starts and answers `--version` (the cheapest proof it links + runs
/// on this platform).
#[test]
fn binary_answers_version() {
    let out = Command::new(bin())
        .arg("--version")
        .output()
        .expect("spawn `trimwire --version`");
    assert!(
        out.status.success(),
        "`trimwire --version` should exit 0, got {:?}",
        out.status
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("trimwire"),
        "`--version` output should name the binary, got: {stdout:?}"
    );
}

/// Daemon smoke: `trimwire daemon --listen` starts the gateway and binds the
/// port. Cross-platform replacement for the per-OS shell smoke that ci.yml used
/// to run against a separately-built binary (#147). Ledger disabled so the smoke
/// touches no on-disk state.
///
/// Dev-env caveat: on a macOS box where trimwire is INSTALLED, an active launchd
/// socket makes the daemon inherit that socket and ignore `--listen` (see
/// `listener::obtain`), so this would time out locally. CI runners have no such
/// install, so the daemon binds the requested port there.
#[test]
fn daemon_binds_the_listen_port() {
    let port = free_port();
    let addr = format!("127.0.0.1:{port}");
    let sock: std::net::SocketAddr = addr.parse().expect("loopback addr is valid");
    let mut child = Command::new(bin())
        .args(["daemon", "--listen", &addr])
        .env("TRIMWIRE_LEDGER__ENABLED", "false")
        .spawn()
        .expect("spawn `trimwire daemon`");

    // Poll for up to ~10s (CI runners can be slow to schedule the child).
    let mut bound = false;
    for _ in 0..50 {
        if TcpStream::connect_timeout(&sock, Duration::from_millis(500)).is_ok() {
            bound = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(200));
    }

    // Always reap the child, even on failure, so the test process exits cleanly.
    let _ = child.kill();
    let _ = child.wait();

    assert!(bound, "daemon did not bind {addr} within the timeout");
}
