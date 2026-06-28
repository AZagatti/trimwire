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

/// Web App Manifest — makes the cockpit an installable PWA (browser "Install app",
/// iOS/Android "Add to Home Screen"). PWA-first is the multi-platform strategy
/// (doc 05): one installable web app covers desktop browsers and both mobile OSes
/// with no app-store account or fee. Served unauthenticated (non-sensitive static).
const COCKPIT_MANIFEST: &str = r##"{
  "name": "trimwire Flightdeck",
  "short_name": "Flightdeck",
  "description": "Control panel for a local trimwire daemon (content-free).",
  "start_url": "/",
  "scope": "/",
  "display": "standalone",
  "background_color": "#0f1417",
  "theme_color": "#0f1417",
  "icons": [
    { "src": "/icon.svg", "sizes": "any", "type": "image/svg+xml", "purpose": "any maskable" }
  ]
}"##;

/// A tiny scalable app icon (teal rounded square + "t" glyph), echoing the site logo.
const COCKPIT_ICON: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 512 512">
<rect width="512" height="512" rx="112" fill="#0f1417"/>
<rect x="40" y="40" width="432" height="432" rx="88" fill="none" stroke="#2aa39c" stroke-width="20"/>
<path d="M180 168h152M256 168v210" fill="none" stroke="#2aa39c" stroke-width="40" stroke-linecap="round"/>
</svg>"##;

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

    // DNS-rebinding / drive-by-browser guard (doc 03 §5, doc 06 R7): the request
    // authority must be the literal loopback authority, and any `Origin` must be
    // same-origin. Use the URI authority (HTTP/2 `:authority`) when present, else
    // the `Host` header (HTTP/1.1) — checking only `Host` would mis-handle h2.
    let authority = req
        .uri()
        .authority()
        .map(|a| a.as_str().to_owned())
        .or_else(|| {
            headers
                .get(HOST)
                .and_then(|h| h.to_str().ok())
                .map(str::to_owned)
        });
    if !authority_allowed(authority.as_deref(), state.port) {
        return text(StatusCode::FORBIDDEN, "forbidden: bad Host");
    }
    if !origin_allowed(headers, state.port) {
        return text(StatusCode::FORBIDDEN, "forbidden: bad Origin");
    }
    // Third independent gate: the browser-set, page-unforgeable `Sec-Fetch-Site`
    // fetch-metadata (OWASP CSRF defense). Reject anything a cross-site context
    // initiated, regardless of Host/Origin. These checks run BEFORE the token
    // compare and before any side effect, so a DNS-rebinding caller never reaches
    // the auth path even if the token check had a bug.
    if !sec_fetch_site_ok(headers) {
        return text(StatusCode::FORBIDDEN, "forbidden: cross-site request");
    }

    let method = req.method().clone();
    let path = req.uri().path().to_owned();

    // Same-origin web UI (token injected at load). Host/Origin-guarded above.
    if method == Method::GET && (path == "/" || path == "/index.html") {
        return serve_cockpit(&state);
    }
    // PWA install assets (static, non-sensitive). Make the cockpit installable on
    // desktop browsers and mobile home screens with no app store (doc 05).
    if method == Method::GET && path == "/manifest.webmanifest" {
        return build(
            StatusCode::OK,
            "application/manifest+json",
            COCKPIT_MANIFEST.as_bytes().to_vec(),
        );
    }
    if method == Method::GET && path == "/icon.svg" {
        return build(
            StatusCode::OK,
            "image/svg+xml",
            COCKPIT_ICON.as_bytes().to_vec(),
        );
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
    let nonce = gen_nonce();
    let html = COCKPIT_HTML
        .replace("__TRIMWIRE_TOKEN__", &state.token)
        .replace("__TRIMWIRE_NONCE__", &nonce);
    html_response(&nonce, html.into_bytes())
}

