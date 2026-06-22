//! Opt-in, **metadata-only** wire audit (`TRIMWIRE_AUDIT=<file>`).
//!
//! Records one JSONL line per `/v1/messages` request describing the *shape* of
//! the inbound body — counts, sizes, flags, the model id, the `anthropic-beta`
//! header, and the opaque `x-claude-code-session-id` — **never any message
//! content, tool input, or tool result text**. It exists to see what Claude
//! Code's own native context handling does on the wire (does it clear/offload
//! big tool results before sending? what session-id do sub-agents carry? how
//! much content is array-shaped?), and to let a user audit their own traffic.
//!
//! In addition to those counts it records the **cache-prefix structure** needed
//! to investigate B-CACHE work: the ordered list of tool *definition* names,
//! which `tools[]`/`system`/`messages[0]` blocks carry a `cache_control`
//! breakpoint, and the block-type sequence of `messages[0]` (including whether a
//! `<system-reminder>` / skill listing sits there and at what position). These
//! are *structural identifiers* drawn from the request schema (tool names, block
//! `type` labels, byte sizes, position indices, presence flags) — **never the
//! text of any message, tool input, tool result, system prompt, or skill body.**
//! Tool names come from the tool-definition schema, not from message content.
//!
//! It is a no-op — and does not even parse the body — unless the sink is
//! configured, so it never touches the hot path when off.

use std::fs::OpenOptions;
use std::io::{BufWriter, Write};
use std::sync::{Arc, Mutex};

use serde::Serialize;
use serde_json::Value;

/// Claude Code's cold-path microcompact placeholder (client-side; it appears on
/// the wire in `tool_result.content` when CC clears a stale result itself).
const CLEARED_MARKER: &str = "Old tool result content cleared";
/// Threshold for "large" tool results (matches the benchmark's notion of big).
const LARGE_RESULT_BYTES: usize = 20_480;
/// Marker that identifies an injected `<system-reminder>` text block. Detected by
/// substring presence only — the surrounding text is never recorded.
const SYSTEM_REMINDER_MARKER: &str = "<system-reminder>";
/// Markers that identify the skill-listing reminder (the B-CACHE-1 target). Any
/// hit flags the block as a skill listing; the text itself is never recorded.
const SKILL_LISTING_MARKERS: [&str; 2] = ["available-skills", "available skills"];

/// Structural shape of a single `messages[0]` content block. Every field is a
/// type label, byte size, position, or presence flag derived transiently from
/// the block — **never the block's text.** Used to study where Claude Code puts
/// the skill-listing `<system-reminder>` and `cache_control` breakpoints across
/// fresh vs `--resume`d turns (B-CACHE-1).
#[derive(Serialize)]
pub struct BlockShape {
    /// The block `type` (e.g. `"text"`, `"tool_result"`, `"image"`, `"thinking"`).
    pub kind: String,
    /// Serialized byte length of the block (size only, never the bytes).
    pub bytes: usize,
    /// Whether this block carries a `cache_control` breakpoint.
    pub cache_control: bool,
    /// Whether a text block contains a `<system-reminder>` marker.
    pub system_reminder: bool,
    /// Whether a text block looks like the skill-listing reminder.
    pub skill_listing: bool,
}

