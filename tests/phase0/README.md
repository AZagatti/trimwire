# Phase 0 — Python test harness

Throwaway Python suite that locks down the mutation semantics of the
gateway BEFORE the Rust port is written. The Rust port (Phase 1) must
produce identical results on the same fixtures with equivalent
assertions in `tests/integration.rs` (insta snapshots).

See [`../../DEVELOPMENT.md`](../../DEVELOPMENT.md) for the full plan.

## Run

```bash
# From repo root
cd tests/phase0
python3 -m pip install --user pytest    # one-time
PYTHONPATH=. python3 -m pytest -v
```

Or via the repo's `Makefile`:

```bash
make phase0
```

## What's in here

| File | Purpose |
|---|---|
| `pairing.py` | `PairingIndex` — mirrors `src/pairing.rs` |
| `strategies.py` | Python reference impls of `SlidingWindow` + `ImageStrip` |
| `fixtures_synth.py` | Synthetic fixture builders (parallel tool_use, long session, huge result, compact boundary, screenshot-heavy, failure-heavy) |
| `test_strategies.py` | 5 blocker invariant tests |
| `test_cache_prefix.py` | Cache-prefix-hash stability + determinism (SPIKE.md §9 silent-failure guard) |
| `capture.py` | Minimal proxy to capture real `/v1/messages` bodies to `tests/fixtures/*.json` |

## Capturing real fixtures (optional, for fidelity)

The synthetic fixtures cover the structural edge cases. To also test
against a real Claude Code session:

```bash
# In one terminal — start the capture proxy
python3 tests/phase0/capture.py --output tests/fixtures/<name>.json

# In another terminal — point claude at it for one turn
ANTHROPIC_BASE_URL=http://127.0.0.1:8765 claude --print "<prompt>"
# claude will receive a fake error response and exit; the capture
# proxy has now written the request body to disk
```

**Redaction discipline:** before committing, review the captured JSON
for personal content (paths under `/home/<you>/`, real names, emails,
real project names) and replace with `[REDACTED]`. The capture proxy
already strips the `Authorization` header by not echoing it; verify with:

```bash
grep -iE 'authorization|bearer|sk-ant-|api[_-]key' tests/fixtures/*.json
# Should be zero matches

grep -E '/home/[a-z]+/' tests/fixtures/*.json
# Should be zero matches (your username should never appear)
```

If a real fixture happens to trigger Anthropic's content-filter when
replayed in Rust tests later, swap it for a hand-crafted synthetic with
the same structural pattern.

## Cache-prefix-hash serialisation contract

The Rust port at `src/ledger.rs` (Phase 1 step 5) MUST use this exact
serialisation to hash the request prefix, or the cache-prefix-stability
invariant won't be portable across Python/Rust:

```python
prefix = {k: v for k, v in body.items() if k != "messages"}
serialised = json.dumps(prefix, sort_keys=True, separators=(",", ":"))
hash_ = hashlib.sha256(serialised.encode("utf-8")).hexdigest()
```

Rust equivalent (target shape, for reference):

```rust
let prefix = body.as_object().map(|m| {
    m.iter().filter(|(k, _)| *k != "messages").collect::<BTreeMap<_, _>>()
});
let serialised = serde_json::to_string(&prefix)?;  // BTreeMap = sorted keys
let hash = hex::encode(Sha256::digest(serialised.as_bytes()));
```

If the two hashes diverge on the same input, the Rust serialisation
isn't matching. Fix the Rust side; the contract above is the source of
truth.

## Deletion plan

This whole directory + `tests/fixtures/` survives Phase 1. Once Phase 1
step 3 (`SlidingWindow` Rust impl) and step 4 (`ImageStrip`) are
passing snapshot tests against the same fixtures via `insta`, the
Python code is no longer load-bearing. Delete `tests/phase0/` then;
keep `tests/fixtures/` (the Rust tests need it).
