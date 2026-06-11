//! HTTP server + per-request dispatch. Owns the request/response lifecycle.
//!
//! Contract (from SPIKE.md §3):
//! - Request body is read FULLY (buffered — `messages[]` mutation needs the
//!   whole array in memory).
//! - Response body is STREAMED back to the caller without buffering (SSE).
//!
//! Each request:
//! 1. Read the inbound body to a `Bytes` buffer.
//! 2. For `POST /v1/messages`, prune `messages[]` via `strategies::apply_to_body`
//!    (delegated — this module contains no mutation logic, only orchestration).
//!    Any non-JSON / missing-`messages` / no-op / strategy-error case forwards
//!    the original bytes verbatim, preserving the prompt-cache prefix.
//! 3. Build an outbound request to `<upstream>/<path-and-query>` with the
//!    forwarded headers (hop-by-hop stripped, `Host` rewritten, stale
//!    `Content-Length` dropped so the re-framed body sets it) and the
//!    (possibly pruned) body bytes.
//! 4. Send it via the shared `upstream::UpstreamClient`.
//! 5. Stream the upstream response back (status + headers + body) via
//!    `proxy_stream::passthrough`.
//! 6. Log one line to stderr:
//!    `[gateway] METHOD PATH in=NB sent=MB status=SSS Tms[ pruned[...]]`.
//!
//! Known follow-ups (not blocking Step 2, but worth revisiting):
//! - `out=` byte counter is upstream's `Content-Length` when present and
//!   `stream` otherwise. A counting body adapter in `proxy_stream` would
//!   give true bytes-out for SSE responses; left out of Step 1 so the
//!   forwarding path stays zero-copy.
//! - Handler-failure error response is plain-text 502; SPIKE §6 spec is
//!   an Anthropic-shaped JSON envelope (`{"type":"error","error":{...}}`).
//!   Step 5 (ledger) is a natural place to align this when the error
//!   surface stabilises.
//! - `ctrl-c` shutdown is immediate; in-flight requests are dropped. A
//!   graceful-drain path (let active connections complete with a small
//!   deadline) would be polite for `trimwire run` exit.
//! - SSE streaming behaviour is correct (no buffering in
//!   `proxy_stream::passthrough`) but `--print` doesn't actually exercise
//!   it; run `claude` interactively or with `--output-format stream-json`
//!   to confirm token-by-token arrival in the TUI.

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Instant;

use anyhow::{Context, Result};
use http_body_util::{BodyExt, Full, LengthLimitError, Limited};
use hyper::body::{Bytes, Incoming};
use hyper::header::{ACCEPT_ENCODING, CONTENT_LENGTH, HOST, HeaderName, HeaderValue};
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode, Uri};
use hyper_util::rt::{TokioExecutor, TokioIo};
use hyper_util::server::conn::auto::Builder as ServerBuilder;

use dashmap::DashMap;

use crate::config::Config;
use crate::ledger::{self, Ledger};
use crate::proxy::audit::{self, AuditSink};
use crate::proxy::metered_body::{MeteredBody, RequestInfo};
use crate::proxy::proxy_stream::{BoxBodyError, passthrough};
use crate::proxy::upstream::{UpstreamClient, build_client};
use crate::reprune::{self, PruneState};
use crate::strategies::{self, BodyOutcome};

/// Per-session stable-prefix re-pruning state, shared across connection tasks.
type RepruneCache = Arc<DashMap<String, PruneState>>;

/// Reprune cache key: `session_id` + `model`. Claude Code interleaves sub-agent
/// and background calls (different models — haiku/sonnet) under one
/// `x-claude-code-session-id`; keying on the session id alone would thrash one
/// `PruneState` between those interleaved streams (each switch fails the prefix
/// fingerprint → perpetual full re-checkpoint = effectively stateless). Adding
/// the model separates the model-distinct streams (most of the aux traffic).
/// Two *same-model* streams (e.g. an Opus sub-agent under an Opus main thread)
/// still share a key — the prefix-fingerprint guard keeps that correct, just
/// less cache-efficient; there is no finer per-stream id on the wire.
fn reprune_key(session_id: &str, model: Option<&str>) -> String {
    format!("{session_id}\u{0}{}", model.unwrap_or(""))
}

