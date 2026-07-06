# Changelog

All notable changes to trimwire will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.5.0](https://github.com/AZagatti/trimwire/compare/v0.4.0...v0.5.0) - 2026-07-06

### Added

- *(cli)* opt-in Remote-Control coexistence mode ([#159](https://github.com/AZagatti/trimwire/pull/159))
- *(cli)* [**breaking**] full-off/on engage-disengage + pause/resume prune toggle (#159, #160) ([#168](https://github.com/AZagatti/trimwire/pull/168))
- *(cli)* advisory validation of API-provider inputs in the wizard ([#148](https://github.com/AZagatti/trimwire/pull/148)) ([#158](https://github.com/AZagatti/trimwire/pull/158))

### Fixed

- *(reprune)* defer the stale-summary clear to commit_checkpoint ([#164](https://github.com/AZagatti/trimwire/pull/164)) ([#169](https://github.com/AZagatti/trimwire/pull/169))
- *(reprune)* correct byte-forced re-checkpoint telemetry + serialize-before-commit ([#144](https://github.com/AZagatti/trimwire/pull/144)) ([#162](https://github.com/AZagatti/trimwire/pull/162))
- *(cli)* finish the bounded-runtime audit — guard ad-hoc runtimes + bound doctor DNS (#152, #153) ([#163](https://github.com/AZagatti/trimwire/pull/163))

### Other

- *(deps)* update dependencies across cargo, npm (site + collector), and actions ([#170](https://github.com/AZagatti/trimwire/pull/170))
- *(changelog)* restore the release-plz-generated 0.4.0 entry ([#166](https://github.com/AZagatti/trimwire/pull/166))

## [0.4.0](https://github.com/AZagatti/trimwire/compare/v0.3.16...v0.4.0) - 2026-07-03

### Changed

- **BREAKING**: `SummarizerProviderConfig` and `StaleInputCapConfig` are now `#[non_exhaustive]`. Earlier releases had silently added public fields to these structs — a SemVer break at an unchanged `0.3.16` (caught by a new `cargo-semver-checks` CI gate). Bumping to `0.4.0` makes the public-API change explicit; `#[non_exhaustive]` lets future config fields be added without another breaking release.

### Added

- *(cli)* research-led UX overhaul of wizard/doctor/status/install ([#118](https://github.com/AZagatti/trimwire/pull/118)) ([#146](https://github.com/AZagatti/trimwire/pull/146))
- *(anomaly)* detect + report invalid-prune rollbacks ([#138](https://github.com/AZagatti/trimwire/pull/138)) ([#142](https://github.com/AZagatti/trimwire/pull/142))
- *(cli)* make `trimwire off` a true bypass, not a dead socket ([#114](https://github.com/AZagatti/trimwire/pull/114)) ([#140](https://github.com/AZagatti/trimwire/pull/140))
- *(anomaly)* make trims legible to the agent + detect/report trimwire anomalies ([#135](https://github.com/AZagatti/trimwire/pull/135))
- *(stale_input_cap)* age-gate authoring content with a recoverable marker ([#122](https://github.com/AZagatti/trimwire/pull/122)) ([#130](https://github.com/AZagatti/trimwire/pull/130))
- *(summarizer)* resolve provider API key from api_key_file (fixes #111) ([#115](https://github.com/AZagatti/trimwire/pull/115))

### Fixed

- *(cli)* bound the collector upload + share the runtime-teardown fix ([#150](https://github.com/AZagatti/trimwire/pull/150)) ([#154](https://github.com/AZagatti/trimwire/pull/154))
- *(cli)* bound the summarizer-setup ollama probe with a timeout ([#145](https://github.com/AZagatti/trimwire/pull/145)) ([#149](https://github.com/AZagatti/trimwire/pull/149))
- *(bloat_cap)* give Read its own recent window to close the 4–16 KB Read gap ([#121](https://github.com/AZagatti/trimwire/pull/121)) ([#137](https://github.com/AZagatti/trimwire/pull/137))
- *(pruning)* safety hardening — keep_recent .max(1) clamp, snapshot glob, NotebookEdit exempt ([#129](https://github.com/AZagatti/trimwire/pull/129))
- *(stale_reads)* age-gate supersession elision so live reads aren't trimmed ([#113](https://github.com/AZagatti/trimwire/pull/113)) ([#120](https://github.com/AZagatti/trimwire/pull/120))
- *(deps)* bump anyhow 1.0.102 → 1.0.103 (RUSTSEC-2026-0190) ([#117](https://github.com/AZagatti/trimwire/pull/117))

### Other

- *(bloat_cap)* enable the stub_age ladder at 16 in the default profile ([#126](https://github.com/AZagatti/trimwire/pull/126)) ([#139](https://github.com/AZagatti/trimwire/pull/139))
- *(bloat_cap)* age-gate Task/Agent subagent results instead of permanent exemption ([#124](https://github.com/AZagatti/trimwire/pull/124)) ([#136](https://github.com/AZagatti/trimwire/pull/136))
- *(stale_input_cap)* shape-reduce old successful Task/Agent inputs ([#125](https://github.com/AZagatti/trimwire/pull/125)) ([#134](https://github.com/AZagatti/trimwire/pull/134))

## [0.3.16](https://github.com/AZagatti/trimwire/compare/v0.3.15...v0.3.16) - 2026-06-24

### Other

- *(update)* pin gateway port in 2 doctor tests + cover rollback guidance ([#96](https://github.com/AZagatti/trimwire/pull/96))
- *(update)* cover extract, redirect/HTTPS-downgrade, doctor advisory + harden FakeGitHub ([#95](https://github.com/AZagatti/trimwire/pull/95))
- run the test suite on macOS/Windows + fix the exposed cross-platform failures ([#94](https://github.com/AZagatti/trimwire/pull/94))
- *(release)* verify all 5 signed release assets + small updater/doc cleanups ([#92](https://github.com/AZagatti/trimwire/pull/92))

## [0.3.15](https://github.com/AZagatti/trimwire/compare/v0.3.14...v0.3.15) - 2026-06-24

### Fixed

- *(cli)* polish 6 minor findings from the acceptance sweep ([#91](https://github.com/AZagatti/trimwire/pull/91))

### Other

- remove obsolete update spike ([#89](https://github.com/AZagatti/trimwire/pull/89))

## [0.3.14](https://github.com/AZagatti/trimwire/compare/v0.3.13...v0.3.14) - 2026-06-24

### Fixed

- *(upgrade)* self-heal legacy "(deleted)" install receipt ([#88](https://github.com/AZagatti/trimwire/pull/88))
- *(upgrade)* refresh receipt from applied binary path ([#86](https://github.com/AZagatti/trimwire/pull/86))

## [0.3.13](https://github.com/AZagatti/trimwire/compare/v0.3.12...v0.3.13) - 2026-06-23

### Added

- *(update)* signed self-update — verify (4b) + apply (4c, draft) ([#85](https://github.com/AZagatti/trimwire/pull/85))
- *(update)* read-only update check + /healthz version (PR4 phase 4a) ([#83](https://github.com/AZagatti/trimwire/pull/83))

## [0.3.12](https://github.com/AZagatti/trimwire/compare/v0.3.11...v0.3.12) - 2026-06-23

### Added

- *(install)* record an install receipt for a future updater ([#81](https://github.com/AZagatti/trimwire/pull/81))
- *(build)* embed the target triple as TRIMWIRE_TARGET ([#79](https://github.com/AZagatti/trimwire/pull/79))

### Other

- *(release)* attest build provenance for release archives ([#82](https://github.com/AZagatti/trimwire/pull/82))

## [0.3.11](https://github.com/AZagatti/trimwire/compare/v0.3.10...v0.3.11) - 2026-06-23

### Added

- *(cli)* add reserved `update` stub pointing to the real update path ([#78](https://github.com/AZagatti/trimwire/pull/78))

### Fixed

- *(cli)* list run + hook in the --help group legend ([#76](https://github.com/AZagatti/trimwire/pull/76))

## [0.3.10](https://github.com/AZagatti/trimwire/compare/v0.3.9...v0.3.10) - 2026-06-22

### Fixed

- *(doctor)* --strict fails in pre-install state + release polish ([#75](https://github.com/AZagatti/trimwire/pull/75))
- proxy/code hygiene (P3-3, P3-4, P3-5) + document P2-7 lock tradeoff ([#71](https://github.com/AZagatti/trimwire/pull/71))

### Other

- UX clarity (P2-8, P2-9, P2-10, P3-8, P3-9, P3-10, P3-12) ([#73](https://github.com/AZagatti/trimwire/pull/73))
- reconcile benchmark numbers + trust wording (P2-1, P2-2, P3-11, ToS) ([#72](https://github.com/AZagatti/trimwire/pull/72))

### Security

- owner-only local files + don't leak internals in 502 (P2-6, P3-1, P3-2) ([#70](https://github.com/AZagatti/trimwire/pull/70))
- prevent shell injection via project [server] listen (P1-1) ([#67](https://github.com/AZagatti/trimwire/pull/67))

## [0.3.9](https://github.com/AZagatti/trimwire/compare/v0.3.8...v0.3.9) - 2026-06-22

### Other

- *(deps)* refresh Rust/site/collector/CI deps + align toolchains ([#65](https://github.com/AZagatti/trimwire/pull/65))

## [0.3.8](https://github.com/AZagatti/trimwire/compare/v0.3.7...v0.3.8) - 2026-06-22

### Other

- Publish Configuration reference + Security & trust docs on the site ([#64](https://github.com/AZagatti/trimwire/pull/64))
- Finalize Trimwire public docs and consistency fixes ([#53](https://github.com/AZagatti/trimwire/pull/53))

## [0.3.7](https://github.com/AZagatti/trimwire/compare/v0.3.6...v0.3.7) - 2026-06-20

### Other

- *(ci)* line-tables-only debug info for dev/test profiles ([#51](https://github.com/AZagatti/trimwire/pull/51))
- *(benchmark)* cover share-benchmark dry-run upload gate (model-free) ([#48](https://github.com/AZagatti/trimwire/pull/48))

## [0.3.6](https://github.com/AZagatti/trimwire/compare/v0.3.5...v0.3.6) - 2026-06-20

### Other

- *(config)* exempt subagent tools in CrossTurnDedupConfig default ([#44](https://github.com/AZagatti/trimwire/pull/44))

## [0.3.5](https://github.com/AZagatti/trimwire/compare/v0.3.4...v0.3.5) - 2026-06-19

### Other

- *(strategies)* pin F7 ordering, subagent exemptions, page-min boundary ([#41](https://github.com/AZagatti/trimwire/pull/41))

## [0.3.4](https://github.com/AZagatti/trimwire/compare/v0.3.3...v0.3.4) - 2026-06-19

### Other

- *(cli)* stabilize ollama wizard coverage

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
  See [CONFIGURATION.md](docs/CONFIGURATION.md).
- **Cross-platform binaries** for Linux, macOS, and Windows.

### Safety & correctness

- Orphan-free tool pairing, untouched system messages, and a content-free ledger —
  enforced by a Rust↔Python parity oracle and an offline harm gate.
- Sanitizes API-rejected empty `thinking` / `text` blocks that Claude Code can
  emit on `--resume` (which would otherwise 400 the request).
