# Design spike: `trimwire update` (self-updater)

> **Internal maintainer design note — NOT a user guide and NOT a committed
> roadmap.** Nothing here is promised or scheduled. For how to update trimwire
> today, see [FAQ.md](FAQ.md#how-do-i-install-it). This file exists to capture the
> security/design questions an updater would have to answer before it could be built.

**Status:** phase **4a SHIPPED** — `trimwire update`/`upgrade` is now a **read-only
update check** (no download/verify/replace/restart). The self-replacing updater
(4b/4c) is **not** implemented; it remains owner-gated, but the two open design
decisions are now made (see **Owner decisions** below).
**Audit item:** P2-11 (read-only check shipped; the self-updater is the remaining
work, with a reviewed plan recorded here as the source of truth).

**Prerequisites already shipped (don't re-do):**
- **Target triple** embedded in the binary (`trimwire::build_target()`, shown in
  `doctor`) — asset selection (§1).
- **Install receipt** (`src/receipt.rs`, `trimwire::receipt`) recording
  `method`/`version`/`target`/`binary_path` — install-method detection + version
  check (§1–§2). Its `method` is NOT a sufficient authority on its own (it can go
  stale after a `cargo install` over the same path) — before self-replacing, the
  updater MUST verify the binary is the managed one (`binary_path ==
  current_exe()` + user-writable), not trust `method` alone.
- **Build provenance** (§3): `release.yml` attests every archive with GitHub
  artifact attestations + a `verify` job runs `gh attestation verify`. `.sha256`
  retained for transit integrity.

**What remains before a real `trimwire update` (PR 4):** the version-check/no-op
logic (§2) is **done** (phase 4a). Still to do: client-side verification code
(native Sigstore — verify the attestation **and** the `.sha256` before trusting a
download), **atomic replace** (§4; Windows self-replace is the hard part),
**service restart** (§5), **rollback** (§6), and **permissions** handling (§7).
See the phased plan + owner decisions below.

> **Verifier MUST pin the workflow identity, not just the repo.** `gh attestation
> verify --repo AZagatti/trimwire` only proves the attestation belongs to this
> repo — any workflow in the repo could attest an arbitrary binary. The check must
> also assert the signer workflow is `.github/workflows/release.yml` via
> `--signer-workflow <owner>/<repo>/.github/workflows/release.yml` (path-based, so
> stable across tags — unlike `--cert-identity`, which embeds the ref).
> **Status:** the release `verify` job now pins this (`--signer-workflow` +
> `--deny-self-hosted-runners`), and SECURITY-MODEL.md's recommended user command
> does too. The **future client-side updater MUST do the same** before trusting a
> download — don't regress to `--repo`-only.

## Problem

`trimwire update` (and `upgrade`) now performs a **read-only check** (phase 4a):
for a managed (`curl|sh`) install it reports whether a newer release exists; for
cargo/manual installs it prints the per-method update paths and exits 2. It does
**not** self-update. Today users still update by re-running their install method
(documented in [FAQ.md](FAQ.md#how-do-i-install-it)):

- `curl … install.sh | sh` re-run (downloads the latest release binary + re-runs
  `trimwire install`),
- `cargo binstall trimwire` / `cargo install trimwire`,
- manual download of the release asset.

That works, but a first-class `trimwire update` would be friendlier — especially
for the `curl|sh` audience, who have no package manager. The reason it isn't
already built is that a self-replacing binary for a **security-adjacent local
proxy** is not a "just download and overwrite" feature; it needs a deliberate
trust + atomicity design, which is an owner decision.

## What a safe updater must get right

1. **Where the update comes from.** GitHub Releases for `AZagatti/trimwire`
   (`releases/latest`), the same source the installer and `cargo binstall`
   already use. Asset names already match per-target (`trimwire-<target>.tar.gz`
   / `.zip`), and each ships a `.sha256` (see `release.yml`).
2. **Version check.** Compare the running `--version` to the latest release tag
   (GitHub API or crates.io) and no-op if already current. Must handle the MSRV
   pin / pre-release tags gracefully.
3. **Integrity vs provenance — the crux. ✅ NOW PROVIDED (provenance shipped).**
   - The published `.sha256` only proves the download wasn't *corrupted in
     transit*. It is served from the **same origin** as the binary, so it does
     **not** defend against a compromised release/account — an attacker who can
     replace the asset can replace the checksum too. (Kept — integrity layer.)
   - For a tool that sits in the request path with the user's API credential,
     true **provenance** is the right bar. `release.yml` now attests every
     release archive with **GitHub artifact attestations**
     (`actions/attest-build-provenance`), and the `verify` job runs
     `gh attestation verify` on every release. A future updater verifies this
     provenance (subject = the archive + digest) before swapping the binary.
     See [SECURITY-MODEL.md](SECURITY-MODEL.md#verifying-a-downloaded-release).
     NOTE: attestations live in GitHub's attestation store, so verification
     queries GitHub — the client must handle that being unreachable/offline.
4. **Atomic replace.** Self-replacing a running executable: on Unix, write the
   new binary to a temp file in the same dir, `fchmod` 0755, then `rename()` over
   the old path (atomic; the running process keeps its open inode). On **Windows**
   you cannot delete/replace a running `.exe` — needs the rename-self-then-replace
   dance (`MoveFileEx`) or a helper. This is the most error-prone part.
5. **Service restart.** After replacing the binary, the always-up service is
   still running the old code; `update` must restart it (`trimwire off && on`,
   or socket-activation re-exec) and tell the user.
6. **Rollback / failure.** Keep the previous binary (`.bak`) and restore on a
   failed post-update health check (`trimwire doctor`).
7. **Permissions.** If the binary lives in `/usr/local/bin` (the installer's
   default), in-place replace needs the same privileges the install used; detect
   and message clearly rather than failing opaquely.

## Owner decisions (made — reviewed by 4 lenses)

- **D1 — client-side provenance verification = native Sigstore (with a
  dependency/API spike before 4b).** Do **not** shell out to `gh` for the
  trust-critical check: it's a PATH-hijack vector for a credential-path tool, and
  requiring `gh` defeats the `curl|sh` audience. Verify the attestation natively
  (Sigstore), enforcing the **workflow-identity pin** (`signer-workflow =
  release.yml`, not just `--repo`), **deny self-hosted runners**, and confirm the
  **attestation subject digest == the downloaded file's digest**. **Never** fall
  back to checksum-only. A small spike sizes the `sigstore` crate dependency/API
  before 4b commits to it.
- **D2 — refuse macOS self-update in v1, same as Windows.** A downloaded binary
  is quarantine/Gatekeeper-blocked from launching as a service unless notarized;
  notarization is **out of scope**. v1 self-update is **Linux-only**; macOS +
  Windows print the manual update path and exit 2.

## PR4 plan (the self-updater) — phased, Unix-first

Phase **4a is implemented** (this PR): read-only check only.

- **4a (DONE) — read-only check.** `trimwire update`/`upgrade`: resolve+canonicalize
  `current_exe()` (abort on `(deleted)`), read the receipt, refuse (exit 2 + the
  per-method guidance) unless the install is self-updatable (`method="script"` +
  `binary_path == canonical current_exe()` + target match + parent writable),
  then query the latest GitHub release and report current/available (exit 0).
  Network failure is non-fatal. `--yes` is accepted but reports "not implemented
  yet" (exit 2). `/healthz` now includes a `version` field (for 4c's post-restart
  check). Pure logic in `trimwire::update`; impure I/O in `src/cli/update.rs`.
- **4b (gated on D1) — verify pipeline.** Download archive + `.sha256` via the
  `releases/latest/download/` redirect; verify checksum (`sha2`) **and** native
  Sigstore provenance (workflow-pinned, deny self-hosted, subject-digest bound);
  extract (shell out to `tar`). No replace yet. Fail-closed at every gate;
  **strictly-greater** version required (anti-downgrade).
- **4c (gated on D1+D2) — apply (Linux).** Atomic replace: temp in the **same
  dir** as the exe → `fchmod 0755` → `fsync(file)` → `rename()` → `fsync(dir)`;
  keep `<exe>.bak`. Restart: `service::off()` → `on()` → poll `healthz_ok()` →
  verify `/healthz` version == target. Rollback on any failure: `off()` → restore
  `.bak` → `on()` → re-verify (never swallow a rollback failure). Refuse on
  macOS/Windows and on non-writable locations (no privilege escalation). Flips
  the user docs atomically.
- **4d (fenced) — Windows self-replace; macOS notarized path** (separate, later).

### Files (4a, shipped)
`src/proxy/gateway.rs` (`/healthz` version), `src/update.rs` (new lib: version
parse/compare, `asset_name`, eligibility predicate, guidance/refusal text),
`src/cli/update.rs` (new: GitHub query, `current_exe` canonicalization,
writability probe, orchestration), `src/cli/mod.rs` (+doctor advisory bullet),
`src/main.rs` (`update --yes` flag), docs (CLI.md/FAQ.md/this file).

### Test plan
4a: unit tests for version parse/compare (v-prefix, build-metadata, downgrade/
equal/newer) + eligibility branches; integration tests drive the binary against a
fake GitHub server (`TRIMWIRE_UPDATE_API_BASE` test-only override) for
available/current/network-fail/refusal, plus a `/healthz` version test. 4b/4c add
checksum/attestation unit tests (mock verifier) + a sandboxed replace/rollback
test behind service/verifier seams + one network-gated real-attestation test.

### Top risks (for 4b/4c)
provenance-without-gh → native Sigstore, fail-closed (D1); macOS Gatekeeper → refuse
(D2); downgrade/replay → strictly-greater semver; EXDEV → temp in same dir;
rollback correctness → `off`→restore→`on`; `/usr/local/bin` not writable → refuse,
no escalation; socket-activation restart window → verify `/healthz` version.

**Remaining owner action:** approve starting **4b** (after the D1 Sigstore spike).
4c follows 4b. The read-only check (4a) is complete and safe to ship on its own.
