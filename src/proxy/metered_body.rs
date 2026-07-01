//! Response-side instrumentation: a passthrough `Body` wrapper that observes
//! SSE frames to record per-request response metrics without buffering or
//! altering the streamed bytes.
//!
//! # Design
//!
//! `MeteredBody` wraps an upstream `Body` and intercepts each `Frame` as
//! hyper polls it. Every frame is forwarded byte-for-byte; the wrapper only
//! *reads* the bytes transiently to derive metrics:
//!
//! - **TTFT** — the `Instant` when the first data frame arrives minus the
//!   `Instant` captured just before the upstream request was sent.
//! - **Usage tokens** — parsed from `data: {...}` lines in SSE events
//!   `message_start` (input/cache tokens) and `message_delta` (output_tokens).
//! - **applied_edits** — if `context_management.applied_edits` appears in any
//!   SSE event, capture the three count fields only.
//!
//! When the stream ends (either by completion or by `Drop`), the wrapper writes
//! a single ledger row combining the request-side fields that were already known
//! at dispatch time with the response-side metrics just collected.
//!
//! # Fail-safety
//!
//! Every parse/allocation step is wrapped in `Option`/`Result`; on any error
//! the frame is forwarded unchanged and no metric update is made. A bounded
//! 64 KB line buffer prevents unbounded growth. Lines that exceed the cap are
//! dropped from parsing (never silently truncated or stored partially). Panics
//! inside `poll_frame` are not possible because no `unwrap()` is used on the
//! instrumentation path.

use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Instant;

use http_body::SizeHint;
use http_body_util::BodyExt;
use http_body_util::combinators::BoxBody;
use hyper::body::{Body, Bytes, Frame};
use serde_json::Value;

use crate::ledger::{Ledger, Record};
use crate::proxy::proxy_stream::BoxBodyError;

/// Maximum line length we will attempt to JSON-parse. Lines longer than this
/// are forwarded unchanged and produce no metric update. 64 KB is large enough
/// for a `message_start` usage block (always tiny) and a full `message_delta`
/// finish event; it is small enough that we never keep a meaningful chunk of
/// model output text in memory.
const MAX_LINE_BYTES: usize = 64 * 1024;

/// Request-side fields known before the stream starts.
pub struct RequestInfo {
    pub ts: i64,
    pub session_id: Option<String>,
    pub model: Option<String>,
    pub in_bytes: i64,
    pub out_bytes: i64,
    pub strategies: String,
    pub strategy_bytes: String,
    pub prefix_hash_in: String,
    pub prefix_hash_out: String,
    /// HTTP response status code from upstream, captured from the response head
    /// BEFORE the body is consumed. Set at [`MeteredBody::wrap`] construction time
    /// so it is always available when the ledger row is written. 0 is the "not
    /// recorded" sentinel used for non-messages paths (but `MeteredBody` is only
    /// constructed for `/v1/messages`, so 0 here means the status was genuinely 0
    /// or was not set by the caller — normal usage sets it from `status.as_u16()`).
    pub response_status: u16,
    /// `true` when trimwire pruned a valid input into an invalid body and rolled
    /// back to the original bytes this turn (issue #138). Carried from prune time
    /// to the stream-end ledger write, like `response_status`. Recorded to the
    /// `rolled_back` ledger column; surfaced by `invalid_prune_rollbacks`.
    pub rolled_back: bool,
}

/// Accumulated response-side state during streaming.
#[derive(Default)]
struct Metrics {
    first_frame_seen: bool,
    /// Time-to-first-token in microseconds. 0 = not yet recorded.
    ttft_us: i64,
    input_tokens: i64,
    cache_read_input_tokens: i64,
    cache_creation_input_tokens: i64,
    output_tokens: i64,
    applied_edits_cleared_thinking_turns: i64,
    applied_edits_cleared_tool_uses: i64,
    applied_edits_cleared_input_tokens: i64,
}

