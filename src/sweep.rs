//! `trimwire sweep` — on-disk Claude Code session-JSONL cleanup (the cozempic /
//! Tier-3 model, ported from `pocs/tier2-sweep.py`).
//!
//! A session JSONL is line-delimited records `{type, uuid, message:{role,
//! content:[...]}}`. Unlike the gateway (which mutates the live request), this
//! rewrites the on-disk history so resumed sessions start leaner. Two safe
//! mutations:
//!   1. Drop *empty* thinking blocks (signature-only — Anthropic rejects these
//!      on replay with "each thinking block must contain thinking").
//!   2. Purge the `input` of tool calls whose result `is_error` (keep name+id;
//!      the error text in the result is preserved).
//!
//! Safety: only mutated lines are re-serialized (unchanged lines — including
//! blank lines and the original's trailing-newline state — are copied verbatim:
//! minimal diff, no key reordering). Write to a temp file + fsync, then re-read
//! the live file: if it changed *at all* since our snapshot (concurrent append,
//! compaction, truncation), abort and leave it untouched — sweep is for
//! inactive sessions. Otherwise back up to `<name>.bak.<ts>`, atomically
//! rename, fsync the dir, and keep the most recent `BAK_RETAIN` backups. A
//! clean file (nothing to mutate) is a true no-op: no rewrite, no backup. The
//! temp file is removed on every error path (no orphan left behind).

use std::collections::HashSet;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde_json::Value;

const BAK_RETAIN: usize = 3;
const PREFIX_UUID_COUNT: usize = 5;

/// Outcome of a sweep.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SweepReport {
    pub orig_bytes: u64,
    pub final_bytes: u64,
    pub thinking_dropped: usize,
    pub inputs_purged: usize,
    pub lines: usize,
    pub backup: Option<String>,
}

impl SweepReport {
    pub fn saved(&self) -> i64 {
        self.orig_bytes as i64 - self.final_bytes as i64
    }
}

/// Sweep a single session JSONL file in place (atomic, backed up).
pub fn sweep_file(path: &Path) -> Result<SweepReport> {
    if !path.is_file() {
        bail!("not a file: {}", path.display());
    }
    // Resolve symlinks so the atomic rename replaces the real transcript rather
    // than turning the symlink into a regular file (which would silently leave
    // the real session un-swept).
    let canonical = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let path = canonical.as_path();
    let orig = fs::read(path).with_context(|| format!("read {}", path.display()))?;
    let orig_bytes = orig.len() as u64;

    // Pre-pass: ids of tool calls whose result errored.
    let failed = collect_failed_ids(&orig);

    // Build pass: re-serialize only the lines we actually change. Blank lines
    // and the original's trailing-newline state are preserved byte-for-byte
    // (the "minimal diff / verbatim" guarantee — only mutated records change).
    let (mutated, out, thinking_dropped, inputs_purged, lines) = build_swept(&orig, &failed)?;

    // Nothing to do and the file is fine as-is → don't touch it (no rewrite, no
    // backup churn). Keeps a re-sweep of an already-clean file a true no-op so
    // repeated runs can't evict the pristine first backup.
    if !mutated {
        return Ok(SweepReport {
            orig_bytes,
            final_bytes: orig_bytes,
            thinking_dropped,
            inputs_purged,
            lines,
            backup: None,
        });
    }

    commit_swept(path, &orig, &out, thinking_dropped, inputs_purged, lines)
}

