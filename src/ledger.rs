//! SQLite savings ledger. The only module that touches the ledger DB.
//!
//! Records one row per `POST /v1/messages` request: in/out byte counts, the
//! strategies that fired, and a SHA-256 of the request **prefix** (the
//! top-level body with `messages` removed) computed before and after
//! mutation. The prefix hash is the load-bearing signal from SPIKE.md §9:
//! if it changes when **no** strategy fired, we are silently busting
//! Anthropic's prompt-cache prefix — the top failure mode. `trimwire stats`
//! surfaces that as a stability ratio.
//!
//! Design (see ARCHITECTURE.md decision log):
//! - A single `Arc<Mutex<Connection>>`; writes run on `spawn_blocking` so the
//!   blocking SQLite call (and any rare WAL-checkpoint stall) never blocks a
//!   tokio worker and the gateway never `.await`s the write.
//! - `record` is fire-and-forget: a failed insert is logged and swallowed; a
//!   DB that won't open yields a *degraded* (no-op) ledger so the gateway
//!   keeps proxying. Telemetry must never gate traffic.
//! - WAL + `synchronous = NORMAL`: losing a few rows on power-loss is fine
//!   for telemetry; corruption is not. `trimwire stats` reads via a separate
//!   READ-ONLY connection (WAL allows one writer + concurrent readers).

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use rusqlite::{Connection, OpenFlags, OptionalExtension};
use serde::Serialize;
use serde_json::Value;
use sha2::{Digest, Sha256};

/// One ledger row. `ts` is caller-supplied (request time, not insert time).
#[derive(Debug, Clone)]
pub struct Record {
    pub ts: i64,
    pub session_id: Option<String>,
    /// Request model from the body's `model` field (v4). `None` = not recorded
    /// (non-messages path or parse miss). Sub-agents share `session_id` but
    /// differ here, so per-session token metrics group by `(session_id, model)`.
    pub model: Option<String>,
    pub in_bytes: i64,
    pub out_bytes: i64,
    /// Comma-separated names of strategies that fired; `""` = none.
    pub strategies: String,
    /// Per-strategy bytes elided this request, as `name:bytes,name:bytes` (the
    /// gateway already computes these per-strategy `Stats`; v2 column). `""` =
    /// none. Byte counts only — never message content.
    pub strategy_bytes: String,
    pub prefix_hash_in: String,
    pub prefix_hash_out: String,
    // response-side instrumentation (MeteredBody) --------------------------------
    /// Time-to-first-token in **microseconds** (from upstream request send to
    /// first response data frame). 0 = not recorded (non-SSE, client disconnect
    /// before first frame, or instrumentation disabled). Stored at microsecond
    /// resolution so sub-millisecond values are distinguishable from "not
    /// recorded". Display layers convert to ms.
    pub ttft_us: i64,
    /// Input tokens billed (from SSE `message_start`). 0 = not recorded.
    pub input_tokens: i64,
    /// Cache-read input tokens (from SSE `message_start`). 0 = not recorded.
    pub cache_read_input_tokens: i64,
    /// Cache-creation input tokens (from SSE `message_start`). 0 = not recorded.
    pub cache_creation_input_tokens: i64,
    /// Output tokens billed (from SSE `message_delta`). 0 = not recorded.
    pub output_tokens: i64,
    /// Counts from `context_management.applied_edits` if seen; 0 = not present.
    pub applied_edits_cleared_thinking_turns: i64,
    /// Counts from `context_management.applied_edits` if seen; 0 = not present.
    pub applied_edits_cleared_tool_uses: i64,
    /// Counts from `context_management.applied_edits` if seen; 0 = not present.
    pub applied_edits_cleared_input_tokens: i64,
    /// HTTP response status code returned by upstream for this `/v1/messages`
    /// request. 0 = not captured (non-messages path, early error return before
    /// the upstream response head was received, or MeteredBody not used). A 4xx
    /// here means Anthropic rejected the (possibly pruned) body — the
    /// attributable-suspicion signal surfaced by `post_prune_errors`.
    pub response_status: u16,
}

/// Cheaply-cloneable handle held by the gateway. `None` = degraded (the DB
/// could not be opened); all operations become no-ops.
#[derive(Clone)]
pub struct Ledger {
    conn: Option<Arc<Mutex<Connection>>>,
}

impl Ledger {
    /// A ledger that records nothing (config `enabled = false`, or tests).
    pub fn disabled() -> Self {
        Self { conn: None }
    }

    /// Open (creating parent dirs + schema), apply PRAGMAs, and prune rows
    /// older than `retain_days`. Never fails loudly: on any error this logs a
    /// warning and returns a degraded (no-op) ledger so the gateway proceeds.
    pub fn open(db_path: &str, retain_days: u32) -> Self {
        match Self::try_open(db_path, retain_days) {
            Ok(l) => l,
            Err(e) => {
                tracing::warn!(error = %e, "ledger disabled: could not open DB");
                Self::disabled()
            }
        }
    }

