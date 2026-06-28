//! POC: the local control API ("cockpit") — a **separate loopback admin
//! listener**, kept off the gateway port that transits the Anthropic OAuth token.
//!
//! This is a deliberately small vertical slice of the design in
//! [`docs/cockpit/03-control-api.md`](../../docs/cockpit/03-control-api.md): it
//! demonstrates every layer (loopback bind + rejection of non-loopback, bearer-
//! token auth behind an [`Authenticator`] seam, a `Host`/`Origin` DNS-rebind
//! guard, content-free read endpoints reusing the ledger `Report`, a one-shot SSE
//! event stream, and an embedded same-origin web UI) without the full router /
//! hot-reload / write surface. It is **off by default** (`[admin] enabled =
//! false`); when disabled the daemon behaves exactly as before.
//!
//! Hard invariants honored (see `docs/cockpit/07-security-tos-redlines.md`):
//! - **loopback-only** in this POC — a non-loopback bind is refused at startup;
//! - the OAuth token / upstream credential is **never read or returned** here —
//!   the only data sources are the content-free ledger `Report` and config
//!   metadata (version, listen addresses, profile);
//! - no message content, prompts, tool results, or file paths are ever exposed.

use std::convert::Infallible;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use http_body_util::Full;
use hyper::HeaderMap;
use hyper::body::{Bytes, Incoming};
use hyper::header::{AUTHORIZATION, CONTENT_TYPE, HOST, ORIGIN};
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::{TokioExecutor, TokioIo};
use hyper_util::server::conn::auto::Builder as ServerBuilder;

use crate::config::Config;
use crate::ledger::Ledger;

/// The running binary version, surfaced on `/api/v1/health` and `/version`.
const VERSION: &str = env!("CARGO_PKG_VERSION");

/// The embedded single-file web cockpit. `__TRIMWIRE_TOKEN__` is substituted with
/// the per-install control token at serve time (the same-origin "token bootstrap"
/// from the design) so the page can authenticate its `fetch` calls.
const COCKPIT_HTML: &str = include_str!("cockpit.html");

// ---- auth seam -------------------------------------------------------------

/// Authentication seam (doc 06, R1): handlers receive a yes/no verdict from an
/// `Authenticator`, never raw trust. v1 has a single implementation,
/// [`LoopbackToken`]; the deferred remote phase swaps in a per-device-token /
/// mTLS implementation **without touching the handlers**.
pub trait Authenticator: Send + Sync {
    /// Returns `true` iff the presented bearer token is valid.
    fn authenticate(&self, presented: Option<&str>) -> bool;
}

/// The only v1 authenticator: a constant 256-bit bearer token, loopback-scoped.
struct LoopbackToken {
    token: String,
}

impl Authenticator for LoopbackToken {
    fn authenticate(&self, presented: Option<&str>) -> bool {
        match presented {
            Some(t) => constant_time_eq(t.as_bytes(), self.token.as_bytes()),
            None => false,
        }
    }
}

/// Length-checked constant-time byte comparison — avoids leaking the token via
/// early-exit timing.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

// ---- state -----------------------------------------------------------------

/// Shared, read-only handles for the admin handlers. Cheap to clone (everything
/// behind it is `Arc`/`String`).
struct AdminState {
    auth: Arc<dyn Authenticator>,
    /// The plaintext token, injected into the served HTML bootstrap.
    token: String,
    /// Loopback port the admin listener bound — drives the `Host`/`Origin` guard.
    port: u16,
    db_path: String,
    gateway_listen: String,
    admin_listen: String,
    profile: String,
    upstream: String,
}

// ---- server ----------------------------------------------------------------

