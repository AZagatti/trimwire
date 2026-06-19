#!/usr/bin/env python3
"""trimwire dogfood harness — OFFLINE, no live model.

Exercises the user-facing read paths (`preview`, `sweep list`, `sweep all
--dry-run`, `stats`, `dashboard`) against a set of synthetic fixtures that model
realistic failure classes, and — with ``--real`` — against your local
``~/.claude`` session corpus in METADATA-ONLY mode (counts/sizes/strategy names;
never transcript text). It flags suspicious outcomes for human review.

This looks for SERIOUS PRODUCT BUGS, not marketing wins. It makes NO
"better than direct" claim and runs NO pruning benchmark.

Levels:
  PASS  invariant held
  FLAG  suspicious — a human should look (known-open items show up here)
  FAIL  a hard invariant was violated (regression)

Exit status is non-zero iff any FAIL occurred, or a harness self-test (the
detector logic check that runs first) failed. FLAGs never fail the run, so this
is safe to gate CI on while still surfacing known-open soft items.

Usage:
  python3 scripts/dogfood.py                         # build/resolve binary, synthetic suite
  python3 scripts/dogfood.py --bin path/to/trimwire  # reuse a built binary (CI)
  python3 scripts/dogfood.py --real                  # also audit ~/.claude (LOCAL ONLY)
  python3 scripts/dogfood.py --json                  # machine-readable report
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

# ---- result accumulation ---------------------------------------------------

PASS, FLAG, FAIL = "PASS", "FLAG", "FAIL"
RESULTS: list[dict] = []


def record(level: str, check: str, detail: str) -> None:
    RESULTS.append({"level": level, "check": check, "detail": detail})


# ---- pure detectors (unit-tested by self_test, independent of trimwire) -----
# Each takes a metrics dict and returns a human string when something looks
# wrong, else None. Keeping them pure lets the self-test prove the LOGIC is
# alive regardless of what trimwire currently does.

# The over-prune RISK is irrecoverable SILENT removal. stale_reads elides old
# Reads behind a demand-page marker (re-readable), so it is excluded here even
# when it dominates the byte savings.
SILENT_LOSSY = ("bloat_cap", "cross_turn_dedup", "sliding_window")
SILENT_LOSSY_PCT = 70.0  # this share removed silently is worth a human look
BLOAT_MISS_BYTES = 16384  # an OLD trimmable result this big surviving is a miss


def detect_over_prune(m: dict) -> str | None:
    """A large share of bytes removed by SILENT lossy strategies (not the
    recoverable stale_reads demand-page) may mean load-bearing context was
    dropped without a re-read path."""
    inb = m.get("in_bytes", 0)
    if inb <= 0 or m.get("messages", 0) < 4:
        return None
    pct = m.get("silent_lossy_bytes", 0) / inb * 100
    if pct >= SILENT_LOSSY_PCT:
        return (f"{pct:.0f}% of bytes removed by silent lossy strategies "
                "(bloat_cap/dedup/window) — verify nothing load-bearing was dropped")
    return None


def detect_image_not_stripped(m: dict) -> str | None:
    """Base64 image present but image_strip elided nothing (F7 class)."""
    if m.get("has_base64_image") and m.get("image_strip_bytes", 0) == 0:
        return "transcript carries a base64 image but image_strip trimmed 0 bytes (F7)"
    return None


def detect_bloat_not_trimmed(m: dict) -> str | None:
    """A large OLD non-exempt result that nothing trimmed = disk-bloat miss."""
    if (
        m.get("max_old_nonexempt_result_bytes", 0) >= BLOAT_MISS_BYTES
        and m.get("bytes_saved", 0) == 0
    ):
        return (
            f"a ~{m['max_old_nonexempt_result_bytes'] // 1024} KB old result survived "
            "with 0 bytes trimmed"
        )
    return None


def detect_subagent_underreport(m: dict) -> str | None:
    """Sibling subagents/ files exist on disk but preview reported none (F6)."""
    if m.get("subagents_on_disk", 0) > 0 and m.get("subagent_transcripts", 0) == 0:
        return (
            f"{m['subagents_on_disk']} sub-agent transcript(s) on disk but preview "
            "reported 0"
        )
    return None


DETECTORS = {
    "over_prune": detect_over_prune,
    "image_not_stripped": detect_image_not_stripped,
    "bloat_not_trimmed": detect_bloat_not_trimmed,
    "subagent_underreport": detect_subagent_underreport,
}


# ---- harness self-test (focused test of the harness's own logic) -----------


def self_test() -> bool:
    """Prove each detector fires on a planted positive and stays quiet on a
    planted negative. Independent of trimwire's behavior, so it stays valid even
    when product items (e.g. F7) get fixed. Returns True if all logic is alive."""
    ok = True
    cases = [
        ("over_prune", {"in_bytes": 100000, "messages": 10, "silent_lossy_bytes": 80000},
         {"in_bytes": 100000, "messages": 10, "silent_lossy_bytes": 1000}),
        # recoverable-dominated (all stale_reads) must NOT flag, even at high total reduction.
        ("over_prune", {"in_bytes": 100000, "messages": 10, "silent_lossy_bytes": 80000},
         {"in_bytes": 100000, "messages": 10, "silent_lossy_bytes": 0}),
        # too few messages must NOT flag.
        ("over_prune", {"in_bytes": 100000, "messages": 10, "silent_lossy_bytes": 80000},
         {"in_bytes": 100000, "messages": 2, "silent_lossy_bytes": 90000}),
        ("image_not_stripped", {"has_base64_image": True, "image_strip_bytes": 0},
         {"has_base64_image": True, "image_strip_bytes": 500}),
        ("bloat_not_trimmed", {"max_old_nonexempt_result_bytes": 40000, "bytes_saved": 0},
         {"max_old_nonexempt_result_bytes": 40000, "bytes_saved": 30000}),
        ("subagent_underreport", {"subagents_on_disk": 2, "subagent_transcripts": 0},
         {"subagents_on_disk": 2, "subagent_transcripts": 2}),
    ]
    for name, positive, negative in cases:
        fn = DETECTORS[name]
        if fn(positive) is None:
            record(FAIL, f"self_test::{name}", "detector did not fire on a planted anomaly (DEAD)")
            ok = False
        if fn(negative) is not None:
            record(FAIL, f"self_test::{name}", "detector fired on a clean input (false positive)")
            ok = False
    if ok:
        record(PASS, "self_test", f"{len(DETECTORS)} detectors alive (fire on positive, quiet on negative)")
    return ok


# ---- fixture construction --------------------------------------------------


def rec(role: str, content, sidechain: bool = False) -> str:
    r = {
        "type": role,
        "uuid": hashlib.sha1(repr(content).encode()).hexdigest()[:12],
        "message": {"role": role, "content": content},
    }
    if sidechain:
        r["isSidechain"] = True
    return json.dumps(r)


def write_session(path: Path, records: list[str]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text("\n".join(records) + "\n")


def tool_use(tid: str, name: str, inp: dict):
    return [{"type": "tool_use", "id": tid, "name": name, "input": inp}]


def tool_result(tid: str, content, is_error: bool = False):
    block = {"type": "tool_result", "tool_use_id": tid, "content": content}
    if is_error:
        block["is_error"] = True
    return [block]


def filler(n: int) -> list[str]:
    """n small back-and-forth turns to age earlier content."""
    out = []
    for i in range(n):
        out.append(rec("assistant", [{"type": "text", "text": f"step {i}"}]))
        out.append(rec("user", [{"type": "text", "text": f"ok {i}"}]))
    return out


def big(nbytes: int) -> str:
    return "Z" * nbytes


PNG_1PX = (
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk"
    "+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg=="
)


def build_fixtures(root: Path) -> dict[str, dict]:
    """Create the synthetic sessions root. Returns name -> {path, expects...}."""
    proj = root / "projects" / "-home-user-dogfood"
    fx: dict[str, dict] = {}

    # 1. plain: small, no subagents, nothing to trim.
    p = proj / "plain.jsonl"
    write_session(p, [
        rec("user", [{"type": "text", "text": "hello"}]),
        rec("assistant", [{"type": "text", "text": "hi"}]),
    ])
    fx["plain"] = {"path": p, "subagents_on_disk": 0, "has_base64_image": False}

    # 2. subagents: main + 2 sidechains + a non-jsonl meta sibling (F6 guard).
    #    All three carry an empty thinking block so `sweep all` actually trims
    #    them — this is the disk-bloat-cleaning path F6 unblocked.
    empty_think = {"type": "thinking", "thinking": "", "signature": "x"}
    p = proj / "withsubs.jsonl"
    write_session(p, [
        rec("user", [{"type": "text", "text": "do a sub task"}]),
        rec("assistant", [empty_think, {"type": "text", "text": "delegating"}]),
    ])
    subdir = proj / "withsubs" / "subagents"
    write_session(subdir / "agent-aaa.jsonl", [
        rec("user", [{"type": "text", "text": "sub"}], sidechain=True),
        rec("assistant", [empty_think, {"type": "text", "text": "sub done"}], sidechain=True),
    ])
    shutil.copy(subdir / "agent-aaa.jsonl", subdir / "agent-bbb.jsonl")
    (subdir / "agent-aaa.meta.json").write_text('{"m":1}\n')
    fx["subagents"] = {"path": p, "subagents_on_disk": 2, "has_base64_image": False}

    # 3. image-heavy: an OLD base64 screenshot result (F7 class).
    p = proj / "image.jsonl"
    write_session(p, [
        rec("user", [{"type": "text", "text": "screenshot"}]),
        rec("assistant", tool_use("t1", "mcp__playwright__browser_take_screenshot", {})),
        rec("user", tool_result("t1", [
            {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": PNG_1PX * 600}},
        ])),
        *filler(4),
    ])
    fx["image"] = {"path": p, "subagents_on_disk": 0, "has_base64_image": True}

    # 4. bloat_exempt_agent: the only big OLD block is an Agent result -> exempt,
    #    so reduction must stay LOW (Task/Agent exemption invariant; DEF-2 class).
    p = proj / "exempt_agent.jsonl"
    write_session(p, [
        rec("user", [{"type": "text", "text": "spawn agent"}]),
        rec("assistant", tool_use("t1", "Agent", {"prompt": "go"})),
        rec("user", tool_result("t1", big(40000))),
        *filler(5),
    ])
    fx["exempt_agent"] = {"path": p, "subagents_on_disk": 0, "has_base64_image": False,
                          "expect_low_reduction": True, "expect_no_overprune": True}

    # 4b. exempt_task: same, but the legacy `Task` subagent name (drift guard —
    #     the v0.3.1 fix added `Agent` alongside `Task`; both must stay exempt).
    p = proj / "exempt_task.jsonl"
    write_session(p, [
        rec("user", [{"type": "text", "text": "spawn task"}]),
        rec("assistant", tool_use("t1", "Task", {"prompt": "go"})),
        rec("user", tool_result("t1", big(40000))),
        *filler(5),
    ])
    fx["exempt_task"] = {"path": p, "subagents_on_disk": 0, "has_base64_image": False,
                         "expect_low_reduction": True}

    # 5. bloat_trimmable_bash: identical shape but a Bash result -> NOT exempt,
    #    so a large OLD result must get trimmed (strategy-alive check).
    p = proj / "trimmable_bash.jsonl"
    write_session(p, [
        rec("user", [{"type": "text", "text": "run it"}]),
        rec("assistant", tool_use("t1", "Bash", {"command": "dump"})),
        rec("user", tool_result("t1", big(40000))),
        *filler(5),
    ])
    fx["trimmable_bash"] = {"path": p, "subagents_on_disk": 0, "has_base64_image": False,
                            "max_old_nonexempt_result_bytes": 40000, "expect_trim": True,
                            "expect_overprune_fires": True}

    # 6. malformed: broken JSONL lines interleaved with valid turns (no-crash).
    p = proj / "malformed.jsonl"
    p.parent.mkdir(parents=True, exist_ok=True)
    p.write_text("\n".join([
        rec("user", [{"type": "text", "text": "a"}]),
        "{ this is not valid json",
        "",
        rec("assistant", [{"type": "text", "text": "b"}]),
        '{"type":"summary","summary":"noise"}',
    ]) + "\n")
    fx["malformed"] = {"path": p, "subagents_on_disk": 0, "has_base64_image": False}

    return fx


# ---- running trimwire ------------------------------------------------------


def run(binary: str, args: list[str], root: Path | None = None) -> subprocess.CompletedProcess:
    env = dict(os.environ)
    if root is not None:
        env["CLAUDE_CONFIG_DIR"] = str(root)
    env["TRIMWIRE_LEDGER__ENABLED"] = "false"  # never touch a real ledger
    return subprocess.run(
        [binary, *args], capture_output=True, text=True, env=env, timeout=120
    )


def preview_metrics(binary: str, root: Path, path: Path) -> dict | None:
    cp = run(binary, ["preview", str(path), "--json"], root)
    if cp.returncode != 0:
        return {"_error": cp.stderr.strip() or cp.stdout.strip()}
    try:
        d = json.loads(cp.stdout)
    except json.JSONDecodeError:
        return {"_error": "preview did not emit valid JSON"}
    per = {s["strategy"]: s["bytes"] for s in d.get("per_strategy", [])}
    return {
        "messages": d.get("messages", 0),
        "in_bytes": d.get("in_bytes", 0),
        "reduction_pct": d.get("reduction_pct", 0.0),
        "bytes_saved": d.get("bytes_saved", 0),
        "silent_lossy_bytes": sum(per.get(s, 0) for s in SILENT_LOSSY),
        "subagent_transcripts": d.get("subagent_transcripts", 0),
        "image_strip_bytes": per.get("image_strip", 0),
        "per_strategy": per,
    }


def hash_tree(root: Path) -> dict[str, str]:
    out = {}
    for f in sorted(root.rglob("*")):
        if f.is_file():
            out[str(f)] = hashlib.sha256(f.read_bytes()).hexdigest()
    return out


# ---- synthetic suite -------------------------------------------------------


def run_synthetic(binary: str) -> None:
    tmp = Path(tempfile.mkdtemp(prefix="trimwire-dogfood-"))
    try:
        fx = build_fixtures(tmp)
        before = hash_tree(tmp)

        for name, meta in fx.items():
            m = preview_metrics(binary, tmp, meta["path"])
            if m is None or "_error" in m:
                # malformed must NOT crash preview; others must reconstruct.
                err = (m or {}).get("_error", "no output")
                record(FAIL, f"preview::{name}", f"preview failed: {err}")
                continue
            merged = {**meta, **m}

            # Malformed transcript must reconstruct EXACTLY the 2 valid records
            # (not crash, not silently drop valid turns).
            if name == "malformed":
                if m["messages"] != 2:
                    record(FAIL, "malformed",
                           f"expected 2 reconstructed msgs from the valid lines, got {m['messages']}")
                else:
                    record(PASS, "malformed", "broken JSONL tolerated; reconstructed exactly 2 msgs")

            # F6: subagent discovery/reporting — must equal the on-disk count.
            if (msg := detect_subagent_underreport(merged)):
                record(FAIL, f"subagent::{name}", msg)
            elif meta["subagents_on_disk"] > 0:
                got, want = m["subagent_transcripts"], meta["subagents_on_disk"]
                if got != want:
                    record(FAIL, f"subagent::{name}", f"reported {got} sub-agent transcript(s), expected {want}")
                else:
                    record(PASS, f"subagent::{name}", f"reported all {got} sub-agent transcript(s)")

            # over-prune detector, exercised end-to-end against the real binary's
            # metrics (both directions): quiet on the exempt session, fires on the
            # heavily silent-trimmed one. Proves the metrics plumbing, not just logic.
            if meta.get("expect_no_overprune"):
                if (msg := detect_over_prune(merged)):
                    record(FAIL, f"overprune::{name}", f"false over-prune flag on an exempt session: {msg}")
                else:
                    record(PASS, f"overprune::{name}", "no false over-prune flag on the exempt session")
            if meta.get("expect_overprune_fires"):
                if detect_over_prune(merged):
                    record(PASS, f"overprune::{name}", "detector fires on heavy silent trim (plumbing alive)")
                else:
                    record(FAIL, f"overprune::{name}", "detector did NOT fire on heavy silent trim (plumbing dead)")

            # F7-class image flag (soft — known open).
            if (msg := detect_image_not_stripped(merged)):
                record(FLAG, f"image::{name}", msg)

            # Agent/Task exemption invariant: the exempt-agent fixture's big
            # block must NOT be trimmed away.
            if meta.get("expect_low_reduction"):
                if m["reduction_pct"] >= 25.0:
                    record(FAIL, f"exempt::{name}",
                           f"subagent result appears trimmed (reduction {m['reduction_pct']:.0f}%) "
                           "— subagent exemption regressed")
                else:
                    record(PASS, f"exempt::{name}",
                           f"subagent result preserved (reduction {m['reduction_pct']:.0f}%)")

            # Strategy-alive: a large OLD non-exempt result MUST get trimmed.
            # Deterministic synthetic outcome → a miss is a hard regression (FAIL).
            if meta.get("expect_trim"):
                if (msg := detect_bloat_not_trimmed(merged)):
                    record(FAIL, f"bloat::{name}", msg)
                else:
                    record(PASS, f"bloat::{name}",
                           f"old result trimmed ({m['reduction_pct']:.0f}% / {m['bytes_saved']} B)")

        # sweep list must include + label the sub-agent transcripts.
        cp = run(binary, ["sweep", "list"], tmp)
        out = cp.stdout
        n_sub = out.count("(sub-agent)")
        if cp.returncode != 0:
            record(FAIL, "sweep::list", f"sweep list failed: {cp.stderr.strip()}")
        elif n_sub != 2:
            record(FAIL, "sweep::list", f"expected 2 labeled sub-agent transcripts, saw {n_sub}")
        elif "agent-aaa.meta.json" in out:
            record(FAIL, "sweep::list", "non-jsonl meta sibling leaked into the listing")
        else:
            record(PASS, "sweep::list", "lists + labels both sub-agent transcripts, excludes meta.json")

        # sweep all --dry-run: the per-file lines are "  would sweep <path>:
        # saved <n>" (the summary line has no ": saved ", so it's excluded).
        # Every path must appear at most once (main + its sidechains, no dup).
        cp = run(binary, ["sweep", "all", "--dry-run"], tmp)
        swept_lines = [l for l in cp.stdout.splitlines()
                       if "would sweep " in l and ": saved " in l]
        paths = [l.split("would sweep ", 1)[1].rsplit(": saved", 1)[0] for l in swept_lines]
        sub_swept = sum(1 for p in paths if "subagents" in p)
        if cp.returncode != 0:
            record(FAIL, "sweep::dryrun", f"sweep all --dry-run failed: {cp.stderr.strip()}")
        elif len(paths) != len(set(paths)):
            record(FAIL, "sweep::dryrun", f"a transcript was swept twice (double-count): {paths}")
        elif sub_swept != 2:
            record(FAIL, "sweep::dryrun",
                   f"expected both sub-agent transcripts in the sweep, saw {sub_swept} (F6 cleaning gap)")
        else:
            record(PASS, "sweep::dryrun",
                   f"dry-run swept {len(paths)} file(s) incl. {sub_swept} sub-agent, no double-count")

        # report paths must not crash.
        for sub in (["stats", "--json"], ["dashboard", "--out", str(tmp / "report.html")]):
            cp = run(binary, sub, tmp)
            if cp.returncode != 0:
                record(FAIL, f"report::{sub[0]}", f"{sub[0]} failed: {cp.stderr.strip()}")
            else:
                record(PASS, f"report::{sub[0]}", "ran without error")

        # READ-ONLY safety: preview + dry-run + reports must not mutate fixtures.
        # (the dashboard wrote report.html *outside* the fixture set; ignore it.)
        after = hash_tree(tmp)
        before_keys = set(before)
        mutated = [k for k in before_keys if after.get(k) != before[k]]
        if mutated:
            record(FAIL, "readonly", f"fixtures changed after read-only ops: {mutated}")
        else:
            record(PASS, "readonly", f"all {len(before_keys)} fixture files byte-identical after read ops")
    finally:
        shutil.rmtree(tmp, ignore_errors=True)


# ---- sweep data-safety round-trip (mutating, on throwaway fixtures) --------


def run_sweep_safety(binary: str) -> None:
    """`sweep` REWRITES files on disk — the most data-loss-prone path. Prove the
    contract on throwaway fixtures: a real `sweep all --yes` mutates a sweepable
    transcript AND its sub-agent sidechain, leaves a `.bak.*` backup beside each,
    and `sweep undo` restores every swept file byte-for-byte."""
    tmp = Path(tempfile.mkdtemp(prefix="trimwire-dogfood-sweep-"))
    try:
        proj = tmp / "projects" / "-home-user-dogfood"
        think = {"type": "thinking", "thinking": "", "signature": "x"}  # sweepable
        main = proj / "s.jsonl"
        write_session(main, [
            rec("user", [{"type": "text", "text": "go"}]),
            rec("assistant", [think, {"type": "text", "text": "done"}]),
        ])
        side = proj / "s" / "subagents" / "agent-x.jsonl"
        write_session(side, [
            rec("user", [{"type": "text", "text": "sub"}], sidechain=True),
            rec("assistant", [think, {"type": "text", "text": "sub done"}], sidechain=True),
        ])
        targets = [main, side]
        orig = {p: hashlib.sha256(p.read_bytes()).hexdigest() for p in targets}

        cp = run(binary, ["sweep", "all", "--yes"], tmp)
        if cp.returncode != 0:
            record(FAIL, "sweep::safety", f"sweep all --yes failed: {cp.stderr.strip()}")
            return

        for p in targets:
            now = hashlib.sha256(p.read_bytes()).hexdigest()
            if now == orig[p]:
                record(FAIL, f"sweep::safety::{p.name}", "sweepable file was NOT modified by sweep all")
                continue
            baks = list(p.parent.glob(p.name + ".bak.*"))
            if not baks:
                record(FAIL, f"sweep::safety::{p.name}", "no .bak.* backup created before rewrite (data-loss risk)")
                continue
            ucp = run(binary, ["sweep", "undo", str(p)], tmp)
            restored = hashlib.sha256(p.read_bytes()).hexdigest()
            if ucp.returncode != 0:
                record(FAIL, f"sweep::safety::{p.name}", f"sweep undo failed: {ucp.stderr.strip()}")
            elif restored != orig[p]:
                record(FAIL, f"sweep::safety::{p.name}", "sweep undo did NOT restore the original bytes")
            else:
                record(PASS, f"sweep::safety::{p.name}", "swept + backed up + undo restored byte-for-byte")
    finally:
        shutil.rmtree(tmp, ignore_errors=True)


# ---- real-corpus audit (LOCAL ONLY, metadata only) -------------------------


def run_real(binary: str, limit: int) -> None:
    root = os.environ.get("CLAUDE_CONFIG_DIR")
    base = Path(root) / "projects" if root else Path.home() / ".claude" / "projects"
    if not base.is_dir():
        record(FLAG, "real", f"no session corpus at {base} — skipping --real audit")
        return
    # Main transcripts only (preview reconstructs those); newest first.
    mains = [p for p in base.rglob("*.jsonl") if "subagents" not in p.parts]
    mains.sort(key=lambda p: p.stat().st_mtime, reverse=True)
    mains = mains[:limit]
    record(PASS, "real", f"auditing {len(mains)} most-recent real session(s) under {base} (metadata only)")
    for p in mains:
        m = preview_metrics(binary, Path(root) if root else Path.home() / ".claude", p)
        if m is None or "_error" in m:
            record(FLAG, f"real::{p.name}", f"preview could not process: {(m or {}).get('_error','?')}")
            continue
        subs = len(list((p.with_suffix("") / "subagents").glob("*.jsonl"))) if (p.with_suffix("") / "subagents").is_dir() else 0
        merged = {**m, "subagents_on_disk": subs}
        flags = [fn(merged) for fn in DETECTORS.values()]
        flags = [f for f in flags if f]
        if flags:
            for f in flags:
                record(FLAG, f"real::{p.name}", f)
        # else: stay quiet — clean sessions don't need a line.


# ---- reporting -------------------------------------------------------------


def report(as_json: bool) -> int:
    fails = [r for r in RESULTS if r["level"] == FAIL]
    flags = [r for r in RESULTS if r["level"] == FLAG]
    if as_json:
        print(json.dumps({
            "results": RESULTS,
            "summary": {"pass": sum(r["level"] == PASS for r in RESULTS),
                        "flag": len(flags), "fail": len(fails)},
        }, indent=2))
    else:
        for r in RESULTS:
            mark = {"PASS": "  ok ", "FLAG": "FLAG ", "FAIL": "FAIL "}[r["level"]]
            print(f"  {mark} {r['check']:<26} {r['detail']}")
        print(f"\n  summary: {sum(r['level']==PASS for r in RESULTS)} ok · "
              f"{len(flags)} flag (review) · {len(fails)} fail")
        if flags and not fails:
            print("  (flags are known-open / soft items for human review — not failures)")
    return 1 if fails else 0


def resolve_bin(arg: str | None) -> str:
    if arg:
        return arg
    rel = Path("target/release/trimwire")
    if rel.exists():
        return str(rel)
    found = shutil.which("trimwire")
    if found:
        return found
    print("[dogfood] building release binary…", file=sys.stderr)
    subprocess.run(["cargo", "build", "--release"], check=True)
    return str(rel)


def main() -> int:
    ap = argparse.ArgumentParser(description="trimwire offline dogfood harness")
    ap.add_argument("--bin", help="path to the trimwire binary (default: target/release or PATH)")
    ap.add_argument("--real", action="store_true", help="also audit ~/.claude (LOCAL ONLY, metadata only)")
    ap.add_argument("--real-limit", type=int, default=25, help="max real sessions to audit (default 25)")
    ap.add_argument("--json", action="store_true", help="machine-readable report")
    args = ap.parse_args()

    if not self_test():  # harness logic must be alive before we trust its verdicts
        report(args.json)
        return 1

    binary = resolve_bin(args.bin)
    run_synthetic(binary)
    run_sweep_safety(binary)
    if args.real:
        run_real(binary, args.real_limit)
    return report(args.json)


if __name__ == "__main__":
    sys.exit(main())