/// Inner state managed during streaming and finalised on stream-end or `Drop`.
struct Inner {
    /// Accumulated metrics.
    metrics: Metrics,
    /// Partial SSE line being assembled across chunk boundaries. Bounded to
    /// `MAX_LINE_BYTES`; bytes beyond that are silently discarded (the line is
    /// too large to hold useful usage data and we must not grow unbounded).
    line_buf: Vec<u8>,
    /// Flag: we are currently discarding a line that exceeded `MAX_LINE_BYTES`.
    line_overrun: bool,
    /// Monotonic clock at which the upstream request was sent.
    send_instant: Instant,
    /// Request-side fields for the ledger row.
    req: RequestInfo,
    /// The ledger handle (cheaply cloneable Arc).
    ledger: Ledger,
    /// Set to true once the ledger row has been written so `Drop` doesn't
    /// double-write.
    written: bool,
}

impl Inner {
    fn write_ledger(&mut self) {
        if self.written {
            return;
        }
        self.written = true;
        let m = &self.metrics;
        let rec = Record {
            ts: self.req.ts,
            session_id: self.req.session_id.clone(),
            model: self.req.model.clone(),
            in_bytes: self.req.in_bytes,
            out_bytes: self.req.out_bytes,
            strategies: self.req.strategies.clone(),
            strategy_bytes: self.req.strategy_bytes.clone(),
            prefix_hash_in: self.req.prefix_hash_in.clone(),
            prefix_hash_out: self.req.prefix_hash_out.clone(),
            ttft_us: m.ttft_us,
            input_tokens: m.input_tokens,
            cache_read_input_tokens: m.cache_read_input_tokens,
            cache_creation_input_tokens: m.cache_creation_input_tokens,
            output_tokens: m.output_tokens,
            applied_edits_cleared_thinking_turns: m.applied_edits_cleared_thinking_turns,
            applied_edits_cleared_tool_uses: m.applied_edits_cleared_tool_uses,
            applied_edits_cleared_input_tokens: m.applied_edits_cleared_input_tokens,
            response_status: self.req.response_status,
            rolled_back: self.req.rolled_back,
        };
        self.ledger.record(rec);
    }

    /// Observe a newly arrived chunk. Updates metrics from any SSE events
    /// found in the chunk. The chunk bytes are not modified.
    fn observe(&mut self, chunk: &Bytes) {
        // TTFT: record on the first non-empty frame. Store as microseconds so
        // sub-millisecond values are distinguishable from "not recorded" (0).
        // In production real network latency is always ≥ a few µs; in tests the
        // in-memory body is also ≥ 1 µs because Instant has nanosecond resolution.
        if !chunk.is_empty() && !self.metrics.first_frame_seen {
            self.metrics.first_frame_seen = true;
            let elapsed = self.send_instant.elapsed();
            // as_micros() returns u128; clamp to i64::MAX on overflow (impossible
            // in practice: ~292 000 years).
            // .max(1): a truly sub-microsecond first frame is impossible on any real
            // network; in tests the in-memory body takes ~nanoseconds — .max(1)
            // ensures ttft_us > 0 (our "recorded" sentinel) without distorting
            // production measurements (real TTFT is always several hundred µs+).
            let us: i64 = elapsed.as_micros().try_into().unwrap_or(i64::MAX);
            self.metrics.ttft_us = us.max(1);
        }

        // Scan the chunk byte-by-byte, reassembling SSE `data:` lines that
        // may span chunk boundaries.
        for &byte in chunk.iter() {
            if byte == b'\n' {
                if !self.line_overrun {
                    // Line complete — swap out the buffer and process it. Replace
                    // with a small pre-sized buffer (not Vec::new()) so the next
                    // line doesn't re-grow from capacity 0 on the hot path.
                    let line = std::mem::replace(&mut self.line_buf, Vec::with_capacity(256));
                    self.process_line(&line);
                } else {
                    // The previous overrun is over; resume normal operation.
                    self.line_buf.clear();
                    self.line_overrun = false;
                }
            } else if self.line_overrun {
                // Still discarding an overrun line — skip.
            } else if self.line_buf.len() < MAX_LINE_BYTES {
                self.line_buf.push(byte);
            } else {
                // Line exceeded the cap — enter overrun mode and drain.
                self.line_buf.clear();
                self.line_overrun = true;
            }
        }
        // Incomplete final line remains in `line_buf` for the next chunk.
    }

