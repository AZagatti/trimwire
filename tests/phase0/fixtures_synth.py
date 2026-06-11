"""Synthetic fixture builders for the Phase 0 test suite.

Real captured fixtures live in `tests/fixtures/`. These synthetic
generators build edge-case scenarios that may be hard to capture from
real sessions: parallel tool_use blocks, 100+ turn sessions, 1MB single
tool_result, compact_boundary system messages.

The Rust port should run against both real captured fixtures AND these
synthetic ones.
"""

from __future__ import annotations

import base64
import random
import string
from typing import Any

DEFAULT_MODEL = "claude-3-5-sonnet-20241022"


def _msg(role: str, content: Any) -> dict[str, Any]:
    return {"role": role, "content": content if isinstance(content, list) else [{"type": "text", "text": content}]}


def _tool_use(uid: str, name: str, input_: dict[str, Any]) -> dict[str, Any]:
    return {"type": "tool_use", "id": uid, "name": name, "input": input_}


def _tool_result(uid: str, content: str | list, is_error: bool = False) -> dict[str, Any]:
    block: dict[str, Any] = {"type": "tool_result", "tool_use_id": uid, "content": content}
    if is_error:
        block["is_error"] = True
    return block


def _envelope(messages: list[dict[str, Any]]) -> dict[str, Any]:
    return {
        "model": DEFAULT_MODEL,
        "max_tokens": 1024,
        "system": "You are Claude, a helpful coding assistant.",
        "tools": [
            {"name": "Bash", "description": "Run a shell command", "input_schema": {"type": "object"}},
            {"name": "Read", "description": "Read a file", "input_schema": {"type": "object"}},
            {"name": "mcp__playwright__browser_take_screenshot", "description": "Take a screenshot",
             "input_schema": {"type": "object"}},
            {"name": "mcp__playwright__browser_navigate", "description": "Navigate browser",
             "input_schema": {"type": "object"}},
        ],
        "messages": messages,
    }


def fixture_parallel_tool_use() -> dict[str, Any]:
    """Single assistant message with 3 parallel tool_use blocks."""
    msgs: list[dict[str, Any]] = [
        _msg("user", "Run echo a, echo b, echo c in parallel and tell me the results."),
        _msg("assistant", [
            {"type": "text", "text": "Running them in parallel."},
            _tool_use("toolu_par_a", "Bash", {"command": "echo a"}),
            _tool_use("toolu_par_b", "Bash", {"command": "echo b"}),
            _tool_use("toolu_par_c", "Bash", {"command": "echo c"}),
        ]),
        _msg("user", [
            _tool_result("toolu_par_a", "a"),
            _tool_result("toolu_par_b", "b"),
            _tool_result("toolu_par_c", "c"),
        ]),
        _msg("assistant", [{"type": "text", "text": "Got a, b, c."}]),
    ]
    return _envelope(msgs)


def fixture_long_session(turns: int = 100) -> dict[str, Any]:
    """N-turn synthetic session of paired Bash calls."""
    msgs: list[dict[str, Any]] = []
    for i in range(turns):
        uid = f"toolu_long_{i:04d}"
        msgs.append(_msg("user", f"Turn {i}: please run echo {i}"))
        msgs.append(_msg("assistant", [
            _tool_use(uid, "Bash", {"command": f"echo {i}"}),
        ]))
        msgs.append(_msg("user", [_tool_result(uid, str(i))]))
    return _envelope(msgs)


def fixture_huge_tool_result(target_kb: int = 1024) -> dict[str, Any]:
    """One tool_result with a ~target_kb KB content body."""
    body = "x" * (target_kb * 1024)
    msgs: list[dict[str, Any]] = [
        _msg("user", "Read the big file"),
        _msg("assistant", [_tool_use("toolu_huge_1", "Read", {"path": "/big.txt"})]),
        _msg("user", [_tool_result("toolu_huge_1", body)]),
    ]
    return _envelope(msgs)


def fixture_compact_boundary() -> dict[str, Any]:
    """Session with a compact_boundary system message in the middle."""
    msgs: list[dict[str, Any]] = [
        _msg("user", "Turn 1"),
        _msg("assistant", [{"type": "text", "text": "Reply 1"}]),
        # The boundary marker — Claude Code inserts these after /compact
        {"role": "user", "content": [{"type": "text", "text": "[compact_boundary marker would be a system msg in real session]"}]},
        _msg("assistant", [
            _tool_use("toolu_post_compact_1", "Bash", {"command": "echo post-compact"}),
        ]),
        _msg("user", [_tool_result("toolu_post_compact_1", "post-compact")]),
    ]
    return _envelope(msgs)


def fixture_screenshot_heavy(num_screenshots: int = 5, image_kb: int = 100) -> dict[str, Any]:
    """Session with N Playwright screenshot tool_results, each ~image_kb KB."""
    fake_png_b64 = base64.b64encode(b"\x89PNG\r\n\x1a\n" + b"x" * (image_kb * 1024)).decode("ascii")
    msgs: list[dict[str, Any]] = []
    for i in range(num_screenshots):
        uid = f"toolu_screen_{i}"
        msgs.append(_msg("user", f"Screenshot {i}"))
        msgs.append(_msg("assistant", [
            _tool_use(uid, "mcp__playwright__browser_take_screenshot", {"path": f"/tmp/s{i}.png"}),
        ]))
        msgs.append(_msg("user", [_tool_result(uid, fake_png_b64)]))
    return _envelope(msgs)


def fixture_failure_heavy(num_failures: int = 4) -> dict[str, Any]:
    """Session with N errored tool_results."""
    msgs: list[dict[str, Any]] = []
    for i in range(num_failures):
        uid = f"toolu_fail_{i}"
        msgs.append(_msg("user", f"Try the broken thing {i}"))
        msgs.append(_msg("assistant", [
            _tool_use(uid, "Bash", {"command": f"foo-does-not-exist-{i}"}),
        ]))
        msgs.append(_msg("user", [_tool_result(
            uid, f"bash: foo-does-not-exist-{i}: command not found", is_error=True
        )]))
    return _envelope(msgs)
