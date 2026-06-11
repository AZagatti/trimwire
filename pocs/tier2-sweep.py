"""
trimwire Tier 2 minimal POC — `sweep` CLI for on-disk JSONL cleanup.

Throwaway validation. Reads a Claude Code session JSONL, strips thinking-block
signatures and failed-tool-call inputs, writes atomically. Cozempic-style
safety: snapshot, mkstemp+fsync, append-detect via UUID prefix hash,
rolling .bak retention.

Usage:  python3 /tmp/trimwire-tier2-sweep.py <path-to-session.jsonl>
"""
import os
import sys
import json
import tempfile
import time
import argparse
from pathlib import Path

BAK_RETAIN = 3
PREFIX_UUID_COUNT = 5  # for append-detect prefix hash

def log(msg):
    print(msg, file=sys.stderr, flush=True)

def read_first_uuids(path, n):
    uuids = []
    with open(path, 'rb') as f:
        for line in f:
            try:
                msg = json.loads(line)
                if isinstance(msg, dict) and "uuid" in msg:
                    uuids.append(msg["uuid"])
                    if len(uuids) >= n:
                        break
            except json.JSONDecodeError:
                pass
    return uuids

def strip_signature_from_message(msg):
    """Drop empty thinking blocks entirely (signature-only blocks would be
    rejected by the API on replay with error 'each thinking block must
    contain thinking'). Returns (mutated_msg, dropped_count)."""
    n = 0
    if not isinstance(msg, dict):
        return msg, 0
    if msg.get("type") != "assistant":
        return msg, 0
    inner = msg.get("message")
    if not isinstance(inner, dict):
        return msg, 0
    content = inner.get("content")
    if not isinstance(content, list):
        return msg, 0
    new_content = []
    for block in content:
        if isinstance(block, dict) and block.get("type") == "thinking":
            thinking_text = block.get("thinking", "")
            # Drop only empty thinking blocks (signature-only). Keep ones with
            # real content untouched.
            if not thinking_text:
                n += 1
                continue
        new_content.append(block)
    if n > 0:
        inner["content"] = new_content
    return msg, n

def collect_failed_tool_use_ids(path):
    """Single pass to find tool_use_ids whose tool_result has is_error: true."""
    failed = set()
    with open(path, 'rb') as f:
        for line in f:
            try:
                msg = json.loads(line)
            except json.JSONDecodeError:
                continue
            if not isinstance(msg, dict) or msg.get("type") != "user":
                continue
            inner = msg.get("message")
            if not isinstance(inner, dict):
                continue
            content = inner.get("content")
            if not isinstance(content, list):
                continue
            for block in content:
                if isinstance(block, dict) and block.get("type") == "tool_result":
                    if block.get("is_error") is True and "tool_use_id" in block:
                        failed.add(block["tool_use_id"])
    return failed

def purge_failed_input_from_message(msg, failed_ids):
    """If a tool_use's id is in failed_ids, replace its input with {}. Returns count."""
    n = 0
    if not isinstance(msg, dict) or msg.get("type") != "assistant":
        return msg, 0
    inner = msg.get("message")
    if not isinstance(inner, dict):
        return msg, 0
    content = inner.get("content")
    if not isinstance(content, list):
        return msg, 0
    for block in content:
        if isinstance(block, dict) and block.get("type") == "tool_use":
            if block.get("id") in failed_ids and block.get("input") != {}:
                block["input"] = {}
                n += 1
    return msg, n

def manage_backups(orig_path, max_keep):
    """Keep most recent max_keep .bak.* files; delete rest."""
    p = Path(orig_path)
    stem = p.name
    parent = p.parent
    backups = sorted(
        [x for x in parent.iterdir() if x.name.startswith(stem + ".bak.")],
        key=lambda x: x.stat().st_mtime,
    )
    while len(backups) > max_keep:
        oldest = backups.pop(0)
        try:
            oldest.unlink()
            log(f"[sweep] removed old backup {oldest.name}")
        except OSError as e:
            log(f"[sweep] WARN: could not remove {oldest}: {e}")