/// Evict idle/over-cap sessions (only scans when over the cap — rare).
fn evict_stale(cache: &DashMap<String, PruneState>, max: usize, ttl_secs: u64) {
    if cache.len() <= max {
        return;
    }
    cache.retain(|_, st| st.idle_secs() < ttl_secs);
    while cache.len() > max {
        let oldest = cache
            .iter()
            .max_by_key(|e| e.idle_secs())
            .map(|e| e.key().clone());
        match oldest {
            Some(k) => {
                cache.remove(&k);
            }
            None => break,
        }
    }
}

/// The endpoint whose `messages[]` we prune. Everything else is passed through.
const MESSAGES_PATH: &str = "/v1/messages";

/// Max time to wait for the upstream to return response *headers*. The SSE
/// body still streams freely after that; this only stops a hung/slow upstream
/// from wedging the caller's session forever.
const UPSTREAM_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

/// Cap on the inbound request body we buffer. Anthropic's own `/v1/messages`
/// limit is well under this; the cap just bounds memory against a runaway
/// local client. Over-limit → 413.
const MAX_BODY_BYTES: usize = 32 * 1024 * 1024;

/// Anthropic rejects a serialized request body over ~20 MB ("request too large").
/// Accumulated images / huge tool results can cross this even after pruning,
/// permanently bricking the session. We warn (once the post-prune body crosses a
/// fraction of the wall) so the user can `/compact` or start fresh before it
/// fails. Detection is a pure fn so it's unit-testable.
const PAYLOAD_LIMIT_BYTES: usize = 20 * 1024 * 1024;
const PAYLOAD_WARN_BYTES: usize = PAYLOAD_LIMIT_BYTES / 5 * 4; // 80% = 16 MB

fn near_payload_limit(body_len: usize) -> bool {
    body_len >= PAYLOAD_WARN_BYTES
}

/// Hop-by-hop headers we must NOT forward (RFC 7230 §6.1). `Host` is
/// rewritten per request to match the upstream authority, not stripped
/// blindly.
const HOP_BY_HOP: &[&str] = &[
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "proxy-connection", // non-standard, but old proxies emit it
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
];

/// Default upstream when no config / env override is supplied.
pub const DEFAULT_UPSTREAM: &str = "https://api.anthropic.com";

/// Bind, then serve forever. Returns only on fatal I/O error.
pub async fn run(
    listen: SocketAddr,
    upstream: String,
    config: Arc<Config>,
    ledger: Ledger,
    audit_path: Option<String>,
) -> Result<()> {
    let (listener, source) = crate::proxy::listener::obtain(listen).await?;
    let actual = listener.local_addr().unwrap_or(listen);
    eprintln!(
        "[gateway] listening on http://{actual} → {upstream} ({})",
        source.label()
    );
    run_from_listener(listener, upstream, config, ledger, audit_path).await
}

