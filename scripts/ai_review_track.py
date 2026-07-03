"""Accepted / false-positive tracker for AI-review findings — the ANALYSIS core.

A weekly workflow mines the bot's inline review-comment threads (reactions + replies)
and feeds them here; this classifies each finding as accepted / rejected / open and
rolls the results up per persona, so checklist tuning can target the noisy personas.
Persistence (a dedicated `ai-review-data` branch) and the gh-api mining live in the
workflow; THIS module is pure + fully unit-tested — no network, stdlib only.

CLI (two phases the weekly workflow chains):
  mine <comments.json> <records.json>       # gh-api review comments -> tracker records
  summarize <records.json> <out_summary.md> # records -> per-persona acceptance table
Both transforms are pure + tested; only the gh-api fetch + branch push live in the workflow.
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

# Hidden marker the post workflow appends to each inline comment so persona attribution
# travels WITH the comment (no separate per-PR findings storage needed). _md_safe defangs
# `<!--` in model output, so a finding can't forge this.
_META_RE = re.compile(r"<!--\s*ai-review-meta\s+(.*?)-->", re.S)


def parse_meta(body: str) -> dict | None:
    """Extract {personas, consensus} from a comment's hidden ai-review-meta marker."""
    m = _META_RE.search(body or "")
    if not m:
        return None
    attrs = dict(re.findall(r"(\w+)=(\S+)", m.group(1)))
    personas = [p for p in attrs.get("personas", "").split(",") if p]
    consensus = int(attrs["consensus"]) if attrs.get("consensus", "").isdigit() else None
    return {"personas": personas, "consensus": consensus}


def _pr_from_url(url: str) -> str | None:
    m = re.search(r"/pulls/(\d+)", url or "")
    return m.group(1) if m else None


def mine(comments: list) -> list:
    """Transform a repo's PR review comments (gh api pulls/comments) into tracker records:
    keep the bot's ROOT comments carrying our meta marker, attach their reactions and any
    threaded replies (joined by in_reply_to_id)."""
    replies_by_parent: dict = {}
    for c in comments:
        if isinstance(c, dict) and c.get("in_reply_to_id"):
            replies_by_parent.setdefault(c["in_reply_to_id"], []).append(c.get("body", ""))
    records = []
    for c in comments:
        if not isinstance(c, dict) or c.get("in_reply_to_id"):
            continue
        if (c.get("user") or {}).get("type") != "Bot":
            continue
        meta = parse_meta(c.get("body", ""))
        if not meta:
            continue
        records.append({
            "comment_id": c.get("id"),
            "pr": _pr_from_url(c.get("pull_request_url", "")),
            "file": c.get("path"),
            "line": c.get("line"),
            "personas": meta["personas"],
            "consensus": meta["consensus"],
            "reactions": c.get("reactions") or {},
            "replies": replies_by_parent.get(c.get("id"), []),
        })
    return records


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


def _read_json_list(path: str) -> list:
    try:
        data = json.loads(Path(path).read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as exc:
        print(f"could not read {path} ({exc})", file=sys.stderr)
        return []
    return data if isinstance(data, list) else []


def main() -> int:
    if len(sys.argv) != 4 or sys.argv[1] not in ("mine", "summarize"):
        print("usage: ai_review_track.py mine <comments.json> <records.json>\n"
              "       ai_review_track.py summarize <records.json> <out_summary.md>",
              file=sys.stderr)
        return 2
    cmd, src, dst = sys.argv[1], sys.argv[2], sys.argv[3]
    if cmd == "mine":
        records = mine(_read_json_list(src))
        Path(dst).write_text(json.dumps(records, indent=2), encoding="utf-8")
        print(f"mined {len(records)} tracked finding(s)")
    else:  # summarize
        finalized = finalize(_read_json_list(src))
        Path(dst).write_text(render_summary_md(summarize(finalized)), encoding="utf-8")
        print(f"summarized {len(finalized)} record(s)")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