/// Atomically replace `path` with `out`, but only if the live file is still
/// byte-identical to `orig` (the snapshot we built `out` from). Backs up first,
/// removes the temp on any error path, and never leaves the session truncated.
fn commit_swept(
    path: &Path,
    orig: &[u8],
    out: &[u8],
    thinking_dropped: usize,
    inputs_purged: usize,
    lines: usize,
) -> Result<SweepReport> {
    let orig_bytes = orig.len() as u64;

    // Write temp file alongside the original (same dir → atomic rename works).
    // The guard removes the temp on any early return (I/O error), so a failed
    // sweep never leaves an orphan `.tmp.*` in the user's session directory.
    let tmp = temp_path(path);
    let mut guard = TempGuard::new(&tmp);
    write_synced(&tmp, out).with_context(|| format!("write temp {}", tmp.display()))?;

    // Concurrent-modification guard: re-read the live file. If it changed *at
    // all* since our snapshot (append, compaction, truncation), abort and leave
    // it untouched — splicing a partial tail risks silently dropping records
    // written between this re-read and the rename. Sweep is for inactive
    // sessions; an active one safely no-ops here.
    let now = fs::read(path).with_context(|| format!("re-read {}", path.display()))?;
    if now != orig {
        let kind = if first_uuids(&now, PREFIX_UUID_COUNT) != first_uuids(orig, PREFIX_UUID_COUNT) {
            "session prefix changed (compaction/rewrite?)"
        } else {
            "file changed (active session?)"
        };
        bail!(
            "{}: {kind} during sweep — aborted, file untouched",
            path.display()
        );
    }

    // Back up ATOMICALLY (temp + fsync + rename), then atomically replace. A
    // bare `fs::copy` could leave a half-written .bak on crash, which a later
    // `undo` (it picks the newest .bak by mtime) would silently restore as
    // garbage. The original at `path` is still intact here (we read from it).
    let backup = backup_name(path);
    let bak_path = path.with_file_name(&backup);
    let orig_content = fs::read(path).with_context(|| format!("read {}", path.display()))?;
    let bak_tmp = temp_path(&bak_path);
    let mut bak_guard = TempGuard::new(&bak_tmp);
    write_synced(&bak_tmp, &orig_content)
        .with_context(|| format!("write backup temp {}", bak_tmp.display()))?;
    fs::rename(&bak_tmp, &bak_path).with_context(|| format!("backup {}", path.display()))?;
    bak_guard.disarm();
    fs::rename(&tmp, path).with_context(|| format!("atomic rename into {}", path.display()))?;
    guard.disarm(); // rename consumed the temp; nothing left to clean up.
    fsync_dir(path);
    prune_backups(path, BAK_RETAIN);

    let final_bytes = fs::metadata(path).map(|m| m.len()).unwrap_or(orig_bytes);
    Ok(SweepReport {
        orig_bytes,
        final_bytes,
        thinking_dropped,
        inputs_purged,
        lines,
        backup: Some(backup),
    })
}

/// Report what `sweep_file` *would* do without modifying anything on disk.
/// Powers `trimwire sweep --dry-run`.
pub fn dry_run_file(path: &Path) -> Result<SweepReport> {
    if !path.is_file() {
        bail!("not a file: {}", path.display());
    }
    let orig = fs::read(path).with_context(|| format!("read {}", path.display()))?;
    let orig_bytes = orig.len() as u64;
    let failed = collect_failed_ids(&orig);
    let (_mutated, out, thinking_dropped, inputs_purged, lines) = build_swept(&orig, &failed)?;
    Ok(SweepReport {
        orig_bytes,
        final_bytes: out.len() as u64,
        thinking_dropped,
        inputs_purged,
        lines,
        backup: None,
    })
}

/// Claude Code's sessions root: `$CLAUDE_CONFIG_DIR/projects` if set, else
/// `~/.claude/projects`. `None` if `$HOME` is unset.
pub fn sessions_root() -> Option<PathBuf> {
    if let Ok(dir) = std::env::var("CLAUDE_CONFIG_DIR") {
        return Some(PathBuf::from(dir).join("projects"));
    }
    std::env::var("HOME")
        .ok()
        .map(|h| PathBuf::from(h).join(".claude/projects"))
}

/// True when `path` is a sub-agent sidechain transcript rather than a
/// main-session transcript. Claude Code stores sidechains in a `subagents/`
/// subdirectory alongside the parent session directory (e.g.
/// `~/.claude/projects/<project>/<session-uuid>/subagents/agent-<id>.jsonl`).
/// Pure path check — no I/O.
pub fn is_sidechain_transcript(path: &Path) -> bool {
    path.components()
        .any(|c| matches!(c, std::path::Component::Normal(s) if s == "subagents"))
}