/// Serve on an already-bound listener. Split out from [`run`] so callers that
/// own the socket — socket activation in production, and tests that bind an
/// ephemeral port and need its address with no bind→rebind race — can hand the
/// live listener straight in.
pub async fn run_from_listener(
    listener: tokio::net::TcpListener,
    upstream: String,
    config: Arc<Config>,
    ledger: Ledger,
    audit_path: Option<String>,
) -> Result<()> {
    let client = build_client();
    let server = ServerBuilder::new(TokioExecutor::new());
    let reprune_cache: RepruneCache = Arc::new(DashMap::new());
    // Opt-in, metadata-only wire audit (--audit / TRIMWIRE_AUDIT, resolved by the
    // CLI and passed in). Off by default; when off it never even parses a body.
    // See `proxy::audit`.
    let audit: Option<Arc<AuditSink>> = audit_path.and_then(|p| match AuditSink::open(&p) {
        Ok(sink) => {
            eprintln!("[gateway] wire audit ON (metadata only) → {p}");
            Some(Arc::new(sink))
        }
        Err(e) => {
            eprintln!("[gateway] could not open audit file {p}: {e}");
            None
        }
    });

    loop {
        let (stream, peer) = match listener.accept().await {
            Ok(s) => s,
            Err(e) => {
                eprintln!("[gateway] accept error: {e}");
                continue;
            }
        };
        let io = TokioIo::new(stream);
        let client = client.clone();
        let upstream = upstream.clone();
        let config = config.clone();
        let ledger = ledger.clone();
        let reprune_cache = reprune_cache.clone();
        let audit = audit.clone();
        let server = server.clone();

        tokio::spawn(async move {
            let svc = service_fn(move |req| {
                let client = client.clone();
                let upstream = upstream.clone();
                let config = config.clone();
                let ledger = ledger.clone();
                let reprune_cache = reprune_cache.clone();
                let audit = audit.clone();
                async move {
                    let res =
                        handle(req, client, upstream, config, ledger, reprune_cache, audit).await;
                    Ok::<_, Infallible>(res.unwrap_or_else(error_response))
                }
            });
            if let Err(e) = server.serve_connection(io, svc).await {
                // Common: clients hang up; not worth logging at high level.
                tracing::debug!(peer = %peer, error = %e, "connection error");
            }
        });
    }
}

