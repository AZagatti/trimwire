//! Error types. Internal errors are `thiserror`; the bin's main() uses `anyhow`.
//!
//! HTTP-code mapping convention (SPIKE.md §6 "Error handling"):
//! - Mutation crash → log + roll back, forward 200 with the original body.
//! - Malformed JSON request body → forward verbatim to upstream (never 400 —
//!   the gateway doesn't validate the body, it just declines to mutate it).
//! - Request body > 32 MB → 413; upstream connect failure → 502; upstream
//!   header timeout → 504.
//! - Gateway-fatal (port bind, DNS) → exit non-zero at startup, never mid-request.
//!
//! Every variant is constructed somewhere — an unused one would trip
//! `clippy -D warnings`.

use thiserror::Error;

/// Internal failures across the pruning pipeline.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum Error {
    /// A `tool_result.tool_use_id` has no matching `tool_use.id` in the
    /// request. Pre-mutation this means the input was already broken (we
    /// forward unmutated); post-mutation it means a strategy is buggy and the
    /// gateway must roll back to the original body. See SPIKE.md §5.
    #[error("orphaned tool_result: tool_use_id {0:?} has no matching tool_use")]
    OrphanResult(String),
}

/// Crate-internal result alias.
pub type Result<T> = std::result::Result<T, Error>;