/// All `*.jsonl` MAIN-session transcripts under the sessions root (recursive),
/// sorted. Empty if the root doesn't exist. Powers `trimwire sweep list/all`
/// and `preview --last` so users never have to hunt for a path.
///
/// Sub-agent sidechain transcripts (see [`is_sidechain_transcript`]) are
/// skipped: they aren't the sessions users mean when listing/sweeping. A
/// sidechain can still be swept explicitly via `sweep file <path>`.
pub fn session_files() -> Vec<PathBuf> {
    let Some(root) = sessions_root() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut stack = vec![root];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for e in entries.flatten() {
            let p = e.path();
            // Skip symlinked dirs: following them risks a cycle (unbounded walk)
            // and escaping the projects tree. Real session dirs are not symlinks.
            if p.is_dir() && !p.is_symlink() {
                stack.push(p);
            } else if p.extension().and_then(|s| s.to_str()) == Some("jsonl")
                && !is_sidechain_transcript(&p)
            {
                out.push(p);
            }
        }
    }
    out.sort();
    out
}

/// Restore the most recent `<name>.bak.*` over `path` (atomic). Returns the
/// backup that was used; errors if there is none.
pub fn restore_backup(path: &Path) -> Result<PathBuf> {
    let dir = path.parent().context("path has no parent directory")?;
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .context("path has no filename")?;
    let prefix = format!("{name}.bak.");
    let mut newest: Option<(std::time::SystemTime, PathBuf)> = None;
    for e in fs::read_dir(dir)
        .with_context(|| format!("read dir {}", dir.display()))?
        .flatten()
    {
        if e.file_name().to_string_lossy().starts_with(&prefix) {
            if let Ok(m) = e.metadata().and_then(|m| m.modified()) {
                if newest.as_ref().is_none_or(|(t, _)| m > *t) {
                    newest = Some((m, e.path()));
                }
            }
        }
    }
    let (_, bak) =
        newest.with_context(|| format!("no backup (.bak.*) found next to {}", path.display()))?;
    let bytes = fs::read(&bak).with_context(|| format!("read backup {}", bak.display()))?;
    let tmp = temp_path(path);
    let mut guard = TempGuard::new(&tmp); // clean up the temp if the rename fails
    write_synced(&tmp, &bytes).with_context(|| format!("write temp {}", tmp.display()))?;
    fs::rename(&tmp, path).with_context(|| format!("restore into {}", path.display()))?;
    guard.disarm();
    fsync_dir(path);
    Ok(bak)
}

/// Re-serialize only the mutated records of a session, preserving blank lines
/// and the original's trailing-newline state. Returns `(mutated, bytes,
/// thinking_dropped, inputs_purged, lines)`. `mutated` is true iff at least one
/// record changed (blank-line/newline normalization alone does not count).
fn build_swept(
    orig: &[u8],
    failed: &HashSet<String>,
) -> Result<(bool, Vec<u8>, usize, usize, usize)> {
    let ends_with_nl = orig.last() == Some(&b'\n');
    let mut segments: Vec<&[u8]> = orig.split(|&b| b == b'\n').collect();
    // A trailing `\n` yields a final empty segment; drop it so we don't re-add
    // a spurious blank line (and so a no-trailing-newline file stays that way).
    if ends_with_nl {
        segments.pop();
    }

    let mut out: Vec<u8> = Vec::with_capacity(orig.len());
    let mut thinking_dropped = 0;
    let mut inputs_purged = 0;
    let mut lines = 0;
    for (i, raw) in segments.iter().enumerate() {
        lines += 1;
        if i > 0 {
            out.push(b'\n');
        }
        match serde_json::from_slice::<Value>(raw) {
            Ok(mut rec) => {
                let d = strip_empty_thinking(&mut rec);
                let p = purge_failed_input(&mut rec, failed);
                if d + p > 0 {
                    thinking_dropped += d;
                    inputs_purged += p;
                    out.extend_from_slice(&serde_json::to_vec(&rec)?);
                } else {
                    out.extend_from_slice(raw); // unchanged → verbatim
                }
            }
            Err(_) => out.extend_from_slice(raw), // non-JSON / blank line → verbatim
        }
    }
    if ends_with_nl {
        out.push(b'\n');
    }
    let mutated = thinking_dropped + inputs_purged > 0;
    Ok((mutated, out, thinking_dropped, inputs_purged, lines))
}

