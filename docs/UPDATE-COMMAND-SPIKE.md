# Design spike: `trimwire update` (self-updater)

**Status:** spike / needs owner sign-off (product + security decision). Not implemented.
**Audit item:** P2-11 (no `trimwire update`/`upgrade`; update path was buried).

## Problem

There is no `trimwire update` command. Today users update by re-running their
install method (documented now in [FAQ.md](FAQ.md#how-do-i-install-it)):

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
3. **Integrity vs provenance — the crux.**
   - The published `.sha256` only proves the download wasn't *corrupted in
     transit*. It is served from the **same origin** as the binary, so it does
     **not** defend against a compromised release/account — an attacker who can
     replace the asset can replace the checksum too.
   - For a tool that sits in the request path with the user's API credential,
     true **provenance** (a signature the client verifies against a pinned
     public key — e.g. minisign/cosign, or GitHub artifact attestations) is the
     right bar. trimwire does not sign releases today.
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

## Options

- **(A) Minimal updater** — download latest release asset over HTTPS, verify the
  published `.sha256`, atomic-swap, restart the service, health-check + rollback.
  *Bounded, no new infra.* Caveat: checksum-only integrity (same-origin) — no
  defense against a compromised release. Acceptable only with an explicit
  in-help caveat and HTTPS via the system trust store to `github.com` (ordinary
  CA validation — *not* certificate pinning).
- **(B) Signed updater** — add release **signing** to `release.yml`, and have
  `update` verify before the swap. Lowest-friction path: **GitHub artifact
  attestations** (`actions/attest-build-provenance` + `gh attestation verify`) —
  no external key to manage, free on public repos, and reaches SLSA Build L2.
  Alternatively minisign/cosign with a public key pinned in the binary (more
  control, but you own the key lifecycle). *Strongest;* the right bar for a tool
  that sits in the request path with the user's credential.
- **(C) Status quo** — keep documenting the manual/`curl|sh`/`binstall` paths
  (done in this PR), defer a built-in updater.

## Recommendation

Ship **(C) now** (done — update paths are documented). Target **(B)** for a
built-in `trimwire update`: implement signing in the release pipeline first, then
build the updater against it. **(A)** is an acceptable interim *only* if the owner
accepts the same-origin-checksum caveat and wants the UX win sooner; if so, scope
it Unix-first (the Windows self-replace is a separate, riskier task).

**Decision required from the owner** before any implementation: pick (A) interim,
(B) target, or (C) hold — and, for (B), approve adding release signing + a pinned
verification key. This is a product/security call, not a mechanical change, so it
is intentionally left as a spike rather than implemented in the audit-resolution
pass.