    /// Try to parse a complete SSE line that might contain usage data.
    /// On any parse failure, returns silently — fail-safe.
    fn process_line(&mut self, line: &[u8]) {
        // Real Anthropic HTTPS responses terminate SSE lines with CRLF (`\r\n`);
        // we split on `\n`, so a trailing `\r` is left on the line. serde_json
        // rejects a trailing `\r` (not JSON whitespace), which silently failed
        // EVERY usage parse on the wire (TTFT survived, tokens came back 0).
        // Strip it so CRLF and bare-LF lines parse identically.
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        // SSE data lines look like: `data: {...}` or `data:{...}`
        let Some(rest) = line
            .strip_prefix(b"data: ")
            .or_else(|| line.strip_prefix(b"data:"))
        else {
            return;
        };

        // Skip large payloads: they are model output text (content_block_delta),
        // not usage metadata. The actual MAX_LINE_BYTES guard above means any
        // line that arrives here is already ≤ 64 KB, but even within that,
        // we further skip anything over 4 KB since usage events are tiny.
        if rest.len() > 4096 {
            return;
        }

        // Fast-path: skip [DONE] terminator.
        if rest == b"[DONE]" {
            return;
        }

        let Ok(json) = serde_json::from_slice::<Value>(rest) else {
            return;
        };

        let event_type = json.get("type").and_then(Value::as_str).unwrap_or("");

        match event_type {
            "message_start" => {
                // {"type":"message_start","message":{"usage":{
                //   "input_tokens":N, "cache_read_input_tokens":N,
                //   "cache_creation_input_tokens":N}}}
                if let Some(usage) = json
                    .get("message")
                    .and_then(|m| m.get("usage"))
                    .and_then(Value::as_object)
                {
                    self.metrics.input_tokens = usage
                        .get("input_tokens")
                        .and_then(Value::as_i64)
                        .unwrap_or(0);
                    self.metrics.cache_read_input_tokens = usage
                        .get("cache_read_input_tokens")
                        .and_then(Value::as_i64)
                        .unwrap_or(0);
                    self.metrics.cache_creation_input_tokens = usage
                        .get("cache_creation_input_tokens")
                        .and_then(Value::as_i64)
                        .unwrap_or(0);
                }
            }
            "message_delta" => {
                // {"type":"message_delta","usage":{"output_tokens":N}}
                // Also check for context_management.applied_edits (can appear
                // on the finish event alongside the usage summary).
                if let Some(usage) = json.get("usage").and_then(Value::as_object) {
                    self.metrics.output_tokens = usage
                        .get("output_tokens")
                        .and_then(Value::as_i64)
                        .unwrap_or(self.metrics.output_tokens);
                }
                self.try_extract_applied_edits(&json);
            }
            // Anthropic also places context_management on dedicated event types
            // (observed but not guaranteed). Catch any event that carries the field.
            _ => {
                self.try_extract_applied_edits(&json);
            }
        }
    }

    /// Look for `context_management.applied_edits` in any JSON value and
    /// capture the three count fields. Counts only — never content.
    fn try_extract_applied_edits(&mut self, json: &Value) {
        let Some(ae) = json
            .get("context_management")
            .and_then(|cm| cm.get("applied_edits"))
            .and_then(Value::as_object)
        else {
            return;
        };
        if let Some(v) = ae.get("cleared_thinking_turns").and_then(Value::as_i64) {
            self.metrics.applied_edits_cleared_thinking_turns = v;
        }
        if let Some(v) = ae.get("cleared_tool_uses").and_then(Value::as_i64) {
            self.metrics.applied_edits_cleared_tool_uses = v;
        }
        if let Some(v) = ae.get("cleared_input_tokens").and_then(Value::as_i64) {
            self.metrics.applied_edits_cleared_input_tokens = v;
        }
    }
}