/// Removes a temp file on drop unless `disarm`ed (e.g. after a successful
/// rename consumes it). Guarantees no orphan `.tmp.*` is left on any error
/// path out of `sweep_file`.
struct TempGuard<'a> {
    path: &'a Path,
    armed: bool,
}

impl<'a> TempGuard<'a> {
    fn new(path: &'a Path) -> Self {
        Self { path, armed: true }
    }
    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for TempGuard<'_> {
    fn drop(&mut self) {
        if self.armed {
            let _ = fs::remove_file(self.path);
        }
    }
}

/// Validate a swept (or any) session JSONL: every line parses, no orphaned
/// tool_results, no empty thinking blocks left. Returns the orphan count + a
/// pass flag.
pub fn validate_file(path: &Path) -> Result<bool> {
    let bytes = fs::read(path).with_context(|| format!("read {}", path.display()))?;
    let mut uses: HashSet<String> = HashSet::new();
    let mut results: HashSet<String> = HashSet::new();
    let mut empty_thinking = 0usize;
    let mut empty_content = 0usize;
    let mut parse_errors = 0usize;
    for raw in split_lines(&bytes) {
        let Ok(rec) = serde_json::from_slice::<Value>(raw) else {
            parse_errors += 1;
            continue;
        };
        let Some(content) = rec
            .get("message")
            .and_then(|m| m.get("content"))
            .and_then(Value::as_array)
        else {
            continue;
        };
        // An empty content array is rejected by the API on replay.
        if content.is_empty() {
            empty_content += 1;
        }
        for block in content {
            match block.get("type").and_then(Value::as_str) {
                Some("thinking")
                    if block
                        .get("thinking")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .is_empty() =>
                {
                    empty_thinking += 1;
                }
                Some("tool_use") => {
                    if let Some(id) = block.get("id").and_then(Value::as_str) {
                        uses.insert(id.to_owned());
                    }
                }
                Some("tool_result") => {
                    if let Some(id) = block.get("tool_use_id").and_then(Value::as_str) {
                        results.insert(id.to_owned());
                    }
                }
                _ => {}
            }
        }
    }
    let orphans: Vec<&String> = results.difference(&uses).collect();
    let ok = parse_errors == 0 && empty_thinking == 0 && empty_content == 0 && orphans.is_empty();
    if !ok {
        tracing::warn!(
            parse_errors,
            empty_thinking,
            empty_content,
            orphans = orphans.len(),
            "sweep validation found issues"
        );
    }
    Ok(ok)
}

// ---- pure mutation helpers (unit-tested) ----

/// Drop signature-only (empty-`thinking`) blocks from an assistant record.
fn strip_empty_thinking(rec: &mut Value) -> usize {
    if rec.get("type").and_then(Value::as_str) != Some("assistant") {
        return 0;
    }
    let Some(content) = rec
        .get_mut("message")
        .and_then(|m| m.get_mut("content"))
        .and_then(Value::as_array_mut)
    else {
        return 0;
    };
    let before = content.len();
    // Never empty the content array: an assistant message with `content: []`
    // is rejected by the API on replay (worse than the empty-thinking block we
    // were removing). If every block is empty-thinking, leave the record as-is.
    if content.iter().all(is_empty_thinking) {
        return 0;
    }
    content.retain(|b| !is_empty_thinking(b));
    before - content.len()
}

/// A signature-only (empty-`thinking`) block — rejected by the API on replay.
fn is_empty_thinking(b: &Value) -> bool {
    b.get("type").and_then(Value::as_str) == Some("thinking")
        && b.get("thinking")
            .and_then(Value::as_str)
            .unwrap_or("")
            .is_empty()
}

