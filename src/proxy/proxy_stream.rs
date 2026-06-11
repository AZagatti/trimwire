//! Server-Sent Events passthrough for the response body.
//!
//! Anthropic's `/v1/messages` returns `text/event-stream` for streamed
//! responses. We MUST NOT buffer — pipe bytes from upstream's `Body` stream
//! straight into the downstream response writer.
//!
//! Do not re-encode, do not parse, do not modify. Bytes in = bytes out.
//!
//! In hyper 1.x, request/response bodies are anything that implements
//! `http_body::Body`. The cheapest way to forward bytes is to wrap the
//! upstream body in `BoxBody` and hand it to the outbound response — hyper
//! polls it lazily, frame-by-frame, with no intermediate copy.

use http_body_util::BodyExt;
use http_body_util::combinators::BoxBody;
use hyper::body::{Body, Bytes};

/// Error type erased into `Box<dyn std::error::Error + Send + Sync>` so the
/// downstream and upstream body error types can be unified behind one
/// `BoxBody`.
pub type BoxBodyError = Box<dyn std::error::Error + Send + Sync>;

/// Wrap an arbitrary `Body<Data = Bytes>` into the `BoxBody` alias used by
/// the outbound response. Erases the concrete body type and the error type
/// without copying any chunks.
pub fn passthrough<B>(body: B) -> BoxBody<Bytes, BoxBodyError>
where
    B: Body<Data = Bytes> + Send + Sync + 'static,
    B::Error: Into<BoxBodyError>,
{
    body.map_err(Into::into).boxed()
}
