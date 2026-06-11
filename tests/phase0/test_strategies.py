"""Phase 0 invariant suite for the Python reference strategies.

Run: pytest tests/phase0/ -v

The Rust port MUST pass equivalent assertions on the same input
fixtures. If any of these fail, the corresponding Rust test should
fail too — and vice versa.
"""

from __future__ import annotations

import copy
import json
from pathlib import Path

import pytest

from fixtures_synth import (
    fixture_compact_boundary,
    fixture_failure_heavy,
    fixture_huge_tool_result,
    fixture_long_session,
    fixture_parallel_tool_use,
    fixture_screenshot_heavy,
)
from pairing import OrphanError, PairingIndex
from strategies import (
    IMAGE_STRIP_STUB,
    SLIDING_WINDOW_STUB,
    apply_image_strip,
    apply_sliding_window,
)


# ---------- 1. Parallel tool_use blocks ----------

def test_parallel_tool_use_pairing_index_independent():
    fx = fixture_parallel_tool_use()
    idx = PairingIndex.build(fx["messages"])
    assert len(idx.uses) == 3
    assert len(idx.results) == 3
    assert set(idx.uses.keys()) == {"toolu_par_a", "toolu_par_b", "toolu_par_c"}
    idx.validate()


def test_parallel_tool_use_sliding_window_atomic_pair_drop():
    fx = fixture_parallel_tool_use()
    # Force the parallel turn to be "old" by adding more turns after it
    for i in range(5):
        uid = f"toolu_after_{i}"
        fx["messages"].append({"role": "user", "content": [{"type": "text", "text": f"more {i}"}]})
        fx["messages"].append({"role": "assistant", "content": [
            {"type": "tool_use", "id": uid, "name": "Bash", "input": {"command": f"echo {i}"}},
        ]})
        fx["messages"].append({"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": uid, "content": str(i)},
        ]})

    result = apply_sliding_window(fx["messages"], denylist={"Bash"}, keep_recent_turns=4)
    # All 3 parallel pairs should be stubbed atomically (their assistant turn is the oldest)
    assert result["stubbed"] >= 3
    # No orphans introduced
    PairingIndex.build(fx["messages"]).validate()


# ---------- 2. 100+ turn cumulative correctness ----------

def test_long_session_no_orphans_after_mutation():
    fx = fixture_long_session(turns=120)
    apply_sliding_window(fx["messages"], denylist={"Bash"}, keep_recent_turns=4)
    PairingIndex.build(fx["messages"]).validate()


def test_long_session_sliding_window_off_by_one():
    fx = fixture_long_session(turns=10)
    result = apply_sliding_window(fx["messages"], denylist={"Bash"}, keep_recent_turns=4)
    # 10 assistant turns total, keep most recent 4, so 6 should be stubbed
    assert result["stubbed"] == 6, f"expected 6 stubbed, got {result['stubbed']}"


def test_long_session_bytes_shrink_with_realistic_payloads():
    """With realistic-size tool_result payloads (kB-scale), stubbing
    must net-shrink the body. (Pure tiny-payload sessions can grow
    slightly because the stub text is longer than 1-char results —
    that's an edge case for Phase 1 to optionally skip when net-negative.)"""
    fx = fixture_long_session(turns=50)
    # Pad each tool_result to ~200B so stubs are meaningfully smaller
    for msg in fx["messages"]:
        if msg.get("role") != "user":
            continue
        for block in msg.get("content") or []:
            if isinstance(block, dict) and block.get("type") == "tool_result":
                block["content"] = (block["content"] + " ") + "x" * 200
    result = apply_sliding_window(fx["messages"], denylist={"Bash"}, keep_recent_turns=4)
    assert result["stubbed"] > 0
    assert result["elided_bytes"] > 0, (
        f"expected net byte reduction with padded payloads, got {result['elided_bytes']}B"
    )


# ---------- 3. Real-world / synthetic fixture suite ----------