/// Bind the loopback admin listener and serve the control API + cockpit UI until
/// fatal I/O error. **Refuses** any non-loopback bind (remote exposure is a
/// deferred, opt-in phase — see `docs/cockpit/06-remote-control.md`).
pub async fn run(addr: SocketAddr, config: Arc<Config>, gateway_listen: String) -> Result<()> {
    if !addr.ip().is_loopback() {
        bail!(
            "[cockpit] refusing to bind {addr}: the control API is loopback-only in this POC \
             (remote control is a deferred, opt-in phase — see docs/cockpit/06-remote-control.md)"
        );
    }

    let token = load_or_create_token(&config.ledger.db_path)?;
    let state = Arc::new(AdminState {
        auth: Arc::new(LoopbackToken {
            token: token.clone(),
        }),
        token,
        port: addr.port(),
        db_path: config.ledger.db_path.clone(),
        gateway_listen,
        admin_listen: addr.to_string(),
        profile: config
            .profile
            .clone()
            .unwrap_or_else(|| "default".to_owned()),
        upstream: config.server.upstream.clone(),
    });

    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("bind admin listener {addr}"))?;
    let actual = listener.local_addr().unwrap_or(addr);
    eprintln!(
        "[cockpit] control API + web UI on http://{actual}  (token: {})",
        token_path(&state.db_path).display()
    );

    let server = ServerBuilder::new(TokioExecutor::new());
    loop {
        let (stream, _peer) = match listener.accept().await {
            Ok(s) => s,
            Err(e) => {
                eprintln!("[cockpit] accept error: {e}");
                continue;
            }
        };
        let io = TokioIo::new(stream);
        let server = server.clone();
        let state = state.clone();
        tokio::spawn(async move {
            let svc = service_fn(move |req| {
                let state = state.clone();
                async move { Ok::<_, Infallible>(handle(req, state).await) }
            });
            if let Err(e) = server.serve_connection(io, svc).await {
                tracing::debug!(error = %e, "cockpit connection error");
            }
        });
    }
}

/// Route one request. Every response is a fully-buffered `Full<Bytes>` (the admin
/// surface is small JSON / one static page / a one-shot SSE snapshot — it never
/// touches or buffers the proxy stream).
async fn handle(req: Request<Incoming>, state: Arc<AdminState>) -> Response<Full<Bytes>> {
    let headers = req.headers();

    // DNS-rebinding / drive-by-browser guard (doc 03 §5, doc 06 R7): the literal
    // loopback `Host` must match our authority, and any `Origin` must be same-origin.
    if !host_allowed(headers, state.port) {
        return text(StatusCode::FORBIDDEN, "forbidden: bad Host");
    }
    if !origin_allowed(headers, state.port) {
        return text(StatusCode::FORBIDDEN, "forbidden: bad Origin");
    }

    let method = req.method().clone();
    let path = req.uri().path().to_owned();

    // Same-origin web UI (token injected at load). Host/Origin-guarded above.
    if method == Method::GET && (path == "/" || path == "/index.html") {
        return serve_cockpit(&state);
    }

    // Unauthenticated, content-free endpoints (parity with `/healthz`):
    //  - /health: liveness + version.
    //  - /events: a one-shot SSE snapshot of aggregate, content-free counts. It
    //    is token-free because the browser `EventSource` API cannot send an
    //    `Authorization` header; it is still Host/Origin-guarded and exposes only
    //    aggregate counts (never content). See the POC notes in docs/cockpit.
    if method == Method::GET && path == "/api/v1/health" {
        return json(
            StatusCode::OK,
            &serde_json::json!({ "ok": true, "version": VERSION }),
        );
    }
    if method == Method::GET && path == "/api/v1/events" {
        return events_response(&state);
    }

    // Everything else under /api/v1 requires the bearer token.
    if let Some(rest) = path.strip_prefix("/api/v1/") {
        if !state.auth.authenticate(bearer(headers).as_deref()) {
            return json(
                StatusCode::UNAUTHORIZED,
                &serde_json::json!({
                    "type": "error",
                    "error": { "type": "unauthorized", "message": "missing or invalid bearer token" }
                }),
            );
        }
        return match (method, rest) {
            (Method::GET, "version") => json(StatusCode::OK, &version_payload(&state)),
            (Method::GET, "service") => json(StatusCode::OK, &service_payload(&state).await),
            (Method::GET, "stats") => json(StatusCode::OK, &stats_payload(&state)),
            _ => json(
                StatusCode::NOT_FOUND,
                &serde_json::json!({
                    "type": "error",
                    "error": { "type": "not_found", "message": "no such endpoint" }
                }),
            ),
        };
    }

    text(StatusCode::NOT_FOUND, "not found")
}

// ---- handlers --------------------------------------------------------------

fn serve_cockpit(state: &AdminState) -> Response<Full<Bytes>> {
    let html = COCKPIT_HTML.replace("__TRIMWIRE_TOKEN__", &state.token);
    build(
        StatusCode::OK,
        "text/html; charset=utf-8",
        html.into_bytes(),
    )
}

