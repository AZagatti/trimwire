//! trimwire — Claude Code Dynamic Context Pruning.
//!
//! A local HTTP gateway that mutates Claude Code's outbound `/v1/messages`
//! requests to strip image payloads and elide stale tool calls, then forwards
//! to `api.anthropic.com`. Anthropic-sanctioned mechanism via
//! `ANTHROPIC_BASE_URL`. No CA cert install, no restart.
//!
//! See `ARCHITECTURE.md` for the design, `SPIKE.md` for the rationale.

pub mod config;
pub mod error;
pub mod ledger;
pub mod pairing;
pub mod proxy;
pub mod reprune;
pub mod strategies;
/// Opt-in summarizer context compaction. Always compiled; disabled unless
/// `[summarizer] engine` is not `"model-free"` in config. See `docs/SUMMARIZER.md`.
pub mod summarizer;
pub mod sweep;
