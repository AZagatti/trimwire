//! HTTP transport layer: the inbound server, outbound client, and response
//! pipe. `gateway` owns the request lifecycle; `upstream` builds the pooled
//! TLS client; `proxy_stream` streams the response body back unbuffered.
//! `metered_body` wraps the upstream body to observe SSE events and write
//! response-side metrics to the ledger without buffering or altering bytes.
pub mod audit;
pub mod gateway;
pub mod listener;
pub mod metered_body;
pub mod proxy_stream;
pub mod upstream;
