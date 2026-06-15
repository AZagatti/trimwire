# Contributing to trimwire

Thanks for your interest. trimwire's first public release is v0.1.0; the design
is in [`SPIKE.md`](SPIKE.md) and the phased
build plan + current status are in [`DEVELOPMENT.md`](DEVELOPMENT.md).

## Quick start

```bash
git clone <repo>
cd trimwire

# Install dev tools (one-time)
cargo install lefthook
lefthook install                       # wires pre-commit gates

# Build + test
cargo build
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt --check

# Touching the opt-in summarizer (src/summarizer/)? The single binary always
# includes it; just run the normal gates:
cargo test
cargo clippy --all-targets -- -D warnings
```

Requirements: **Rust 1.85+** (edition 2024).

## Optional: Cloudflare agent tooling

The collector (`collector/`, a Cloudflare Worker) and docs site (`site/`,
Astro on Workers) go faster with Cloudflare's agent skills + MCP servers.
These are personal, opt-in dev tooling — third-party and separately
licensed, so they're not vendored into this repo. If you use Claude Code:

```bash
# Skills (wrangler, workers-best-practices, cloudflare platform):
npx skills add https://github.com/cloudflare/skills

# Cloudflare MCP servers (read-only set; skip the write-capable `bindings`):
claude mcp add --transport http cf-docs         https://docs.mcp.cloudflare.com/mcp
claude mcp add --transport http cf-builds        https://builds.mcp.cloudflare.com/mcp
claude mcp add --transport http cf-observability https://observability.mcp.cloudflare.com/mcp
```

Then run `/mcp` to authenticate (OAuth to your own Cloudflare account;
`cf-docs` needs none). `/plugin install cloudflare@cloudflare` installs the
skills *and* the MCP servers in one step — use that or the commands above,
not both. Other agents (Cursor, Codex) work too; see the
[cloudflare/skills](https://github.com/cloudflare/skills) README.

## Workflow

1. **Read [`AGENTS.md`](AGENTS.md) first.** It's the single source of truth
   for repo conventions, layer rules, and what NOT to do.
2. **Read the relevant SPIKE.md / ARCHITECTURE.md section** before touching
   load-bearing code (especially `src/pairing.rs` — see SPIKE.md §5).
3. **Branch off `main`**, make focused commits using
   [conventional commits](https://www.conventionalcommits.org/) (`feat:`,
   `fix:`, `refactor:`, `docs:`, `test:`, `chore:`, etc.).
4. **Pre-commit hooks (via lefthook) must pass** — no `--no-verify`. If
   clippy or tests fail, fix the underlying issue.
5. **Open a PR** against `main`. CI runs the same gates as lefthook.
6. **PRs that change module boundaries** must also update
   [`ARCHITECTURE.md`](ARCHITECTURE.md) in the same commit.
7. **PRs that contradict spike-level decisions** must also update
   [`SPIKE.md`](SPIKE.md) with the new empirical evidence or reasoning.

## Layer rules (enforced by review)

See [`ARCHITECTURE.md`](ARCHITECTURE.md) for the full table. Highlights:

- `strategies/*` must NOT do any I/O — pure functions over `&mut [Value]`.
- `pairing.rs` must NOT mutate messages — only read.
- `proxy/gateway.rs` owns the HTTP lifecycle; no mutation logic lives there
  (it delegates to `strategies::apply_to_body`).
- `ledger.rs` is the only module that touches the SQLite DB.

## Testing

See [`docs/TESTING.md`](docs/TESTING.md) for the whole-project strategy (Rust +
collector + site), which CI workflow gates what, and the researched backlog. In
short:

- **Unit tests** live in the same file as the code they test, in a
  `#[cfg(test)] mod tests { ... }` block.
- **Snapshot tests** for mutation strategies use
  [`insta`](https://insta.rs/) over fixture JSON in `tests/fixtures/`.
  After making a change, `cargo insta review` to accept new snapshots.
- **Integration tests** in `tests/` use
  [`wiremock`](https://crates.io/crates/wiremock) to stub the Anthropic
  API.
- **The pairing module needs property tests** — it's the load-bearing
  correctness layer (SPIKE.md §5).

## Reporting issues

- **Bugs:** open an issue with steps to reproduce + your Rust version
  (`rustc --version`) + your platform.
- **Vulnerabilities:** do NOT open a public issue. See
  [`SECURITY.md`](SECURITY.md) for private reporting.
- **Feature requests:** open an issue with the use case. Note that we're
  scope-disciplined — see SPIKE.md §8 for the build/defer/document split.

## Known repo footguns

- **`skills-lock.json` hash drift:** the locked hashes for installed
  skills (`find-skills`, `rust-engineer`) may not match the on-disk
  `.agents/skills/*/SKILL.md` files. This is an upstream
  [`@vercel-labs/skills`](https://github.com/vercel-labs/skills) artifact
  (the lock format expects upstream-blob hashes, not local-file hashes).
  Functionally harmless; ignore unless skills break.
- **`.mcp.json` uses default `cwd` for the CCE server** — works when you
  run `claude` from this repo's root. If you run from elsewhere, set
  `--project-dir` explicitly in `.mcp.json` or in your local override.
- **`lefthook` must be installed manually** — it's not auto-installed by
  `cargo build`. Run `cargo install lefthook && lefthook install` once.

## Code of conduct

This project follows the [Contributor Covenant](CODE_OF_CONDUCT.md). By
participating, you agree to abide by it.

## License

By contributing, you agree that your contributions will be dual-licensed
under [MIT](LICENSE-MIT) and [Apache-2.0](LICENSE-APACHE) (the same terms
as the rest of the project).