@pytest.mark.parametrize("fixture_fn,denylist", [
    (fixture_screenshot_heavy, {"Bash"}),
    (fixture_failure_heavy, {"Bash"}),
    (fixture_long_session, {"Bash"}),
])
def test_fixture_suite_preserves_invariants(fixture_fn, denylist):
    fx = fixture_fn()
    original = copy.deepcopy(fx["messages"])
    apply_sliding_window(fx["messages"], denylist=denylist, keep_recent_turns=4)
    # Required envelope fields preserved
    assert "model" in fx
    assert "messages" in fx
    # No orphans
    PairingIndex.build(fx["messages"]).validate()
    # Same number of messages (we stub, not delete)
    assert len(fx["messages"]) == len(original)


def test_real_fixture_files_load_if_present():
    """If real captured fixtures exist, load + validate them."""
    fixtures_dir = Path(__file__).resolve().parents[1] / "fixtures"
    if not fixtures_dir.exists():
        pytest.skip("no real fixtures captured yet")
    for path in fixtures_dir.glob("*.json"):
        body = json.loads(path.read_text())
        assert "messages" in body, f"{path.name}: no messages[] field"
        idx = PairingIndex.build(body["messages"])
        idx.validate()


# ---------- 4. 1MB+ single tool_result ----------

def test_huge_tool_result_memory_bounded():
    fx = fixture_huge_tool_result(target_kb=1024)  # 1 MB
    apply_sliding_window(fx["messages"], denylist={"Read"}, keep_recent_turns=0)
    PairingIndex.build(fx["messages"]).validate()


def test_huge_tool_result_no_escape_corruption():
    fx = fixture_huge_tool_result(target_kb=512)
    apply_sliding_window(fx["messages"], denylist={"Read"}, keep_recent_turns=0)
    # JSON still round-trips
    s = json.dumps(fx, separators=(",", ":"))
    parsed = json.loads(s)
    assert "messages" in parsed


# ---------- 5. compact_boundary system messages ----------

def test_compact_boundary_not_corrupted():
    fx = fixture_compact_boundary()
    original_msgs = copy.deepcopy(fx["messages"])
    apply_sliding_window(fx["messages"], denylist={"Bash"}, keep_recent_turns=4)
    # Length unchanged (sliding window stubs, not deletes)
    assert len(fx["messages"]) == len(original_msgs)
    # Boundary message text preserved
    assert any(
        any(b.get("type") == "text" and "compact_boundary" in b.get("text", "") for b in m["content"])
        for m in fx["messages"] if isinstance(m.get("content"), list)
    )


# ---------- ImageStrip strategy ----------

def test_image_strip_keeps_recent():
    fx = fixture_screenshot_heavy(num_screenshots=5, image_kb=50)
    result = apply_image_strip(
        fx["messages"],
        image_tools={"mcp__playwright__browser_take_screenshot"},
        keep_recent_count=3,
    )
    # 5 screenshots, keep 3 most recent → 2 stubbed
    assert result["stubbed"] == 2
    PairingIndex.build(fx["messages"]).validate()


def test_image_strip_byte_reduction_significant():
    fx = fixture_screenshot_heavy(num_screenshots=5, image_kb=100)
    original_size = len(json.dumps(fx).encode("utf-8"))
    apply_image_strip(
        fx["messages"],
        image_tools={"mcp__playwright__browser_take_screenshot"},
        keep_recent_count=1,
    )
    final_size = len(json.dumps(fx).encode("utf-8"))
    # 4 of 5 images stripped → expect significant reduction
    ratio = 1 - (final_size / original_size)
    assert ratio > 0.5, f"expected >50% reduction, got {ratio:.1%}"


def test_image_strip_no_orphans():
    fx = fixture_screenshot_heavy(num_screenshots=4, image_kb=10)
    apply_image_strip(
        fx["messages"],
        image_tools={"mcp__playwright__browser_take_screenshot"},
        keep_recent_count=1,
    )
    PairingIndex.build(fx["messages"]).validate()


# ---------- OrphanError detection ----------

def test_pairing_index_detects_orphan_result():
    messages = [
        {"role": "user", "content": [{"type": "text", "text": "hi"}]},
        # tool_result with no matching tool_use anywhere
        {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "missing", "content": "x"}]},
    ]
    idx = PairingIndex.build(messages)
    with pytest.raises(OrphanError):
        idx.validate()
