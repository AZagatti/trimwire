---
description: Rust review rules (the load-bearing layer contract)
globs: ["**/*.rs", "Cargo.toml"]
---

**Layer rules — violations are `bug` severity (architectural correctness, not style):**
- `src/strategies/*` must be **pure**: no I/O, no env, no randomness, and output
  must not depend on wall-clock time. (Internal timing for logging only is OK — note it.)
- `src/pairing.rs` (`PairingIndex`) must not mutate the request-body bytes it indexes.
  Interior caching behind `&self` (interior mutability) is fine.
- `src/proxy/gateway.rs` orchestrates strategies; it must not mutate request content itself.
- `src/ledger.rs` is the **only** module that may touch SQLite.

**Correctness & tests:**
- No `unwrap()` / `expect()` / `panic!` / indexing that can panic on fallible *runtime*
  paths (parsing input, slicing by a computed length). Propagate with `?`.
- No blocking I/O or `std::thread::sleep` inside `async fn` — use the tokio equivalents.
- New strategy module → snapshot tests over fixture JSON. New public fn in `pairing.rs`
  → a test. Flag missing coverage as `test` **only** for non-trivial logic (skip getters,
  re-exports, trivial trait impls) and only if no test in the diff already covers it.