/// Per-request handler. Returns the outbound `Response` ready to send.
async fn handle(
    req: Request<Incoming>,
    client: UpstreamClient,
    upstream: String,
    config: Arc<Config>,
    ledger: Ledger,
    reprune_cache: RepruneCache,
    audit: Option<Arc<AuditSink>>,
) -> Result<Response<http_body_util::combinators::BoxBody<Bytes, BoxBodyError>>> {
    let start = Instant::now();
    let method = req.method().clone();
    let path_and_query = req
        .uri()
        .path_and_query()
        .map(|p| p.as_str().to_owned())
        .unwrap_or_else(|| req.uri().path().to_owned());

    // 0. Liveness probe for `trimwire status` — answered locally, never
    //    forwarded upstream.
    if path_and_query.split('?').next() == Some("/healthz") {
        return Ok(health_response());
    }

    // 1. Buffer the inbound request body (mutation needs the whole array),
    //    capped at MAX_BODY_BYTES so a runaway client can't exhaust memory.
    let (parts, body) = req.into_parts();
    let body_bytes = match Limited::new(body, MAX_BODY_BYTES).collect().await {
        Ok(collected) => collected.to_bytes(),
        Err(e) if e.downcast_ref::<LengthLimitError>().is_some() => {
            return Ok(anthropic_error(
                StatusCode::PAYLOAD_TOO_LARGE,
                "trimwire: request body exceeds the 32 MB limit",
            ));
        }
        Err(e) => return Err(anyhow::anyhow!("read inbound request body: {e}")),
    };
    let in_len = body_bytes.len();

    // 1b. Prune `messages[]` for POST /v1/messages. Anything else — non-JSON,
    // missing messages, no-op, or a strategy error — forwards verbatim, which
    // keeps Anthropic's prompt-cache prefix intact (SPIKE.md §9).
    // Session id (Claude Code sets it; also reused by the ledger below). It keys
    // the per-session stable-prefix state.
    let session_id = parts
        .headers
        .get("x-claude-code-session-id")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);

    let mut out_body = body_bytes;
    let mut prune_log = String::new();
    // (prefix_hash_in, prefix_hash_out, fired-strategy CSV, per-strategy bytes
    // CSV) for the ledger, recorded only for the messages endpoint we prune.
    let mut ledger_entry: Option<(String, String, String, String, Option<String>)> = None;
    let is_messages = method == Method::POST && path_only(&path_and_query) == MESSAGES_PATH;
    if is_messages {
        // One parse: the cache-prefix hash + the request model (for the reprune key).
        let (hash_in, model) = ledger::prefix_hash_and_model(&out_body);
        // The un-pruned inbound bytes — the local-model compactor summarizes the
        // ORIGINAL slice (never the model-free-pruned version). Bytes clone is O(1).
        let original_in = out_body.clone();
        // Metadata-only wire audit (opt-in). Captures the SHAPE of the inbound
        // body (counts/flags only, never content) before we prune, so we can
        // see what Claude Code's own native handling did on the wire.
        if let Some(sink) = audit.as_deref() {
            let beta = parts
                .headers
                .get("anthropic-beta")
                .and_then(|v| v.to_str().ok());
            if let Some(cap) = audit::capture(
                &out_body,
                session_id.as_deref(),
                beta,
                hash_in.clone(),
                unix_secs(),
            ) {
                sink.record(&cap);
            }
        }
        // Stable-prefix re-pruning (opt-in) when we can key on a session id;
        // otherwise the pure stateless prune. Both fail open to the original body.
        let outcome = match (config.reprune.enabled, &session_id) {
            (true, Some(sid)) => {
                evict_stale(
                    &reprune_cache,
                    config.reprune.max_sessions,
                    config.reprune.ttl_secs,
                );
                let mut st = reprune_cache
                    .entry(reprune_key(sid, model.as_deref()))
                    .or_default();
                reprune::stable_apply_to_body(&out_body, &config, &mut st, config.reprune.threshold)
            }
            _ => strategies::apply_to_body(&out_body, &config),
        };
        let (strategies, strategy_bytes) = match outcome {
            BodyOutcome::Mutated { bytes, fired } => {
                prune_log = format_fired(&fired);
                let fired: Vec<_> = fired.iter().filter(|(_, s)| s.stubbed > 0).collect();
                let csv = fired
                    .iter()
                    .map(|(name, _)| *name)
                    .collect::<Vec<_>>()
                    .join(",");
                // Per-strategy bytes elided this turn (clamp negatives to 0; a
                // stub can nudge a tiny result up). Byte counts only — no content.
                let bytes_csv = fired
                    .iter()
                    .map(|(name, s)| format!("{name}:{}", s.elided_bytes().max(0)))
                    .collect::<Vec<_>>()
                    .join(",");
                out_body = Bytes::from(bytes);
                (csv, bytes_csv)
            }
            BodyOutcome::Unchanged => (String::new(), String::new()),
        };
        let hash_out = ledger::prefix_hash(&out_body);

        // OPT-IN local-model compaction: if enabled, spawn a BACKGROUND model
        // call to summarize the OLD slice for FUTURE turns. Requires reprune (it
        // carries the cached summary) + a session id. Never blocks this request;
        // any failure leaves the model-free output untouched.
        if crate::summarizer::engages(&config) {
            if let Some(sid) = session_id.as_deref() {
                crate::summarizer::maybe_spawn_summarization(
                    reprune_cache.clone(),
                    reprune_key(sid, model.as_deref()),
                    config.clone(),
                    original_in,
                    ledger.clone(),
                );
            }
        }

        ledger_entry = Some((hash_in, hash_out, strategies, strategy_bytes, model));
    }
    let out_len = out_body.len();
    // B-7: warn when the (post-prune) body nears Anthropic's ~20 MB request-size
    // wall — beyond it the request fails and the session bricks. Fires only near
    // the limit, so it isn't spammy; it's actionable (compact / fresh session).
    // Image / large-tool-result accumulation is the usual cause.
    if is_messages && near_payload_limit(out_len) {
        tracing::warn!(
            out_bytes = out_len,
            limit_bytes = PAYLOAD_LIMIT_BYTES,
            "trimwire: request body is near Anthropic's ~20MB size limit — the session may start failing; consider /compact or a fresh session (accumulated images / large tool results are the usual cause)"
        );
    }

    // 2. Build the outbound URI: <upstream>/<path-and-query>.
    let upstream_uri: Uri = format!("{}{}", upstream.trim_end_matches('/'), path_and_query)
        .parse()
        .with_context(|| format!("parse upstream uri from {upstream} + {path_and_query}"))?;

    // 3. Build the outbound request. Copy headers minus hop-by-hop, rewrite Host.
    let mut out = Request::builder().method(method.clone()).uri(&upstream_uri);
    let out_headers = out.headers_mut().expect("builder headers");
    for (name, value) in parts.headers.iter() {
        if is_hop_by_hop(name) {
            continue;
        }
        out_headers.append(name.clone(), value.clone());
    }
    // The body is always re-framed as `Full<Bytes>`, whose exact length the
    // client sets — drop any inbound Content-Length (it is stale after a prune
    // and redundant otherwise).
    out_headers.remove(CONTENT_LENGTH);
    // On the instrumented messages path, ask upstream for UNCOMPRESSED SSE.
    // Claude Code advertises `accept-encoding: gzip`; if we forward it, Anthropic
    // gzips the event stream and `MeteredBody` scans compressed bytes — finding no
    // `data:` lines, so usage tokens silently come back 0 (the response still
    // forwards fine and the client decompresses). Forcing identity keeps the SSE
    // parseable. Scoped to messages only; other passthrough keeps client encoding.
    if is_messages {
        out_headers.insert(ACCEPT_ENCODING, HeaderValue::from_static("identity"));
    }
    if let Some(host) = upstream_uri.host() {
        let host_hdr = if let Some(port) = upstream_uri.port_u16() {
            format!("{host}:{port}")
        } else {
            host.to_owned()
        };
        if let Ok(v) = HeaderValue::from_str(&host_hdr) {
            out_headers.insert(HOST, v);
        }
    }
    let out_req = out
        .body(Full::new(out_body))
        .context("build upstream request")?;

    // 4. Capture the send instant for TTFT measurement, then send to upstream,
    //    bounding time-to-response-headers.
    let send_instant = std::time::Instant::now();
    let up_res = match tokio::time::timeout(UPSTREAM_TIMEOUT, client.request(out_req)).await {
        Ok(Ok(res)) => res,
        Ok(Err(e)) => {
            // Content-free, fire-and-forget — records the failure for `trimwire stats`
            // without touching the response path (the request still returns a 502).
            ledger.record_upstream_error(unix_secs(), 'e');
            return Err(anyhow::Error::new(e).context("upstream request failed"));
        }
        Err(_elapsed) => {
            ledger.record_upstream_error(unix_secs(), 't');
            eprintln!(
                "[gateway] upstream timed out after {}s",
                UPSTREAM_TIMEOUT.as_secs()
            );
            return Ok(anthropic_error(
                StatusCode::GATEWAY_TIMEOUT,
                "trimwire: upstream request timed out",
            ));
        }
    };

    // 5. Convert into a streaming downstream response.
    //    For POST /v1/messages we wrap the body with `MeteredBody` which:
    //    - observes SSE frames to collect TTFT, usage tokens, applied_edits,
    //    - forwards every byte unchanged (passthrough contract preserved),
    //    - writes the ledger row on stream-end OR Drop (client disconnect).
    //    For all other paths, plain `passthrough` is used and no ledger row is
    //    written (ledger_entry is None for non-messages paths).
    let (up_parts, up_body) = up_res.into_parts();
    // Permanent safety guard: if upstream compresses the messages response despite
    // our `identity` request, MeteredBody would scan compressed bytes and silently
    // record zero usage — warn rather than report misleading zeros. (Confirmed live:
    // Anthropic honors `identity` and returns plaintext SSE, so this should not fire.)
    if is_messages {
        if let Some(enc) = up_parts
            .headers
            .get(hyper::header::CONTENT_ENCODING)
            .and_then(|v| v.to_str().ok())
            .filter(|e| !e.eq_ignore_ascii_case("identity"))
        {
            eprintln!(
                "[trimwire] WARN: upstream returned content-encoding={enc} despite `identity`; \
                 usage-token capture will be blind for this request (cache metrics unavailable)."
            );
        }
    }
    let status = up_parts.status;
    let mut down = Response::builder().status(status);
    let down_headers = down.headers_mut().expect("builder headers");
    for (name, value) in up_parts.headers.iter() {
        if is_hop_by_hop(name) {
            continue;
        }
        down_headers.append(name.clone(), value.clone());
    }

    let ts = unix_secs();
    let response_body =
        if let Some((prefix_hash_in, prefix_hash_out, strategies, strategy_bytes, model)) =
            ledger_entry
        {
            // Messages path: use MeteredBody — the ledger write happens at stream-end
            // (or Drop). No separate fire-and-forget record call here.
            MeteredBody::wrap(
                up_body,
                send_instant,
                RequestInfo {
                    ts,
                    session_id,
                    model,
                    in_bytes: in_len as i64,
                    out_bytes: out_len as i64,
                    strategies,
                    strategy_bytes,
                    prefix_hash_in,
                    prefix_hash_out,
                },
                ledger,
            )
        } else {
            // Non-messages path: pure passthrough, no ledger row.
            passthrough(up_body)
        };

    let down = down
        .body(response_body)
        .context("build downstream response")?;

    eprintln!(
        "[gateway] {method} {path_and_query} in={in_len}B sent={out_len}B status={} {}ms{}",
        status.as_u16(),
        start.elapsed().as_millis(),
        prune_log,
    );

    Ok(down)
}

