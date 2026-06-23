# Design spike: `trimwire update` (self-updater)

> **Internal maintainer design note — NOT a user guide and NOT a committed
> roadmap.** Nothing here is promised or scheduled. For how to update trimwire
> today, see [FAQ.md](FAQ.md#how-do-i-install-it). This file exists to capture the
> security/design questions an updater would have to answer before it could be built.

**Status:** phase **4a SHIPPED** (read-only check). Phases **4b (verify) + 4c
(apply)** are **implemented in an OPEN/DRAFT PR — NOT merged, NOT released.** 4b
adds `trimwire update --dry-run` (download + verify SHA-256 **and** a minisign
signature against a pinned key, fail-closed). 4c adds `--apply`/`--yes`
(verified atomic replace + restart + rollback; Linux + managed installs only).
**The pinned public key is not yet set** (`PINNED_PUBKEY` is empty), so in a real
build verification fails closed until the owner generates a signing key, adds the
CI secret, and pins the public key (see **Release signing — owner setup** below).
**Audit item:** P2-11.

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
logic (§2) is **done** (4a, shipped). Client-side verification (§3 —
**minisign signature against a pinned key + the `.sha256`**, see D1), **atomic
replace** (§4), **service restart** (§5), and **rollback** (§6) are **implemented
in this open/draft PR** for Linux (4b/4c). What's left is the **owner setup**
(generate + pin the signing key) and the fenced **4d** (Windows/macOS). See the
phased plan + owner decisions below.

> **Verifier MUST pin the workflow identity, not just the repo.** `gh attestation
> verify --repo AZagatti/trimwire` only proves the attestation belongs to this
> repo — any workflow in the repo could attest an arbitrary binary. The check must
> also assert the signer workflow is `.github/workflows/release.yml` via
> `--signer-workflow <owner>/<repo>/.github/workflows/release.yml` (path-based, so
> stable across tags — unlike `--cert-identity`, which embeds the ref).
> **Status:** the release `verify` job pins this (`--signer-workflow` +
> `--deny-self-hosted-runners`), and SECURITY-MODEL.md's recommended `gh` command
> does too. The **client-side updater does NOT use `gh`/attestations** (it can't
> require `gh`, and `sigstore-rs` can't verify attestations at our MSRV — see D1);
> it instead verifies a **minisign signature against a pinned key**. The
> attestation remains the public/CI provenance bar; anyone manually verifying a
> download with `gh` must still pin `--signer-workflow`, not `--repo`-only.

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
     true **provenance** is the right bar. Two layers provide it: (a) `release.yml`
     attests every archive with **GitHub artifact attestations**
     (`actions/attest-build-provenance`) and the `verify` job runs
     `gh attestation verify` — the public/CI bar, verifiable by anyone with `gh`;
     and (b) the `sign` job emits a detached **minisign** signature per archive,
     which the **client updater** verifies against a key **pinned in the binary**
     (no `gh`, no network round-trip to GitHub's attestation store). The client
     uses (b); see D1 for why not native Sigstore. See
     [SECURITY-MODEL.md](SECURITY-MODEL.md#verifying-a-downloaded-release).
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

- **D1 — client-side provenance verification = minisign/Ed25519 with a PINNED
  public key** (revised after a go/no-go spike; **native Sigstore is NO-GO**).
  Do **not** shell out to `gh` for the trust-critical check: it's a PATH-hijack
  vector for a credential-path tool, and requiring `gh` defeats the `curl|sh`
  audience. The client verifies a detached **minisign** signature
  (`minisign-verify`, zero-dependency, ~2.5K LoC) against a public key **embedded
  in the binary**, plus the existing `.sha256`. **Never** fall back to
  checksum-only: an empty/unset pinned key fails closed.
  - **Why not native Sigstore (the original D1):** the `sigstore` crate
    (`sigstore-rs`) **cannot verify GitHub artifact attestations** yet — its
    README states it "does not handle verification of attestations" (the DSSE/
    in-toto envelope `attest-build-provenance` emits; tracking issue open, the
    enabling PR unmerged). It also requires **edition 2024 / toolchain ≥ 1.89**,
    which **breaks our MSRV 1.85**, and pulls a mandatory **`aws-lc-rs` C
    dependency** (+~4 MB, NASM on Windows, 60–120 crates). Revisit if/when it
    gains attestation verification at our MSRV.
  - **Why not an in-house DSSE/Fulcio verifier:** zero new deps, but ~800–1500
    lines of bespoke X.509/ASN.1/Rekor crypto = an audit liability. Only worth it
    if transparency-log auditability becomes a hard requirement.
  - **The GitHub attestation stays** as the public/CI provenance bar (the
    `release.yml` `verify` job pins `--signer-workflow` + `--deny-self-hosted-runners`);
    minisign is the *client-side* gate that needs no `gh` and no network round-trip
    to GitHub's attestation store.
- **D2 — refuse macOS self-update in v1, same as Windows.** A downloaded binary
  is quarantine/Gatekeeper-blocked from launching as a service unless notarized;
  notarization is **out of scope**. v1 self-update is **Linux-only**; macOS +
  Windows print the manual update path and exit 2.

## PR4 plan (the self-updater) — phased, Unix-first

Phase **4a is shipped**; **4b + 4c are implemented in this open/draft PR** (not
merged, not released).

- **4a (SHIPPED) — read-only check.** `trimwire update`/`upgrade`:
  resolve+canonicalize `current_exe()` (abort on `(deleted)`), read the receipt,
  refuse (exit 2 + the per-method guidance) unless the install is self-updatable
  (`method="script"` + `binary_path == canonical current_exe()` + target match +
  parent writable), then query the latest GitHub release and report
  current/available (exit 0). Network failure is non-fatal. `/healthz` includes a
  `version` field (for 4c's post-restart check).
- **4b (IMPLEMENTED, draft) — verify.** `trimwire update --dry-run` downloads the
  latest release archive + `.sha256` + `.minisig` (following the
  `releases/<tag>/download/` redirect, HTTPS-only, size-capped), then verifies
  **checksum** (`sha2`) **then** the **minisign signature** against the pinned key
  — both must pass. Fail-closed on mismatch / missing signature / bad key /
  malformed signature / legacy (non-prehashed) signature / network or download
  failure. Reports verified (exit 0) or NOT verified (exit 1). No apply. Pure
  gates in `trimwire::update` (`verify_sha256`, `verify_minisig`,
  `verify_artifact`, `VerifyError`); I/O in `src/cli/update.rs`.
- **4c (IMPLEMENTED, draft) — apply (Linux).** `--apply` (TTY-confirmed) /
  `--yes` (non-interactive): re-checks eligibility, requires a pinned key,
  enforces **strictly-greater** version (anti-downgrade), downloads + verifies
  (4b), extracts via `tar`, then atomic replace — temp in the **same dir** →
  `fchmod 0755` → `fsync(file)` → copy old to `<exe>.bak` → `rename()` →
  `fsync(dir)`. Restart: `service::off()` → `on()` → poll `/healthz` until the
  served `version` == target. Rollback on any health failure: `off()` → restore
  `.bak` → `on()` → re-verify, and a rollback failure is surfaced loudly (never
  swallowed). Refuses on non-Linux (D2), non-managed installs, non-writable
  locations, and a non-interactive shell without `--yes` (no privilege
  escalation, no unattended apply). A localhost-only test seam
  (`TRIMWIRE_UPDATE_DRYRUN_APPLY`) exercises every gate up to — but not
  including — the swap, so the apply path is integration-tested without
  overwriting the test binary.
- **4d (fenced, NOT in this PR) — Windows self-replace; macOS notarized path.**

### Release signing — owner setup (REQUIRED before self-update works)

Until these steps are done, `PINNED_PUBKEY` is empty and the `release.yml` `sign`
job no-ops, so `--dry-run`/`--apply` fail closed (`NoPinnedKey`). The read-only
check is unaffected.

1. **Generate a key** (passwordless recommended for CI):
   `minisign -G -W -p trimwire.pub -s trimwire.key`
2. **Add CI secrets** in the repo: `MINISIGN_SECRET_KEY` = the full contents of
   `trimwire.key` (and `MINISIGN_PASSWORD` if you used a password-protected key).
   The `sign` job writes the `.minisig` for each archive on the next release.
3. **Pin the public key**: open `trimwire.pub` and copy the line that is the
   **base64 key payload** (the line that does NOT start with `untrusted comment:`)
   into `PINNED_PUBKEY` in `src/update.rs`, then cut a release built from that
   commit. From then on, every client verifies downloads against this key.
4. **Keep `trimwire.key` offline** (password manager / hardware token), never in
   the repo. **Rotation:** repeat 1–3 and publish the new public key in a minor
   release; old clients keep trusting the old key until they update through it, so
   overlap one release before retiring the old signing key.

### Files (this PR — 4b/4c)
`src/update.rs` (verification gates + `VerifyError` + `PINNED_PUBKEY`),
`src/cli/update.rs` (download w/ redirect+cap, `--dry-run`, `--apply`/`--yes`,
atomic replace, restart, rollback, test seam), `src/cli/service.rs`
(`healthz_version`), `src/main.rs` (`--dry-run`/`--apply`/`--yes` flags),
`.github/workflows/release.yml` (`sign` job), `Cargo.toml` (`minisign-verify`
dep; `minisign`/`sha2`/`hex` dev-deps), docs (this file, SECURITY-MODEL.md,
CLI.md, FAQ.md).

### Test plan (this PR)
Unit (`src/update.rs`): valid signature; tampered artifact (checksum gate);
tampered artifact with recomputed checksum (signature gate); tampered signature;
wrong key; missing/malformed signature; malformed pinned key; checksum
parse/mismatch/malformed; empty-key fail-closed. Integration (`tests/cli.rs`,
fake GitHub serving real minisign-signed fixtures): `--dry-run`
verified/tampered/missing-sig/no-key/network-fail; `--apply` refusals
(no-receipt, non-interactive-without-`--yes`, no-key) + no-op-when-current +
full verified path to the swap stage via the test seam.

### Top risks (4b/4c) and mitigations
provenance-without-gh → minisign pinned key, fail-closed (D1); macOS/Windows →
refuse (D2); downgrade/replay → strictly-greater semver; missing signature →
download error propagates (never skipped); EXDEV → temp in same dir; rollback
correctness → `off`→restore→`on`→re-verify, loud on failure; `/usr/local/bin`
not writable → refuse, no escalation; socket-activation restart window → poll
`/healthz` version; unbounded download → 200 MB cap + HTTPS-only redirects.

**Remaining owner action:** complete **Release signing — owner setup** (generate
key, add `MINISIGN_SECRET_KEY`, pin `PINNED_PUBKEY`), then review + merge this
draft. The read-only check (4a) is shipped and safe on its own.