/// Per-request metadata. Every field is a count, byte size, flag, hash, model
/// id, header value, or opaque session UUID — by construction it cannot carry
/// message content. (No field is, or is derived by copying, body text.)
#[derive(Serialize)]
pub struct Capture {
    pub ts: i64,
    pub session_id: Option<String>,
    pub is_sidechain: Option<bool>,
    pub anthropic_beta: Option<String>,
    pub model: Option<String>,
    pub total_body_bytes: usize,
    pub num_messages: usize,
    pub num_tool_result_blocks: usize,
    pub num_string_content: usize,
    pub num_array_content: usize,
    pub num_results_over_20kb: usize,
    pub num_cleared_markers: usize,
    pub num_offload_refs: usize,
    pub num_thinking_blocks: usize,
    pub thinking_bytes: usize,
    pub largest_result_bytes: usize,
    pub prefix_hash_in: String,
    // --- cache-prefix structure (B-CACHE) ---------------------------------
    /// Ordered tool *definition* names from `tools[]` (schema identifiers, not
    /// message content). Empty when the request carries no tools. Comparing this
    /// across turns answers "does Claude Code reorder tools between requests?".
    ///
    /// Privacy note (audit P3-2): these are the client's tool-definition names,
    /// so for MCP servers they take the form `mcp__<server>__<tool>` and can
    /// reveal configured MCP server names (e.g. internal service names). The
    /// audit log is opt-in and local-only (and now owner-only `0600` on Unix),
    /// but treat the file as you would your MCP config.
    pub tool_names: Vec<String>,
    /// Indices into `tool_names` whose tool definition carries a `cache_control`
    /// breakpoint (usually just the last one).
    pub tool_cache_control_idx: Vec<usize>,
    /// Shape of the top-level `system` field: `"absent"`, `"string"`, or `"array"`.
    pub system_kind: &'static str,
    /// Indices of `system[]` blocks carrying `cache_control` (array system only).
    pub system_cache_control_idx: Vec<usize>,
    /// Role of `messages[0]` (e.g. `"user"`), if present.
    pub first_msg_role: Option<String>,
    /// Structural shape of each `messages[0]` content block, in order. Empty when
    /// `messages[0].content` is a plain string rather than a block array.
    pub first_msg_blocks: Vec<BlockShape>,
}

/// Build a metadata `Capture` from a raw `/v1/messages` body. Returns `None` if
/// the body is not the JSON `{messages:[...]}` shape we audit. Content is read
/// transiently to derive counts/flags and is **never retained** in the result.
pub fn capture(
    body: &[u8],
    session_id: Option<&str>,
    anthropic_beta: Option<&str>,
    prefix_hash_in: String,
    ts: i64,
) -> Option<Capture> {
    let root: Value = serde_json::from_slice(body).ok()?;
    let messages = root.get("messages")?.as_array()?;

    let mut c = Capture {
        ts,
        session_id: session_id.map(str::to_owned),
        is_sidechain: root.get("isSidechain").and_then(Value::as_bool),
        anthropic_beta: anthropic_beta.map(str::to_owned),
        model: root.get("model").and_then(Value::as_str).map(str::to_owned),
        total_body_bytes: body.len(),
        num_messages: messages.len(),
        num_tool_result_blocks: 0,
        num_string_content: 0,
        num_array_content: 0,
        num_results_over_20kb: 0,
        num_cleared_markers: 0,
        num_offload_refs: 0,
        num_thinking_blocks: 0,
        thinking_bytes: 0,
        largest_result_bytes: 0,
        prefix_hash_in,
        tool_names: tool_names(&root),
        tool_cache_control_idx: tool_cache_control_idx(&root),
        system_kind: system_kind(&root),
        system_cache_control_idx: system_cache_control_idx(&root),
        first_msg_role: messages
            .first()
            .and_then(|m| m.get("role"))
            .and_then(Value::as_str)
            .map(str::to_owned),
        first_msg_blocks: first_msg_blocks(messages),
    };

    for m in messages {
        let Some(content) = m.get("content").and_then(Value::as_array) else {
            continue;
        };
        for block in content {
            let btype = block.get("type").and_then(Value::as_str);
            // Count thinking blocks (size only, never the text) so we can measure
            // how much OLD reasoning Claude Code actually sends back on the wire —
            // the question of whether thinking is a prunable lever or already stripped.
            if btype == Some("thinking") || btype == Some("redacted_thinking") {
                c.num_thinking_blocks += 1;
                c.thinking_bytes += serde_json::to_string(block).map(|s| s.len()).unwrap_or(0);
                continue;
            }
            if btype != Some("tool_result") {
                continue;
            }
            c.num_tool_result_blocks += 1;
            // Scan the result content transiently for size + signals; keep only
            // the derived numbers, never the text.
            let (len, is_str, is_arr, cleared, offload) = match block.get("content") {
                Some(Value::String(s)) => (
                    s.len() + 2, // + quotes, ~ serialized length
                    true,
                    false,
                    s.contains(CLEARED_MARKER),
                    s.contains("tool-results/") || s.contains("Preview (first"),
                ),
                Some(v @ Value::Array(_)) => {
                    let ser = serde_json::to_string(v).unwrap_or_default();
                    let cl = ser.contains(CLEARED_MARKER);
                    let off = ser.contains("tool-results/") || ser.contains("Preview (first");
                    (ser.len(), false, true, cl, off)
                }
                _ => (0, false, false, false, false),
            };
            c.largest_result_bytes = c.largest_result_bytes.max(len);
            if len > LARGE_RESULT_BYTES {
                c.num_results_over_20kb += 1;
            }
            if is_str {
                c.num_string_content += 1;
            }
            if is_arr {
                c.num_array_content += 1;
            }
            if cleared {
                c.num_cleared_markers += 1;
            }
            if offload {
                c.num_offload_refs += 1;
            }
        }
    }
    Some(c)
}

