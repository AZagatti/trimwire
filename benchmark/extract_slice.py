#!/usr/bin/env python3
"""Extract summarization-benchmark slices + ground-truth facts from real Claude
Code transcripts (JSONL). Usage:
    python3 extract_slice.py SESSION.jsonl FRACTION OUT_PREFIX
Writes OUT_PREFIX.txt (the slice, ~10k tokens) and OUT_PREFIX.facts (one fact
per line, lowercase). Facts are DISTINCTIVE identifiers a faithful summary should
keep: file-path tails + error codes + backtick identifiers + CamelCase/CONST
symbols. Near-duplicate facts (one a substring of another) are removed so they
can't double-count (results-review finding)."""
import json, re, sys
from collections import Counter

SRC, FRAC, OUTP = sys.argv[1], float(sys.argv[2]), sys.argv[3]
CHAR_BUDGET, PER_BLOCK_CAP = 40000, 1500

def cap(s):
    s = s if isinstance(s, str) else json.dumps(s)
    if len(s) <= PER_BLOCK_CAP: return s
    h = PER_BLOCK_CAP // 2
    return s[:h] + f"\n…[{len(s)-PER_BLOCK_CAP} chars elided]…\n" + s[-(PER_BLOCK_CAP-h):]

def blocks_text(content):
    if isinstance(content, str): return cap(content)
    out = []
    for b in content if isinstance(content, list) else []:
        t = b.get("type")
        if t == "text": out.append(cap(b.get("text", "")))
        elif t == "tool_use": out.append(f"[tool_use {b.get('name','?')}] " + cap(b.get("input", "")))
        elif t == "tool_result":
            c = b.get("content", "")
            if isinstance(c, list): c = " ".join(x.get("text","") if isinstance(x,dict) else str(x) for x in c)
            out.append("[tool_result] " + cap(c))
    return "\n".join(out)

segs = []
with open(SRC) as f:
    for line in f:
        try: o = json.loads(line)
        except Exception: continue
        if o.get("isSidechain"): continue
        m = o.get("message")
        if not isinstance(m, dict) or m.get("role") not in ("user", "assistant"): continue
        txt = blocks_text(m.get("content"))
        if txt.strip(): segs.append(f"### {m['role']}\n{txt}\n")

start = int(len(segs) * FRAC); slice_text = ""
for s in segs[start:]:
    if len(slice_text) >= CHAR_BUDGET: break
    slice_text += s
slice_text = slice_text[:CHAR_BUDGET]
open(OUTP + ".txt", "w").write(slice_text)

# Broadened fact set (not just paths). EXCLUDE tool/config-dir noise.
EXCLUDE = re.compile(r'/\.(claude|codex|antigravity|antigravity-server|cache|gemini|local|config)/|playwright-mcp|\.mcp\.|node_modules')
def tail2(p):
    seg = [x for x in p.split('/') if x]
    return "/".join(seg[-2:]) if len(seg) >= 2 else (seg[-1] if seg else p)
# `(?![A-Za-z])` stops "css" matching inside "cssText" (the overlay.style.css phantom).
paths  = [tail2(p) for p in re.findall(r'[\w./@-]*\w+\.(?:rs|ts|tsx|js|jsx|py|go|json|toml|md|sql|sh|css|html|svelte|vue|yaml|yml)(?![A-Za-z])', slice_text) if not EXCLUDE.search(p)]
errs   = re.findall(r'error\[[A-Z]\d+\]|[A-Z][A-Za-z]+Error\b|ECONN\w+|E[A-Z]{3,}', slice_text)
ticks  = re.findall(r'`([A-Za-z_][A-Za-z0-9_./-]{4,40})`', slice_text)
camel  = re.findall(r'\b([A-Z][a-z]+[A-Z][A-Za-z0-9]{2,})\b', slice_text)        # CamelCase symbols
const  = re.findall(r'\b([A-Z][A-Z0-9_]{3,})\b', slice_text)                     # CONSTANT_NAMES

# A fact must be a DISTINCTIVE identifier, not a common word. Keep if it has a
# path sep / dot / underscore / digit, or internal CamelCase, or is ALLCAPS — and
# isn't in the stoplist of generic words.
STOP = {"fixed","delete","enable","disable","partial","trending","floating","typescript",
        "javascript","python","error","warning","update","create","return","import","export",
        "default","content","element","button","string","number","object","result","status",
        "true","false","null","none","value","function","feature","report","testing","verified"}
def distinctive(t):
    tl = t.lower()
    if tl in STOP: return False
    if re.search(r'[/._0-9]', t): return True            # path/dotted/underscored/numbered
    if re.search(r'[a-z][A-Z]', t): return True          # internal CamelCase
    if t.isupper() and len(t) >= 4: return True          # ALLCAPS const
    return False
syms = [t for t in (ticks + camel + const) if distinctive(t)]
cand = (Counter(p.lower() for p in paths) + Counter(e.lower() for e in errs) + Counter(s.lower() for s in syms))
ranked = [w for w, _ in cand.most_common() if len(w) >= 5]
# substring-dedup: drop a fact that is a substring of another candidate (keep the longer, more distinctive one)
facts = []
for w in ranked:
    if any(w != k and w in k for k in ranked): continue
    if w in facts: continue
    facts.append(w)
    if len(facts) >= 14: break
open(OUTP + ".facts", "w").write("\n".join(facts) + "\n")
print(f"{OUTP}: slice from #{start}/{len(segs)} ({len(slice_text)} chars ~{len(slice_text)//4} tok); facts({len(facts)}): {', '.join(facts)}")
