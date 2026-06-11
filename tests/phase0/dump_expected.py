"""Generate the Python-reference strategy output for each fixture.

The Rust port must produce byte-identical `messages[]` (after canonical
sorted-key compact serialization) on the same input + config. This script
runs the Python reference strategies over every committed fixture and writes
the mutated `messages` array to
`tests/fixtures/expected/<strategy>__<name>.json`. The Rust parity tests
(tests/integration.rs) diff their own output against these files — no Python
needed at `cargo test` time.

Canonical form: `json.dumps(messages, sort_keys=True, separators=(",", ":"))`
+ trailing newline. serde_json's default object serialization is also
sorted-key + compact, so the two are directly comparable.

Run from the repo root:

    python3 tests/phase0/dump_expected.py
"""

from __future__ import annotations

import json
from pathlib import Path

from strategies import apply_image_strip, apply_sliding_window

# The standard configs the Python invariant suite + Rust parity tests share.
SW_DENYLIST = {"Bash"}
SW_KEEP_RECENT_TURNS = 4
# Exact tool name (not the "*screenshot*" glob) so Python set-membership and
# Rust glob agree byte-for-byte.
IMAGE_TOOLS = {"mcp__playwright__browser_take_screenshot"}
IMAGE_KEEP_RECENT_COUNT = 1

FIXTURE_NAMES = [
    "parallel_tool_use",
    "long_session",
    "huge_tool_result",
    "compact_boundary",
    "screenshot_heavy",
    "failure_heavy",
]


def _canonical(messages: list) -> str:
    # ensure_ascii=False so the canonical form matches serde_json::to_vec, which
    # emits raw UTF-8 (no \uXXXX escaping). ASCII fixtures are unaffected; this
    # keeps the Rust↔Python parity true for non-ASCII content too.
    return json.dumps(messages, sort_keys=True, ensure_ascii=False, separators=(",", ":")) + "\n"


def main() -> None:
    root = Path(__file__).resolve().parents[1]
    fixtures_dir = root / "fixtures"
    out_dir = fixtures_dir / "expected"
    out_dir.mkdir(parents=True, exist_ok=True)

    for name in FIXTURE_NAMES:
        raw = json.loads((fixtures_dir / f"{name}.json").read_text())

        sw = json.loads(json.dumps(raw))  # deep copy
        apply_sliding_window(
            sw["messages"], denylist=SW_DENYLIST, keep_recent_turns=SW_KEEP_RECENT_TURNS
        )
        (out_dir / f"sliding_window__{name}.json").write_text(
            _canonical(sw["messages"]), encoding="utf-8"
        )

        img = json.loads(json.dumps(raw))  # deep copy
        apply_image_strip(
            img["messages"],
            image_tools=IMAGE_TOOLS,
            keep_recent_count=IMAGE_KEEP_RECENT_COUNT,
        )
        (out_dir / f"image_strip__{name}.json").write_text(
            _canonical(img["messages"]), encoding="utf-8"
        )

        print(f"wrote sliding_window__{name}.json + image_strip__{name}.json")


if __name__ == "__main__":
    main()
