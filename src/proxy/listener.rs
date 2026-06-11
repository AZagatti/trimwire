//! Obtain the gateway's listening socket — from a service manager if it passed
//! one (socket activation), otherwise by binding the configured address.
//!
//! Socket activation is what makes trimwire safe to point your whole CLI at:
//! when systemd (`.socket`) or launchd (`Sockets`) owns the listening socket,
//! a client connecting while the worker is down/restarting is **queued, not
//! refused** — so a crashed or not-yet-started daemon never strands Claude
//! Code with a connection error. See `docs/ALTERNATIVES.md` / the README
//! integration section.
//!
//! Resolution order:
//!   1. **systemd** — `LISTEN_FDS`/`LISTEN_PID` (via the `listenfd` crate).
//!   2. **launchd** (macOS) — `launch_activate_socket("Listeners")`.
//!   3. **bind** — fall back to `TcpListener::bind(addr)` (foreground/dev,
//!      `trimwire run`, the WSL2 self-supervisor, and tests).

use std::net::SocketAddr;

use anyhow::{Context, Result};
use tokio::net::TcpListener;

/// How the listening socket was obtained — surfaced in the startup log so the
/// user can confirm fail-open (socket-activated) vs a plain bind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListenerSource {
    Systemd,
    Launchd,
    Bound,
}

impl ListenerSource {
    pub fn label(self) -> &'static str {
        match self {
            ListenerSource::Systemd => "systemd socket activation",
            ListenerSource::Launchd => "launchd socket activation",
            ListenerSource::Bound => "bound",
        }
    }
}

/// Get the gateway listener, preferring an inherited (socket-activated) fd.
pub async fn obtain(addr: SocketAddr) -> Result<(TcpListener, ListenerSource)> {
    if let Some(l) = systemd_listener().context("inherit systemd socket")? {
        return Ok((l, ListenerSource::Systemd));
    }
    #[cfg(target_os = "macos")]
    if let Some(l) = launchd_listener("Listeners").context("inherit launchd socket")? {
        return Ok((l, ListenerSource::Launchd));
    }
    let l = TcpListener::bind(addr)
        .await
        .with_context(|| format!("bind {addr}"))?;
    Ok((l, ListenerSource::Bound))
}

/// systemd passes listening sockets as fds starting at 3, with `LISTEN_FDS`
/// (count) and `LISTEN_PID` (our pid). `listenfd` validates both and hands back
/// the first as a ready-to-use `std` listener.
fn systemd_listener() -> Result<Option<TcpListener>> {
    let mut fds = listenfd::ListenFd::from_env();
    match fds.take_tcp_listener(0)? {
        Some(std_listener) => {
            std_listener.set_nonblocking(true)?;
            Ok(Some(TcpListener::from_std(std_listener)?))
        }
        None => Ok(None),
    }
}

/// launchd hands back the fd(s) registered under the named `Sockets` key in our
/// LaunchAgent plist. `launch_activate_socket` lives in libSystem (always
/// linked on macOS). We use the first fd and free the array launchd allocated.
#[cfg(target_os = "macos")]
#[allow(unsafe_code)] // launchd socket-activation FFI; SAFETY notes inline
fn launchd_listener(name: &str) -> Result<Option<TcpListener>> {
    use std::ffi::CString;
    use std::os::unix::io::FromRawFd;

    unsafe extern "C" {
        fn launch_activate_socket(
            name: *const libc::c_char,
            fds: *mut *mut libc::c_int,
            count: *mut libc::size_t,
        ) -> libc::c_int;
    }

    let cname = CString::new(name)?;
    let mut fds: *mut libc::c_int = std::ptr::null_mut();
    let mut count: libc::size_t = 0;
    // SAFETY: launchd writes a malloc'd fd array + count; we read `count`
    // entries and free the array. A non-zero return means "not launched by
    // launchd / no such socket" → fall through to bind.
    let rc = unsafe { launch_activate_socket(cname.as_ptr(), &mut fds, &mut count) };
    if rc != 0 || fds.is_null() || count == 0 {
        if !fds.is_null() {
            unsafe { libc::free(fds as *mut libc::c_void) };
        }
        return Ok(None);
    }
    let fd = unsafe { *fds };
    unsafe { libc::free(fds as *mut libc::c_void) };
    // SAFETY: `fd` is a listening socket owned by launchd, handed to us.
    let std_listener = unsafe { std::net::TcpListener::from_raw_fd(fd) };
    std_listener.set_nonblocking(true)?;
    Ok(Some(TcpListener::from_std(std_listener)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn falls_back_to_bind_when_no_activation() {
        // No LISTEN_FDS in the test env → must bind the requested port.
        let (listener, src) = obtain("127.0.0.1:0".parse().unwrap()).await.unwrap();
        assert_eq!(src, ListenerSource::Bound);
        assert_eq!(listener.local_addr().unwrap().ip().to_string(), "127.0.0.1");
    }

    #[test]
    fn source_labels_are_distinct() {
        assert_ne!(
            ListenerSource::Systemd.label(),
            ListenerSource::Bound.label()
        );
        assert_ne!(
            ListenerSource::Launchd.label(),
            ListenerSource::Bound.label()
        );
    }
}
