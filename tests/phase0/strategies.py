"""Python reference implementations of the MVP strategies.

These mirror `src/strategies/*.rs` in the Rust port. The Rust
implementations MUST produce identical output on the same input fixtures.

Strategies:
  - apply_sliding_window: stub tool_use/tool_result pairs older than N
    assistant turns whose tool name is in the denylist.
  - apply_image_strip: replace base64 image payloads in tool_result
    blocks (per the allowlist) with a marker, keeping the K most recent.

Both functions mutate `messages` in place and return:
  { "stubbed": int, "elided_bytes": int, "original_bytes": int }
"""

from __future__ import annotations

import json
import re
from typing import Any

from pairing import PairingIndex

SLIDING_WINDOW_STUB = "[trimwire: elided, older than sliding window]"
IMAGE_STRIP_STUB = "[trimwire: image stripped]"

# Heuristic: detect base64-looking content (length > 4KB of base64 alphabet).
_BASE64_RE = re.compile(r"^[A-Za-z0-9+/=\s]+$")


def _serialized_size(obj: Any) -> int:
    return len(json.dumps(obj, separators=(",", ":")).encode("utf-8"))


def _elision_marker(stub: str, content: Any) -> str:
    """Mirror of Rust `strategies::elision_marker`: append the canonical
    serialized size of the elided content so the marker is a content-free
    breadcrumb of *how much* was dropped. Sorted-key + compact to match
    serde_json's `to_string`."""
    # ensure_ascii=False to match serde_json's raw-UTF-8 output (it does NOT
    # escape non-ASCII), so the size N agrees on accented/CJK/emoji content too.
    n = len(json.dumps(content, sort_keys=True, ensure_ascii=False, separators=(",", ":")).encode("utf-8"))
    return f"{stub} ({n}B elided)"


def _is_base64_image_content(content: Any) -> bool:
    """Return True if content looks like a base64-encoded image payload.

    DESIGN NOTE (Phase 1 shipped): the string-only heuristic below — long +
    matches the base64 alphabet — is permissive. Any large alphanumeric blob (hex
    dumps, code output) would be misclassified. This permissiveness was kept
    INTENTIONALLY in the Rust port (`src/strategies/image_strip.rs`). Real Anthropic
    image payloads arrive as structured content blocks with `type: "image"`;
    those are caught reliably by the `isinstance(content, list)` and
    `isinstance(content, dict)` branches below. The string fallback is
    a defensive catch-all; it was deliberately KEPT permissive in the Rust port
    (matching this reference) rather than tightened, since real payloads use the
    structured branches and the catch-all only ever helps.
    """
    if isinstance(content, str):
        if len(content) < 4096:
            return False
        return bool(_BASE64_RE.match(content))
    if isinstance(content, list):
        return any(
            isinstance(b, dict) and b.get("type") == "image" and "source" in b
            for b in content
        )
    if isinstance(content, dict) and content.get("type") == "image":
        return True
    return False


def apply_sliding_window(
    messages: list[dict[str, Any]],
    denylist: set[str],
    keep_recent_turns: int = 4,
    stub: str = SLIDING_WINDOW_STUB,
) -> dict[str, int]:
    """Stub tool_use/tool_result pairs older than N assistant turns whose
    tool name is on the denylist. Pair-aware via PairingIndex.

    Returns: {"stubbed": N, "elided_bytes": B, "original_bytes": O}.
    """
    original_bytes = _serialized_size(messages)
    idx = PairingIndex.build(messages)
    idx.validate()  # pre-check

    # Walk backwards counting assistant turns; find cutoff message index.
    assistant_seen = 0
    cutoff_idx = -1
    for i in range(len(messages) - 1, -1, -1):
        if messages[i].get("role") == "assistant":
            assistant_seen += 1
            if assistant_seen > keep_recent_turns:
                cutoff_idx = i
                break

    stubbed = 0
    if cutoff_idx >= 0:
        # Collect ids to stub: tool_use blocks at or before cutoff, name in denylist
        ids_to_stub: set[str] = set()
        for mi in range(0, cutoff_idx + 1):
            msg = messages[mi]
            if msg.get("role") != "assistant":
                continue
            for block in msg.get("content") or []:
                if (
                    isinstance(block, dict)
                    and block.get("type") == "tool_use"
                    and block.get("name") in denylist
                    and block.get("id") in idx.uses
                ):
                    ids_to_stub.add(block["id"])

        # Atomically stub both halves of each pair
        for tid in ids_to_stub:
            use_loc, res_loc = idx.pair(tid)
            if use_loc is not None:
                mi, ci = use_loc
                messages[mi]["content"][ci]["input"] = {}
            if res_loc is not None:
                mi, ci = res_loc
                block = messages[mi]["content"][ci]
                block["content"] = _elision_marker(stub, block.get("content"))
            stubbed += 1

    # Post-validate
    PairingIndex.build(messages).validate()

    final_bytes = _serialized_size(messages)
    return {
        "stubbed": stubbed,
        "elided_bytes": original_bytes - final_bytes,
        "original_bytes": original_bytes,
    }


def apply_image_strip(
    messages: list[dict[str, Any]],
    image_tools: set[str],
    keep_recent_count: int = 3,
    stub: str = IMAGE_STRIP_STUB,
) -> dict[str, int]:
    """Strip base64 image payloads in tool_result blocks whose paired
    tool_use.name is in image_tools, keeping the K most recent.

    Returns: {"stubbed": N, "elided_bytes": B, "original_bytes": O}.
    """
    original_bytes = _serialized_size(messages)
    idx = PairingIndex.build(messages)
    idx.validate()

    # Identify image-tool tool_use_ids in chronological order
    image_use_ids: list[str] = []
    for mi, msg in enumerate(messages):
        if msg.get("role") != "assistant":
            continue
        for block in msg.get("content") or []:
            if (
                isinstance(block, dict)
                and block.get("type") == "tool_use"
                and block.get("name") in image_tools
                and "id" in block
            ):
                image_use_ids.append(block["id"])

    # Stub all but the K most recent
    to_stub = image_use_ids[:-keep_recent_count] if keep_recent_count > 0 else image_use_ids

    stubbed = 0
    for tid in to_stub:
        res_loc = idx.results.get(tid)
        if res_loc is None:
            continue
        mi, ci = res_loc
        block = messages[mi]["content"][ci]
        original_content = block.get("content")
        if _is_base64_image_content(original_content):
            block["content"] = _elision_marker(stub, original_content)
            stubbed += 1

    PairingIndex.build(messages).validate()

    final_bytes = _serialized_size(messages)
    return {
        "stubbed": stubbed,
        "elided_bytes": original_bytes - final_bytes,
        "original_bytes": original_bytes,
    }
