#!/usr/bin/env python3
"""Reconstruct a `/v1/messages`-style body from a real Claude Code transcript
(JSONL) so the cost-replay (examples/cost_replay.rs) can model turn-by-turn
prompt-cache cost on REAL session structure.

Usage:
    python3 reconstruct_session.py SESSION.jsonl OUT.json [MAX_TURNS]

Writes OUT.json = {"messages": [ ... ]} — the ordered user/assistant message
objects exactly as logged (`.message` field), with consecutive same-role lines
MERGED into one message (CC batches tool_results into a single user turn before
sending, so the wire body alternates user/assistant; the JSONL logs them as
separate lines). Sidechain (sub-agent) lines are dropped — they are not part of
the main request body. MAX_TURNS optionally caps the number of user turns kept
(from the START, mirroring a session that ran that long).

CAVEAT: this is a faithful reconstruction of the conversation array, not a
byte-exact capture of the wire body (system prompt + tool schemas live outside
`messages[]`; the cost model adds them as a constant prefix). Good enough for the
SIGN and magnitude of a cost delta, which is what P0a needs.
"""
import json, sys

SRC = sys.argv[1]
OUT = sys.argv[2]
MAX_TURNS = int(sys.argv[3]) if len(sys.argv) > 3 else None

msgs = []
with open(SRC) as f:
    for line in f:
        line = line.strip()
        if not line:
            continue
        try:
            o = json.loads(line)
        except Exception:
            continue
        if o.get("isSidechain"):
            continue
        if o.get("type") not in ("user", "assistant"):
            continue
        m = o.get("message")
        if not isinstance(m, dict):
            continue
        role = m.get("role")
        content = m.get("content")
        if role not in ("user", "assistant") or content is None:
            continue
        # Normalize content to a block list so consecutive same-role lines merge.
        if isinstance(content, str):
            blocks = [{"type": "text", "text": content}]
        elif isinstance(content, list):
            blocks = content
        else:
            continue
        if msgs and msgs[-1]["role"] == role:
            # Merge into the previous same-role message (CC sends one turn).
            prev = msgs[-1]["content"]
            if isinstance(prev, str):
                prev = [{"type": "text", "text": prev}]
            msgs[-1]["content"] = prev + blocks
        else:
            msgs.append({"role": role, "content": blocks})

# A real body opens with a user turn; drop any leading assistant.
while msgs and msgs[0]["role"] != "user":
    msgs.pop(0)

if MAX_TURNS is not None:
    # Keep through the MAX_TURNS-th user turn (a turn closes on a user message
    # that FOLLOWS an assistant message — i.e. a genuine round-trip boundary).
    user_turns = 0
    cut = len(msgs)
    for i, m in enumerate(msgs):
        if m["role"] == "user" and i > 0 and msgs[i - 1]["role"] == "assistant":
            user_turns += 1
            if user_turns >= MAX_TURNS:
                cut = i + 1
                break
    msgs = msgs[:cut]

body = {"messages": msgs}
with open(OUT, "w") as f:
    json.dump(body, f)

n_user = sum(1 for m in msgs if m["role"] == "user")
n_asst = sum(1 for m in msgs if m["role"] == "assistant")
nbytes = len(json.dumps(msgs))
print(f"{OUT}: {len(msgs)} messages ({n_user} user / {n_asst} assistant), {nbytes} bytes")
