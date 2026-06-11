//! Outbound HTTPS client to api.anthropic.com (or `[server].upstream`).
//!
//! Uses hyper-rustls with webpki-roots for cert validation. HTTP/2 enabled.
//! Connection pool is hyper's default (keep-alive per upstream host).
//!
//! In hyper 1.x the high-level pooled client lives in `hyper-util`'s
//! `client::legacy::Client`. Combined with `hyper-rustls`'s
//! `HttpsConnectorBuilder` this gives us a single shared `Client<C, B>`
//! that handles TLS, HTTP/2 negotiation, and keep-alive pooling.

use http_body_util::Full;
use hyper::body::Bytes;
use hyper_rustls::HttpsConnector;
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::TokioExecutor;

/// Shared pooled HTTPS client. Cheap to clone (internal `Arc`).
pub type UpstreamClient = Client<HttpsConnector<HttpConnector>, Full<Bytes>>;

/// Build the shared client. Call once at startup; clone the resulting handle
/// into each request task.
///
/// Configuration:
/// - Roots: `webpki-roots` (Mozilla bundle, no system store, no OpenSSL).
/// - Protocols: HTTP/1.1 and HTTP/2 via ALPN. Anthropic negotiates h2.
/// - Pool: hyper-util defaults (per-host keep-alive, idle eviction).
pub fn build_client() -> UpstreamClient {
    // Install the default crypto provider for rustls 0.23. Without this,
    // `HttpsConnectorBuilder::with_webpki_roots()` panics on first use.
    // We ignore the error: another module may have installed it first.
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();

    let https = hyper_rustls::HttpsConnectorBuilder::new()
        .with_webpki_roots()
        .https_or_http()
        .enable_http1()
        .enable_http2()
        .build();

    Client::builder(TokioExecutor::new()).build(https)
}
