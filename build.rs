//! Build script: embed a short git SHA + commit date into the version string so
//! `trimwire --version` reads e.g. `0.1.0 (6fcbbb9 2026-06-07)` — handy in bug
//! reports. Hand-rolled (a few `git` calls) rather than pulling in `vergen`,
//! matching the repo's dependency-light ethos. Degrades gracefully: a build with
//! no git (e.g. a `cargo install` from the published crate) just reports the
//! plain Cargo version.

use std::process::Command;

fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8(out.stdout).ok()?.trim().to_owned();
    if s.is_empty() { None } else { Some(s) }
}

fn main() {
    let pkg = env!("CARGO_PKG_VERSION");
    // %h = short SHA, %cs = committer date as YYYY-MM-DD (no extra deps).
    let sha = git(&["rev-parse", "--short", "HEAD"]);
    let date = git(&["show", "-s", "--format=%cs", "HEAD"]);
    let version = match (sha, date) {
        (Some(sha), Some(date)) => format!("{pkg} ({sha} {date})"),
        (Some(sha), None) => format!("{pkg} ({sha})"),
        _ => pkg.to_owned(),
    };
    println!("cargo:rustc-env=TRIMWIRE_VERSION={version}");
    // Re-run when HEAD moves so the embedded SHA stays fresh.
    println!("cargo:rerun-if-changed=.git/HEAD");
    println!("cargo:rerun-if-changed=.git/refs");
}
