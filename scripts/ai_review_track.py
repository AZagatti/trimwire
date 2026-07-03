"""Accepted / false-positive tracker for AI-review findings — the ANALYSIS core.

A weekly workflow mines the bot's inline review-comment threads (reactions + replies)
and feeds them here; this classifies each finding as accepted / rejected / open and
rolls the results up per persona, so checklist tuning can target the noisy personas.
Persistence (a dedicated `ai-review-data` branch) and the gh-api mining live in the
workflow; THIS module is pure + fully unit-tested — no network, stdlib only.

CLI:  python3 scripts/ai_review_track.py <records.json> <out_summary.md>
  records.json: list of {personas:[...], reactions:{...}, replies:[...], ...}
  -> writes the per-persona summary markdown to <out_summary.md>
  -> prints the finalized records (each with a "status") as JSON to stdout
"""
import json
import re
import sys
from pathlib import Path

# A maintainer's reply/reaction on the bot's comment is the acceptance signal. Keep the
# vocab conservative — a wrong label pollutes the tuning data worse than an "open".
_REJECT_RE = re.compile(
    r"\b(false[- ]positive|not a (bug|real )|wont ?fix|won'?t fix|no[t]? an issue|"
    r"noise|invalid|disagree|incorrect|nonsense|nitpick|out of scope|n/?a)\b", re.I)
_ACCEPT_RE = re.compile(
    r"\b(good catch|nice catch|great catch|fixed|resolved|addressed|applied|"
    r"will fix|makes sense|agreed|done|thanks|thank you)\b", re.I)
# GitHub reaction content keys (from a comment's `reactions` summary object).
_NEG_REACTIONS = ("-1", "confused")
_POS_REACTIONS = ("+1", "heart", "hooray", "rocket")


def classify(reactions: dict, replies: list) -> str:
    """-> 'accepted' | 'rejected' | 'open'. Reject signals take precedence over accept
    (a thumbs-down / "false positive" is a stronger, more deliberate maintainer act)."""
    reactions = reactions if isinstance(reactions, dict) else {}
    bodies = [r for r in (replies or []) if isinstance(r, str)]
    neg = any(int(reactions.get(k, 0) or 0) > 0 for k in _NEG_REACTIONS) \
        or any(_REJECT_RE.search(b) for b in bodies)
    if neg:
        return "rejected"
    pos = any(int(reactions.get(k, 0) or 0) > 0 for k in _POS_REACTIONS) \
        or any(_ACCEPT_RE.search(b) for b in bodies)
    return "accepted" if pos else "open"


def finalize(records: list) -> list:
    """Attach a `status` to each record via classify(). Records without one are left
    as-is if already carrying a valid status (idempotent re-runs)."""
    out = []
    for r in records:
        if not isinstance(r, dict):
            continue
        r = dict(r)
        if r.get("status") not in ("accepted", "rejected", "open"):
            r["status"] = classify(r.get("reactions") or {}, r.get("replies") or [])
        out.append(r)
    return out


def summarize(records: list) -> dict:
    """Per-persona {total, accepted, rejected, open, accept_rate}. A finding with several
    personas counts once for EACH (all contributed). accept_rate excludes 'open' from the
    denominator (unresolved findings aren't evidence either way); None until there's data."""
    per: dict = {}
    for r in records:
        if not isinstance(r, dict):
            continue
        status = r.get("status", "open")
        personas = [p for p in (r.get("personas") or []) if isinstance(p, str)] or ["(none)"]
        for p in personas:
            s = per.setdefault(p, {"total": 0, "accepted": 0, "rejected": 0, "open": 0})
            s["total"] += 1
            if status in s:
                s[status] += 1
    for s in per.values():
        decided = s["accepted"] + s["rejected"]
        s["accept_rate"] = round(s["accepted"] / decided, 3) if decided else None
    return per


def render_summary_md(per: dict) -> str:
    lines = ["# AI-review accepted / false-positive tracker", "",
             "Per-persona maintainer acceptance (accept_rate excludes still-open threads).", "",
             "| Persona | Findings | Accepted | Rejected | Open | Accept rate |",
             "|---|--:|--:|--:|--:|--:|"]
    # noisiest first: lowest accept_rate (None sorts last), then most findings
    for p, s in sorted(per.items(),
                       key=lambda kv: (kv[1]["accept_rate"] is None,
                                       kv[1]["accept_rate"] if kv[1]["accept_rate"] is not None else 1,
                                       -kv[1]["total"])):
        rate = "—" if s["accept_rate"] is None else f"{s['accept_rate'] * 100:.0f}%"
        lines.append(f"| {p} | {s['total']} | {s['accepted']} | {s['rejected']} | {s['open']} | {rate} |")
    if not per:
        lines.append("| _(no data yet)_ | | | | | |")
    return "\n".join(lines) + "\n"


def main() -> int:
    if len(sys.argv) != 3:
        print("usage: ai_review_track.py <records.json> <out_summary.md>", file=sys.stderr)
        return 2
    try:
        records = json.loads(Path(sys.argv[1]).read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as exc:
        print(f"could not read records ({exc})", file=sys.stderr)
        records = []
    if not isinstance(records, list):
        records = []
    finalized = finalize(records)
    Path(sys.argv[2]).write_text(render_summary_md(summarize(finalized)), encoding="utf-8")
    print(json.dumps(finalized, indent=2))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