fn version_payload(state: &AdminState) -> serde_json::Value {
    serde_json::json!({
        "version": VERSION,
        "control_api": "v1-poc",
        "profile": state.profile,
        // The upstream DESTINATION URL (not the OAuth token — that is never read
        // or returned by the control plane; doc 07 R3).
        "upstream": state.upstream,
        "gateway_listen": state.gateway_listen,
        "admin_listen": state.admin_listen,
    })
}

async fn service_payload(state: &AdminState) -> serde_json::Value {
    serde_json::json!({
        "gateway_listen": state.gateway_listen,
        "admin_listen": state.admin_listen,
        "serving": probe(&state.gateway_listen).await,
        "version": VERSION,
    })
}

/// Read the content-free savings ledger and return the same shape as
/// `stats --json`. The `Report` is content-free by construction (counts, hashes,
/// timestamps, names) — there is no content to leak.
fn stats_payload(state: &AdminState) -> serde_json::Value {
    if !crate::ledger::resolve_path(&state.db_path).exists() {
        return serde_json::json!({ "available": false, "reason": "ledger not created" });
    }
    match Ledger::report(&state.db_path) {
        Ok(report) => match serde_json::to_value(&report) {
            Ok(mut v) => {
                if let Some(obj) = v.as_object_mut() {
                    obj.insert("available".to_owned(), serde_json::Value::Bool(true));
                }
                v
            }
            Err(e) => serde_json::json!({ "available": false, "reason": e.to_string() }),
        },
        Err(e) => serde_json::json!({ "available": false, "reason": e.to_string() }),
    }
}

/// One-shot SSE snapshot of aggregate, content-free counts. The browser
/// `EventSource` reconnects on stream end, so this behaves as a light live feed
/// for the POC; production replaces it with the broadcast channel in doc 03 §4.
fn events_response(state: &AdminState) -> Response<Full<Bytes>> {
    let stats = stats_payload(state);
    let snapshot = serde_json::json!({
        "requests": stats.get("total_requests").cloned().unwrap_or(serde_json::Value::Null),
        "bytes_saved": stats.get("bytes_saved").cloned().unwrap_or(serde_json::Value::Null),
        "reduction_pct": stats.get("reduction_pct").cloned().unwrap_or(serde_json::Value::Null),
    });
    let body = format!(
        "retry: 2000\nevent: snapshot\ndata: {}\n\n",
        serde_json::to_string(&snapshot).unwrap_or_else(|_| "{}".to_owned())
    );
    build(StatusCode::OK, "text/event-stream", body.into_bytes())
}

// ---- header guards ---------------------------------------------------------

/// The loopback authorities we accept for this port.
fn authorities(port: u16) -> [String; 3] {
    [
        format!("127.0.0.1:{port}"),
        format!("localhost:{port}"),
        format!("[::1]:{port}"),
    ]
}

fn host_allowed(headers: &HeaderMap, port: u16) -> bool {
    match headers.get(HOST).and_then(|h| h.to_str().ok()) {
        Some(h) => authorities(port).iter().any(|a| a == h),
        None => false,
    }
}

fn origin_allowed(headers: &HeaderMap, port: u16) -> bool {
    match headers.get(ORIGIN).and_then(|h| h.to_str().ok()) {
        // A same-origin browser page sends our own loopback origin; a malicious
        // cross-origin page sends its own (rejected). Non-browser clients (curl,
        // top-level navigation) send no Origin — allowed.
        Some(o) => authorities(port).iter().any(|a| format!("http://{a}") == o),
        None => true,
    }
}

fn bearer(headers: &HeaderMap) -> Option<String> {
    let v = headers.get(AUTHORIZATION)?.to_str().ok()?;
    v.strip_prefix("Bearer ").map(|s| s.to_owned())
}

// ---- helpers ---------------------------------------------------------------

async fn probe(addr: &str) -> bool {
    let Ok(sa) = addr.parse::<SocketAddr>() else {
        return false;
    };
    matches!(
        tokio::time::timeout(
            Duration::from_millis(300),
            tokio::net::TcpStream::connect(sa)
        )
        .await,
        Ok(Ok(_))
    )
}

fn json(status: StatusCode, v: &serde_json::Value) -> Response<Full<Bytes>> {
    let bytes = serde_json::to_vec(v).unwrap_or_else(|_| b"{}".to_vec());
    build(status, "application/json", bytes)
}

fn text(status: StatusCode, msg: &str) -> Response<Full<Bytes>> {
    build(status, "text/plain; charset=utf-8", msg.as_bytes().to_vec())
}