    fn try_open(db_path: &str, retain_days: u32) -> Result<Self> {
        let path = resolve_path(db_path);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create ledger dir {}", parent.display()))?;
        }
        let conn =
            Connection::open(&path).with_context(|| format!("open ledger {}", path.display()))?;
        // Owner-only perms (0600 on Unix): the ledger holds no message content, but on a
        // shared host it should not be world-readable. Best-effort — a perms failure must
        // not disable the ledger. (The WAL/SHM sidecars inherit umask and carry the same
        // content-free page data; the durable store is this main DB file.)
        let _ = crate::fsperm::restrict_to_owner(&path);
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "busy_timeout", 5000)?;
        init_schema(&conn)?;
        add_missing_columns(&conn)?;
        prune(&conn, retain_days)?;
        Ok(Self {
            conn: Some(Arc::new(Mutex::new(conn))),
        })
    }

    /// Fire-and-forget insert. Returns immediately; the blocking write runs on
    /// the tokio blocking pool. Errors are logged and swallowed — a ledger
    /// failure must never affect the proxied request.
    pub fn record(&self, rec: Record) {
        let Some(conn) = self.conn.clone() else {
            return;
        };
        tokio::task::spawn_blocking(move || {
            let guard = match conn.lock() {
                Ok(g) => g,
                Err(poison) => poison.into_inner(), // a prior panic shouldn't wedge telemetry
            };
            if let Err(e) = insert(&guard, &rec) {
                tracing::warn!(error = %e, "ledger insert failed");
            }
        });
    }

    /// Fire-and-forget insert of one local-model summarizer outcome: `'a'`
    /// accepted (summary installed), `'r'` rejected/empty (model-free won or the
    /// decision was empty), `'e'` model error. `engine` is the coarse backend kind
    /// that produced the winning summary: `"local"` | `"api"` when outcome=`'a'`;
    /// `"model-free"` for `'r'` and `'e'` outcomes (the fallback stood).
    /// Same contract as [`record`](Self::record) — runs on the blocking pool,
    /// errors swallowed. Content-free: timestamp + outcome code + engine kind.
    pub fn record_summarizer_event(&self, ts: i64, outcome: char, engine: &str, collapsed: bool) {
        let Some(conn) = self.conn.clone() else {
            return;
        };
        let outcome = outcome.to_string();
        let engine = engine.to_owned();
        tokio::task::spawn_blocking(move || {
            let guard = match conn.lock() {
                Ok(g) => g,
                Err(poison) => poison.into_inner(),
            };
            if let Err(e) = guard.execute(
                "INSERT INTO summarizer_events (ts, outcome, engine, collapsed) VALUES (?1, ?2, ?3, ?4)",
                rusqlite::params![ts, outcome, engine, i64::from(collapsed)],
            ) {
                tracing::warn!(error = %e, "summarizer_event insert failed");
            }
        });
    }

    /// Record a content-free UPSTREAM failure (the proxy couldn't reach / timed out
    /// talking to Anthropic). `kind`: `'t'` = timeout (504), `'e'` = connection error
    /// (502). Same fire-and-forget contract as [`Self::record_summarizer_event`] —
    /// runs on the blocking pool, errors swallowed, NEVER affects the response path.
    /// Content-free: a timestamp + one code (there is only one upstream, so the kind
    /// can't identify an endpoint).
    pub fn record_upstream_error(&self, ts: i64, kind: char) {
        let Some(conn) = self.conn.clone() else {
            return;
        };
        let kind = kind.to_string();
        tokio::task::spawn_blocking(move || {
            let guard = match conn.lock() {
                Ok(g) => g,
                Err(poison) => poison.into_inner(),
            };
            if let Err(e) = guard.execute(
                "INSERT INTO upstream_errors (ts, kind) VALUES (?1, ?2)",
                rusqlite::params![ts, kind],
            ) {
                tracing::warn!(error = %e, "upstream_error insert failed");
            }
        });
    }

    /// Aggregate report for `trimwire stats`. Opens a fresh READ-ONLY connection
    /// (WAL allows reads alongside the daemon's writer) so it works whether or
    /// not the daemon is running.
    pub fn report(db_path: &str) -> Result<Report> {
        Self::report_window(db_path, i64::MIN, i64::MAX)
    }

    /// Like [`report`](Self::report) but restricted to rows with
    /// `since <= ts < until` (unix seconds). Pass `i64::MIN`/`i64::MAX` for an
    /// open bound. Powers `trimwire stats --since/--until`.
    pub fn report_window(db_path: &str, since: i64, until: i64) -> Result<Report> {
        let path = resolve_path(db_path);
        let conn = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY)
            .with_context(|| format!("open ledger read-only {}", path.display()))?;
        build_report(&conn, path, since, until)
    }

    /// Per-session savings for the live statusline. Read-only; returns zeros if
    /// the ledger doesn't exist yet or the session has no rows.
    pub fn session_savings(db_path: &str, session_id: &str) -> Result<SessionSavings> {
        let path = resolve_path(db_path);
        let conn = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY)
            .with_context(|| format!("open ledger read-only {}", path.display()))?;
        let (requests, in_bytes, out_bytes) = conn.query_row(
            "SELECT COUNT(*), COALESCE(SUM(in_bytes),0), COALESCE(SUM(out_bytes),0)
             FROM requests WHERE session_id = ?1",
            [session_id],
            |row| {
                Ok((
                    row.get::<_, i64>(0)? as u64,
                    row.get::<_, i64>(1)? as u64,
                    row.get::<_, i64>(2)? as u64,
                ))
            },
        )?;
        Ok(SessionSavings {
            requests,
            in_bytes,
            out_bytes,
        })
    }

    /// Per-session cache + token report, broken down by **model**. Read-only.
    ///
    /// `session`: an explicit `x-claude-code-session-id`, or `None`/`"last"` for
    /// the most recently-seen session (resolved by insert order — `MAX(id)` —
    /// NOT `MAX(ts)`, because `ts` is second-resolution and ties are ambiguous).
    ///
    /// Grouped by `model` because Claude Code interleaves sub-agent (haiku) and
    /// main-thread (opus/sonnet) calls under one session id; a single token sum
    /// would mix wildly different profiles and mislead. Returns `None` if there
    /// are no rows for the resolved session (or the ledger doesn't exist).
    pub fn session_report(db_path: &str, session: Option<&str>) -> Result<Option<SessionReport>> {
        let path = resolve_path(db_path);
        let conn = match Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY) {
            Ok(c) => c,
            Err(_) => return Ok(None), // no ledger yet → nothing to report
        };
        // Resolve the session id ("last"/None → most recent by insert order).
        let sid: Option<String> = match session {
            Some(s) if !s.is_empty() && s != "last" => Some(s.to_owned()),
            _ => conn
                .query_row(
                    "SELECT session_id FROM requests
                     WHERE session_id IS NOT NULL ORDER BY id DESC LIMIT 1",
                    [],
                    |r| r.get(0),
                )
                .optional()?,
        };
        let Some(sid) = sid else {
            return Ok(None);
        };
        let (started_at, ended_at): (i64, i64) = conn.query_row(
            "SELECT COALESCE(MIN(ts),0), COALESCE(MAX(ts),0)
             FROM requests WHERE session_id = ?1",
            [&sid],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        let mut stmt = conn.prepare(
            "SELECT model, COUNT(*),
                    COALESCE(SUM(in_bytes),0), COALESCE(SUM(out_bytes),0),
                    COALESCE(SUM(input_tokens),0), COALESCE(SUM(cache_read_input_tokens),0),
                    COALESCE(SUM(cache_creation_input_tokens),0), COALESCE(SUM(output_tokens),0),
                    COALESCE(SUM(applied_edits_cleared_input_tokens),0)
             FROM requests WHERE session_id = ?1
             GROUP BY model ORDER BY model",
        )?;
        let per_model = stmt
            .query_map([&sid], |r| {
                Ok(SessionModelStat {
                    model: r.get::<_, Option<String>>(0)?,
                    turns: r.get::<_, i64>(1)? as u64,
                    in_bytes: r.get::<_, i64>(2)? as u64,
                    out_bytes: r.get::<_, i64>(3)? as u64,
                    input_tokens: r.get::<_, i64>(4)? as u64,
                    cache_read_input_tokens: r.get::<_, i64>(5)? as u64,
                    cache_creation_input_tokens: r.get::<_, i64>(6)? as u64,
                    output_tokens: r.get::<_, i64>(7)? as u64,
                    native_cleared_input_tokens: r.get::<_, i64>(8)? as u64,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        if per_model.is_empty() {
            return Ok(None);
        }
        // Post-prune HTTP errors for this session. Column-tolerant: a READ_ONLY
        // connection can predate the `response_status` migration → treat as zero.
        let post_prune_errors: u64 = if column_exists(&conn, "requests", "response_status")? {
            conn.query_row(
                "SELECT COUNT(*) FROM requests
                 WHERE session_id = ?1 AND response_status >= 400 AND strategies != ''",
                [&sid],
                |row| row.get::<_, i64>(0),
            )? as u64
        } else {
            0
        };
        Ok(Some(SessionReport {
            session_id: sid,
            started_at,
            ended_at,
            per_model,
            post_prune_errors,
        }))
    }

    /// List recent sessions (most-recent activity first) with content-free
    /// aggregates — the `recall` discovery view. `query`, when non-empty, keeps
    /// only sessions whose id or model contains it (case-insensitive substring;
    /// the ledger holds NO prompt content, so this matches metadata only). Read
    /// -only; an absent ledger yields an empty list. `limit` caps the rows.
    pub fn list_sessions(
        db_path: &str,
        query: Option<&str>,
        limit: usize,
    ) -> Result<Vec<SessionRow>> {
        let path = resolve_path(db_path);
        let conn = match Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY) {
            Ok(c) => c,
            Err(_) => return Ok(Vec::new()),
        };
        let q = query.map(str::trim).filter(|s| !s.is_empty());
        let filter = if q.is_some() {
            "AND (session_id LIKE ?1 OR model LIKE ?1)"
        } else {
            ""
        };
        // `limit` is a usize we format in (not user-controlled SQL); the filter term
        // is bound as a parameter. No injection surface.
        let sql = format!(
            "SELECT session_id, date(MAX(ts),'unixepoch'), COUNT(*),
                    COALESCE(SUM(in_bytes),0), COALESCE(SUM(out_bytes),0),
                    COALESCE(SUM(input_tokens),0), COALESCE(SUM(cache_read_input_tokens),0),
                    COALESCE(SUM(cache_creation_input_tokens),0), MAX(model)
             FROM requests WHERE session_id IS NOT NULL {filter}
             GROUP BY session_id ORDER BY MAX(ts) DESC LIMIT {lim}",
            lim = limit.max(1),
        );
        let mut stmt = conn.prepare(&sql)?;
        let map_row = |r: &rusqlite::Row| -> rusqlite::Result<SessionRow> {
            Ok(SessionRow {
                session_id: r.get(0)?,
                last_day: r.get::<_, Option<String>>(1)?.unwrap_or_default(),
                requests: r.get::<_, i64>(2)? as u64,
                in_bytes: r.get::<_, i64>(3)? as u64,
                out_bytes: r.get::<_, i64>(4)? as u64,
                input_tokens: r.get::<_, i64>(5)? as u64,
                cache_read_input_tokens: r.get::<_, i64>(6)? as u64,
                cache_creation_input_tokens: r.get::<_, i64>(7)? as u64,
                model: r.get::<_, Option<String>>(8)?,
            })
        };
        let rows = if let Some(term) = q {
            let like = format!("%{term}%");
            stmt.query_map([like], map_row)?
                .collect::<rusqlite::Result<Vec<_>>>()?
        } else {
            stmt.query_map([], map_row)?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };
        Ok(rows)
    }
}

/// One session's content-free aggregate (the `recall` discovery view).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct SessionRow {
    pub session_id: String,
    /// `date(MAX(ts),'unixepoch')` — the last-activity day (YYYY-MM-DD).
    pub last_day: String,
    pub requests: u64,
    pub in_bytes: u64,
    pub out_bytes: u64,
    pub input_tokens: u64,
    pub cache_read_input_tokens: u64,
    pub cache_creation_input_tokens: u64,
    /// `None` = model not recorded for the request.
    pub model: Option<String>,
}

impl SessionRow {
    pub fn saved_bytes(&self) -> i64 {
        self.in_bytes as i64 - self.out_bytes as i64
    }
    /// Body reduction (in→out) as a percentage; 0 when nothing was recorded.
    /// Usually 0–100 but can be NEGATIVE on tiny payloads where a stub exceeds
    /// the original content (out > in) — the display layer clamps/labels it.
    pub fn reduction_pct(&self) -> f64 {
        if self.in_bytes == 0 {
            0.0
        } else {
            100.0 * self.saved_bytes() as f64 / self.in_bytes as f64
        }
    }
    pub fn total_input_tokens(&self) -> u64 {
        self.input_tokens + self.cache_read_input_tokens + self.cache_creation_input_tokens
    }
    /// Cache-hit ratio `cache_read / total_input` as 0–100 (0 if no tokens recorded).
    pub fn cache_hit_pct(&self) -> f64 {
        let total = self.total_input_tokens();
        if total == 0 {
            0.0
        } else {
            (self.cache_read_input_tokens as f64 / total as f64 * 100.0).min(100.0)
        }
    }
}

/// Savings for a single Claude Code session (statusline source).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SessionSavings {
    pub requests: u64,
    pub in_bytes: u64,
    pub out_bytes: u64,
}

impl SessionSavings {
    pub fn saved_bytes(&self) -> i64 {
        self.in_bytes as i64 - self.out_bytes as i64
    }
    /// Reduction as a percentage of the inbound bytes (0 if no traffic; can be
    /// negative on tiny payloads where the stub exceeds the original content).
    pub fn reduction_pct(&self) -> f64 {
        if self.in_bytes == 0 {
            0.0
        } else {
            (self.saved_bytes() as f64 / self.in_bytes as f64) * 100.0
        }
    }
}

/// One model's slice of a session (the `stats --session` breakdown). Token
/// counts are billed values from the response (0 = not recorded). Content-free.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct SessionModelStat {
    /// `None` = model not recorded (or a parse miss).
    pub model: Option<String>,
    pub turns: u64,
    pub in_bytes: u64,
    pub out_bytes: u64,
    pub input_tokens: u64,
    pub cache_read_input_tokens: u64,
    pub cache_creation_input_tokens: u64,
    pub output_tokens: u64,
    /// Tokens Anthropic's OWN server-side context editing cleared (applied_edits).
    pub native_cleared_input_tokens: u64,
}

impl SessionModelStat {
    pub fn saved_bytes(&self) -> i64 {
        self.in_bytes as i64 - self.out_bytes as i64
    }
    /// Total prompt-input tokens this turn. Anthropic reports three DISJOINT
    /// buckets that sum to the full input: `input_tokens` (uncached, base rate),
    /// `cache_read_input_tokens` (~0.1×), `cache_creation_input_tokens` (~1.25–2×).
    /// (Confirmed on live traffic: `cache_read` ≫ `input_tokens`, so input_tokens
    /// is NOT the total — it's the uncached remainder.)
    pub fn total_input_tokens(&self) -> u64 {
        self.input_tokens + self.cache_read_input_tokens + self.cache_creation_input_tokens
    }
    /// Cache-hit ratio: `cache_read / total_input` as 0–100 (0 if no tokens
    /// recorded). NOT a cost figure — read it ALONGSIDE `cache_creation_input_tokens`
    /// (creation is ~1.25–2×, read ~0.1×), which is why both are surfaced.
    pub fn cache_hit_pct(&self) -> f64 {
        let total = self.total_input_tokens();
        if total == 0 {
            0.0
        } else {
            (self.cache_read_input_tokens as f64 / total as f64 * 100.0).min(100.0)
        }
    }
}

/// A per-session, per-model cache + token report (the Track A measurement
/// instrument: cache_read vs cache_creation is how thinking_strip's cache cost
/// becomes visible on a real A/B run). Content-free; all counts/tokens.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct SessionReport {
    pub session_id: String,
    /// Unix seconds of the first / last request in the session.
    pub started_at: i64,
    pub ended_at: i64,
    /// One entry per distinct `model` seen in the session, sorted by model.
    pub per_model: Vec<SessionModelStat>,
    /// Requests in this session where `response_status >= 400` AND `strategies`
    /// is non-empty (trimwire pruned the body before the upstream rejected it).
    /// The per-session analogue of the all-time `Report::post_prune_errors`.
    /// 0 when the `response_status` column is absent (older ledger).
    pub post_prune_errors: u64,
}

/// SHA-256 (hex) of the request **prefix**: the top-level JSON object with the
/// `messages` key removed, serialized with sorted keys and no whitespace.
/// serde_json's default `Map` is a sorted `BTreeMap` (no `preserve_order`
/// feature), so output is deterministic and lexicographically ordered.
///
/// The contract that matters is **Rust self-consistency**: the same body
/// hashes the same every time, so `prefix_hash_in == prefix_hash_out` whenever
/// only `messages` changed (the §9 stability check). This hash is recorded
/// only in the local ledger and is never compared against another
/// implementation at runtime. It is *close* to the Phase 0 Python
/// `cache_prefix_hash` (also sorted-key + compact) but not guaranteed
/// byte-identical: serde_json emits non-ASCII as raw UTF-8 whereas Python's
/// `json.dumps` defaults to `ensure_ascii=True` (`\uXXXX` escapes). Parity is
/// not required, so we don't pay to match it.
///
/// Non-JSON or non-object bodies hash a stable empty object, so a malformed
/// request still produces equal in/out hashes (a no-op, not a false alarm).
pub fn prefix_hash(body: &[u8]) -> String {
    prefix_hash_and_model(body).0
}