def sweep(path):
    p = Path(path).resolve()
    if not p.is_file():
        log(f"[sweep] ERROR: not a file: {p}")
        return 1

    # Snapshot
    orig_size = p.stat().st_size
    orig_mtime = p.stat().st_mtime
    orig_uuid_prefix = read_first_uuids(p, PREFIX_UUID_COUNT)
    log(f"[sweep] {p.name}: orig_size={orig_size}B mtime={orig_mtime:.0f} prefix_uuids={len(orig_uuid_prefix)}")

    # Count failed tool_use_ids first (single pre-pass)
    failed_ids = collect_failed_tool_use_ids(p)
    log(f"[sweep] failed tool_use_ids found: {len(failed_ids)}")

    # Stream-read, mutate, write to temp
    sigs_stripped = 0
    inputs_purged = 0
    lines_in = 0
    lines_parsed = 0
    lines_skipped = 0
    fd, temp_path = tempfile.mkstemp(prefix=p.name + ".tmp.", dir=str(p.parent))
    try:
        with os.fdopen(fd, "wb") as out:
            with open(p, "rb") as inp:
                for raw in inp:
                    lines_in += 1
                    try:
                        msg = json.loads(raw)
                        lines_parsed += 1
                    except json.JSONDecodeError as e:
                        lines_skipped += 1
                        log(f"[sweep] WARN: line {lines_in} unparseable, copying verbatim: {e}")
                        out.write(raw)
                        continue
                    msg, n_sig = strip_signature_from_message(msg)
                    sigs_stripped += n_sig
                    msg, n_inp = purge_failed_input_from_message(msg, failed_ids)
                    inputs_purged += n_inp
                    # Serialize back compact (no extra whitespace)
                    out.write(json.dumps(msg, separators=(",", ":")).encode("utf-8"))
                    out.write(b"\n")
            out.flush()
            os.fsync(out.fileno())
    except Exception as e:
        try:
            os.unlink(temp_path)
        except OSError:
            pass
        log(f"[sweep] FAILED during stream-rewrite: {e}")
        return 1

    # Append-detect
    new_size = p.stat().st_size
    new_uuid_prefix = read_first_uuids(p, PREFIX_UUID_COUNT)
    if new_uuid_prefix != orig_uuid_prefix:
        os.unlink(temp_path)
        log(f"[sweep] CONFLICT: prefix UUIDs changed (likely /compact or rewrite). Aborting.")
        return 2
    if new_size > orig_size:
        log(f"[sweep] append detected: {new_size - orig_size}B of new lines, splicing delta")
        try:
            with open(p, "rb") as inp:
                inp.seek(orig_size)
                delta = inp.read()
            with open(temp_path, "ab") as out:
                out.write(delta)
                out.flush()
                os.fsync(out.fileno())
        except Exception as e:
            os.unlink(temp_path)
            log(f"[sweep] FAILED splicing delta: {e}")
            return 1

    # Backup, then atomic rename
    bak_name = f"{p.name}.bak.{time.strftime('%Y%m%d_%H%M%S')}"
    bak_path = p.parent / bak_name
    try:
        # Use os.replace for atomic copy-via-link? Just use shutil.copy to be safe.
        import shutil
        shutil.copy2(str(p), str(bak_path))
        log(f"[sweep] backup {bak_path.name}")
    except Exception as e:
        os.unlink(temp_path)
        log(f"[sweep] FAILED creating backup: {e}")
        return 1

    try:
        os.replace(temp_path, str(p))
    except Exception as e:
        os.unlink(temp_path)
        log(f"[sweep] FAILED atomic rename: {e}")
        return 1

    # fsync directory
    try:
        dfd = os.open(str(p.parent), os.O_RDONLY)
        try:
            os.fsync(dfd)
        finally:
            os.close(dfd)
    except OSError:
        pass

    manage_backups(p, BAK_RETAIN)

    final_size = p.stat().st_size
    saved = orig_size - final_size
    pct = (saved / orig_size * 100) if orig_size > 0 else 0
    log(f"[sweep] DONE: orig={orig_size}B final={final_size}B saved={saved}B ({pct:.2f}%) "
        f"sigs_stripped={sigs_stripped} inputs_purged={inputs_purged} "
        f"lines_in={lines_in} parsed={lines_parsed} skipped={lines_skipped} "
        f"backup={bak_name}")

    return 0

def validate(path):
    """Run the 6-check validation suite on a swept JSONL."""
    p = Path(path)
    ok = True
    # 1. JSON parse every line
    lines = 0
    json_errors = 0
    with open(p, "rb") as f:
        for raw in f:
            lines += 1
            try:
                json.loads(raw)
            except json.JSONDecodeError as e:
                json_errors += 1
                if json_errors < 4:
                    log(f"[validate] line {lines} unparseable: {e}")
    if json_errors > 0:
        log(f"[validate] FAIL: {json_errors} unparseable lines"); ok = False

    # 2/3. Required fields + 5. Signatures stripped + 4. Pairing
    tool_uses, tool_results = set(), set()
    sigs_left = 0
    missing_required = 0
    with open(p, "rb") as f:
        for raw in f:
            try:
                msg = json.loads(raw)
            except json.JSONDecodeError:
                continue
            if not isinstance(msg, dict):
                continue
            mtype = msg.get("type")
            if mtype in ("user", "assistant"):
                if "uuid" not in msg or "message" not in msg:
                    missing_required += 1
                    continue
                inner = msg.get("message", {})
                content = inner.get("content")
                if isinstance(content, list):
                    for block in content:
                        if not isinstance(block, dict): continue
                        if block.get("type") == "thinking" and block.get("signature"):
                            sigs_left += 1
                        if block.get("type") == "tool_use" and "id" in block:
                            tool_uses.add(block["id"])
                        if block.get("type") == "tool_result" and "tool_use_id" in block:
                            tool_results.add(block["tool_use_id"])
    if missing_required > 0:
        log(f"[validate] FAIL: {missing_required} user/assistant messages missing required fields"); ok = False
    if sigs_left > 0:
        log(f"[validate] FAIL: {sigs_left} thinking signatures still present"); ok = False
    orphans = tool_results - tool_uses
    if orphans:
        log(f"[validate] FAIL: {len(orphans)} orphaned tool_results: {list(orphans)[:3]}"); ok = False

    log(f"[validate] checked {lines} lines, "
        f"tool_uses={len(tool_uses)} tool_results={len(tool_results)} sigs_left={sigs_left} "
        f"missing_required={missing_required} orphans={len(orphans)} "
        f"{'PASS' if ok else 'FAIL'}")
    return 0 if ok else 1

def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("path", help="Session JSONL path")
    ap.add_argument("--validate-only", action="store_true",
                    help="Just run validation suite on the file (no sweep)")
    args = ap.parse_args()
    if args.validate_only:
        sys.exit(validate(args.path))
    rc = sweep(args.path)
    if rc != 0:
        sys.exit(rc)
    sys.exit(validate(args.path))

if __name__ == "__main__":
    main()
