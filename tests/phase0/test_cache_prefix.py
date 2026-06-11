"""Cache-prefix-hash invariant tests.

The Anthropic API caches request prefixes. If our mutations change the
prefix non-deterministically (or change it at all when no strategy
fires), we BUST the cache on every request — making the gateway HURT
performance instead of helping (SPIKE.md §9, top silent-failure risk).

Serialisation contract (Rust port MUST match exactly):
  - The "prefix" is the top-level request dict with the `messages` key
    removed.
  - Serialised as `json.dumps(prefix, sort_keys=True, separators=(',', ':'))`.
  - Hashed via `hashlib.sha256(...).hexdigest()` over the UTF-8 bytes.
"""

from __future__ import annotations

import copy
import hashlib
import json

import pytest

from fixtures_synth import (
    fixture_long_session,
    fixture_parallel_tool_use,
    fixture_screenshot_heavy,
)
from strategies import apply_image_strip, apply_sliding_window


def cache_prefix_hash(body: dict) -> str:
    prefix = {k: v for k, v in body.items() if k != "messages"}
    serialised = json.dumps(prefix, sort_keys=True, separators=(",", ":"))
    return hashlib.sha256(serialised.encode("utf-8")).hexdigest()


def test_prefix_hash_stable_when_no_strategy_fires():
    """No-op strategy run must NOT change the cache prefix hash."""
    fx = fixture_long_session(turns=20)
    h_before = cache_prefix_hash(fx)
    # Apply sliding window with empty denylist → nothing matches → no mutation
    result = apply_sliding_window(fx["messages"], denylist=set(), keep_recent_turns=4)
    h_after = cache_prefix_hash(fx)
    assert result["stubbed"] == 0
    assert h_before == h_after, "prefix hash changed even though no strategy fired"


def test_prefix_hash_stable_across_runs_when_no_op():
    """Two no-op runs must produce identical hashes (determinism)."""
    fx1 = fixture_screenshot_heavy(num_screenshots=3)
    fx2 = copy.deepcopy(fx1)
    apply_sliding_window(fx1["messages"], denylist=set(), keep_recent_turns=4)
    apply_sliding_window(fx2["messages"], denylist=set(), keep_recent_turns=4)
    assert cache_prefix_hash(fx1) == cache_prefix_hash(fx2)


def test_prefix_hash_unchanged_by_messages_mutation():
    """Mutating messages[] must not change the prefix hash (messages is
    excluded from the prefix by design)."""
    fx = fixture_long_session(turns=20)
    h_before = cache_prefix_hash(fx)
    apply_sliding_window(fx["messages"], denylist={"Bash"}, keep_recent_turns=4)
    h_after = cache_prefix_hash(fx)
    assert h_before == h_after, (
        "prefix hash changed when messages[] mutated — "
        "messages should be excluded from the prefix"
    )


def test_prefix_hash_changes_if_system_or_tools_change():
    """Sanity: actually changing the prefix (system field) must change the hash."""
    fx = fixture_parallel_tool_use()
    h_before = cache_prefix_hash(fx)
    fx["system"] = "Different system prompt"
    h_after = cache_prefix_hash(fx)
    assert h_before != h_after, "prefix hash should change when system changes"


def test_prefix_hash_deterministic_under_image_strip():
    """ImageStrip mutates messages[] but should not change prefix hash."""
    fx = fixture_screenshot_heavy(num_screenshots=4, image_kb=20)
    h_before = cache_prefix_hash(fx)
    apply_image_strip(
        fx["messages"],
        image_tools={"mcp__playwright__browser_take_screenshot"},
        keep_recent_count=1,
    )
    h_after = cache_prefix_hash(fx)
    assert h_before == h_after, "ImageStrip mutated messages should not change prefix hash"
