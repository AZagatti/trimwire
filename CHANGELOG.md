# Changelog

All notable changes to trimwire will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.3.3](https://github.com/AZagatti/trimwire/compare/v0.3.2...v0.3.3) - 2026-06-18

### Fixed

- *(sweep)* include subagent transcripts in sweep and preview ([#36](https://github.com/AZagatti/trimwire/pull/36))

## [0.3.2](https://github.com/AZagatti/trimwire/compare/v0.3.1...v0.3.2) - 2026-06-18

### Fixed

- *(failed_input_purge)* exempt subagent Agent/Task inputs ([#33](https://github.com/AZagatti/trimwire/pull/33))

## [0.3.1](https://github.com/AZagatti/trimwire/compare/v0.3.0...v0.3.1) - 2026-06-18

### Fixed

- *(bloat_cap)* exempt subagent Agent tool outputs (Task→Agent drift) ([#30](https://github.com/AZagatti/trimwire/pull/30))

## [0.3.0](https://github.com/AZagatti/trimwire/compare/v0.2.4...v0.3.0) - 2026-06-18

### Added

- *(stale_reads)* lower default page_min_bytes 32KB to 16KB for recoverability ([#28](https://github.com/AZagatti/trimwire/pull/28))

### Fixed

- fix trimwire read-heavy live compression gaps ([#27](https://github.com/AZagatti/trimwire/pull/27))
- *(reprune)* splice cached summary across the content string/block shape flip ([#26](https://github.com/AZagatti/trimwire/pull/26))

### Other

- Protect override-marked turns from summarization ([#24](https://github.com/AZagatti/trimwire/pull/24))

## [0.2.4](https://github.com/AZagatti/trimwire/compare/v0.2.3...v0.2.4) - 2026-06-17

### Fixed

- *(reprune)* ignore cache_control in the append_only stability check
- *(summarizer)* false-done gate no longer flags honest hedged phrasing
- *(share)* strip the [1m] context marker before coarsening model_family
- *(summarizer)* probe budget follows the --model target, not the config engine

### Other

- *(changelog)* drop the manual [Unreleased] F10 entry
- *(changelog)* document F10 fix + Phase 4 validation

## [0.2.3](https://github.com/AZagatti/trimwire/compare/v0.2.2...v0.2.3) - 2026-06-15

### Added

- *(benchmark)* enforce model_family ↔ model_bucket consistency (fail-closed)
- *(benchmark)* tighten cross-field validation + preserve open-model families
- *(benchmark)* support provider/API benchmark sharing (backend-aware)

### Fixed

- 3 remaining run/uninstall doc-vs-source leftovers

### Other

- *(benchmark)* lock down the api-dry-run never-uploaded invariant
- final v0.2.3 audit fixes (off/run/ToS/consent accuracy)
- fix `trimwire run` usage, off/uninstall claims, install trust order
- tighten consent/release-state wording + keep serve internal
- align agent guidance + CLI docs with v0.2.3 reality
- *(install)* label curl|sh as Linux/macOS only, document Windows
- correct post-v0.2.2 stale "not deployed / inert" claims
- reject nonsensical model_size_bucket/family at the collector

## [0.2.2](https://github.com/AZagatti/trimwire/compare/v0.2.1...v0.2.2) - 2026-06-13

### Other

- benchmark sharing: wire the live endpoint + flip docs to "live"

## [0.2.1](https://github.com/AZagatti/trimwire/compare/v0.2.0...v0.2.1) - 2026-06-13

### Other

- drop placeholder /v1/benchmark route name, note sharing not live
- *(readme)* lead install with binary download, not curl | sh
- stop claiming benchmark sharing is live (it always dry-runs)

## [0.2.0](https://github.com/AZagatti/trimwire/compare/v0.1.2...v0.2.0) - 2026-06-13

### Other

- clear the deferred review items (reviewed, SHIP)
- fix intra-doc links broken in 945fc62 (cargo doc -D warnings)
- harden the background-task lifecycle (RAII slot + epoch + resp cap)
- post-deploy accuracy + honesty pass
- add funding links (GitHub Sponsors + Ko-fi)
- go live — fill D1/KV ids, observability, wire endpoints
- add harness field to the stats schema before first deploy
- point homepage at the live docs site (trimwire.dev)

### Security

- make summarizer provider URLs + listen safe from project files

## [0.1.2](https://github.com/AZagatti/trimwire/compare/v0.1.1...v0.1.2) - 2026-06-12

### Fixed

- *(dx)* cache-hit% in stats --verbose + doctor exit-contract docstring

### Other

- round-2 CLI guidance audit fixes (advisory exit codes, clearer messages)
- surface the summarizer in the CLI + fix round-2 doc bugs
- *(dx)* wizard-first + config-location for the summarizer; trim README

## [0.1.1](https://github.com/AZagatti/trimwire/compare/v0.1.0...v0.1.1) - 2026-06-12

### Other

- drop stale pre-release language now that v0.1.0 is published
- *(deps)* upgrade all ecosystems to latest (Rust / collector / site)

## [0.1.0] - 2026-06-11

First public release. trimwire is a transparent local proxy for Claude Code: point
`ANTHROPIC_BASE_URL` at it and it prunes each request's context on the way out —
cutting cost and delaying the context limit — while keeping Anthropic's prompt
cache intact. It runs on your own key, makes no model calls on the default path,
and fails open to the original request.

### Added

- **Deterministic, cache-safe context pruning** — model-free strategies
  (cross-turn dedup, near-duplicate simhash dedup, failed-input purge, bloat cap,
  sliding window, image strip, stale-input cap, stale reads, thinking strip),
  tuned by two profiles: `default` (aggressive) and `gentle`.
- **Stable-prefix re-pruning** keeps the cached request prefix byte-stable across
  turns, so pruning saves bytes without busting the prompt cache.
- **Opt-in summarizer** for heavier reduction — pluggable backends (local ollama
  or a cloud API provider) with a fallback cascade; never load-bearing.
- **Savings visibility** — a local SQLite ledger and `trimwire stats` show bytes
  saved, cache-hit rate, and which strategies earned their keep; plus an opt-in
  statusline savings bar.
- **Opt-in, anonymous, content-free telemetry** (`trimwire share stats` /
  `share benchmark`) feeding a community dashboard. Off by default.
- **Local-model benchmark** (`trimwire summarizer benchmark`) scoring small models
  against a bundled quality corpus behind a false-done safety gate.
- **On-disk transcript cleanup** — `trimwire sweep` (`list` / `all` / `file` /
  `undo`) with atomic, reversible writes.
- **Always-up background service** — `trimwire install` / `on` / `off` / `status`
  / `uninstall` (socket-activated systemd / launchd), plus `run`, `config`, and
  `recall` / `preview` / `dashboard` inspection commands.
- **Opt-in tuning levers** for advanced cases — catastrophic-result cap,
  tool-result age ladder, protected-file globs, and system-shape normalization.
  See [CONFIGURATION.md](CONFIGURATION.md).
- **Cross-platform binaries** for Linux, macOS, and Windows.

### Safety & correctness

- Orphan-free tool pairing, untouched system messages, and a content-free ledger —
  enforced by a Rust↔Python parity oracle and an offline harm gate.
- Sanitizes API-rejected empty `thinking` / `text` blocks that Claude Code can
  emit on `--resume` (which would otherwise 400 the request).