/// Like [`prefix_hash`], but also returns the request's `model` from the *same*
/// parse. The gateway keys its per-session reprune state on `session_id + model`
/// because Claude Code interleaves sub-agent and background calls (different
/// models — haiku/sonnet) under one `x-claude-code-session-id`; keying on the
/// session id alone would thrash the state between those streams. No extra
/// hot-path cost over `prefix_hash` (one parse, reused).
pub fn prefix_hash_and_model(body: &[u8]) -> (String, Option<String>) {
    let parsed = serde_json::from_slice::<Value>(body).ok();
    let model = parsed
        .as_ref()
        .and_then(|v| v.get("model"))
        .and_then(Value::as_str)
        .map(str::to_owned);
    let prefix = match parsed {
        Some(Value::Object(mut map)) => {
            map.remove("messages");
            Value::Object(map)
        }
        _ => Value::Object(serde_json::Map::new()),
    };
    // Serializing a `Value` that was just parsed by serde_json cannot fail (no
    // non-string keys, no custom serializer). Use a content-derived fallback
    // instead of `expect` so a future refactor can't panic a connection task in
    // the hot path (audit P3-5) — while preserving the original invariant: the
    // fallback is the prefix's Debug bytes, which stay DISTINCT per prefix, so a
    // busted prefix can never hash equal to a different one and look "stable".
    let serialized =
        serde_json::to_vec(&prefix).unwrap_or_else(|_| format!("{prefix:?}").into_bytes());
    (hex::encode(Sha256::digest(&serialized)), model)
}

fn init_schema(conn: &Connection) -> Result<()> {
    // Single full schema — `IF NOT EXISTS` so it creates a fresh DB and no-ops on
    // an existing one. v0.1.0 is the first release, so no older on-disk schema
    // predates it; the previous v1→v4 ALTER-TABLE migration chain was dropped.
    // Real ledgers now exist, so future ADD COLUMN is safe but removals/renames
    // need a migration. (Columns are referenced by name, so their order here is
    // irrelevant to an already-populated ledger.)
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS requests (
            id              INTEGER PRIMARY KEY,
            ts              INTEGER NOT NULL,
            session_id      TEXT,
            in_bytes        INTEGER NOT NULL,
            out_bytes       INTEGER NOT NULL,
            strategies      TEXT NOT NULL DEFAULT '',
            prefix_hash_in  TEXT NOT NULL,
            prefix_hash_out TEXT NOT NULL,
            strategy_bytes  TEXT NOT NULL DEFAULT '',
            ttft_us                              INTEGER NOT NULL DEFAULT 0,
            input_tokens                         INTEGER NOT NULL DEFAULT 0,
            cache_read_input_tokens              INTEGER NOT NULL DEFAULT 0,
            cache_creation_input_tokens          INTEGER NOT NULL DEFAULT 0,
            output_tokens                        INTEGER NOT NULL DEFAULT 0,
            applied_edits_cleared_thinking_turns INTEGER NOT NULL DEFAULT 0,
            applied_edits_cleared_tool_uses      INTEGER NOT NULL DEFAULT 0,
            applied_edits_cleared_input_tokens   INTEGER NOT NULL DEFAULT 0,
            model           TEXT,
            response_status INTEGER NOT NULL DEFAULT 0
        ) STRICT;
        CREATE INDEX IF NOT EXISTS idx_requests_ts ON requests(ts);
        -- Opt-in summarizer outcomes — one append-only row per model
        -- call (only written when the summarizer engine is not model-free).
        -- CONTENT-FREE: a timestamp + a single outcome code ('a' accepted /
        -- 'r' rejected-or-empty / 'e' error) + the winning engine kind
        -- ('local' | 'api' | 'model-free'). Powers the summarizer install-rate
        -- + trigger-rate + won-backend in `share stats`; append-only so
        -- concurrent outcomes never overwrite each other.
        CREATE TABLE IF NOT EXISTS summarizer_events (
            id        INTEGER PRIMARY KEY,
            ts        INTEGER NOT NULL,
            outcome   TEXT NOT NULL,
            engine    TEXT NOT NULL DEFAULT 'model-free',
            -- 1 when this accepted summary was a chain COLLAPSE (accumulator hit
            -- max_summary_segments then REPLACE): the long-session context-pressure
            -- signal surfaced by trimwire stats. 0 otherwise.
            collapsed INTEGER NOT NULL DEFAULT 0
        ) STRICT;
        CREATE INDEX IF NOT EXISTS idx_summarizer_events_ts ON summarizer_events(ts);

        -- Content-free UPSTREAM failure events (proxy couldn't reach / timed out on
        -- Anthropic). One append-only row per failure: a timestamp + a single kind
        -- code ('t' timeout / 'e' connection error). Lets `trimwire stats` surface
        -- 'how often did trimwire fail to reach Anthropic?'. Never blocks traffic.
        CREATE TABLE IF NOT EXISTS upstream_errors (
            id   INTEGER PRIMARY KEY,
            ts   INTEGER NOT NULL,
            kind TEXT NOT NULL
        ) STRICT;
        CREATE INDEX IF NOT EXISTS idx_upstream_errors_ts ON upstream_errors(ts);",
    )?;
    Ok(())
}

/// Add columns introduced after the initial schema to existing ledger files.
/// SQLite has no `ADD COLUMN IF NOT EXISTS`, so we check `pragma_table_info`
/// first and skip the ALTER if the column already exists. New ledgers created
/// by `init_schema` already have the column (in the CREATE TABLE), so this
/// only fires for older on-disk files. Errors propagate to `Ledger::open`,
/// which degrades the ledger gracefully rather than crashing the daemon.
fn add_missing_columns(conn: &Connection) -> Result<()> {
    // summarizer_events.engine: added in §8C/Q4 to record the winning engine
    // kind ('local' | 'api' | 'model-free'). DEFAULT 'model-free' so pre-existing
    // rows read as "no engine won" — the correct interpretation for old data.
    let has_engine = column_exists(conn, "summarizer_events", "engine")?;
    if !has_engine {
        conn.execute_batch(
            "ALTER TABLE summarizer_events ADD COLUMN engine TEXT NOT NULL DEFAULT 'model-free'",
        )?;
    }
    // summarizer_events.collapsed: added in §18/T4 to count chain collapses (the
    // long-session degradation inflection). DEFAULT 0 so pre-existing rows read as
    // "not a collapse" — the correct interpretation for old data.
    let has_collapsed = column_exists(conn, "summarizer_events", "collapsed")?;
    if !has_collapsed {
        conn.execute_batch(
            "ALTER TABLE summarizer_events ADD COLUMN collapsed INTEGER NOT NULL DEFAULT 0",
        )?;
    }
    // requests.response_status: added to record the upstream HTTP response status
    // code per /v1/messages request. DEFAULT 0 = "not captured" (non-messages path
    // or no upstream response head received) — the correct reading for pre-existing
    // rows where no status was recorded.
    let has_response_status = column_exists(conn, "requests", "response_status")?;
    if !has_response_status {
        conn.execute_batch(
            "ALTER TABLE requests ADD COLUMN response_status INTEGER NOT NULL DEFAULT 0",
        )?;
    }
    Ok(())
}

fn prune(conn: &Connection, retain_days: u32) -> Result<()> {
    let cutoff = now_secs() - i64::from(retain_days) * 86_400;
    conn.execute("DELETE FROM requests WHERE ts < ?1", [cutoff])?;
    conn.execute("DELETE FROM summarizer_events WHERE ts < ?1", [cutoff])?;
    conn.execute("DELETE FROM upstream_errors WHERE ts < ?1", [cutoff])?;
    Ok(())
}

fn insert(conn: &Connection, r: &Record) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO requests
            (ts, session_id, model, in_bytes, out_bytes, strategies, strategy_bytes,
             prefix_hash_in, prefix_hash_out,
             ttft_us, input_tokens, cache_read_input_tokens, cache_creation_input_tokens,
             output_tokens, applied_edits_cleared_thinking_turns,
             applied_edits_cleared_tool_uses, applied_edits_cleared_input_tokens,
             response_status)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18)",
        rusqlite::params![
            r.ts,
            r.session_id,
            r.model,
            r.in_bytes,
            r.out_bytes,
            r.strategies,
            r.strategy_bytes,
            r.prefix_hash_in,
            r.prefix_hash_out,
            r.ttft_us,
            r.input_tokens,
            r.cache_read_input_tokens,
            r.cache_creation_input_tokens,
            r.output_tokens,
            r.applied_edits_cleared_thinking_turns,
            r.applied_edits_cleared_tool_uses,
            r.applied_edits_cleared_input_tokens,
            r.response_status as i64,
        ],
    )?;
    Ok(())
}

/// Per-day savings line.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DaySavings {
    pub day: String,
    pub requests: u64,
    pub in_bytes: u64,
    pub out_bytes: u64,
}

/// Cache-prefix stability over the **no-strategy-fired** cohort — the SPIKE §9
/// silent-failure guard. `ratio` should be 1.0; anything below means a
/// pass-through changed the prefix (cache-busting bug).
#[derive(Debug, Clone, Default, Serialize)]
pub struct CacheStability {
    pub no_strategy_total: u64,
    pub no_strategy_stable: u64,
    pub ratio: f64,
}

/// Response-side aggregate metrics. Counts are 0 when nothing was recorded.
#[derive(Debug, Clone, Default, Serialize)]
pub struct ResponseMetrics {
    /// Number of requests where TTFT was recorded (ttft_us > 0).
    pub requests_with_ttft: u64,
    /// Average TTFT in **microseconds** across requests that have it (0.0 if none).
    /// Display layers divide by 1000 to show ms.
    pub avg_ttft_us: f64,
    /// Total input tokens billed across all requests.
    pub total_input_tokens: u64,
    /// Total cache-read input tokens (tokens served from Anthropic's cache).
    pub total_cache_read_input_tokens: u64,
    /// Total cache-creation input tokens (tokens written to Anthropic's cache).
    pub total_cache_creation_input_tokens: u64,
    /// Total output tokens billed across all requests.
    pub total_output_tokens: u64,
    /// Number of requests that saw a `context_management.applied_edits` event.
    pub requests_with_applied_edits: u64,
    /// Cumulative count of thinking turns Anthropic's native compaction cleared.
    pub total_applied_edits_cleared_thinking_turns: u64,
    /// Cumulative count of tool uses Anthropic's native compaction cleared.
    pub total_applied_edits_cleared_tool_uses: u64,
    /// Cumulative input tokens Anthropic's native compaction cleared.
    pub total_applied_edits_cleared_input_tokens: u64,
}

impl ResponseMetrics {
    /// Cache-hit rate as a percentage: cache-read over the cache total
    /// (read + creation). NOTE the denominator is deliberately the two *cache*
    /// buckets only — NOT `SessionModelStat::cache_hit_pct`'s read/(read +
    /// creation + uncached). This is the "of the bytes that touched the cache,
    /// how many were reads" view the `--share` bucketer reports. 0.0 when the
    /// cache total is 0. Single source so the CLI doesn't inline the formula.
    pub fn cache_hit_pct(&self) -> f64 {
        let denom = self.total_cache_read_input_tokens + self.total_cache_creation_input_tokens;
        if denom == 0 {
            0.0
        } else {
            self.total_cache_read_input_tokens as f64 / denom as f64 * 100.0
        }
    }
}