fn version_payload(state: &AdminState) -> serde_json::Value {
    // Deliberately does NOT include `[server] upstream`: it's the field that decides
    // where the OAuth token is forwarded, and a *custom* upstream URL can embed
    // credentials (userinfo/query). Even the destination is "derived" credential-
    // routing info (doc 07 R3), and the UI doesn't need it — so it never crosses the
    // control surface. (Cross-model PR review consensus.)
    serde_json::json!({
        "version": VERSION,
        "control_api": "v1-poc",
        "profile": state.profile,
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
                    // Content-free guarantee (doc 07 R7/R8): `Report` carries the
                    // local ledger `db_path` (an absolute filesystem path leaking
                    // the OS username/layout). Never expose it on the network
                    // surface — the CLI `stats --json` keeps it, the control API
                    // strips it.
                    obj.remove("db_path");
                }
                v
            }
            Err(e) => serde_json::json!({ "available": false, "reason": e.to_string() }),
        },
        Err(e) => serde_json::json!({ "available": false, "reason": e.to_string() }),
    }
}

/// One-shot SSE snapshot of aggregate, content-free counts. The browser
/// `EventSource` reconnects on stream end (~2s), so this behaves as a light live
/// feed for the POC.
///
/// COST (cross-model PR review): each reconnect re-runs `Ledger::report()` (a
/// SQLite aggregate over the `requests` table), so an open tab issues one ledger
/// read every ~2s. Fine for a POC on a single loopback client; production replaces
/// this with a `tokio::sync::broadcast` channel fed at ledger-write time (doc 03
/// §4) so there is zero per-connection query.
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