/// Seconds since the Unix epoch (request time for the ledger).
fn unix_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Path portion of a path-and-query string (everything before `?`).
fn path_only(path_and_query: &str) -> &str {
    path_and_query.split('?').next().unwrap_or(path_and_query)
}

/// Render fired-strategy stats for the request log, e.g.
/// ` pruned[sliding_window: 12 stubbed, 8431B]`.
fn format_fired(fired: &[(&'static str, strategies::Stats)]) -> String {
    let parts: Vec<String> = fired
        .iter()
        .filter(|(_, s)| s.stubbed > 0)
        .map(|(name, s)| format!("{name}: {} stubbed, {}B", s.stubbed, s.elided_bytes()))
        .collect();
    if parts.is_empty() {
        String::new()
    } else {
        format!(" pruned[{}]", parts.join("; "))
    }
}

/// Map handler-level failures to a 502. We never crash the connection — the
/// caller sees an Anthropic-shaped error envelope.
fn error_response(
    e: anyhow::Error,
) -> Response<http_body_util::combinators::BoxBody<Bytes, BoxBodyError>> {
    eprintln!("[gateway] error: {e:#}");
    anthropic_error(
        StatusCode::BAD_GATEWAY,
        &format!("trimwire gateway error: {e}"),
    )
}

/// Build an Anthropic-shaped JSON error envelope
/// (`{"type":"error","error":{"type":"gateway_error","message":...}}`) so
/// Claude Code's SDK renders it cleanly rather than as opaque text (SPIKE §6).
/// Plain `200 ok` for `GET /healthz` — used by `trimwire status` to confirm
/// the gateway is actually serving (fail-open is live), not just that something
/// holds the port.
fn health_response() -> Response<http_body_util::combinators::BoxBody<Bytes, BoxBodyError>> {
    let boxed = Full::new(Bytes::from_static(b"ok"))
        .map_err(|never: Infallible| match never {})
        .boxed();
    Response::builder()
        .status(StatusCode::OK)
        .header(hyper::header::CONTENT_TYPE, "text/plain")
        .body(boxed)
        .expect("static health response")
}

fn anthropic_error(
    status: StatusCode,
    message: &str,
) -> Response<http_body_util::combinators::BoxBody<Bytes, BoxBodyError>> {
    let body = serde_json::json!({
        "type": "error",
        "error": { "type": "gateway_error", "message": message },
    });
    let bytes = serde_json::to_vec(&body).unwrap_or_else(|_| b"{}".to_vec());
    let boxed = Full::new(Bytes::from(bytes))
        .map_err(|never: Infallible| match never {})
        .boxed();
    Response::builder()
        .status(status)
        .header(hyper::header::CONTENT_TYPE, "application/json")
        .body(boxed)
        .expect("static error response")
}

fn is_hop_by_hop(name: &HeaderName) -> bool {
    let n = name.as_str();
    HOP_BY_HOP.iter().any(|h| n.eq_ignore_ascii_case(h))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state_idle(secs: u64) -> PruneState {
        let mut s = PruneState::default();
        s.set_idle_for_test(secs);
        s
    }

    #[test]
    fn near_payload_limit_fires_only_near_the_wall() {
        assert!(!near_payload_limit(0));
        assert!(
            !near_payload_limit(8 * 1024 * 1024),
            "8 MB is comfortably under"
        );
        assert!(
            !near_payload_limit(PAYLOAD_WARN_BYTES - 1),
            "just below the 80% warn line does not fire"
        );
        assert!(near_payload_limit(PAYLOAD_WARN_BYTES), "at 80% it fires");
        assert!(
            near_payload_limit(PAYLOAD_LIMIT_BYTES),
            "at the wall it fires"
        );
    }

    #[test]
    fn reprune_key_separates_streams_by_model() {
        let sid = "sess-1";
        // Same session, different model (main vs sub-agent/background) → distinct
        // keys, so their PruneStates don't thrash one slot.
        assert_ne!(
            reprune_key(sid, Some("claude-opus-4-8")),
            reprune_key(sid, Some("claude-haiku-4-5")),
        );
        // Same session + same model → same key (stable per stream across turns).
        assert_eq!(
            reprune_key(sid, Some("claude-opus-4-8")),
            reprune_key(sid, Some("claude-opus-4-8")),
        );
        // Different sessions never collide, even with the same model.
        assert_ne!(reprune_key("a", Some("m")), reprune_key("b", Some("m")),);
        // Missing model is handled (no panic; distinct from a present model).
        assert_ne!(reprune_key(sid, None), reprune_key(sid, Some("m")));
    }

    // Idle offsets are kept small (≤ 50 s) so the backdate never approaches the
    // host's monotonic uptime (set_idle_for_test saturates rather than panics,
    // but small offsets keep the ordering exact regardless).
    #[test]
    fn evict_stale_is_a_noop_at_or_under_cap() {
        let cache: DashMap<String, PruneState> = DashMap::new();
        // Two idle-past-TTL entries, but we're under the cap → nothing scanned.
        cache.insert("a".to_owned(), state_idle(50));
        cache.insert("b".to_owned(), state_idle(50));
        evict_stale(&cache, 8, 10);
        assert_eq!(cache.len(), 2, "under cap: idle entries are kept (lazy)");
    }

    #[test]
    fn evict_stale_drops_ttl_expired_when_over_cap() {
        let cache: DashMap<String, PruneState> = DashMap::new();
        for i in 0..3 {
            cache.insert(format!("fresh{i}"), state_idle(0));
        }
        for i in 0..3 {
            cache.insert(format!("stale{i}"), state_idle(50)); // > the 10 s ttl
        }
        // 6 entries, cap 4 → over cap, so the TTL sweep runs and clears the
        // three expired ones, leaving the three fresh (now under cap).
        evict_stale(&cache, 4, 10);
        assert_eq!(cache.len(), 3);
        assert!((0..3).all(|i| cache.contains_key(&format!("fresh{i}"))));
        assert!((0..3).all(|i| !cache.contains_key(&format!("stale{i}"))));
    }

    #[test]
    fn evict_stale_drops_the_most_idle_first_down_to_cap() {
        let cache: DashMap<String, PruneState> = DashMap::new();
        // All within TTL, so the TTL sweep keeps them; the LRU loop must trim
        // to the cap, evicting the most-idle entries first.
        for i in 0..6u64 {
            cache.insert(format!("s{i}"), state_idle(i * 8)); // s5 idlest, s0 freshest
        }
        evict_stale(&cache, 3, 3600);
        assert_eq!(cache.len(), 3);
        // The three freshest survive; the three idlest are gone.
        assert!((0..3).all(|i| cache.contains_key(&format!("s{i}"))));
        assert!((3..6).all(|i| !cache.contains_key(&format!("s{i}"))));
    }
}