/// Aggregated `trimwire stats` output.
#[derive(Debug, Clone, Serialize)]
pub struct Report {
    pub total_requests: u64,
    pub total_in_bytes: u64,
    pub total_out_bytes: u64,
    pub per_day: Vec<DaySavings>,
    pub per_strategy: Vec<(String, u64)>,
    /// Total bytes elided per strategy across all rows (the `strategy_bytes`
    /// CSV). Answers "which strategy earns its keep on my traffic"; empty when
    /// no row recorded a breakdown.
    pub per_strategy_bytes: Vec<(String, i64)>,
    pub cache_stability: CacheStability,
    /// Response-side metrics (TTFT, token billing, Anthropic native edits).
    pub response_metrics: ResponseMetrics,
    /// Number of requests where at least one pruning strategy fired (strategies
    /// column is non-empty). Used by `--share` to compute `strategy_any_fired_pct_bucket`.
    pub requests_with_strategy: u64,
    /// Summarizer outcomes in-window (all 0 unless the summarizer engine is not
    /// model-free): `accepted` = a model summary was installed;
    /// `rejected` = model-free pruning won (or an empty decision); `errored` = the
    /// model call failed. Power the `--share` summarizer install-rate + trigger-rate.
    pub summarizer_accepted: u64,
    pub summarizer_rejected: u64,
    pub summarizer_errored: u64,
    /// Of the accepted summaries, how many were produced by the local engine.
    pub summarizer_accepted_local: u64,
    /// Of the accepted summaries, how many were produced by an API engine.
    pub summarizer_accepted_api: u64,
    /// Of the accepted summaries, how many were a chain COLLAPSE (accumulator hit
    /// `max_summary_segments` → REPLACE). The long-session "context-pressure"
    /// signal: a non-zero count is the cue to `/compact` or start a fresh session.
    pub summarizer_collapses: u64,
    /// Upstream failures in-window: `errors` = connection errors (502), `timeouts`
    /// = the upstream call exceeded the proxy timeout (504). Answers "how often did
    /// trimwire fail to reach Anthropic?" — these never produce a normal request row.
    pub upstream_errors: u64,
    pub upstream_timeouts: u64,
    /// Requests where `response_status >= 400` AND `strategies` is non-empty:
    /// upstream returned an HTTP error AFTER trimwire mutated the body. The
    /// attributable-suspicion signal — a 4xx caused by pruning is detectable here.
    /// 0 when the `response_status` column is absent (older ledger).
    pub post_prune_errors: u64,
    /// Requests where `response_status >= 400` regardless of strategies (all
    /// upstream HTTP ≥400 responses). Superset of `post_prune_errors`.
    /// 0 when the `response_status` column is absent (older ledger).
    pub upstream_http_errors: u64,
    pub db_path: PathBuf,
}

impl Report {
    /// Bytes saved across all recorded requests (may be negative on tiny
    /// synthetic payloads where stub text exceeds original content).
    pub fn bytes_saved(&self) -> i64 {
        self.total_in_bytes as i64 - self.total_out_bytes as i64
    }

    /// Overall reduction as a percentage of inbound bytes (0.0 when nothing was
    /// sent). The single source for the figure shown by `stats`, the local
    /// `dashboard`, and the `--share` bucketer — so the formula can't drift.
    pub fn reduction_pct(&self) -> f64 {
        if self.total_in_bytes == 0 {
            0.0
        } else {
            self.bytes_saved() as f64 / self.total_in_bytes as f64 * 100.0
        }
    }

    /// Rough count of context tokens removed, at ~4 bytes/token (matches the
    /// benchmark's estimate). A display ESTIMATE — never a billing figure; the
    /// single home for the `4` so `stats` and `dashboard` agree.
    pub fn est_tokens_removed(&self) -> i64 {
        self.bytes_saved() / 4
    }
}

/// The 9 strategy names `strategies::run` can push, in order. The single Rust
/// source of truth — guarded by `known_strategies_is_the_expected_set` against
/// `strategies::run`, and reused by `cli::share` (so the telemetry allowlist
/// can't drift from the ledger). `collector/src/validate.ts` mirrors this list
/// across the language boundary (kept in sync by hand — see the note there).
pub const KNOWN_STRATEGIES: [&str; 9] = [
    "failed_input_purge",
    "stale_input_cap",
    "cross_turn_dedup",
    "stale_reads",
    "simhash_dedup",
    "bloat_cap",
    "sliding_window",
    "image_strip",
    "thinking_strip",
];

/// True when `name` exists as a table in the open database. Read paths use
/// this to tolerate older ledgers that predate later-added tables.
fn table_exists(conn: &Connection, name: &str) -> Result<bool> {
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
        [name],
        |row| row.get(0),
    )?;
    Ok(count > 0)
}

/// True when `table.column` exists. Companion to [`table_exists`] for columns
/// added to an existing table after the initial schema (READ_ONLY connections
/// can see a pre-migration table).
fn column_exists(conn: &Connection, table: &str, column: &str) -> Result<bool> {
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM pragma_table_info(?1) WHERE name = ?2",
        [table, column],
        |row| row.get(0),
    )?;
    Ok(count > 0)
}