/// Replace the `input` of any tool_use whose id is in `failed` with `{}`.
fn purge_failed_input(rec: &mut Value, failed: &HashSet<String>) -> usize {
    if rec.get("type").and_then(Value::as_str) != Some("assistant") {
        return 0;
    }
    let Some(content) = rec
        .get_mut("message")
        .and_then(|m| m.get_mut("content"))
        .and_then(Value::as_array_mut)
    else {
        return 0;
    };
    let mut n = 0;
    for block in content {
        if block.get("type").and_then(Value::as_str) != Some("tool_use") {
            continue;
        }
        let is_failed = block
            .get("id")
            .and_then(Value::as_str)
            .is_some_and(|id| failed.contains(id));
        let has_input = block
            .get("input")
            .is_some_and(|v| !matches!(v, Value::Object(m) if m.is_empty()));
        if is_failed && has_input {
            block["input"] = serde_json::json!({});
            n += 1;
        }
    }
    n
}

fn collect_failed_ids(bytes: &[u8]) -> HashSet<String> {
    let mut failed = HashSet::new();
    for raw in split_lines(bytes) {
        let Ok(rec) = serde_json::from_slice::<Value>(raw) else {
            continue;
        };
        let Some(content) = rec
            .get("message")
            .and_then(|m| m.get("content"))
            .and_then(Value::as_array)
        else {
            continue;
        };
        for block in content {
            if block.get("type").and_then(Value::as_str) == Some("tool_result")
                && block.get("is_error").and_then(Value::as_bool) == Some(true)
            {
                if let Some(id) = block.get("tool_use_id").and_then(Value::as_str) {
                    failed.insert(id.to_owned());
                }
            }
        }
    }
    failed
}

// ---- file helpers ----

/// Split on `\n`, dropping the trailing empty fragment so we don't emit a
/// spurious blank line (each line is re-emitted with a trailing `\n`).
fn split_lines(bytes: &[u8]) -> impl Iterator<Item = &[u8]> {
    bytes.split(|&b| b == b'\n').filter(|l| !l.is_empty())
}

fn first_uuids(bytes: &[u8], n: usize) -> Vec<String> {
    let mut uuids = Vec::new();
    for raw in split_lines(bytes) {
        if let Ok(rec) = serde_json::from_slice::<Value>(raw) {
            if let Some(u) = rec.get("uuid").and_then(Value::as_str) {
                uuids.push(u.to_owned());
                if uuids.len() >= n {
                    break;
                }
            }
        }
    }
    uuids
}

fn temp_path(path: &Path) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    path.with_file_name(format!("{name}.tmp.{}.{nanos}", std::process::id()))
}

fn backup_name(path: &Path) -> String {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{name}.bak.{nanos}")
}

fn write_synced(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut f = fs::File::create(path)?;
    f.write_all(bytes)?;
    f.sync_all()?;
    Ok(())
}

fn fsync_dir(path: &Path) {
    if let Some(dir) = path.parent() {
        if let Ok(f) = fs::File::open(dir) {
            let _ = f.sync_all();
        }
    }
}