fn build(status: StatusCode, content_type: &str, body: Vec<u8>) -> Response<Full<Bytes>> {
    Response::builder()
        .status(status)
        .header(CONTENT_TYPE, content_type)
        .header("Cache-Control", "no-store")
        .header("X-Content-Type-Options", "nosniff")
        .body(Full::new(Bytes::from(body)))
        .unwrap_or_else(|_| Response::new(Full::new(Bytes::from_static(b"{}"))))
}

/// `control.token` lives next to the ledger DB (same `~/.trimwire/` dir by
/// default, or wherever a custom `[ledger] db_path` points). The `db_path` may
/// carry a leading `~/`, so expand it the same way the ledger does.
fn token_path(db_path: &str) -> PathBuf {
    let resolved = crate::ledger::resolve_path(db_path);
    let parent = resolved
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    parent.join("control.token")
}

/// Load the existing per-install control token, or generate a fresh 256-bit one
/// (hex) and persist it `0600`.
fn load_or_create_token(db_path: &str) -> Result<String> {
    let path = token_path(db_path);
    if let Ok(existing) = std::fs::read_to_string(&path) {
        let t = existing.trim().to_owned();
        if !t.is_empty() {
            return Ok(t);
        }
    }
    let mut buf = [0u8; 32];
    getrandom::fill(&mut buf).map_err(|e| anyhow::anyhow!("getrandom: {e}"))?;
    let token = hex::encode(buf);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    std::fs::write(&path, &token).with_context(|| format!("write {}", path.display()))?;
    let _ = crate::fsperm::restrict_to_owner(&path);
    Ok(token)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constant_time_eq_matches_and_rejects() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab")); // length mismatch
    }

    #[test]
    fn host_guard_pins_loopback_authority() {
        let mut h = HeaderMap::new();
        h.insert(HOST, "127.0.0.1:8766".parse().unwrap());
        assert!(host_allowed(&h, 8766));

        // DNS-rebinding: attacker domain resolved to 127.0.0.1 keeps its own Host.
        let mut bad = HeaderMap::new();
        bad.insert(HOST, "evil.example.com".parse().unwrap());
        assert!(!host_allowed(&bad, 8766));

        // Wrong port is rejected too.
        let mut wrong_port = HeaderMap::new();
        wrong_port.insert(HOST, "127.0.0.1:9999".parse().unwrap());
        assert!(!host_allowed(&wrong_port, 8766));
    }

    #[test]
    fn origin_guard_allows_same_origin_and_absent_only() {
        let mut same = HeaderMap::new();
        same.insert(ORIGIN, "http://127.0.0.1:8766".parse().unwrap());
        assert!(origin_allowed(&same, 8766));

        let mut cross = HeaderMap::new();
        cross.insert(ORIGIN, "https://evil.example.com".parse().unwrap());
        assert!(!origin_allowed(&cross, 8766));

        // No Origin (curl / top-level navigation) is allowed.
        assert!(origin_allowed(&HeaderMap::new(), 8766));
    }

    #[test]
    fn bearer_extracts_only_with_prefix() {
        let mut h = HeaderMap::new();
        h.insert(AUTHORIZATION, "Bearer abc123".parse().unwrap());
        assert_eq!(bearer(&h).as_deref(), Some("abc123"));

        let mut basic = HeaderMap::new();
        basic.insert(AUTHORIZATION, "Basic abc123".parse().unwrap());
        assert_eq!(bearer(&basic), None);

        assert_eq!(bearer(&HeaderMap::new()), None);
    }

    #[test]
    fn token_generated_is_256_bit_hex_and_stable() {
        let dir = std::env::temp_dir().join(format!("tw-cockpit-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("ledger.db");
        let db = db.to_string_lossy().into_owned();

        let t1 = load_or_create_token(&db).unwrap();
        assert_eq!(t1.len(), 64, "32 random bytes hex-encoded"); // 256-bit
        // A second call reuses the persisted token rather than rotating it.
        let t2 = load_or_create_token(&db).unwrap();
        assert_eq!(t1, t2);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn loopback_authenticator_rejects_wrong_and_missing() {
        let a = LoopbackToken {
            token: "secret".to_owned(),
        };
        assert!(a.authenticate(Some("secret")));
        assert!(!a.authenticate(Some("nope")));
        assert!(!a.authenticate(None));
    }
}
