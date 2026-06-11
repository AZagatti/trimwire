"""Materialize the synthetic Phase 0 fixtures to `tests/fixtures/*.json`.

The Rust port (Phase 1) cannot call the Python reference, so the shared
fixture corpus must exist as committed JSON. This script is the single
reproducible source for those files: it imports the same generators the
pytest suite uses (`fixtures_synth`) and writes each to disk with stable,
sorted-key formatting so diffs stay reviewable.

Run from the repo root:

    python3 tests/phase0/dump_fixtures.py

Heavy fixtures are intentionally kept small here — pairing correctness is
independent of payload size, so a 32 KB `huge_tool_result` exercises the
same index paths as a 1 MB one. The byte-savings strategies (Phase 1
steps 3-4) add their own larger fixtures when they need them.
"""

from __future__ import annotations

import json
from pathlib import Path

import fixtures_synth as fs

# name -> generator thunk. Keep this list in sync with the corpus the
# Rust tests iterate over (tests/integration.rs FIXTURES).
FIXTURES = {
    "parallel_tool_use": lambda: fs.fixture_parallel_tool_use(),
    "long_session": lambda: fs.fixture_long_session(turns=100),
    "huge_tool_result": lambda: fs.fixture_huge_tool_result(target_kb=32),
    "compact_boundary": lambda: fs.fixture_compact_boundary(),
    "screenshot_heavy": lambda: fs.fixture_screenshot_heavy(num_screenshots=3, image_kb=8),
    "failure_heavy": lambda: fs.fixture_failure_heavy(num_failures=4),
}


def main() -> None:
    out_dir = Path(__file__).resolve().parents[1] / "fixtures"
    out_dir.mkdir(parents=True, exist_ok=True)
    for name, build in FIXTURES.items():
        path = out_dir / f"{name}.json"
        with path.open("w", encoding="utf-8") as fh:
            json.dump(build(), fh, sort_keys=True, indent=2)
            fh.write("\n")
        print(f"wrote {path} ({path.stat().st_size} bytes)")


if __name__ == "__main__":
    main()