/// Whether a JSON object carries a (non-null) `cache_control` field.
fn has_cache_control(v: &Value) -> bool {
    v.get("cache_control").is_some_and(|c| !c.is_null())
}

/// Ordered `tools[].name` list. Names come from the tool-definition schema, never
/// from message content. A tool with no string `name` records an empty string so
/// positions stay aligned with [`tool_cache_control_idx`].
fn tool_names(root: &Value) -> Vec<String> {
    root.get("tools")
        .and_then(Value::as_array)
        .map(|tools| {
            tools
                .iter()
                .map(|t| {
                    t.get("name")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned()
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Indices into `tools[]` whose definition carries a `cache_control` breakpoint.
fn tool_cache_control_idx(root: &Value) -> Vec<usize> {
    root.get("tools")
        .and_then(Value::as_array)
        .map(|tools| {
            tools
                .iter()
                .enumerate()
                .filter(|(_, t)| has_cache_control(t))
                .map(|(i, _)| i)
                .collect()
        })
        .unwrap_or_default()
}

/// Shape of the top-level `system` field.
fn system_kind(root: &Value) -> &'static str {
    match root.get("system") {
        None | Some(Value::Null) => "absent",
        Some(Value::String(_)) => "string",
        Some(Value::Array(_)) => "array",
        Some(_) => "other",
    }
}

/// Indices of `system[]` blocks carrying a `cache_control` breakpoint (array
/// system only; empty for string/absent system).
fn system_cache_control_idx(root: &Value) -> Vec<usize> {
    root.get("system")
        .and_then(Value::as_array)
        .map(|blocks| {
            blocks
                .iter()
                .enumerate()
                .filter(|(_, b)| has_cache_control(b))
                .map(|(i, _)| i)
                .collect()
        })
        .unwrap_or_default()
}

/// Structural shape of each `messages[0]` content block, in order. Reads the
/// block `type`, size, and presence flags transiently; the text is never kept.
fn first_msg_blocks(messages: &[Value]) -> Vec<BlockShape> {
    let Some(content) = messages.first().and_then(|m| m.get("content")) else {
        return Vec::new();
    };
    let Some(blocks) = content.as_array() else {
        // `content` is a plain string (or other) — no block array to describe.
        return Vec::new();
    };
    blocks
        .iter()
        .map(|block| {
            let kind = block
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or("unknown")
                .to_owned();
            let bytes = serde_json::to_string(block).map(|s| s.len()).unwrap_or(0);
            // Text-marker detection by substring presence only — the text itself
            // is never recorded, only the booleans below.
            let text = block.get("text").and_then(Value::as_str).unwrap_or("");
            let system_reminder = text.contains(SYSTEM_REMINDER_MARKER);
            let skill_listing = SKILL_LISTING_MARKERS.iter().any(|m| text.contains(m));
            BlockShape {
                kind,
                bytes,
                cache_control: has_cache_control(block),
                system_reminder,
                skill_listing,
            }
        })
        .collect()
}

/// Append-only JSONL sink. All errors are swallowed so the audit can never
/// block or fail a request (same fire-and-forget contract as the ledger). The
/// writer is `Arc`-shared so the (blocking) file write can move to the blocking
/// pool — see [`AuditSink::record`].
pub struct AuditSink {
    writer: Arc<Mutex<BufWriter<std::fs::File>>>,
}

impl AuditSink {
    /// Open (create + append) the audit file at `path`.
    pub fn open(path: &str) -> std::io::Result<Self> {
        let f = OpenOptions::new().create(true).append(true).open(path)?;
        // Owner-only perms (0600 on Unix): the audit log is shape-metadata-only, but it
        // can embed structural identifiers (e.g. MCP tool names) — don't leave it
        // world-readable on a shared host. Best-effort.
        let _ = crate::fsperm::restrict_to_owner(std::path::Path::new(path));
        Ok(Self {
            writer: Arc::new(Mutex::new(BufWriter::new(f))),
        })
    }

    /// Append one capture record as a JSONL line. Best-effort and **non-blocking**
    /// on the async hot path: the line is serialized here (cheap) but the file
    /// `write`+`flush` run on the blocking pool via `spawn_blocking`, so a slow
    /// disk can never stall the tokio worker or serialize concurrent requests
    /// through the file lock (same fire-and-forget contract as the ledger writer).
    /// Per-line flush is kept so a tailed audit file stays current.
    pub fn record(&self, c: &Capture) {
        let Ok(line) = serde_json::to_string(c) else {
            return;
        };
        let writer = Arc::clone(&self.writer);
        tokio::task::spawn_blocking(move || {
            if let Ok(mut w) = writer.lock() {
                let _ = writeln!(w, "{line}");
                let _ = w.flush();
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// The capture must NEVER contain message content — only counts/flags. This
    /// plants the secret in a tool_result, in a system prompt, in a tool
    /// description, and inside a `messages[0]` skill-listing text block (all the
    /// places the new structural fields read transiently) and asserts none of it
    /// survives into the serialized record.
    #[test]
    fn capture_never_leaks_content() {
        let secret = "SUPER_SECRET_TOKEN_sk-abc123";
        let body = serde_json::to_vec(&json!({
            "model": "claude-sonnet-4-5",
            "system": [{"type": "text", "text": format!("system prompt {secret}")}],
            "tools": [
                {"name": "Bash", "description": format!("runs {secret}")},
                {"name": "Read", "description": "reads files", "cache_control": {"type": "ephemeral"}},
            ],
            "messages": [
                {"role": "user", "content": [
                    {"type": "text", "text": format!("<system-reminder>available-skills {secret}</system-reminder>")},
                    {"type": "tool_result", "tool_use_id": "t1", "content": format!("here is the {secret} value")}
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "t2", "content": [{"type": "text", "text": secret}]}
                ]},
            ]
        }))
        .unwrap();

        let cap = capture(&body, Some("sess-1"), None, "abcd".into(), 1).unwrap();
        let serialized = serde_json::to_string(&cap).unwrap();
        assert!(
            !serialized.contains(secret),
            "audit record leaked message content"
        );
        // It still measured the shape.
        assert_eq!(cap.num_tool_result_blocks, 2);
        assert_eq!(cap.num_string_content, 1);
        assert_eq!(cap.num_array_content, 1);
        assert_eq!(cap.model.as_deref(), Some("claude-sonnet-4-5"));
        assert_eq!(cap.session_id.as_deref(), Some("sess-1"));
        // Structural fields recorded (names/types/positions/flags, no text).
        assert_eq!(cap.tool_names, vec!["Bash", "Read"]);
        assert_eq!(cap.tool_cache_control_idx, vec![1]);
        assert_eq!(cap.system_kind, "array");
        assert_eq!(cap.first_msg_role.as_deref(), Some("user"));
        assert_eq!(cap.first_msg_blocks.len(), 2);
        assert!(cap.first_msg_blocks[0].system_reminder);
        assert!(cap.first_msg_blocks[0].skill_listing);
    }

    /// The structure fields capture tool order, cache_control breakpoints, the
    /// system shape, and the `messages[0]` block sequence — the data B-CACHE-1
    /// analysis needs.
    #[test]
    fn capture_records_cache_prefix_structure() {
        let body = serde_json::to_vec(&json!({
            "system": "you are helpful",
            "tools": [
                {"name": "Read"},
                {"name": "Write"},
                {"name": "Bash", "cache_control": {"type": "ephemeral"}},
            ],
            "messages": [
                {"role": "user", "content": [
                    {"type": "text", "text": "<system-reminder>note</system-reminder>"},
                    {"type": "text", "text": "do the thing", "cache_control": {"type": "ephemeral"}},
                ]},
                {"role": "assistant", "content": [{"type": "text", "text": "ok"}]},
            ]
        }))
        .unwrap();
        let cap = capture(&body, None, None, "h".into(), 0).unwrap();
        assert_eq!(cap.tool_names, vec!["Read", "Write", "Bash"]);
        assert_eq!(cap.tool_cache_control_idx, vec![2]);
        assert_eq!(cap.system_kind, "string");
        assert!(cap.system_cache_control_idx.is_empty());
        assert_eq!(cap.first_msg_role.as_deref(), Some("user"));
        let kinds: Vec<&str> = cap
            .first_msg_blocks
            .iter()
            .map(|b| b.kind.as_str())
            .collect();
        assert_eq!(kinds, vec!["text", "text"]);
        assert!(cap.first_msg_blocks[0].system_reminder);
        assert!(!cap.first_msg_blocks[0].skill_listing);
        assert!(!cap.first_msg_blocks[0].cache_control);
        assert!(cap.first_msg_blocks[1].cache_control);
    }

    /// An array `system` with `cache_control` on one block records that block's
    /// index — the positive path of `system_cache_control_idx`.
    #[test]
    fn capture_records_system_array_cache_control() {
        let body = serde_json::to_vec(&json!({
            "system": [
                {"type": "text", "text": "static preamble"},
                {"type": "text", "text": "more preamble", "cache_control": {"type": "ephemeral"}},
            ],
            "messages": [{"role": "user", "content": "hi"}]
        }))
        .unwrap();
        let cap = capture(&body, None, None, "h".into(), 0).unwrap();
        assert_eq!(cap.system_kind, "array");
        assert_eq!(cap.system_cache_control_idx, vec![1]);
    }

    /// An empty `messages: []` array is handled without panic: no first message.
    #[test]
    fn capture_empty_messages_array() {
        let body = serde_json::to_vec(&json!({"messages": []})).unwrap();
        let cap = capture(&body, None, None, "h".into(), 0).unwrap();
        assert_eq!(cap.num_messages, 0);
        assert_eq!(cap.first_msg_role, None);
        assert!(cap.first_msg_blocks.is_empty());
    }

    /// A plain-string `messages[0].content` (no block array) yields no block
    /// shapes but still records the role.
    #[test]
    fn capture_first_msg_string_content() {
        let body = serde_json::to_vec(&json!({
            "messages": [{"role": "user", "content": "hello there"}]
        }))
        .unwrap();
        let cap = capture(&body, None, None, "h".into(), 0).unwrap();
        assert_eq!(cap.first_msg_role.as_deref(), Some("user"));
        assert!(cap.first_msg_blocks.is_empty());
        assert!(cap.tool_names.is_empty());
        assert_eq!(cap.system_kind, "absent");
    }

    #[test]
    fn capture_counts_cleared_and_offload_and_size() {
        let body = serde_json::to_vec(&json!({
            "messages": [
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "a", "content": "[Old tool result content cleared]"},
                    {"type": "tool_result", "tool_use_id": "b", "content": "see tool-results/toolu_x.txt\n\nPreview (first 2KB):\n..."},
                    {"type": "tool_result", "tool_use_id": "c", "content": "x".repeat(30_000)},
                ]}
            ]
        }))
        .unwrap();
        let cap = capture(
            &body,
            None,
            Some("context-management-2025-06-27"),
            "h".into(),
            0,
        )
        .unwrap();
        assert_eq!(cap.num_cleared_markers, 1);
        assert_eq!(cap.num_offload_refs, 1);
        assert_eq!(cap.num_results_over_20kb, 1);
        assert!(cap.largest_result_bytes >= 30_000);
        assert_eq!(
            cap.anthropic_beta.as_deref(),
            Some("context-management-2025-06-27")
        );
    }

    #[test]
    fn non_messages_body_is_skipped() {
        assert!(capture(b"not json", None, None, "h".into(), 0).is_none());
        assert!(capture(b"{\"foo\":1}", None, None, "h".into(), 0).is_none());
    }
}