/// Default-deny: a missing/unparseable authority is rejected; otherwise it must
/// be one of the literal loopback authorities for our port (defeats DNS-rebinding,
/// which keeps the attacker's `Host`/authority even when the name resolves to
/// 127.0.0.1).
fn authority_allowed(authority: Option<&str>, port: u16) -> bool {
    match authority {
        Some(a) => authorities(port).iter().any(|allowed| allowed == a),
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

/// `Sec-Fetch-Site` is set by the browser and cannot be forged by page JS. A
/// same-origin `fetch`/`EventSource` sends `same-origin`; a typed/bookmarked
/// navigation sends `none`; a cross-site page sends `cross-site`/`same-site`.
/// Allow only `same-origin`/`none`; absent header (curl, non-browser) is allowed.
fn sec_fetch_site_ok(headers: &HeaderMap) -> bool {
    match headers.get("sec-fetch-site").and_then(|h| h.to_str().ok()) {
        Some(v) => v == "same-origin" || v == "none",
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

/// Response builder for the non-HTML surface (JSON / manifest / SVG). These carry
/// no inline script or style, so the CSP is maximally strict — no `script-src`,
/// no `'unsafe-inline'`. The token-bearing HTML uses [`html_response`] instead.
fn build(status: StatusCode, content_type: &str, body: Vec<u8>) -> Response<Full<Bytes>> {
    Response::builder()
        .status(status)
        .header(CONTENT_TYPE, content_type)
        .header("Cache-Control", "no-store")
        .header("X-Content-Type-Options", "nosniff")
        .header("X-Frame-Options", "DENY")
        .header(
            "Content-Security-Policy",
            "default-src 'none'; img-src 'self'; connect-src 'self'; manifest-src 'self'; \
             base-uri 'none'; form-action 'none'",
        )
        .body(Full::new(Bytes::from(body)))
        .unwrap_or_else(|_| Response::new(Full::new(Bytes::from_static(b"{}"))))
}

/// Response builder for the token-bearing cockpit HTML. Uses a **per-render CSP
/// nonce** instead of `'unsafe-inline'` so an injected `<script>` can't run and
/// exfiltrate the in-DOM control token (cross-model PR review consensus / doc 10
/// G3). `worker-src 'none'` blocks a script-registered service worker.
fn html_response(nonce: &str, body: Vec<u8>) -> Response<Full<Bytes>> {
    let csp = format!(
        "default-src 'none'; script-src 'nonce-{nonce}'; style-src 'nonce-{nonce}'; \
         connect-src 'self'; img-src 'self'; manifest-src 'self'; worker-src 'none'; \
         base-uri 'none'; form-action 'none'"
    );
    Response::builder()
        .status(StatusCode::OK)
        .header(CONTENT_TYPE, "text/html; charset=utf-8")
        .header("Cache-Control", "no-store")
        .header("X-Content-Type-Options", "nosniff")
        .header("X-Frame-Options", "DENY")
        .header("Content-Security-Policy", csp)
        .body(Full::new(Bytes::from(body)))
        .unwrap_or_else(|_| Response::new(Full::new(Bytes::from_static(b"<!doctype html>"))))
}

/// A random 128-bit CSP nonce (hex), fresh per response.
fn gen_nonce() -> String {
    let mut buf = [0u8; 16];
    // A nonce only needs to be unguessable per response; if the OS RNG hiccups we
    // still must not reuse a constant, so fall back to a process+addr-derived value.
    if getrandom::fill(&mut buf).is_err() {
        return format!("{:x}", std::process::id() as u128).repeat(2);
    }
    hex::encode(buf)
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

/// Load the existing per-install control token, or atomically create a fresh
/// 256-bit one (hex, `0600`). Concurrency-safe: two daemons starting at once
/// can't end up with mismatched tokens — the create is `O_EXCL`, and the loser of
/// the race adopts the winner's token (cross-model PR review: TOCTOU fix).
fn load_or_create_token(db_path: &str) -> Result<String> {
    let path = token_path(db_path);
    if let Some(t) = read_token(&path) {
        return Ok(t);
    }
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let mut buf = [0u8; 32];
    getrandom::fill(&mut buf).map_err(|e| anyhow::anyhow!("getrandom: {e}"))?;
    let token = hex::encode(buf);
    match create_new_token(&path, &token) {
        Ok(()) => Ok(token),
        // Lost a concurrent-create race: adopt the token the other process wrote,
        // so both processes share one token rather than overwriting each other.
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            read_token(&path).ok_or_else(|| {
                anyhow::anyhow!(
                    "control token {} exists but is empty/corrupt — delete it and restart",
                    path.display()
                )
            })
        }
        Err(e) => Err(anyhow::anyhow!("create {}: {e}", path.display())),
    }
}

/// Read a non-empty trimmed token from `path`, or `None` if absent/empty.
fn read_token(path: &Path) -> Option<String> {
    let t = std::fs::read_to_string(path).ok()?.trim().to_owned();
    (!t.is_empty()).then_some(t)
}

/// Atomically create the token file with `O_EXCL` (fails if it already exists) and
/// `0600` from the start on Unix — no TOCTOU, no concurrent-create overwrite, and
/// no world-readable window between create and chmod (the token IS a secret).
fn create_new_token(path: &Path, token: &str) -> std::io::Result<()> {
    use std::io::Write;
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts.open(path)?;
    f.write_all(token.as_bytes())?;
    // Belt-and-braces on platforms where mode() at create isn't honored (non-Unix).
    let _ = crate::fsperm::restrict_to_owner(path);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A synthetic state for the payload-contract tests (no I/O).
    fn test_state(db_path: &str) -> AdminState {
        AdminState {
            auth: Arc::new(LoopbackToken {
                token: "t".to_owned(),
            }),
            token: "t".to_owned(),
            port: 8766,
            db_path: db_path.to_owned(),
            gateway_listen: "127.0.0.1:8765".to_owned(),
            admin_listen: "127.0.0.1:8766".to_owned(),
            profile: "default".to_owned(),
        }
    }

    /// CONTRACT TEST (see docs/cockpit/11-api-stability.md): the cockpit frontend
    /// codes against these exact top-level keys. If a refactor changes the
    /// `/api/v1/version` shape, this fails loudly — forcing a conscious, additive
    /// change or an `/api/v2`, never a silent break. The CLI command surface can
    /// change freely; the cockpit depends only on this contract.
    #[test]
    fn version_payload_contract_keys_are_stable() {
        let v = version_payload(&test_state("/nope/ledger.db"));
        let mut keys: Vec<&str> = v
            .as_object()
            .expect("object")
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            [
                "admin_listen",
                "control_api",
                "gateway_listen",
                "profile",
                "version"
            ]
        );
        // The credential-routing `upstream` must NEVER be on the control surface.
        assert!(
            v.get("upstream").is_none(),
            "upstream (credential routing) must not be exposed"
        );
    }

    /// CONTRACT TEST: the missing-ledger branch is a documented, stable shape
    /// (`available:false`), and the control API must NEVER expose `db_path` (the
    /// content-free red line, doc 07 R7/R8) even though the underlying `Report`
    /// carries it.
    #[test]
    fn stats_payload_missing_ledger_is_available_false_and_pathless() {
        let v = stats_payload(&test_state("/nonexistent/dir/ledger.db"));
        assert_eq!(v["available"], serde_json::Value::Bool(false));
        assert!(
            v.get("db_path").is_none(),
            "control API must never expose db_path"
        );
    }

    /// The PWA manifest must be valid JSON with the keys a browser needs to offer
    /// "Install" / "Add to Home Screen" (doc 05 — PWA is the multi-platform path).
    #[test]
    fn pwa_manifest_is_valid_and_installable() {
        let m: serde_json::Value =
            serde_json::from_str(COCKPIT_MANIFEST).expect("manifest is valid JSON");
        assert_eq!(m["display"], "standalone");
        assert_eq!(m["start_url"], "/");
        assert!(m["name"].is_string());
        let icons = m["icons"].as_array().expect("icons array");
        assert!(!icons.is_empty(), "at least one icon for installability");
        assert!(COCKPIT_ICON.starts_with("<svg"), "icon is an SVG document");
    }

    /// Build-correctness guard (Sonnet PR review S1): the served HTML must carry
    /// both substitution placeholders, else the token/nonce silently won't inject.
    #[test]
    fn cockpit_html_has_token_and_nonce_placeholders() {
        assert!(
            COCKPIT_HTML.contains("__TRIMWIRE_TOKEN__"),
            "token placeholder"
        );
        assert!(
            COCKPIT_HTML.contains("__TRIMWIRE_NONCE__"),
            "nonce placeholder"
        );
        // The nonce must guard both the inline <style> and <script>.
        assert_eq!(
            COCKPIT_HTML.matches("nonce=\"__TRIMWIRE_NONCE__\"").count(),
            2,
            "both inline <style> and <script> must carry the nonce"
        );
    }

    /// The token-bearing HTML response must use a nonce CSP, never `'unsafe-inline'`
    /// (cross-model review consensus / doc 10 G3).
    #[test]
    fn html_response_uses_nonce_csp_not_unsafe_inline() {
        let resp = html_response("deadbeef", b"<!doctype html>".to_vec());
        let csp = resp
            .headers()
            .get("Content-Security-Policy")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        assert!(
            csp.contains("script-src 'nonce-deadbeef'"),
            "nonce script-src"
        );
        assert!(
            !csp.contains("'unsafe-inline'"),
            "no unsafe-inline on the token page"
        );
        assert!(
            csp.contains("worker-src 'none'"),
            "block script-registered workers"
        );
    }

    #[test]
    fn gen_nonce_is_unique_and_hex() {
        let a = gen_nonce();
        let b = gen_nonce();
        assert_eq!(a.len(), 32, "128-bit hex");
        assert_ne!(a, b, "fresh per call");
        assert!(a.bytes().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn constant_time_eq_matches_and_rejects() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab")); // length mismatch
    }

    #[test]
    fn authority_guard_pins_loopback() {
        assert!(authority_allowed(Some("127.0.0.1:8766"), 8766));
        assert!(authority_allowed(Some("localhost:8766"), 8766));
        assert!(authority_allowed(Some("[::1]:8766"), 8766));

        // DNS-rebinding: attacker domain resolved to 127.0.0.1 keeps its own authority.
        assert!(!authority_allowed(Some("evil.example.com"), 8766));
        // Wrong port, bare scheme-less host, and a missing authority are all rejected.
        assert!(!authority_allowed(Some("127.0.0.1:9999"), 8766));
        assert!(!authority_allowed(Some("127.0.0.1"), 8766));
        assert!(!authority_allowed(None, 8766)); // default-deny
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
    fn sec_fetch_site_rejects_cross_site_only() {
        let mut same = HeaderMap::new();
        same.insert("sec-fetch-site", "same-origin".parse().unwrap());
        assert!(sec_fetch_site_ok(&same));

        let mut nav = HeaderMap::new();
        nav.insert("sec-fetch-site", "none".parse().unwrap());
        assert!(sec_fetch_site_ok(&nav));

        let mut cross = HeaderMap::new();
        cross.insert("sec-fetch-site", "cross-site".parse().unwrap());
        assert!(!sec_fetch_site_ok(&cross));

        let mut sibling = HeaderMap::new();
        sibling.insert("sec-fetch-site", "same-site".parse().unwrap());
        assert!(!sec_fetch_site_ok(&sibling));

        // Non-browser clients (curl) send no fetch-metadata — allowed.
        assert!(sec_fetch_site_ok(&HeaderMap::new()));
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