/// A `Body` wrapper that observes SSE frames for instrumentation and fires a
/// ledger write on stream-end or `Drop`. Bytes are forwarded unchanged.
///
/// Created via [`MeteredBody::wrap`]; returned as a `BoxBody` so the concrete
/// type is erased and the downstream response builder stays unchanged.
pub struct MeteredBody {
    inner: Box<Inner>,
    /// The upstream body, pinned on the heap so we can poll it from our own
    /// `poll_frame` without needing `unsafe`.
    upstream: Pin<Box<BoxBody<Bytes, BoxBodyError>>>,
}

impl MeteredBody {
    /// Wrap an upstream body. Consumes `send_instant` (captured just before the
    /// upstream request was sent) and `req` (the request-side ledger fields
    /// known at dispatch time). The ledger write fires when the stream is
    /// exhausted or dropped.
    pub fn wrap<B>(
        upstream: B,
        send_instant: Instant,
        req: RequestInfo,
        ledger: Ledger,
    ) -> BoxBody<Bytes, BoxBodyError>
    where
        B: Body<Data = Bytes> + Send + Sync + 'static,
        B::Error: Into<BoxBodyError>,
    {
        let inner = Box::new(Inner {
            metrics: Metrics::default(),
            line_buf: Vec::new(),
            line_overrun: false,
            send_instant,
            req,
            ledger,
            written: false,
        });
        let body = Self {
            inner,
            upstream: Box::pin(upstream.map_err(Into::into).boxed()),
        };
        body.boxed()
    }
}

impl Drop for MeteredBody {
    fn drop(&mut self) {
        // Fire the ledger write if the stream ended early (client disconnect,
        // runtime cancellation, or an upstream error that already wrote the row).
        // `write_ledger` is idempotent — the `written` flag prevents double-writes.
        self.inner.write_ledger();
    }
}