fn build_report(conn: &Connection, db_path: PathBuf, since: i64, until: i64) -> Result<Report> {
    // Every aggregate below is restricted to the [since, until) window. Callers
    // pass i64::MIN/i64::MAX for the all-time report, so the predicate is always
    // present and binds the same two params — no conditional SQL to get wrong.
    let (total_requests, total_in_bytes, total_out_bytes) = conn.query_row(
        "SELECT COUNT(*), COALESCE(SUM(in_bytes),0), COALESCE(SUM(out_bytes),0) \
         FROM requests WHERE ts >= ?1 AND ts < ?2",
        rusqlite::params![since, until],
        |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, i64>(2)?,
            ))
        },
    )?;

    let mut stmt = conn.prepare(
        "SELECT date(ts,'unixepoch') AS day, COUNT(*), COALESCE(SUM(in_bytes),0),
                COALESCE(SUM(out_bytes),0)
         FROM requests WHERE ts >= ?1 AND ts < ?2 GROUP BY day ORDER BY day DESC",
    )?;
    let per_day = stmt
        .query_map(rusqlite::params![since, until], |row| {
            Ok(DaySavings {
                day: row.get(0)?,
                requests: row.get::<_, i64>(1)? as u64,
                in_bytes: row.get::<_, i64>(2)? as u64,
                out_bytes: row.get::<_, i64>(3)? as u64,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    // Per-strategy fired counts. `strategies` is a comma-separated CSV; wrap
    // both sides in commas and use `instr` (literal substring, unlike LIKE
    // which would treat `_` as a wildcard) so we match whole CSV tokens and
    // never over-count when one strategy name is a substring of another.
    let mut per_strategy = Vec::new();
    for name in KNOWN_STRATEGIES {
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM requests \
             WHERE instr(',' || strategies || ',', ',' || ?1 || ',') > 0 \
               AND ts >= ?2 AND ts < ?3",
            rusqlite::params![name, since, until],
            |row| row.get(0),
        )?;
        if count > 0 {
            per_strategy.push((name.to_owned(), count as u64));
        }
    }

    // Per-strategy elided bytes: sum the `name:bytes` CSV tokens across rows.
    // Done in Rust (the column is a compact CSV, not a normalized table).
    let mut bytes_by_name: std::collections::HashMap<String, i64> =
        std::collections::HashMap::new();
    {
        let mut sb = conn.prepare(
            "SELECT strategy_bytes FROM requests \
             WHERE strategy_bytes != '' AND ts >= ?1 AND ts < ?2",
        )?;
        let rows = sb.query_map(rusqlite::params![since, until], |row| {
            row.get::<_, String>(0)
        })?;
        for csv in rows {
            for tok in csv?.split(',').filter(|t| !t.is_empty()) {
                if let Some((name, bytes)) = tok.rsplit_once(':') {
                    if let Ok(b) = bytes.parse::<i64>() {
                        *bytes_by_name.entry(name.to_owned()).or_insert(0) += b;
                    }
                }
            }
        }
    }
    // Order by KNOWN_STRATEGIES, then any others (e.g. `stable_reprune`).
    let mut per_strategy_bytes: Vec<(String, i64)> = Vec::new();
    for name in KNOWN_STRATEGIES {
        if let Some(b) = bytes_by_name.remove(name) {
            per_strategy_bytes.push((name.to_owned(), b));
        }
    }
    let mut rest: Vec<_> = bytes_by_name.into_iter().collect();
    rest.sort_by_key(|(_, b)| std::cmp::Reverse(*b));
    per_strategy_bytes.extend(rest);

    let (no_strategy_total, no_strategy_stable) = conn.query_row(
        "SELECT COUNT(*), COALESCE(SUM(prefix_hash_in = prefix_hash_out),0)
         FROM requests WHERE strategies = '' AND ts >= ?1 AND ts < ?2",
        rusqlite::params![since, until],
        |row| Ok((row.get::<_, i64>(0)? as u64, row.get::<_, i64>(1)? as u64)),
    )?;
    let ratio = if no_strategy_total == 0 {
        1.0
    } else {
        no_strategy_stable as f64 / no_strategy_total as f64
    };

    // Response-side aggregate metrics. The columns are always present (single
    // schema) and rows without them recorded carry 0; COALESCE guards SUM-over-
    // zero-rows returning NULL.
    let (
        requests_with_ttft,
        avg_ttft_us_raw,
        total_input_tokens,
        total_cache_read_input_tokens,
        total_cache_creation_input_tokens,
        total_output_tokens,
        requests_with_applied_edits,
        total_applied_edits_cleared_thinking_turns,
        total_applied_edits_cleared_tool_uses,
        total_applied_edits_cleared_input_tokens,
    ) = conn.query_row(
        "SELECT
            COUNT(CASE WHEN ttft_us > 0 THEN 1 END),
            COALESCE(AVG(CASE WHEN ttft_us > 0 THEN ttft_us END), 0.0),
            COALESCE(SUM(input_tokens), 0),
            COALESCE(SUM(cache_read_input_tokens), 0),
            COALESCE(SUM(cache_creation_input_tokens), 0),
            COALESCE(SUM(output_tokens), 0),
            COUNT(CASE WHEN applied_edits_cleared_thinking_turns > 0
                         OR applied_edits_cleared_tool_uses > 0
                         OR applied_edits_cleared_input_tokens > 0
                       THEN 1 END),
            COALESCE(SUM(applied_edits_cleared_thinking_turns), 0),
            COALESCE(SUM(applied_edits_cleared_tool_uses), 0),
            COALESCE(SUM(applied_edits_cleared_input_tokens), 0)
         FROM requests WHERE ts >= ?1 AND ts < ?2",
        rusqlite::params![since, until],
        |row| {
            Ok((
                row.get::<_, i64>(0)? as u64,
                row.get::<_, f64>(1)?,
                row.get::<_, i64>(2)? as u64,
                row.get::<_, i64>(3)? as u64,
                row.get::<_, i64>(4)? as u64,
                row.get::<_, i64>(5)? as u64,
                row.get::<_, i64>(6)? as u64,
                row.get::<_, i64>(7)? as u64,
                row.get::<_, i64>(8)? as u64,
                row.get::<_, i64>(9)? as u64,
            ))
        },
    )?;

    // Count requests where at least one strategy fired (strategies column non-empty).
    let requests_with_strategy: u64 = conn.query_row(
        "SELECT COUNT(*) FROM requests WHERE strategies != '' AND ts >= ?1 AND ts < ?2",
        rusqlite::params![since, until],
        |row| row.get::<_, i64>(0),
    )? as u64;

    // Summarizer outcomes in-window (all 0 unless the feature ran).
    // Read connections are READ_ONLY and may see an older ledger created before
    // this table existed (the gateway auto-migrates on its read-write open, but
    // `stats`/`dashboard` can run first) — treat a missing table as zero events
    // rather than leaking a raw `no such table` SQLite error.
    // The `engine` column was added in §8C/Q4 by the read-write open's ALTER; a
    // READ_ONLY connection can hit the pre-migration table, so the engine-split
    // counts also need column-level tolerance (zeros when the column is absent —
    // the correct reading: no recorded engine ever won on that ledger).
    let (
        summarizer_accepted,
        summarizer_rejected,
        summarizer_errored,
        summarizer_accepted_local,
        summarizer_accepted_api,
        summarizer_collapses,
    ) = if table_exists(conn, "summarizer_events")? {
        let (accepted, rejected, errored) = conn.query_row(
            "SELECT
                    COUNT(CASE WHEN outcome = 'a' THEN 1 END),
                    COUNT(CASE WHEN outcome = 'r' THEN 1 END),
                    COUNT(CASE WHEN outcome = 'e' THEN 1 END)
                 FROM summarizer_events WHERE ts >= ?1 AND ts < ?2",
            rusqlite::params![since, until],
            |row| {
                Ok((
                    row.get::<_, i64>(0)? as u64,
                    row.get::<_, i64>(1)? as u64,
                    row.get::<_, i64>(2)? as u64,
                ))
            },
        )?;
        let (won_local, won_api) = if column_exists(conn, "summarizer_events", "engine")? {
            conn.query_row(
                "SELECT
                        COUNT(CASE WHEN outcome = 'a' AND engine = 'local' THEN 1 END),
                        COUNT(CASE WHEN outcome = 'a' AND engine = 'api'   THEN 1 END)
                     FROM summarizer_events WHERE ts >= ?1 AND ts < ?2",
                rusqlite::params![since, until],
                |row| Ok((row.get::<_, i64>(0)? as u64, row.get::<_, i64>(1)? as u64)),
            )?
        } else {
            (0, 0)
        };
        // Chain collapses in-window. Column-tolerant: a READ_ONLY connection can
        // predate the `collapsed` migration → 0 (the correct reading for old data).
        let collapses = if column_exists(conn, "summarizer_events", "collapsed")? {
            conn.query_row(
                "SELECT COUNT(CASE WHEN collapsed = 1 THEN 1 END)
                     FROM summarizer_events WHERE ts >= ?1 AND ts < ?2",
                rusqlite::params![since, until],
                |row| Ok(row.get::<_, i64>(0)? as u64),
            )?
        } else {
            0
        };
        (accepted, rejected, errored, won_local, won_api, collapses)
    } else {
        (0, 0, 0, 0, 0, 0)
    };

    // Upstream failures in-window (timeout 't' / connection error 'e'). Same
    // older-ledger tolerance as summarizer_events above.
    let (upstream_errors, upstream_timeouts) = if table_exists(conn, "upstream_errors")? {
        conn.query_row(
            "SELECT
                COUNT(CASE WHEN kind = 'e' THEN 1 END),
                COUNT(CASE WHEN kind = 't' THEN 1 END)
             FROM upstream_errors WHERE ts >= ?1 AND ts < ?2",
            rusqlite::params![since, until],
            |row| Ok((row.get::<_, i64>(0)? as u64, row.get::<_, i64>(1)? as u64)),
        )?
    } else {
        (0, 0)
    };

    // Post-prune HTTP errors + total upstream HTTP errors. Column-tolerant: a
    // READ_ONLY connection can predate the `response_status` migration that the
    // read-write `Ledger::open` applies → treat as zero rather than erroring.
    let (post_prune_errors, upstream_http_errors) =
        if column_exists(conn, "requests", "response_status")? {
            conn.query_row(
                "SELECT
                    COUNT(CASE WHEN response_status >= 400 AND strategies != '' THEN 1 END),
                    COUNT(CASE WHEN response_status >= 400 THEN 1 END)
                 FROM requests WHERE ts >= ?1 AND ts < ?2",
                rusqlite::params![since, until],
                |row| Ok((row.get::<_, i64>(0)? as u64, row.get::<_, i64>(1)? as u64)),
            )?
        } else {
            (0, 0)
        };

    Ok(Report {
        total_requests: total_requests as u64,
        total_in_bytes: total_in_bytes as u64,
        total_out_bytes: total_out_bytes as u64,
        per_day,
        per_strategy,
        per_strategy_bytes,
        cache_stability: CacheStability {
            no_strategy_total,
            no_strategy_stable,
            ratio,
        },
        response_metrics: ResponseMetrics {
            requests_with_ttft,
            avg_ttft_us: avg_ttft_us_raw,
            total_input_tokens,
            total_cache_read_input_tokens,
            total_cache_creation_input_tokens,
            total_output_tokens,
            requests_with_applied_edits,
            total_applied_edits_cleared_thinking_turns,
            total_applied_edits_cleared_tool_uses,
            total_applied_edits_cleared_input_tokens,
        },
        requests_with_strategy,
        summarizer_accepted,
        summarizer_rejected,
        summarizer_errored,
        summarizer_accepted_local,
        summarizer_accepted_api,
        summarizer_collapses,
        upstream_errors,
        upstream_timeouts,
        post_prune_errors,
        upstream_http_errors,
        db_path,
    })
}

/// Expand a leading `~/` to `$HOME`. Public so the CLI can check whether the
/// ledger file exists before reporting.
pub fn resolve_path(raw: &str) -> PathBuf {
    if let Some(rest) = raw.strip_prefix("~/") {
        if let Ok(home) = std::env::var("HOME") {
            return Path::new(&home).join(rest);
        }
    }
    PathBuf::from(raw)
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn body(system: &str, n: usize) -> Vec<u8> {
        let msgs: Vec<Value> = (0..n)
            .map(|i| json!({"role": "user", "content": format!("m{i}")}))
            .collect();
        serde_json::to_vec(&json!({
            "model": "claude", "system": system, "tools": [], "messages": msgs
        }))
        .unwrap()
    }

    /// SPIKE §9 guard: mutating `messages[]` never changes the prefix hash;
    /// changing a prefix field (system) does.
    #[test]
    fn prefix_hash_excludes_messages() {
        let a = prefix_hash(&body("sys", 3));
        let b = prefix_hash(&body("sys", 9)); // different messages, same prefix
        assert_eq!(a, b, "messages count must not affect the prefix hash");
        let c = prefix_hash(&body("other", 3)); // changed prefix
        assert_ne!(a, c, "changing system must change the prefix hash");
    }

    #[test]
    fn report_derived_figures_are_single_sourced() {
        let r = Report {
            total_requests: 3,
            total_in_bytes: 1000,
            total_out_bytes: 600,
            per_day: vec![],
            per_strategy: vec![],
            per_strategy_bytes: vec![],
            cache_stability: CacheStability {
                no_strategy_total: 0,
                no_strategy_stable: 0,
                ratio: 0.0,
            },
            response_metrics: ResponseMetrics::default(),
            requests_with_strategy: 0,
            summarizer_accepted: 0,
            summarizer_rejected: 0,
            summarizer_errored: 0,
            summarizer_accepted_local: 0,
            summarizer_accepted_api: 0,
            summarizer_collapses: 0,
            upstream_errors: 0,
            upstream_timeouts: 0,
            post_prune_errors: 0,
            upstream_http_errors: 0,
            db_path: std::path::PathBuf::from(":memory:"),
        };
        assert_eq!(r.bytes_saved(), 400);
        assert!((r.reduction_pct() - 40.0).abs() < 1e-9);
        assert_eq!(r.est_tokens_removed(), 100); // 400 / 4
        // Empty ledger: no division by zero.
        let empty = Report {
            total_in_bytes: 0,
            total_out_bytes: 0,
            ..r
        };
        assert_eq!(empty.reduction_pct(), 0.0);
        assert_eq!(empty.est_tokens_removed(), 0);
    }

    #[test]
    fn build_report_aggregates_summarizer_events_in_window() {
        let conn = Connection::open_in_memory().unwrap();
        init_schema(&conn).unwrap();
        // 2 accepted (1 local, 1 api), 2 rejected, 1 error at ts=1000; one stray
        // local accept far outside. Third accepted uses explicit 'api' engine.
        for (ts, outcome, engine) in [
            (1000, "a", "local"),
            (1000, "a", "api"),
            (1000, "r", "model-free"),
            (1000, "r", "model-free"),
            (1000, "e", "model-free"),
            (9_999_999, "a", "local"),
        ] {
            conn.execute(
                "INSERT INTO summarizer_events (ts, outcome, engine) VALUES (?1, ?2, ?3)",
                rusqlite::params![ts as i64, outcome, engine],
            )
            .unwrap();
        }
        // A window around ts=1000 sees the 5; the far-future accept is excluded.
        let r = build_report(&conn, PathBuf::from(":memory:"), 500, 2000).unwrap();
        assert_eq!(r.summarizer_accepted, 2);
        assert_eq!(r.summarizer_rejected, 2);
        assert_eq!(r.summarizer_errored, 1);
        assert_eq!(r.summarizer_accepted_local, 1);
        assert_eq!(r.summarizer_accepted_api, 1);
        // All-time sees the stray local accept too.
        let all = build_report(&conn, PathBuf::from(":memory:"), i64::MIN, i64::MAX).unwrap();
        assert_eq!(all.summarizer_accepted, 3);
        assert_eq!(all.summarizer_accepted_local, 2);
        assert_eq!(all.summarizer_accepted_api, 1);
    }

    #[test]
    fn build_report_counts_chain_collapses_in_window() {
        let conn = Connection::open_in_memory().unwrap();
        init_schema(&conn).unwrap();
        // Two accepted summaries; one of them a chain COLLAPSE. Plus a collapse
        // far outside the window (must be excluded).
        for (ts, collapsed) in [(1000_i64, 0), (1000, 1), (9_999_999, 1)] {
            conn.execute(
                "INSERT INTO summarizer_events (ts, outcome, engine, collapsed) VALUES (?1, 'a', 'api', ?2)",
                rusqlite::params![ts, collapsed],
            )
            .unwrap();
        }
        let r = build_report(&conn, PathBuf::from(":memory:"), 500, 2000).unwrap();
        assert_eq!(r.summarizer_accepted, 2, "both in-window accepts counted");
        assert_eq!(
            r.summarizer_collapses, 1,
            "only the in-window collapse counts"
        );
        let all = build_report(&conn, PathBuf::from(":memory:"), i64::MIN, i64::MAX).unwrap();
        assert_eq!(all.summarizer_collapses, 2, "all-time sees both collapses");
    }

    #[test]
    fn record_summarizer_event_persists_collapse_flag() {
        // The fire-and-forget recorder writes the collapsed flag; build_report reads it.
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("led.db");
            let ledger = Ledger::open(path.to_str().unwrap(), 7);
            ledger.record_summarizer_event(1000, 'a', "api", true);
            ledger.record_summarizer_event(1000, 'a', "local", false);
            // Poll until both fire-and-forget inserts land. The lock guard is scoped
            // to a block so it is dropped BEFORE the await (clippy await_holding_lock
            // doesn't track explicit drop()).
            for _ in 0..100 {
                let n = {
                    let conn = ledger.conn.as_ref().unwrap().lock().unwrap();
                    conn.query_row("SELECT COUNT(*) FROM summarizer_events", [], |r| {
                        r.get::<_, i64>(0)
                    })
                    .unwrap()
                };
                if n >= 2 {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
            let collapses = {
                let conn = ledger.conn.as_ref().unwrap().lock().unwrap();
                conn.query_row(
                    "SELECT COUNT(CASE WHEN collapsed = 1 THEN 1 END) FROM summarizer_events",
                    [],
                    |r| r.get::<_, i64>(0),
                )
                .unwrap()
            };
            assert_eq!(collapses, 1, "exactly one event recorded the collapse flag");
        });
    }

    #[test]
    fn build_report_aggregates_upstream_errors_in_window() {
        let conn = Connection::open_in_memory().unwrap();
        init_schema(&conn).unwrap();
        // 2 connection errors + 1 timeout at ts=1000; one stray timeout far outside.
        for (ts, kind) in [(1000, "e"), (1000, "e"), (1000, "t"), (9_999_999, "t")] {
            conn.execute(
                "INSERT INTO upstream_errors (ts, kind) VALUES (?1, ?2)",
                rusqlite::params![ts as i64, kind],
            )
            .unwrap();
        }
        let r = build_report(&conn, PathBuf::from(":memory:"), 500, 2000).unwrap();
        assert_eq!(r.upstream_errors, 2);
        assert_eq!(r.upstream_timeouts, 1);
        let all = build_report(&conn, PathBuf::from(":memory:"), i64::MIN, i64::MAX).unwrap();
        assert_eq!(all.upstream_timeouts, 2); // includes the stray
    }

    #[test]
    fn prefix_hash_is_deterministic_and_handles_garbage() {
        assert_eq!(prefix_hash(&body("s", 1)), prefix_hash(&body("s", 1)));
        // Non-JSON / no-messages bodies hash a stable empty object.
        assert_eq!(prefix_hash(b"not json"), prefix_hash(b"\xff\x00"));
    }

    #[test]
    fn prefix_hash_and_model_returns_model_and_matches_prefix_hash() {
        let b = body("sys", 3);
        let (h, model) = prefix_hash_and_model(&b);
        assert_eq!(
            model.as_deref(),
            Some("claude"),
            "model extracted from body"
        );
        assert_eq!(h, prefix_hash(&b), "hash half matches prefix_hash");
        // Garbage → no model, stable empty-object hash.
        let (hg, mg) = prefix_hash_and_model(b"not json");
        assert_eq!(mg, None);
        assert_eq!(hg, prefix_hash(b"not json"));
    }

    fn rec(ts: i64, strategies: &str, hin: &str, hout: &str) -> Record {
        Record {
            ts,
            session_id: Some("sess".to_owned()),
            model: None,
            in_bytes: 1000,
            out_bytes: 600,
            strategies: strategies.to_owned(),
            strategy_bytes: strategies
                .split(',')
                .filter(|s| !s.is_empty())
                .map(|s| format!("{s}:100"))
                .collect::<Vec<_>>()
                .join(","),
            prefix_hash_in: hin.to_owned(),
            prefix_hash_out: hout.to_owned(),
            ttft_us: 0,
            input_tokens: 0,
            cache_read_input_tokens: 0,
            cache_creation_input_tokens: 0,
            output_tokens: 0,
            applied_edits_cleared_thinking_turns: 0,
            applied_edits_cleared_tool_uses: 0,
            applied_edits_cleared_input_tokens: 0,
            response_status: 0,
        }
    }

    fn temp_ledger() -> (Connection, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let conn = Connection::open(dir.path().join("ledger.db")).unwrap();
        init_schema(&conn).unwrap();
        (conn, dir)
    }

    #[test]
    fn insert_and_report_roundtrip() {
        let (conn, dir) = temp_ledger();
        // Two stable no-op rows, one firing row, one UNSTABLE no-op (the bug).
        insert(&conn, &rec(1000, "", "aaa", "aaa")).unwrap();
        insert(&conn, &rec(2000, "", "bbb", "bbb")).unwrap();
        insert(&conn, &rec(3000, "sliding_window", "ccc", "ccc")).unwrap();
        insert(&conn, &rec(4000, "", "ddd", "XXX")).unwrap(); // prefix changed w/o strategy
        drop(conn);

        let report = Ledger::report(dir.path().join("ledger.db").to_str().unwrap()).unwrap();
        assert_eq!(report.total_requests, 4);
        assert_eq!(report.total_in_bytes, 4000);
        assert_eq!(report.total_out_bytes, 2400);
        assert_eq!(report.bytes_saved(), 1600);
        assert_eq!(report.per_strategy, vec![("sliding_window".to_owned(), 1)]);
        // 3 no-strategy rows, 2 stable → the offender drags the ratio below 1.0.
        assert_eq!(report.cache_stability.no_strategy_total, 3);
        assert_eq!(report.cache_stability.no_strategy_stable, 2);
        assert!((report.cache_stability.ratio - 2.0 / 3.0).abs() < 1e-9);
    }

    #[test]
    fn report_tolerates_older_table_missing_engine_column() {
        // A READ_ONLY report against a pre-Q4 summarizer_events table (no
        // `engine` column) must not crash — outcome counts still aggregate,
        // engine-split counts read as zero.
        let (conn, dir) = temp_ledger();
        insert(&conn, &rec(1000, "bloat_cap", "a", "a")).unwrap();
        conn.execute_batch(
            "DROP TABLE summarizer_events;
             CREATE TABLE summarizer_events (
                 id INTEGER PRIMARY KEY AUTOINCREMENT,
                 ts INTEGER NOT NULL,
                 outcome TEXT NOT NULL
             );
             INSERT INTO summarizer_events (ts, outcome) VALUES (1000, 'a');",
        )
        .unwrap();
        drop(conn);

        let report = Ledger::report(dir.path().join("ledger.db").to_str().unwrap()).unwrap();
        assert_eq!(report.summarizer_accepted, 1);
        assert_eq!(report.summarizer_accepted_local, 0);
        assert_eq!(report.summarizer_accepted_api, 0);
    }

    #[test]
    fn report_tolerates_older_ledger_missing_event_tables() {
        // An older ledger created before summarizer_events/upstream_errors
        // existed must still produce a report (zeros), not a raw SQLite error.
        let (conn, dir) = temp_ledger();
        insert(&conn, &rec(1000, "bloat_cap", "a", "a")).unwrap();
        conn.execute_batch("DROP TABLE summarizer_events; DROP TABLE upstream_errors;")
            .unwrap();
        drop(conn);

        let report = Ledger::report(dir.path().join("ledger.db").to_str().unwrap()).unwrap();
        assert_eq!(report.total_requests, 1);
        assert_eq!(report.summarizer_accepted, 0);
        assert_eq!(report.summarizer_rejected, 0);
        assert_eq!(report.summarizer_errored, 0);
        assert_eq!(report.upstream_errors, 0);
        assert_eq!(report.upstream_timeouts, 0);
    }

    /// An older ledger with `summarizer_events` but WITHOUT the `engine` column
    /// must not crash on `Ledger::open` (which calls `add_missing_columns`). After
    /// migration, a new row should record the engine and be queryable.
    #[test]
    fn open_migrates_missing_engine_column() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ledger.db");
        // Create a ledger WITHOUT the engine column by using a bare CREATE TABLE.
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE summarizer_events (
                    id INTEGER PRIMARY KEY,
                    ts INTEGER NOT NULL,
                    outcome TEXT NOT NULL
                ) STRICT;
                INSERT INTO summarizer_events (ts, outcome) VALUES (9_999_999_999, 'a');",
            )
            .unwrap();
        }
        // Ledger::open should run add_missing_columns and succeed.
        let l = Ledger::open(path.to_str().unwrap(), 365);
        assert!(l.conn.is_some(), "ledger must open after column migration");
        // Old row should be readable with the DEFAULT engine value.
        {
            let conn =
                Connection::open_with_flags(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
                    .unwrap();
            let engine: String = conn
                .query_row(
                    "SELECT engine FROM summarizer_events WHERE ts = 9999999999",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(
                engine, "model-free",
                "migrated row must default to model-free"
            );
        }
    }

    #[test]
    fn report_window_filters_by_ts() {
        let (conn, dir) = temp_ledger();
        insert(&conn, &rec(1000, "bloat_cap", "a", "a")).unwrap();
        insert(&conn, &rec(2000, "bloat_cap", "b", "b")).unwrap();
        insert(&conn, &rec(3000, "bloat_cap", "c", "c")).unwrap();
        drop(conn);
        let path = dir.path().join("ledger.db");
        let p = path.to_str().unwrap();

        assert_eq!(Ledger::report(p).unwrap().total_requests, 3); // all-time
        // half-open [0, 2500) → ts 1000 + 2000 only
        let w = Ledger::report_window(p, 0, 2500).unwrap();
        assert_eq!(w.total_requests, 2);
        assert_eq!(w.total_in_bytes, 2000);
        assert_eq!(w.per_strategy, vec![("bloat_cap".to_owned(), 2)]);
        assert_eq!(w.per_strategy_bytes, vec![("bloat_cap".to_owned(), 200)]);
        // [2500, MAX) → only ts 3000
        assert_eq!(
            Ledger::report_window(p, 2500, i64::MAX)
                .unwrap()
                .total_requests,
            1
        );
        // empty window
        assert_eq!(
            Ledger::report_window(p, 5000, 6000).unwrap().total_requests,
            0
        );
    }

    #[test]
    fn list_sessions_orders_newest_first_and_filters() {
        let (conn, dir) = temp_ledger();
        let mut a = rec(1000, "bloat_cap", "a", "a");
        a.session_id = Some("alpha-uuid".to_owned());
        a.model = Some("claude-opus".to_owned());
        let mut a2 = rec(2000, "", "c", "c");
        a2.session_id = Some("alpha-uuid".to_owned());
        a2.model = Some("claude-opus".to_owned());
        let mut b = rec(5000, "", "b", "b");
        b.session_id = Some("beta-uuid".to_owned());
        b.model = Some("claude-haiku".to_owned());
        insert(&conn, &a).unwrap();
        insert(&conn, &a2).unwrap();
        insert(&conn, &b).unwrap();
        drop(conn);
        let pb = dir.path().join("ledger.db");
        let p = pb.to_str().unwrap();

        let all = Ledger::list_sessions(p, None, 20).unwrap();
        assert_eq!(all.len(), 2, "two distinct sessions");
        assert_eq!(
            all[0].session_id, "beta-uuid",
            "newest last-activity (ts 5000) first"
        );
        assert_eq!(all[1].session_id, "alpha-uuid");
        assert_eq!(all[1].requests, 2, "alpha aggregates its two rows");
        // alpha: in 2*1000, out 2*600 → 40% reduction.
        assert!((all[1].reduction_pct() - 40.0).abs() < 1e-9);

        // Filter by model substring, then by session-id substring (content-free).
        let haiku = Ledger::list_sessions(p, Some("haiku"), 20).unwrap();
        assert_eq!(haiku.len(), 1);
        assert_eq!(haiku[0].session_id, "beta-uuid");
        let alpha = Ledger::list_sessions(p, Some("alpha"), 20).unwrap();
        assert_eq!(alpha.len(), 1);
        assert_eq!(alpha[0].session_id, "alpha-uuid");
        assert!(
            Ledger::list_sessions(p, Some("nomatch"), 20)
                .unwrap()
                .is_empty()
        );
        // Limit caps the row count.
        assert_eq!(Ledger::list_sessions(p, None, 1).unwrap().len(), 1);
    }

    #[test]
    fn list_sessions_missing_ledger_is_empty() {
        assert!(
            Ledger::list_sessions("/proc/trimwire-nope/ledger.db", None, 20)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn session_report_groups_by_model_and_resolves_last() {
        let (conn, dir) = temp_ledger();
        let p = dir.path().join("ledger.db");
        let p = p.to_str().unwrap();

        // A row with an explicit session_id + model + token fields.
        let row =
            |ts: i64, sess: &str, model: Option<&str>, input: i64, cread: i64, ccreate: i64| {
                Record {
                    ts,
                    session_id: Some(sess.to_owned()),
                    model: model.map(str::to_owned),
                    in_bytes: 1000,
                    out_bytes: 600,
                    strategies: String::new(),
                    strategy_bytes: String::new(),
                    prefix_hash_in: "h".to_owned(),
                    prefix_hash_out: "h".to_owned(),
                    ttft_us: 0,
                    input_tokens: input,
                    cache_read_input_tokens: cread,
                    cache_creation_input_tokens: ccreate,
                    output_tokens: 0,
                    applied_edits_cleared_thinking_turns: 0,
                    applied_edits_cleared_tool_uses: 0,
                    applied_edits_cleared_input_tokens: 0,
                    response_status: 0,
                }
            };
        // Session s1: two Opus rows + one Haiku row. Then s2 (one Opus row) is
        // inserted LAST so it wins the "last" (MAX(id)) resolution.
        insert(&conn, &row(1000, "s1", Some("opus"), 1000, 800, 50)).unwrap();
        insert(&conn, &row(2000, "s1", Some("opus"), 2000, 1900, 10)).unwrap();
        insert(&conn, &row(3000, "s1", Some("haiku"), 500, 0, 200)).unwrap();
        insert(&conn, &row(4000, "s2", Some("opus"), 100, 0, 100)).unwrap();
        drop(conn);

        // Explicit session → grouped by model, sorted by model name.
        let rep = Ledger::session_report(p, Some("s1")).unwrap().unwrap();
        assert_eq!(rep.session_id, "s1");
        assert_eq!(rep.per_model.len(), 2, "haiku + opus");
        assert_eq!(rep.per_model[0].model.as_deref(), Some("haiku"));
        assert_eq!(rep.per_model[1].model.as_deref(), Some("opus"));
        let opus = &rep.per_model[1];
        assert_eq!(opus.turns, 2);
        assert_eq!(opus.input_tokens, 3000);
        assert_eq!(opus.cache_read_input_tokens, 2700);
        assert_eq!(opus.cache_creation_input_tokens, 60);
        // total input = 3000 + 2700 + 60 = 5760; cache-hit = 2700/5760 = 46.875%.
        assert_eq!(opus.total_input_tokens(), 5760);
        assert!((opus.cache_hit_pct() - 46.875).abs() < 1e-9);

        // post_prune_errors: no 400+pruned rows in this test → 0 for all sessions.
        assert_eq!(rep.post_prune_errors, 0, "no 4xx rows → 0 per-session errors");

        // None → "last" resolves to s2 by insert order (MAX(id)), not MAX(ts).
        let last = Ledger::session_report(p, None).unwrap().unwrap();
        assert_eq!(last.session_id, "s2");
        assert_eq!(last.per_model.len(), 1);
        assert_eq!(last.post_prune_errors, 0);

        // Unknown session → None (not an error).
        assert!(Ledger::session_report(p, Some("nope")).unwrap().is_none());
    }

    /// `SessionReport.post_prune_errors` counts only rows for the given session
    /// where response_status >= 400 AND strategies is non-empty; other sessions
    /// and non-qualifying rows must not be counted.
    #[test]
    fn session_report_post_prune_errors_per_session() {
        let (conn, dir) = temp_ledger();
        let p = dir.path().join("ledger.db");
        let p = p.to_str().unwrap();

        // Session "sa": one pruned-400 row (should count) + one non-pruned-400 row (shouldn't).
        let mut r1 = rec(1000, "sliding_window", "a", "a");
        r1.session_id = Some("sa".to_owned());
        r1.response_status = 400;
        let mut r2 = rec(2000, "", "b", "b");
        r2.session_id = Some("sa".to_owned());
        r2.response_status = 400;

        // Session "sb": pruned-400 row (should count for sb, NOT for sa).
        let mut r3 = rec(3000, "bloat_cap", "c", "c");
        r3.session_id = Some("sb".to_owned());
        r3.response_status = 400;

        // Session "sc": pruned row with 200 (default=0) → no error for sc.
        let mut r4 = rec(4000, "bloat_cap", "d", "d");
        r4.session_id = Some("sc".to_owned());

        insert(&conn, &r1).unwrap();
        insert(&conn, &r2).unwrap();
        insert(&conn, &r3).unwrap();
        insert(&conn, &r4).unwrap();
        drop(conn);

        let rep_a = Ledger::session_report(p, Some("sa")).unwrap().unwrap();
        assert_eq!(
            rep_a.post_prune_errors, 1,
            "sa: only r1 (pruned+400); r2 has no strategies"
        );

        let rep_b = Ledger::session_report(p, Some("sb")).unwrap().unwrap();
        assert_eq!(rep_b.post_prune_errors, 1, "sb: r3 qualifies");

        let rep_c = Ledger::session_report(p, Some("sc")).unwrap().unwrap();
        assert_eq!(rep_c.post_prune_errors, 0, "sc: pruned but status 0 (ok)");
    }

    #[test]
    fn per_strategy_bytes_aggregate() {
        let (conn, dir) = temp_ledger();
        // rec() synthesizes "name:100" per fired strategy.
        insert(&conn, &rec(1000, "bloat_cap,cross_turn_dedup", "a", "a")).unwrap();
        insert(&conn, &rec(2000, "bloat_cap", "b", "b")).unwrap();
        drop(conn);
        let report = Ledger::report(dir.path().join("ledger.db").to_str().unwrap()).unwrap();
        let by = |n: &str| {
            report
                .per_strategy_bytes
                .iter()
                .find(|(m, _)| m == n)
                .map_or(0, |(_, b)| *b)
        };
        assert_eq!(by("bloat_cap"), 200, "summed across both rows");
        assert_eq!(by("cross_turn_dedup"), 100);
    }

    #[test]
    fn prune_drops_old_rows() {
        let (conn, _dir) = temp_ledger();
        let old = now_secs() - 400 * 86_400;
        let recent = now_secs() - 10 * 86_400;
        insert(&conn, &rec(old, "", "a", "a")).unwrap();
        insert(&conn, &rec(recent, "", "b", "b")).unwrap();
        prune(&conn, 365).unwrap();
        let remaining: i64 = conn
            .query_row("SELECT COUNT(*) FROM requests", [], |r| r.get(0))
            .unwrap();
        assert_eq!(remaining, 1, "row older than 365 days should be pruned");
    }

    /// Rows written in one "session" survive a drop+reopen, and `open()`
    /// prunes stale rows at startup (acceptance: persists across restarts).
    #[test]
    fn open_prunes_old_rows_and_persists_recent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ledger.db");
        let p = path.to_str().unwrap();
        {
            let conn = Connection::open(&path).unwrap();
            init_schema(&conn).unwrap();
            insert(&conn, &rec(now_secs() - 400 * 86_400, "", "a", "a")).unwrap();
            insert(&conn, &rec(now_secs() - 86_400, "", "b", "b")).unwrap();
        } // conn dropped = "gateway restart"
        // Reopen through the real public API, which prunes at startup.
        let _ledger = Ledger::open(p, 365);
        let report = Ledger::report(p).unwrap();
        assert_eq!(
            report.total_requests, 1,
            "old row pruned; recent row persists across reopen"
        );
    }

    /// Per-strategy counts match whole CSV tokens, never substrings — the
    /// reason the query uses comma-wrapped `instr` instead of `LIKE '%x%'`.
    #[test]
    fn per_strategy_counts_csv_tokens() {
        let (conn, dir) = temp_ledger();
        insert(&conn, &rec(1, "sliding_window,image_strip", "a", "a")).unwrap();
        insert(&conn, &rec(2, "image_strip", "b", "b")).unwrap();
        // A name that CONTAINS "sliding_window" as a substring must NOT be
        // counted as `sliding_window` (the substring trap the fix prevents).
        insert(&conn, &rec(3, "extended_sliding_window", "c", "c")).unwrap();
        drop(conn);
        let r = Ledger::report(dir.path().join("ledger.db").to_str().unwrap()).unwrap();
        assert!(r.per_strategy.contains(&("sliding_window".to_owned(), 1)));
        assert!(r.per_strategy.contains(&("image_strip".to_owned(), 2)));
    }

    /// Guard: `KNOWN_STRATEGIES` must mirror the names `strategies::run`
    /// pushes. If a strategy is added there but not here, it silently
    /// disappears from `trimwire stats` — this test is the tripwire.
    #[test]
    fn known_strategies_is_the_expected_set() {
        // Mirror of the names `strategies::run` pushes, IN ORDER (incl. the opt-in
        // simhash_dedup, which fires after stale_reads when enabled — omitting it
        // silently zeroed its per-strategy count in `stats`).
        assert_eq!(
            KNOWN_STRATEGIES,
            [
                "failed_input_purge",
                "stale_input_cap",
                "cross_turn_dedup",
                "stale_reads",
                "simhash_dedup",
                "bloat_cap",
                "sliding_window",
                "image_strip",
                "thinking_strip",
            ]
        );
    }

    #[test]
    fn disabled_ledger_record_is_noop() {
        // No panic, no runtime needed: degraded ledger returns before spawn.
        Ledger::disabled().record(rec(1, "", "a", "a"));
    }

    #[test]
    fn open_bad_path_degrades() {
        // A path whose parent cannot be created → degraded, not a panic. Use a
        // regular FILE where a directory component is needed, which fails
        // identically on every platform. (The previous `/proc/...` path only
        // fails on Unix — Windows happily creates `C:\proc\...`, so the ledger
        // opened and the test flaked there.)
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("afile");
        std::fs::write(&file, b"x").unwrap();
        let bad = file.join("ledger.db"); // parent `afile` is a file, not a dir
        let l = Ledger::open(bad.to_str().unwrap(), 365);
        assert!(l.conn.is_none());
    }

    /// Response-side metrics round-trip through insert+report.
    #[test]
    fn response_metrics_roundtrip() {
        let (conn, dir) = temp_ledger();
        // Row 1: has TTFT + tokens + applied_edits.
        let r1 = Record {
            ts: 1000,
            session_id: Some("s".to_owned()),
            model: Some("claude-opus-4-8".to_owned()),
            in_bytes: 500,
            out_bytes: 500,
            strategies: String::new(),
            strategy_bytes: String::new(),
            prefix_hash_in: "a".to_owned(),
            prefix_hash_out: "a".to_owned(),
            ttft_us: 123_000,
            input_tokens: 1000,
            cache_read_input_tokens: 400,
            cache_creation_input_tokens: 100,
            output_tokens: 250,
            applied_edits_cleared_thinking_turns: 2,
            applied_edits_cleared_tool_uses: 5,
            applied_edits_cleared_input_tokens: 3000,
            response_status: 0,
        };
        // Row 2: has TTFT + tokens, no applied_edits.
        let r2 = Record {
            ts: 2000,
            session_id: Some("s".to_owned()),
            model: Some("claude-haiku-4-5".to_owned()),
            in_bytes: 200,
            out_bytes: 200,
            strategies: String::new(),
            strategy_bytes: String::new(),
            prefix_hash_in: "b".to_owned(),
            prefix_hash_out: "b".to_owned(),
            ttft_us: 77_000,
            input_tokens: 500,
            cache_read_input_tokens: 0,
            cache_creation_input_tokens: 200,
            output_tokens: 100,
            applied_edits_cleared_thinking_turns: 0,
            applied_edits_cleared_tool_uses: 0,
            applied_edits_cleared_input_tokens: 0,
            response_status: 0,
        };
        // Row 3: no TTFT (ttft_ms=0), no tokens.
        let r3 = Record {
            ts: 3000,
            session_id: Some("s".to_owned()),
            model: None,
            in_bytes: 100,
            out_bytes: 100,
            strategies: String::new(),
            strategy_bytes: String::new(),
            prefix_hash_in: "c".to_owned(),
            prefix_hash_out: "c".to_owned(),
            ttft_us: 0,
            input_tokens: 0,
            cache_read_input_tokens: 0,
            cache_creation_input_tokens: 0,
            output_tokens: 0,
            applied_edits_cleared_thinking_turns: 0,
            applied_edits_cleared_tool_uses: 0,
            applied_edits_cleared_input_tokens: 0,
            response_status: 0,
        };
        insert(&conn, &r1).unwrap();
        insert(&conn, &r2).unwrap();
        insert(&conn, &r3).unwrap();
        drop(conn);

        let report = Ledger::report(dir.path().join("ledger.db").to_str().unwrap()).unwrap();
        let rm = &report.response_metrics;

        assert_eq!(rm.requests_with_ttft, 2, "r1 and r2 have ttft_us > 0");
        // avg of 123_000 + 77_000 = 100_000 µs
        assert!(
            (rm.avg_ttft_us - 100_000.0).abs() < 1.0,
            "avg_ttft_us ≈ 100ms"
        );
        assert_eq!(rm.total_input_tokens, 1500);
        assert_eq!(rm.total_cache_read_input_tokens, 400);
        assert_eq!(rm.total_cache_creation_input_tokens, 300);
        assert_eq!(rm.total_output_tokens, 350);
        assert_eq!(
            rm.requests_with_applied_edits, 1,
            "only r1 has applied_edits"
        );
        assert_eq!(rm.total_applied_edits_cleared_thinking_turns, 2);
        assert_eq!(rm.total_applied_edits_cleared_tool_uses, 5);
        assert_eq!(rm.total_applied_edits_cleared_input_tokens, 3000);
    }

    #[test]
    fn session_savings_sums_one_session() {
        let (conn, dir) = temp_ledger();
        insert(&conn, &rec(1, "cross_turn_dedup", "a", "a")).unwrap(); // 1000→600
        insert(&conn, &rec(2, "image_strip", "b", "b")).unwrap(); // 1000→600
        // A different session must not bleed into the total.
        let mut other = rec(3, "", "c", "c");
        other.session_id = Some("other".to_owned());
        insert(&conn, &other).unwrap();
        drop(conn);

        let path = dir.path().join("ledger.db");
        let s = Ledger::session_savings(path.to_str().unwrap(), "sess").unwrap();
        assert_eq!(s.requests, 2);
        assert_eq!(s.in_bytes, 2000);
        assert_eq!(s.out_bytes, 1200);
        assert_eq!(s.saved_bytes(), 800);
        assert!((s.reduction_pct() - 40.0).abs() < 1e-9);

        // Unknown session → zeros (the statusline shows "ready").
        let empty = Ledger::session_savings(path.to_str().unwrap(), "nope").unwrap();
        assert_eq!(empty, SessionSavings::default());
    }

    // -----------------------------------------------------------------------
    // response_status: new column tests
    // -----------------------------------------------------------------------

    /// response_status is stored and read back; post_prune_errors and
    /// upstream_http_errors aggregate it correctly.
    #[test]
    fn response_status_roundtrip() {
        let (conn, dir) = temp_ledger();
        // r1: pruned + 400 → qualifies for both post_prune_errors and
        // upstream_http_errors.
        let mut r1 = rec(1000, "sliding_window", "a", "a");
        r1.response_status = 400;
        // r2: not pruned + 400 → upstream_http_errors only (not post_prune_errors).
        let mut r2 = rec(2000, "", "b", "b");
        r2.response_status = 400;
        // r3: pruned + 200 (status = 0 = "not captured") → neither error counter.
        let r3 = rec(3000, "bloat_cap", "c", "c");
        insert(&conn, &r1).unwrap();
        insert(&conn, &r2).unwrap();
        insert(&conn, &r3).unwrap();
        drop(conn);

        let report = Ledger::report(dir.path().join("ledger.db").to_str().unwrap()).unwrap();
        assert_eq!(
            report.post_prune_errors, 1,
            "only r1: pruned body + upstream 4xx"
        );
        assert_eq!(
            report.upstream_http_errors, 2,
            "r1 + r2: any response_status >= 400"
        );
        assert_eq!(
            report.total_requests, 3,
            "all three rows recorded regardless"
        );
    }

    /// post_prune_errors counts only rows where BOTH response_status >= 400 AND
    /// strategies is non-empty; upstream_http_errors counts any >= 400.
    #[test]
    fn post_prune_errors_aggregate() {
        let (conn, dir) = temp_ledger();
        // 1. Pruned + 400 → both counters.
        let mut r1 = rec(1000, "bloat_cap", "a", "a");
        r1.response_status = 400;
        // 2. Not pruned + 429 (rate-limit) → upstream_http_errors only.
        let mut r2 = rec(2000, "", "b", "b");
        r2.response_status = 429;
        // 3. Pruned + status 0 (200-ish, no error) → neither.
        let r3 = rec(3000, "sliding_window", "c", "c");
        // 4. Not pruned + status 0 → neither.
        let r4 = rec(4000, "", "d", "d");
        insert(&conn, &r1).unwrap();
        insert(&conn, &r2).unwrap();
        insert(&conn, &r3).unwrap();
        insert(&conn, &r4).unwrap();
        drop(conn);

        let report = Ledger::report(dir.path().join("ledger.db").to_str().unwrap()).unwrap();
        assert_eq!(
            report.post_prune_errors, 1,
            "only r1: pruned + 4xx qualifies"
        );
        assert_eq!(
            report.upstream_http_errors, 2,
            "r1 + r2: any >= 400 regardless of strategies"
        );
        // Sanity: a 400 on a non-pruned row should NOT inflate post_prune_errors.
        // Already covered by the assertions above (r2 has empty strategies).
    }

    /// add_missing_columns adds response_status to an older requests table that
    /// lacks it (mirrors open_migrates_missing_engine_column). After the read-write
    /// open, the column is present and old rows default to 0.
    #[test]
    fn open_migrates_missing_response_status_column() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ledger.db");
        // Create a requests table WITHOUT response_status (pre-migration ledger).
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "PRAGMA journal_mode = WAL;
                 CREATE TABLE requests (
                     id              INTEGER PRIMARY KEY,
                     ts              INTEGER NOT NULL,
                     session_id      TEXT,
                     in_bytes        INTEGER NOT NULL DEFAULT 0,
                     out_bytes       INTEGER NOT NULL DEFAULT 0,
                     strategies      TEXT NOT NULL DEFAULT '',
                     prefix_hash_in  TEXT NOT NULL DEFAULT '',
                     prefix_hash_out TEXT NOT NULL DEFAULT '',
                     strategy_bytes  TEXT NOT NULL DEFAULT '',
                     ttft_us         INTEGER NOT NULL DEFAULT 0,
                     input_tokens    INTEGER NOT NULL DEFAULT 0,
                     cache_read_input_tokens    INTEGER NOT NULL DEFAULT 0,
                     cache_creation_input_tokens INTEGER NOT NULL DEFAULT 0,
                     output_tokens   INTEGER NOT NULL DEFAULT 0,
                     applied_edits_cleared_thinking_turns INTEGER NOT NULL DEFAULT 0,
                     applied_edits_cleared_tool_uses      INTEGER NOT NULL DEFAULT 0,
                     applied_edits_cleared_input_tokens   INTEGER NOT NULL DEFAULT 0,
                     model           TEXT
                 ) STRICT;
                 CREATE TABLE summarizer_events (
                     id      INTEGER PRIMARY KEY,
                     ts      INTEGER NOT NULL,
                     outcome TEXT NOT NULL,
                     engine  TEXT NOT NULL DEFAULT 'model-free',
                     collapsed INTEGER NOT NULL DEFAULT 0
                 ) STRICT;
                 CREATE TABLE upstream_errors (
                     id   INTEGER PRIMARY KEY,
                     ts   INTEGER NOT NULL,
                     kind TEXT NOT NULL
                 ) STRICT;
                 INSERT INTO requests
                     (ts, in_bytes, out_bytes, strategies, prefix_hash_in, prefix_hash_out)
                 VALUES (9_000_000_001, 100, 80, 'sliding_window', 'h', 'h');",
            )
            .unwrap();
        }
        // Ledger::open calls add_missing_columns → the column is added.
        let l = Ledger::open(path.to_str().unwrap(), 365);
        assert!(
            l.conn.is_some(),
            "ledger must open after response_status column migration"
        );
        // The pre-existing row should expose the DEFAULT value 0.
        {
            let conn =
                Connection::open_with_flags(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
                    .unwrap();
            let status: i64 = conn
                .query_row(
                    "SELECT response_status FROM requests WHERE ts = 9000000001",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(status, 0, "migrated row must default to 0 (not captured)");
        }
    }

    /// A READ_ONLY report against a requests table that lacks response_status
    /// (pre-migration) must not crash — post_prune_errors and upstream_http_errors
    /// come back as 0.
    #[test]
    fn report_tolerates_requests_missing_response_status() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ledger.db");
        // Build an older-style schema without response_status.
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "PRAGMA journal_mode = WAL;
                 CREATE TABLE requests (
                     id              INTEGER PRIMARY KEY,
                     ts              INTEGER NOT NULL,
                     session_id      TEXT,
                     in_bytes        INTEGER NOT NULL DEFAULT 0,
                     out_bytes       INTEGER NOT NULL DEFAULT 0,
                     strategies      TEXT NOT NULL DEFAULT '',
                     prefix_hash_in  TEXT NOT NULL DEFAULT '',
                     prefix_hash_out TEXT NOT NULL DEFAULT '',
                     strategy_bytes  TEXT NOT NULL DEFAULT '',
                     ttft_us         INTEGER NOT NULL DEFAULT 0,
                     input_tokens    INTEGER NOT NULL DEFAULT 0,
                     cache_read_input_tokens    INTEGER NOT NULL DEFAULT 0,
                     cache_creation_input_tokens INTEGER NOT NULL DEFAULT 0,
                     output_tokens   INTEGER NOT NULL DEFAULT 0,
                     applied_edits_cleared_thinking_turns INTEGER NOT NULL DEFAULT 0,
                     applied_edits_cleared_tool_uses      INTEGER NOT NULL DEFAULT 0,
                     applied_edits_cleared_input_tokens   INTEGER NOT NULL DEFAULT 0,
                     model           TEXT
                 ) STRICT;
                 CREATE TABLE summarizer_events (
                     id INTEGER PRIMARY KEY, ts INTEGER NOT NULL,
                     outcome TEXT NOT NULL,
                     engine  TEXT NOT NULL DEFAULT 'model-free',
                     collapsed INTEGER NOT NULL DEFAULT 0
                 ) STRICT;
                 CREATE TABLE upstream_errors (
                     id INTEGER PRIMARY KEY, ts INTEGER NOT NULL, kind TEXT NOT NULL
                 ) STRICT;
                 INSERT INTO requests
                     (ts, in_bytes, out_bytes, strategies, prefix_hash_in, prefix_hash_out)
                 VALUES (1000, 1000, 600, 'bloat_cap', 'a', 'a');",
            )
            .unwrap();
        }
        // Read-only path (build_report) must tolerate the absent column → zeros.
        let report = Ledger::report(path.to_str().unwrap()).unwrap();
        assert_eq!(report.total_requests, 1, "request row visible");
        assert_eq!(report.post_prune_errors, 0, "absent column → 0");
        assert_eq!(report.upstream_http_errors, 0, "absent column → 0");
    }
}