/// Keep the most recent `keep` `<name>.bak.*` files; delete older ones.
fn prune_backups(path: &Path, keep: usize) {
    let Some(dir) = path.parent() else { return };
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return;
    };
    let prefix = format!("{name}.bak.");
    let mut baks: Vec<(std::time::SystemTime, PathBuf)> = Vec::new();
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        if e.file_name().to_string_lossy().starts_with(&prefix) {
            if let Ok(meta) = e.metadata() {
                baks.push((meta.modified().unwrap_or(std::time::UNIX_EPOCH), e.path()));
            }
        }
    }
    baks.sort_by_key(|(t, _)| *t);
    while baks.len() > keep {
        let (_, p) = baks.remove(0);
        let _ = fs::remove_file(p);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn is_sidechain_transcript_matches_subagents_component_exactly() {
        // Top-level session transcript — NOT a sidechain.
        assert!(!is_sidechain_transcript(Path::new(
            "/home/user/.claude/projects/-home-user-repo/abc123.jsonl"
        )));
        // Inside a subagents/ directory — a sidechain.
        assert!(is_sidechain_transcript(Path::new(
            "/home/user/.claude/projects/-home-user-repo/abc123/subagents/agent-deadbeef.jsonl"
        )));
        // "subagents" must be a whole path component, not a substring of a name.
        assert!(!is_sidechain_transcript(Path::new(
            "/home/user/.claude/projects/-home-user-repo/subagents-like-name.jsonl"
        )));
        // Relative sidechain paths match too.
        assert!(is_sidechain_transcript(Path::new(
            "subagents/agent-cafebabe.jsonl"
        )));
    }

    fn rec_assistant(uuid: &str, content: Value) -> String {
        json!({"type": "assistant", "uuid": uuid, "message": {"role": "assistant", "content": content}})
            .to_string()
    }
    fn rec_user(uuid: &str, content: Value) -> String {
        json!({"type": "user", "uuid": uuid, "message": {"role": "user", "content": content}})
            .to_string()
    }

    /// A session with: an empty thinking block, a failed Bash call, and a
    /// normal turn. Sweep drops the empty thinking + purges the failed input.
    fn sample_session() -> String {
        [
            rec_assistant(
                "a1",
                json!([
                    {"type": "thinking", "thinking": "", "signature": "sigonly"},
                    {"type": "tool_use", "id": "t1", "name": "Bash", "input": {"command": "boom --very-long"}}
                ]),
            ),
            rec_user(
                "u1",
                json!([{"type": "tool_result", "tool_use_id": "t1", "is_error": true, "content": "failed"}]),
            ),
            rec_assistant(
                "a2",
                json!([
                    {"type": "thinking", "thinking": "real reasoning", "signature": "sig"},
                    {"type": "tool_use", "id": "t2", "name": "Read", "input": {"path": "/ok"}}
                ]),
            ),
            rec_user("u2", json!([{"type": "tool_result", "tool_use_id": "t2", "content": "ok"}])),
        ]
        .join("\n")
            + "\n"
    }

    #[test]
    fn sweep_strips_empty_thinking_and_purges_failed_input() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        fs::write(&path, sample_session()).unwrap();

        let report = sweep_file(&path).unwrap();
        assert_eq!(
            report.thinking_dropped, 1,
            "one empty thinking block dropped"
        );
        assert_eq!(report.inputs_purged, 1, "one failed input purged");
        assert!(report.saved() > 0);

        // Reload + check: no empty thinking, failed input cleared, real thinking kept.
        let swept = fs::read_to_string(&path).unwrap();
        assert!(!swept.contains("sigonly"), "empty thinking block gone");
        assert!(swept.contains("real reasoning"), "real thinking kept");
        let a1: Value = serde_json::from_str(swept.lines().next().unwrap()).unwrap();
        assert_eq!(
            a1["message"]["content"][0]["input"],
            json!({}),
            "failed input cleared"
        );
        // Successful Read input untouched.
        assert!(swept.contains("\"/ok\""));
        // A backup was created.
        assert!(report.backup.is_some());
        assert!(validate_file(&path).unwrap(), "swept file validates");
    }

    #[test]
    fn idempotent_second_sweep_is_noop() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        fs::write(&path, sample_session()).unwrap();
        sweep_file(&path).unwrap();
        let after_first = fs::read(&path).unwrap();
        let r2 = sweep_file(&path).unwrap();
        assert_eq!(r2.thinking_dropped, 0);
        assert_eq!(r2.inputs_purged, 0);
        // Content unchanged on the second pass (only mutated lines re-serialize).
        assert_eq!(fs::read(&path).unwrap(), after_first);
    }

    #[test]
    fn non_json_lines_preserved_verbatim() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        let content = format!(
            "not json at all\n{}\n",
            rec_user("u1", json!([{"type": "text", "text": "hi"}]))
        );
        fs::write(&path, &content).unwrap();
        sweep_file(&path).unwrap();
        let swept = fs::read_to_string(&path).unwrap();
        assert!(
            swept.starts_with("not json at all\n"),
            "garbage line kept verbatim"
        );
    }

    #[test]
    fn validate_detects_orphan() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad.jsonl");
        fs::write(
            &path,
            rec_user(
                "u1",
                json!([{"type": "tool_result", "tool_use_id": "ghost", "content": "x"}]),
            ) + "\n",
        )
        .unwrap();
        assert!(
            !validate_file(&path).unwrap(),
            "orphaned tool_result fails validation"
        );
    }

    #[test]
    fn backup_retention_keeps_three() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        // Re-dirty before each run so every sweep actually rewrites + backs up
        // (a no-op sweep is skipped and makes no backup by design).
        for _ in 0..5 {
            fs::write(&path, sample_session()).unwrap();
            sweep_file(&path).unwrap();
        }
        let baks = fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().contains(".bak."))
            .count();
        assert_eq!(baks, BAK_RETAIN, "keeps exactly {BAK_RETAIN} backups");
    }

    #[test]
    fn restore_backup_brings_back_pre_sweep_content() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        let original = sample_session();
        fs::write(&path, &original).unwrap();
        sweep_file(&path).unwrap(); // mutates + writes a .bak
        assert_ne!(
            fs::read_to_string(&path).unwrap(),
            original,
            "swept changed it"
        );

        let bak = restore_backup(&path).unwrap();
        assert!(bak.to_string_lossy().contains(".bak."));
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            original,
            "restore brings back the pre-sweep content"
        );
    }

    #[test]
    fn restore_backup_errors_when_none() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        fs::write(&path, "x\n").unwrap();
        assert!(restore_backup(&path).is_err(), "no backup → error");
    }

    #[test]
    fn clean_file_resweep_is_true_noop_no_backup() {
        // Sweeping an already-clean file must not rewrite or back it up, so
        // repeated runs can never evict the pristine first backup.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        fs::write(&path, sample_session()).unwrap();
        sweep_file(&path).unwrap(); // first sweep mutates + backs up
        let after_first = fs::read(&path).unwrap();

        let r = sweep_file(&path).unwrap(); // now clean
        assert_eq!(r.thinking_dropped, 0);
        assert_eq!(r.inputs_purged, 0);
        assert!(r.backup.is_none(), "no backup for a no-op sweep");
        assert_eq!(r.saved(), 0);
        assert_eq!(fs::read(&path).unwrap(), after_first, "file untouched");
        let baks = fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().contains(".bak."))
            .count();
        assert_eq!(baks, 1, "only the first sweep's backup exists");
    }

    #[test]
    fn commit_aborts_and_leaves_file_untouched_when_changed_concurrently() {
        // `commit_swept` compares the live file to the snapshot it built from.
        // Passing a *stale* `orig` simulates a concurrent append/rewrite that
        // landed between the read and the rename: it must abort, leave the live
        // file byte-for-byte intact, and remove its temp file.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        let live =
            sample_session() + &rec_user("u9", json!([{"type": "text", "text": "appended"}]));
        fs::write(&path, &live).unwrap();

        let stale_orig = sample_session(); // what we "read" before the append
        let out = b"whatever we would have written".to_vec();
        let err = commit_swept(&path, stale_orig.as_bytes(), &out, 0, 0, 0).unwrap_err();
        assert!(err.to_string().contains("aborted, file untouched"));
        // Live file unchanged; no temp orphan; no backup created.
        assert_eq!(fs::read_to_string(&path).unwrap(), live);
        let mut tmp = false;
        let mut bak = false;
        for e in fs::read_dir(dir.path()).unwrap().flatten() {
            let n = e.file_name().to_string_lossy().into_owned();
            tmp |= n.contains(".tmp.");
            bak |= n.contains(".bak.");
        }
        assert!(!tmp, "temp orphan removed on abort");
        assert!(!bak, "no backup made on abort");
    }

    #[test]
    fn no_temp_orphan_after_successful_sweep() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        fs::write(&path, sample_session()).unwrap();
        sweep_file(&path).unwrap();
        let orphan = fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .any(|e| e.file_name().to_string_lossy().contains(".tmp."));
        assert!(!orphan, "no temp orphan left behind");
    }

    #[test]
    fn empty_thinking_only_record_is_not_emptied() {
        // An assistant record whose ONLY block is an empty thinking block must
        // not become `content: []` (API-rejectable). Leave it as-is.
        let mut rec: Value = serde_json::from_str(&rec_assistant(
            "a1",
            json!([{"type": "thinking", "thinking": "", "signature": "s"}]),
        ))
        .unwrap();
        assert_eq!(strip_empty_thinking(&mut rec), 0, "must not strip to empty");
        assert_eq!(
            rec["message"]["content"].as_array().unwrap().len(),
            1,
            "content not emptied"
        );
    }

    #[test]
    fn blank_lines_and_no_trailing_newline_preserved_verbatim() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        // Internal blank line + no trailing newline; only one record is mutable.
        let content = format!(
            "{}\n\n{}",
            rec_user("u1", json!([{"type": "text", "text": "hi"}])),
            rec_assistant(
                "a1",
                json!([{"type": "thinking", "thinking": "", "signature": "x"},
                       {"type": "text", "text": "bye"}])
            )
        );
        fs::write(&path, &content).unwrap();
        let r = sweep_file(&path).unwrap();
        assert_eq!(r.thinking_dropped, 1);
        let swept = fs::read(&path).unwrap();
        // Blank line preserved, no trailing newline added.
        assert!(swept.windows(2).any(|w| w == b"\n\n"), "blank line kept");
        assert_ne!(swept.last(), Some(&b'\n'), "no trailing newline added");
    }

    #[test]
    fn unchanged_noncanonical_line_is_byte_identical() {
        // A valid-but-oddly-formatted line with no mutation must survive byte
        // for byte (cache-prefix / minimal-diff guarantee).
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        let weird = r#"{ "type":"user" ,  "uuid":"u1", "message":{"role":"user","content":[{"type":"text","text":"hi"}]} }"#;
        // Pair it with a record that DOES mutate so sweep actually rewrites.
        let content = format!(
            "{weird}\n{}\n",
            rec_assistant(
                "a1",
                json!([{"type": "thinking", "thinking": "", "signature": "x"},
                       {"type": "text", "text": "ok"}])
            )
        );
        fs::write(&path, &content).unwrap();
        sweep_file(&path).unwrap();
        let swept = fs::read_to_string(&path).unwrap();
        assert!(
            swept.lines().next().unwrap() == weird,
            "unchanged odd line preserved verbatim"
        );
    }

    #[test]
    fn dry_run_reports_without_writing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        let before = sample_session();
        fs::write(&path, &before).unwrap();
        let r = dry_run_file(&path).unwrap();
        assert_eq!(r.thinking_dropped, 1);
        assert_eq!(r.inputs_purged, 1);
        assert!(r.backup.is_none());
        assert_eq!(fs::read_to_string(&path).unwrap(), before, "file untouched");
    }

    #[test]
    fn validate_failure_modes() {
        let dir = tempfile::tempdir().unwrap();
        // parse error
        let p1 = dir.path().join("p.jsonl");
        fs::write(&p1, "not json\n").unwrap();
        assert!(!validate_file(&p1).unwrap(), "parse error fails");
        // leftover empty thinking
        let p2 = dir.path().join("t.jsonl");
        fs::write(
            &p2,
            rec_assistant(
                "a1",
                json!([{"type": "thinking", "thinking": "", "signature": "s"}]),
            ) + "\n",
        )
        .unwrap();
        assert!(!validate_file(&p2).unwrap(), "empty thinking fails");
        // empty content array
        let p3 = dir.path().join("e.jsonl");
        fs::write(&p3, rec_assistant("a1", json!([])) + "\n").unwrap();
        assert!(!validate_file(&p3).unwrap(), "empty content fails");
        // a lone tool_use with no result is NOT an orphan (only results can orphan)
        let p4 = dir.path().join("ok.jsonl");
        fs::write(
            &p4,
            rec_assistant(
                "a1",
                json!([{"type": "tool_use", "id": "t1", "name": "Read", "input": {}}]),
            ) + "\n",
        )
        .unwrap();
        assert!(validate_file(&p4).unwrap(), "unpaired tool_use is fine");
    }
}