impl Body for MeteredBody {
    type Data = Bytes;
    type Error = BoxBodyError;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        match self.upstream.as_mut().poll_frame(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(None) => {
                // Stream exhausted normally — write the ledger row.
                self.inner.write_ledger();
                Poll::Ready(None)
            }
            Poll::Ready(Some(Err(e))) => {
                // Upstream error — write what we have, then forward the error.
                self.inner.write_ledger();
                Poll::Ready(Some(Err(e)))
            }
            Poll::Ready(Some(Ok(frame))) => {
                // Observe data frames (not trailers). Bytes are forwarded
                // unchanged regardless of any parse outcome.
                if let Some(data) = frame.data_ref() {
                    self.inner.observe(data);
                }
                Poll::Ready(Some(Ok(frame)))
            }
        }
    }

    fn is_end_stream(&self) -> bool {
        self.upstream.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        self.upstream.size_hint()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::pin::Pin;
    use std::task::{Context, Poll};
    use std::time::Instant;

    use http_body::SizeHint;
    use http_body_util::BodyExt;
    use http_body_util::combinators::BoxBody;
    use hyper::body::{Body, Bytes, Frame};

    use crate::ledger::Ledger;
    use crate::proxy::proxy_stream::BoxBodyError;

    use super::{MeteredBody, RequestInfo};

    // -----------------------------------------------------------------------
    // Helper: a simple Body that yields pre-loaded chunks from a VecDeque.
    // Yields one chunk per poll; returns Pending if all chunks are exhausted
    // but the queue is not marked done. For tests we always mark done after
    // the last chunk by dropping the producer.
    // -----------------------------------------------------------------------

    struct ChunkBody {
        chunks: VecDeque<Bytes>,
    }

    impl ChunkBody {
        fn new(chunks: impl IntoIterator<Item = &'static [u8]>) -> Self {
            Self {
                chunks: chunks.into_iter().map(Bytes::from_static).collect(),
            }
        }
    }

    impl Body for ChunkBody {
        type Data = Bytes;
        type Error = BoxBodyError;

        fn poll_frame(
            mut self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
        ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
            match self.chunks.pop_front() {
                Some(b) => Poll::Ready(Some(Ok(Frame::data(b)))),
                None => Poll::Ready(None),
            }
        }

        fn is_end_stream(&self) -> bool {
            self.chunks.is_empty()
        }

        fn size_hint(&self) -> SizeHint {
            SizeHint::default()
        }
    }

    /// A Body that yields one chunk then hangs (simulates a half-open stream).
    struct HalfOpenBody {
        chunk: Option<Bytes>,
    }

    impl HalfOpenBody {
        fn new(chunk: &'static [u8]) -> Self {
            Self {
                chunk: Some(Bytes::from_static(chunk)),
            }
        }
    }

    impl Body for HalfOpenBody {
        type Data = Bytes;
        type Error = BoxBodyError;

        fn poll_frame(
            mut self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
        ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
            if let Some(b) = self.chunk.take() {
                Poll::Ready(Some(Ok(Frame::data(b))))
            } else {
                // Hang forever — simulates client reading one frame then disconnecting.
                Poll::Pending
            }
        }

        fn is_end_stream(&self) -> bool {
            false
        }

        fn size_hint(&self) -> SizeHint {
            SizeHint::default()
        }
    }

    /// Collect all bytes from a `BoxBody`.
    async fn collect_body(body: BoxBody<Bytes, BoxBodyError>) -> Vec<u8> {
        body.collect()
            .await
            .expect("collect body")
            .to_bytes()
            .to_vec()
    }

    /// Convenience: build a minimal `RequestInfo` for tests.
    fn req_info() -> RequestInfo {
        RequestInfo {
            ts: 0,
            session_id: None,
            model: None,
            in_bytes: 0,
            out_bytes: 0,
            strategies: String::new(),
            strategy_bytes: String::new(),
            prefix_hash_in: "h".to_owned(),
            prefix_hash_out: "h".to_owned(),
            response_status: 0,
            rolled_back: false,
        }
    }

    // -----------------------------------------------------------------------
    // Byte-identity: forwarded bytes must be identical to the input.
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn forwarded_bytes_are_identical() {
        let raw: &[u8] = b"data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":10,\"cache_read_input_tokens\":2,\"cache_creation_input_tokens\":1}}}\n\ndata: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"hello\"}}\n\ndata: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":5}}\n\ndata: [DONE]\n";

        let upstream = ChunkBody::new([raw]);
        let wrapped = MeteredBody::wrap(upstream, Instant::now(), req_info(), Ledger::disabled());
        let got = collect_body(wrapped).await;
        assert_eq!(got, raw, "forwarded bytes must be byte-identical to input");
    }

    // -----------------------------------------------------------------------
    // SSE parsing: correct metrics extracted from a well-formed stream.
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn sse_metrics_parsed_correctly() {
        // message_start with all three token types.
        let start: &[u8] = b"data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":1000,\"cache_read_input_tokens\":400,\"cache_creation_input_tokens\":100}}}\n\n";
        // content_block_delta frames (text — should be skipped/ignored).
        let delta1: &[u8] = b"data: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"hello world\"}}\n\n";
        let delta2: &[u8] = b"data: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\" more\"}}\n\n";
        // message_delta with output_tokens + applied_edits.
        let finish: &[u8] = b"data: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":250},\"context_management\":{\"applied_edits\":{\"cleared_thinking_turns\":2,\"cleared_tool_uses\":5,\"cleared_input_tokens\":3000}}}\n\n";
        let done: &[u8] = b"data: [DONE]\n";

        let upstream = ChunkBody::new([start, delta1, delta2, finish, done]);

        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("l.db");
        let ledger = Ledger::open(db_path.to_str().unwrap(), 365);

        let wrapped = MeteredBody::wrap(upstream, Instant::now(), req_info(), ledger);
        collect_body(wrapped).await;
        // Give spawn_blocking a moment.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let report = Ledger::report(db_path.to_str().unwrap()).unwrap();
        let rm = &report.response_metrics;

        assert_eq!(report.total_requests, 1);
        assert_eq!(rm.requests_with_ttft, 1, "TTFT should be recorded");
        assert!(rm.avg_ttft_us >= 0.0, "TTFT is non-negative");
        assert_eq!(rm.total_input_tokens, 1000);
        assert_eq!(rm.total_cache_read_input_tokens, 400);
        assert_eq!(rm.total_cache_creation_input_tokens, 100);
        assert_eq!(rm.total_output_tokens, 250);
        assert_eq!(rm.requests_with_applied_edits, 1);
        assert_eq!(rm.total_applied_edits_cleared_thinking_turns, 2);
        assert_eq!(rm.total_applied_edits_cleared_tool_uses, 5);
        assert_eq!(rm.total_applied_edits_cleared_input_tokens, 3000);
    }

    // -----------------------------------------------------------------------
    // REAL wire format: CRLF line endings + `event:` lines. Regression test for
    // the bug where a trailing `\r` made serde_json reject every usage line
    // (TTFT survived, tokens came back 0 on real Anthropic SSE).
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn sse_crlf_with_event_lines_parses_usage() {
        // Two-line SSE events (`event:` then `data:`) terminated with CRLF, as
        // Anthropic actually sends them over HTTPS.
        let start: &[u8] = b"event: message_start\r\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":1000,\"cache_read_input_tokens\":400,\"cache_creation_input_tokens\":100}}}\r\n\r\n";
        let delta: &[u8] = b"event: content_block_delta\r\ndata: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"hi\"}}\r\n\r\n";
        let finish: &[u8] = b"event: message_delta\r\ndata: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":250}}\r\n\r\n";

        let upstream = ChunkBody::new([start, delta, finish]);
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("l.db");
        let ledger = Ledger::open(db_path.to_str().unwrap(), 365);

        let wrapped = MeteredBody::wrap(upstream, Instant::now(), req_info(), ledger);
        collect_body(wrapped).await;
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let rm = Ledger::report(db_path.to_str().unwrap())
            .unwrap()
            .response_metrics;
        assert_eq!(rm.total_input_tokens, 1000, "CRLF input_tokens must parse");
        assert_eq!(rm.total_cache_read_input_tokens, 400);
        assert_eq!(rm.total_cache_creation_input_tokens, 100);
        assert_eq!(rm.total_output_tokens, 250, "CRLF output_tokens must parse");
    }

    // -----------------------------------------------------------------------
    // Split frames: SSE lines split across chunk boundaries.
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn sse_lines_split_across_chunks_parsed_correctly() {
        // The message_start line split into three arbitrary chunks mid-line.
        let full: &[u8] = b"data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":777,\"cache_read_input_tokens\":0,\"cache_creation_input_tokens\":0}}}\n\n";
        let (a, rest) = full.split_at(30);
        let (b_slice, c) = rest.split_at(30.min(rest.len()));

        // We need 'static slices for ChunkBody — box and leak for tests.
        let a_static: &'static [u8] = Box::leak(a.to_vec().into_boxed_slice());
        let b_static: &'static [u8] = Box::leak(b_slice.to_vec().into_boxed_slice());
        let c_static: &'static [u8] = Box::leak(c.to_vec().into_boxed_slice());

        let upstream = ChunkBody::new([a_static, b_static, c_static]);

        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("l2.db");
        let ledger = Ledger::open(db_path.to_str().unwrap(), 365);

        let wrapped = MeteredBody::wrap(upstream, Instant::now(), req_info(), ledger);
        collect_body(wrapped).await;
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let report = Ledger::report(db_path.to_str().unwrap()).unwrap();
        assert_eq!(
            report.response_metrics.total_input_tokens, 777,
            "split-line reassembly must parse correctly"
        );
    }

    // -----------------------------------------------------------------------
    // Fail-safe: malformed / garbage frames forwarded unchanged, no panic.
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn malformed_frames_forwarded_unchanged_no_panic() {
        let garbage: &[u8] =
            b"data: not json at all\ndata: {broken\nsome random bytes\x00\xff\ndata: [DONE]\n";

        let upstream = ChunkBody::new([garbage]);
        let wrapped = MeteredBody::wrap(upstream, Instant::now(), req_info(), Ledger::disabled());
        let got = collect_body(wrapped).await;
        assert_eq!(
            got, garbage,
            "malformed frames must be forwarded byte-for-byte"
        );
        // No panic occurred (if we reach here, fail-safety holds).
    }

    // -----------------------------------------------------------------------
    // Oversized line: lines exceeding MAX_LINE_BYTES are skipped for parsing
    // but the bytes are still forwarded intact.
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn oversized_line_bytes_forwarded_no_panic() {
        // Construct a line that is MAX_LINE_BYTES + 1 long.
        let mut big_line = b"data: ".to_vec();
        big_line.extend(std::iter::repeat_n(b'x', super::MAX_LINE_BYTES + 1));
        big_line.push(b'\n');
        let big_clone = big_line.clone();

        // Leak for 'static lifetime required by ChunkBody.
        let leaked: &'static [u8] = Box::leak(big_line.into_boxed_slice());

        let upstream = ChunkBody::new([leaked]);
        let wrapped = MeteredBody::wrap(upstream, Instant::now(), req_info(), Ledger::disabled());
        let got = collect_body(wrapped).await;
        assert_eq!(
            got, big_clone,
            "oversized line bytes must still be forwarded"
        );
    }

    // -----------------------------------------------------------------------
    // Non-SSE response: TTFT recorded, usage stays zero — degrade gracefully.
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn ttft_is_positive_for_non_sse_response() {
        let body_bytes: &[u8] =
            b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\r\n{\"id\":\"msg_01\"}";

        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("l3.db");
        let ledger = Ledger::open(db_path.to_str().unwrap(), 365);

        let upstream = ChunkBody::new([body_bytes]);
        let wrapped = MeteredBody::wrap(upstream, Instant::now(), req_info(), ledger);
        collect_body(wrapped).await;
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let report = Ledger::report(db_path.to_str().unwrap()).unwrap();
        assert_eq!(
            report.response_metrics.requests_with_ttft, 1,
            "TTFT should be recorded for any non-empty response"
        );
        assert_eq!(report.response_metrics.total_input_tokens, 0);
        assert_eq!(report.response_metrics.total_output_tokens, 0);
    }

    // -----------------------------------------------------------------------
    // Drop-fires: Drop mid-stream still writes a ledger row.
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn drop_fires_ledger_write() {
        // A body that yields one chunk then hangs (half-open).
        let chunk: &[u8] = b"data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":42,\"cache_read_input_tokens\":0,\"cache_creation_input_tokens\":0}}}\n";
        let upstream = HalfOpenBody::new(chunk);

        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("l4.db");
        let ledger = Ledger::open(db_path.to_str().unwrap(), 365);

        let wrapped = MeteredBody::wrap(upstream, Instant::now(), req_info(), ledger);

        // Manually pin and poll one frame, then drop.
        let mut body = Box::pin(wrapped);
        let first = std::future::poll_fn(|cx| body.as_mut().poll_frame(cx)).await;
        assert!(first.is_some(), "first frame should arrive");
        // Drop the body — should fire the ledger write via Drop.
        drop(body);

        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        let report = Ledger::report(db_path.to_str().unwrap()).unwrap();
        assert_eq!(
            report.total_requests, 1,
            "Drop-path must still write a ledger row"
        );
        assert_eq!(
            report.response_metrics.total_input_tokens, 42,
            "input_tokens from the one observed frame"
        );
    }
}
