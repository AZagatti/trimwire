"""Tool-use ↔ tool-result pairing index — Python reference impl.

This mirrors `src/pairing.rs` in the Rust port (SPIKE.md §5). Used by both
the Python reference strategies and the pytest invariant suite. The Rust
port must produce identical results on the same fixtures.

Invariants the index enforces:
  1. Every `tool_result.tool_use_id` resolves to a `tool_use.id` in an
     earlier message (no orphan results).
  2. Pair drops are atomic — strategies always drop both halves.
  3. Parallel `tool_use` blocks in one assistant message are independent
     pairs (each stands alone).
"""

from __future__ import annotations

from dataclasses import dataclass, field
from typing import Any


@dataclass
class OrphanError(Exception):
    """A `tool_result.tool_use_id` has no matching `tool_use.id`."""

    orphan_ids: list[str] = field(default_factory=list)

    def __str__(self) -> str:
        return f"orphaned tool_result ids: {self.orphan_ids[:3]}"


@dataclass
class PairingIndex:
    # tool_use_id -> (message_idx, content_block_idx)
    uses: dict[str, tuple[int, int]] = field(default_factory=dict)
    results: dict[str, tuple[int, int]] = field(default_factory=dict)

    @classmethod
    def build(cls, messages: list[dict[str, Any]]) -> "PairingIndex":
        idx = cls()
        for mi, msg in enumerate(messages):
            content = msg.get("content")
            if not isinstance(content, list):
                continue
            for ci, block in enumerate(content):
                if not isinstance(block, dict):
                    continue
                btype = block.get("type")
                if btype == "tool_use" and "id" in block:
                    idx.uses[block["id"]] = (mi, ci)
                elif btype == "tool_result" and "tool_use_id" in block:
                    idx.results[block["tool_use_id"]] = (mi, ci)
        return idx

    def validate(self) -> None:
        """Raise OrphanError if any tool_result has no matching tool_use.

        NOTE: this only checks the `tool_result -> tool_use` direction.
        The reverse (a `tool_use` with no matching `tool_result`) is
        intentionally left un-checked: a lone `tool_use` is legitimate
        mid-turn (the assistant turn issuing the call may be the last
        message), and Anthropic's API tolerates it. Phase 1 step 2 settled
        this — see `src/pairing.rs::validate` and its `lone_use_is_not_orphan`
        test. Ordering ("earlier message", SPIKE §5) is likewise not
        enforced here; the index records positions so a check can be added
        if a strategy ever needs it.
        """
        orphans = [tid for tid in self.results if tid not in self.uses]
        if orphans:
            raise OrphanError(orphan_ids=orphans)

    def pair(self, tool_use_id: str) -> tuple[tuple[int, int] | None, tuple[int, int] | None]:
        """Return (use_loc, result_loc) for a given tool_use_id."""
        return self.uses.get(tool_use_id), self.results.get(tool_use_id)
