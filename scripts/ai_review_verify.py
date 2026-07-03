"""Compile-verify the AI reviewer's single-line ```suggestion fixes.

The reviewer emits `replacement` fields (one-click committable single-line fixes).
This applies them to a checkout so the manual (maintainer-triggered, trusted) workflow
can run `cargo check` and tell the maintainer which suggestions actually compile —
turning "unverified text" into "machine-verified patch". Manual flow ONLY: it mutates
a working tree and is meant to run against a PR-head checkout in that trusted context.

Usage:  python3 scripts/ai_review_verify.py <findings.json> <root_dir>
Prints a JSON summary to stdout: {applied, skipped, files, touched_rust}.
Stdlib only (matches ai_review.py). Never raises on bad input — best-effort.
"""
import json
import sys
from pathlib import Path


def apply_replacements(findings: list, root) -> tuple[list, list]:
    """Apply each finding's single-line `replacement` at its 1-based `line` in
    `root/<file>`. Returns (applied, skipped) — lists of {file, line[, reason]}.

    Only single-line replacements are applied (a ```suggestion that spans lines can't be
    placed from a single line:col anchor safely). Edits within a file are applied highest-
    line-first so earlier edits never shift the line numbers of later ones."""
    root = Path(root)
    applied: list = []
    skipped: list = []
    by_file: dict = {}
    for f in findings:
        if not isinstance(f, dict):
            continue
        rep, line, file = f.get("replacement"), f.get("line"), f.get("file")
        if not (isinstance(rep, str) and rep.strip()
                and isinstance(line, int) and not isinstance(line, bool) and line > 0
                and isinstance(file, str) and file):
            continue
        if rep.rstrip("\n").find("\n") != -1:            # multi-line -> can't anchor safely
            skipped.append({"file": file, "line": line, "reason": "multi-line replacement"})
            continue
        by_file.setdefault(file, []).append((line, rep))
    for file, edits in by_file.items():
        p = root / file
        try:
            resolved = p.resolve()
            resolved.relative_to(root.resolve())        # never write outside the checkout
        except (ValueError, OSError):
            skipped += [{"file": file, "line": ln, "reason": "path escapes root"} for ln, _ in edits]
            continue
        if not p.is_file():
            skipped += [{"file": file, "line": ln, "reason": "file not found"} for ln, _ in edits]
            continue
        lines = p.read_text(encoding="utf-8", errors="replace").splitlines(keepends=True)
        for line, rep in sorted(edits, key=lambda e: -e[0]):   # highest line first
            if line > len(lines):
                skipped.append({"file": file, "line": line, "reason": "line out of range"})
                continue
            nl = "\n" if lines[line - 1].endswith("\n") else ""
            lines[line - 1] = rep.rstrip("\n") + nl
            applied.append({"file": file, "line": line})
        p.write_text("".join(lines), encoding="utf-8")
    return applied, skipped


def summarize(applied: list, skipped: list) -> dict:
    files = sorted({a["file"] for a in applied})
    return {
        "applied": applied,
        "skipped": skipped,
        "files": files,
        "touched_rust": any(f.endswith(".rs") for f in files),
    }


def main() -> int:
    if len(sys.argv) != 3:
        print("usage: ai_review_verify.py <findings.json> <root_dir>", file=sys.stderr)
        return 2
    findings_path, root = sys.argv[1], sys.argv[2]
    try:
        findings = json.loads(Path(findings_path).read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as exc:
        print(f"could not read findings ({exc})", file=sys.stderr)
        print(json.dumps(summarize([], [])))            # empty summary, exit 0 (best-effort)
        return 0
    if not isinstance(findings, list):
        findings = []
    applied, skipped = apply_replacements(findings, root)
    print(json.dumps(summarize(applied, skipped)))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
