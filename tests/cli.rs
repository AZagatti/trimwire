//! End-to-end CLI tests: drive the built `trimwire` binary as a subprocess with
//! a controlled environment (a fake `claude` shim, a temp `$HOME`). Unix-only
//! — the fake-binary shim is a shell script.
#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::process::Command;

/// Path to the binary built by cargo for this test run.
fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_trimwire")
}

/// An OS-assigned free port (bound, read, released).
fn free_port() -> u16 {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    l.local_addr().unwrap().port()
}

/// `trimwire run` starts the gateway, launches `claude` with the gateway env
/// set, and propagates its exit code (acceptance: "run claude … in one
/// command").
#[test]
fn run_launches_claude_with_env_and_propagates_exit() {
    let dir = tempfile::tempdir().unwrap();
    let bindir = dir.path().join("bin");
    fs::create_dir_all(&bindir).unwrap();

    // Fake `claude`: record the gateway env + args, then exit 7.
    let fake = bindir.join("claude");
    fs::write(
        &fake,
        "#!/bin/sh\nprintf 'BASE=%s TS=%s ARGS=%s' \"$ANTHROPIC_BASE_URL\" \"$ENABLE_TOOL_SEARCH\" \"$*\" > \"$CLAUDE_OUT\"\nexit 7\n",
    )
    .unwrap();
    fs::set_permissions(&fake, fs::Permissions::from_mode(0o755)).unwrap();

    let port = free_port();
    let out = dir.path().join("out.txt");
    let path = format!(
        "{}:{}",
        bindir.display(),
        std::env::var("PATH").unwrap_or_default()
    );

    let status = Command::new(bin())
        .args(["run", "--print", "hi"])
        .env("HOME", dir.path())
        .env("PATH", path)
        .env("CLAUDE_OUT", &out)
        .env("TRIMWIRE_SERVER__LISTEN", format!("127.0.0.1:{port}"))
        .env("TRIMWIRE_LEDGER__ENABLED", "false")
        .env_remove("XDG_CONFIG_HOME")
        .status()
        .expect("spawn trimwire run");

    assert_eq!(status.code(), Some(7), "claude's exit code propagates");
    let recorded = fs::read_to_string(&out).expect("fake claude wrote output");
    assert!(
        recorded.contains(&format!("BASE=http://127.0.0.1:{port}")),
        "ANTHROPIC_BASE_URL set on child: {recorded}"
    );
    assert!(
        recorded.contains("TS=true"),
        "ENABLE_TOOL_SEARCH set: {recorded}"
    );
    assert!(
        recorded.contains("ARGS=--print hi"),
        "args forwarded: {recorded}"
    );
}

/// The `--help` "Commands by group" legend must mention every visible
/// top-level command. Regression guard: when a command is unhidden (as `run`
/// and `hook` were in v0.3.10), it must be added to the legend too — otherwise
/// it shows in the flat `Commands:` list but is missing from the grouped
/// summary users scan. Tokenize the legend and assert membership so any future
/// unhidden command is caught.
#[test]
fn help_legend_covers_every_visible_command() {
    let out = Command::new(bin())
        .arg("--help")
        .output()
        .expect("spawn trimwire --help");
    assert!(out.status.success(), "--help exits 0");
    let help = String::from_utf8_lossy(&out.stdout);

    // Visible top-level commands = the first token of each indented line in the
    // `Commands:` block (up to the blank line before `Options:`). Excludes the
    // clap-builtin `help`, which is intentionally absent from the legend.
    let commands_block = help
        .split("Commands:\n")
        .nth(1)
        .expect("`Commands:` section present");
    // Relies on clap 4 emitting a blank line between the `Commands:` block and
    // the `Options:` block — stable in clap 4's default help template.
    let visible: Vec<&str> = commands_block
        .lines()
        .take_while(|l| !l.trim().is_empty())
        .filter_map(|l| l.strip_prefix("  "))
        .map(|l| l.split_whitespace().next().unwrap_or(""))
        .filter(|c| !c.is_empty() && *c != "help")
        .collect();
    assert!(
        visible.contains(&"run") && visible.contains(&"hook"),
        "sanity: run + hook are visible top-level commands"
    );

    // The legend = everything after the "Commands by group:" heading. Tokenize
    // on non-alphanumerics so parenthetical sub-actions don't cause false hits.
    let legend = help
        .split("Commands by group:")
        .nth(1)
        .expect("`Commands by group:` legend present");
    let legend_tokens: std::collections::HashSet<&str> = legend
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|t| !t.is_empty())
        .collect();

    for cmd in &visible {
        assert!(
            legend_tokens.contains(cmd),
            "visible command `{cmd}` is missing from the `Commands by group:` legend"
        );
    }
}

/// `update` and `upgrade` are now SEPARATE top-level commands (not aliases):
/// both appear in `--help`, and `upgrade` has its own `--dry-run`/`--yes` flags
/// while `update` does not advertise them. (Behavior is covered by the
/// update/upgrade test sections below.)
#[test]
fn update_and_upgrade_are_distinct_commands() {
    let out = Command::new(bin())
        .arg("--help")
        .output()
        .expect("spawn trimwire --help");
    let help = String::from_utf8_lossy(&out.stdout);
    assert!(help.contains("update"), "update listed: {help}");
    assert!(help.contains("upgrade"), "upgrade listed: {help}");

    // `upgrade --help` advertises the state-changing flags…
    let up = Command::new(bin())
        .args(["upgrade", "--help"])
        .output()
        .expect("spawn trimwire upgrade --help");
    let uph = String::from_utf8_lossy(&up.stdout);
    assert!(
        uph.contains("--dry-run") && uph.contains("--yes"),
        "upgrade exposes --dry-run/--yes: {uph}"
    );

    // …while `update --help` does NOT (the flags are hidden/deprecated there).
    let upd = Command::new(bin())
        .args(["update", "--help"])
        .output()
        .expect("spawn trimwire update --help");
    let updh = String::from_utf8_lossy(&upd.stdout);
    assert!(
        !updh.contains("--apply"),
        "update must not advertise --apply: {updh}"
    );
}

/// `trimwire install` writes a config + a guarded shell-rc block, and is
/// idempotent on re-run (acceptance: "fresh install works end-to-end").
#[test]
fn install_writes_config_and_rc_idempotently() {
    let dir = tempfile::tempdir().unwrap();
    let install = || {
        Command::new(bin())
            .arg("install")
            .env("HOME", dir.path())
            .env("SHELL", "/bin/zsh")
            .env_remove("XDG_CONFIG_HOME")
            // Keep the install receipt under the temp HOME, never the real
            // XDG_DATA_HOME of the machine running the tests.
            .env_remove("XDG_DATA_HOME")
            .output()
            .expect("spawn trimwire install")
    };

    assert!(install().status.success(), "first install succeeds");
    let cfg = dir.path().join(".config/trimwire.toml");
    let rc = dir.path().join(".zshrc");
    assert!(cfg.exists(), "starter config written");
    let rc1 = fs::read_to_string(&rc).expect("zshrc written");
    assert!(rc1.contains("ANTHROPIC_BASE_URL"));
    assert_eq!(rc1.matches("# >>> trimwire >>>").count(), 1);

    assert!(install().status.success(), "second install succeeds");
    let rc2 = fs::read_to_string(&rc).unwrap();
    assert_eq!(
        rc2.matches("# >>> trimwire >>>").count(),
        1,
        "re-running install must not duplicate the block"
    );
}

/// `trimwire install` (no curl|sh installer) records an install receipt with
/// method "unknown" — a cargo/manual install we did not place, which a future
/// `trimwire update` must NOT assume is self-updatable. Records the build target.
#[test]
fn install_writes_install_receipt() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    let out = Command::new(bin())
        .arg("install")
        .env("HOME", dir.path())
        .env("SHELL", "/bin/zsh")
        .env("XDG_DATA_HOME", &data)
        .env_remove("XDG_CONFIG_HOME")
        .output()
        .expect("spawn trimwire install");
    assert!(out.status.success(), "install succeeds");
    let rcpt = data.join("trimwire").join("install-receipt.json");
    let s = fs::read_to_string(&rcpt).expect("install receipt written");
    assert!(s.contains("\"schema_version\": 1"), "receipt: {s}");
    assert!(s.contains("\"method\": \"unknown\""), "receipt: {s}");
    assert!(
        s.contains(env!("TRIMWIRE_TARGET")),
        "receipt records the build target, got: {s}"
    );
}

/// `trimwire doctor` reports the install receipt when present, and says so
/// (non-fatally) when absent.
#[test]
fn doctor_reports_install_receipt_presence() {
    // Absent: a fresh HOME with an empty data dir → "no receipt recorded".
    let dir = tempfile::tempdir().unwrap();
    let absent = Command::new(bin())
        .arg("doctor")
        // Keep the doctor update-advisory check offline + deterministic: a
        // localhost base is honored, and port 1 refuses instantly → no real
        // GitHub call, no 6s timeout. (Override is ignored for non-localhost.)
        .env("TRIMWIRE_UPDATE_API_BASE", "http://127.0.0.1:1")
        .env("HOME", dir.path())
        // Pin the gateway probe to a free (closed) port so it can't collide with
        // the default :8765 under parallel test runs (gateway state is irrelevant
        // to this test's assertion, but keep the probe deterministic + fast).
        .env(
            "TRIMWIRE_SERVER__LISTEN",
            format!("127.0.0.1:{}", free_port()),
        )
        .env("XDG_DATA_HOME", dir.path().join("data"))
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("ANTHROPIC_BASE_URL")
        .output()
        .expect("spawn doctor");
    let s = String::from_utf8_lossy(&absent.stdout);
    assert!(
        s.contains("no receipt recorded"),
        "doctor should note a missing receipt, got: {s}"
    );

    // Present: seed one via `trimwire install`, then doctor reports it.
    let dir2 = tempfile::tempdir().unwrap();
    let data2 = dir2.path().join("data");
    assert!(
        Command::new(bin())
            .arg("install")
            .env("HOME", dir2.path())
            .env("SHELL", "/bin/zsh")
            .env("XDG_DATA_HOME", &data2)
            .env_remove("XDG_CONFIG_HOME")
            .output()
            .expect("spawn install")
            .status
            .success()
    );
    let present = Command::new(bin())
        .arg("doctor")
        // Keep the doctor update-advisory check offline + deterministic: a
        // localhost base is honored, and port 1 refuses instantly → no real
        // GitHub call, no 6s timeout. (Override is ignored for non-localhost.)
        .env("TRIMWIRE_UPDATE_API_BASE", "http://127.0.0.1:1")
        .env("HOME", dir2.path())
        // Pin the gateway probe to a free (closed) port (see note above).
        .env(
            "TRIMWIRE_SERVER__LISTEN",
            format!("127.0.0.1:{}", free_port()),
        )
        .env("XDG_DATA_HOME", &data2)
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("ANTHROPIC_BASE_URL")
        .output()
        .expect("spawn doctor");
    let s2 = String::from_utf8_lossy(&present.stdout);
    assert!(
        s2.contains("install:") && s2.contains("unknown"),
        "doctor should report the receipt method, got: {s2}"
    );
}

/// A corrupt/unparseable receipt must degrade gracefully: `load()` returns None
/// and `doctor` falls back to "no receipt recorded" rather than erroring.
#[test]
fn doctor_tolerates_corrupt_install_receipt() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    let rcpt_dir = data.join("trimwire");
    fs::create_dir_all(&rcpt_dir).unwrap();
    fs::write(rcpt_dir.join("install-receipt.json"), "{ not valid json").unwrap();

    let out = Command::new(bin())
        .arg("doctor")
        // Keep the doctor update-advisory check offline + deterministic: a
        // localhost base is honored, and port 1 refuses instantly → no real
        // GitHub call, no 6s timeout. (Override is ignored for non-localhost.)
        .env("TRIMWIRE_UPDATE_API_BASE", "http://127.0.0.1:1")
        .env("HOME", dir.path())
        // Pin the gateway probe to a free (closed) port (see note above).
        .env(
            "TRIMWIRE_SERVER__LISTEN",
            format!("127.0.0.1:{}", free_port()),
        )
        .env("XDG_DATA_HOME", &data)
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("ANTHROPIC_BASE_URL")
        .output()
        .expect("spawn doctor");
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(
        s.contains("no receipt recorded"),
        "corrupt receipt should fall back to 'no receipt recorded', got: {s}"
    );
}

/// `trimwire stats` with the ledger disabled prints a friendly message, exit 0.
#[test]
fn stats_reports_disabled_ledger() {
    let dir = tempfile::tempdir().unwrap();
    let out = Command::new(bin())
        .arg("stats")
        .env("HOME", dir.path())
        .env("TRIMWIRE_LEDGER__ENABLED", "false")
        .env_remove("XDG_CONFIG_HOME")
        .output()
        .expect("spawn trimwire stats");
    assert!(out.status.success(), "stats exits 0 when ledger disabled");
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains("ledger is disabled"), "got: {s}");
}

/// `trimwire config show` prints the resolved effective config + active profile.
#[test]
fn config_show_prints_resolved_config() {
    let dir = tempfile::tempdir().unwrap();
    let out = Command::new(bin())
        .args(["config", "show"])
        .env("HOME", dir.path())
        .env_remove("XDG_CONFIG_HOME")
        .output()
        .expect("spawn config show");
    assert!(out.status.success());
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains("active profile: default"), "got: {s}");
    assert!(s.contains("[strategies.cross_turn_dedup]"), "got: {s}");
}

/// `trimwire config show --json` emits valid JSON with the resolved profile.
#[test]
fn config_show_json_is_valid() {
    let dir = tempfile::tempdir().unwrap();
    let out = Command::new(bin())
        .args(["config", "show", "--json"])
        .env("HOME", dir.path())
        .env_remove("XDG_CONFIG_HOME")
        .output()
        .expect("spawn config show --json");
    assert!(out.status.success());
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).expect("valid json");
    assert_eq!(v["profile"], "default");
    assert_eq!(v["strategies"]["bloat_cap"]["threshold_bytes"], 4096);
}

/// `config show` reflects the *resolved* merge — a TRIMWIRE_PROFILE override
/// changes the printed knobs (proves it's not just echoing the default).
#[test]
fn config_show_reflects_profile_override() {
    let dir = tempfile::tempdir().unwrap();
    let out = Command::new(bin())
        .args(["config", "show", "--json"])
        .env("HOME", dir.path())
        .env_remove("XDG_CONFIG_HOME")
        .env("TRIMWIRE_PROFILE", "gentle")
        .output()
        .expect("spawn config show --json");
    assert!(out.status.success());
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).expect("valid json");
    assert_eq!(v["profile"], "gentle");
    // `gentle` uses a conservative bloat_cap threshold of 32 KB — proves the merge resolved.
    assert_eq!(v["strategies"]["bloat_cap"]["threshold_bytes"], 32768);
}

/// `trimwire stats --json` emits valid JSON; on a fresh HOME (no ledger) it
/// reports availability=false rather than erroring.
#[test]
fn stats_json_is_valid_when_no_ledger() {
    let dir = tempfile::tempdir().unwrap();
    let out = Command::new(bin())
        .args(["stats", "--json"])
        .env("HOME", dir.path())
        .env_remove("XDG_CONFIG_HOME")
        .output()
        .expect("spawn stats --json");
    assert!(out.status.success());
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).expect("valid json");
    assert_eq!(v["available"], false);
}

/// `trimwire doctor` — pre-install state: no config file, no gateway, no env var.
/// This is expected right after downloading the binary; doctor must exit 0 and
/// print a friendly "run `trimwire install`" hint, not a scary failure.
#[test]
fn doctor_pre_install_exits_zero() {
    let dir = tempfile::tempdir().unwrap();
    let out = Command::new(bin())
        .arg("doctor")
        // Keep the doctor update-advisory check offline + deterministic: a
        // localhost base is honored, and port 1 refuses instantly → no real
        // GitHub call, no 6s timeout. (Override is ignored for non-localhost.)
        .env("TRIMWIRE_UPDATE_API_BASE", "http://127.0.0.1:1")
        .env("HOME", dir.path())
        // Pin the gateway probe to a free (closed) port so "gateway down" is
        // deterministic under parallel test runs — the default :8765 can be
        // transiently bound by another test.
        .env(
            "TRIMWIRE_SERVER__LISTEN",
            format!("127.0.0.1:{}", free_port()),
        )
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("ANTHROPIC_BASE_URL")
        .output()
        .expect("spawn doctor");
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "doctor exits 0 in pre-install state (no config, no gateway, no env). got: {s}"
    );
    assert!(s.contains("trimwire doctor"), "got: {s}");
    assert!(
        s.contains("not installed yet"),
        "should indicate pre-install state, got: {s}"
    );
    assert!(
        s.contains("trimwire install"),
        "should hint at the next step, got: {s}"
    );
}

/// `trimwire doctor` reports the build platform (the target triple embedded by
/// build.rs as `TRIMWIRE_TARGET`). It must appear even in the pre-install state,
/// and must match the triple this test binary was built for — it's the
/// asset-selection primitive a future `trimwire update` relies on.
#[test]
fn doctor_reports_build_platform() {
    let dir = tempfile::tempdir().unwrap();
    let out = Command::new(bin())
        .arg("doctor")
        // Keep the doctor update-advisory check offline + deterministic: a
        // localhost base is honored, and port 1 refuses instantly → no real
        // GitHub call, no 6s timeout. (Override is ignored for non-localhost.)
        .env("TRIMWIRE_UPDATE_API_BASE", "http://127.0.0.1:1")
        .env("HOME", dir.path())
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("ANTHROPIC_BASE_URL")
        .output()
        .expect("spawn doctor");
    let s = String::from_utf8_lossy(&out.stdout);
    let target = env!("TRIMWIRE_TARGET");
    assert!(
        !target.is_empty() && target != "unknown",
        "target: {target}"
    );
    assert!(
        s.contains("platform:") && s.contains(target),
        "doctor should report `platform: {target}`, got: {s}"
    );
}

/// Regression (release-polish): in the pre-install state (no config, no env,
/// gateway down) plain `doctor` exits 0 — but `doctor --strict` must exit
/// non-zero, matching the documented `--strict` contract in docs/CLI.md (it's
/// for CI / scripted health checks, which should fail when trimwire isn't set up
/// at all). The pre-install early-return previously bypassed the strict check.
#[test]
fn doctor_strict_pre_install_exits_nonzero() {
    let dir = tempfile::tempdir().unwrap();
    let out = Command::new(bin())
        .args(["doctor", "--strict"])
        // Offline + deterministic update-advisory check (see note above).
        .env("TRIMWIRE_UPDATE_API_BASE", "http://127.0.0.1:1")
        .env("HOME", dir.path())
        // Pin the gateway probe to a free (closed) port so "gateway down" is
        // deterministic under parallel test runs — the default :8765 can be
        // transiently bound by another test.
        .env(
            "TRIMWIRE_SERVER__LISTEN",
            format!("127.0.0.1:{}", free_port()),
        )
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("ANTHROPIC_BASE_URL")
        .output()
        .expect("spawn doctor --strict");
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(
        !out.status.success(),
        "doctor --strict must exit non-zero in pre-install state. got: {s}"
    );
    assert!(
        s.contains("not installed yet"),
        "still prints pre-install guidance: {s}"
    );
}

/// `trimwire doctor` — installed-but-gateway-not-running: config file EXISTS (so
/// it's not pre-install), but the gateway is not up and `ANTHROPIC_BASE_URL` is
/// not set. Both are ADVISORY/recoverable, so doctor exits 0 (so that
/// `trimwire doctor && claude` still works) while printing a warning.
#[test]
fn doctor_installed_but_gateway_down_exits_zero_advisory() {
    let dir = tempfile::tempdir().unwrap();
    // Write a minimal valid config so the "pre-install" early-exit path is bypassed.
    let config_dir = dir.path().join(".config");
    std::fs::create_dir_all(&config_dir).unwrap();
    std::fs::write(
        config_dir.join("trimwire.toml"),
        "profile = \"default\"\n[server]\nlisten = \"127.0.0.1:8765\"\nupstream = \"https://api.anthropic.com\"\n",
    )
    .unwrap();
    let out = Command::new(bin())
        .arg("doctor")
        // Keep the doctor update-advisory check offline + deterministic: a
        // localhost base is honored, and port 1 refuses instantly → no real
        // GitHub call, no 6s timeout. (Override is ignored for non-localhost.)
        .env("TRIMWIRE_UPDATE_API_BASE", "http://127.0.0.1:1")
        .env("HOME", dir.path())
        // Override the config's listen to a free (closed) port so the gateway
        // probe deterministically sees "down" under parallel test runs, even if
        // another test transiently binds the config's :8765.
        .env(
            "TRIMWIRE_SERVER__LISTEN",
            format!("127.0.0.1:{}", free_port()),
        )
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("ANTHROPIC_BASE_URL")
        .output()
        .expect("spawn doctor");
    let s = String::from_utf8_lossy(&out.stdout);
    // Gateway-not-running and ANTHROPIC_BASE_URL-not-set are recoverable, so
    // doctor exits 0 (advisory warnings only) so `trimwire doctor && claude` works.
    assert!(
        out.status.success(),
        "doctor exits 0 when gateway is not yet running (advisory, not a hard failure). got: {s}"
    );
    assert!(s.contains("trimwire doctor"), "got: {s}");
    assert!(s.contains("config loads"), "got: {s}");
    assert!(s.contains("ANTHROPIC_BASE_URL not set"), "got: {s}");
    // The warning (not an error marker) should appear.
    assert!(
        s.contains("gateway not responding") || s.contains("trimwire on"),
        "got: {s}"
    );
}

#[test]
fn doctor_strict_exits_one_on_advisory() {
    // Same recoverable state as the advisory test, but `--strict` must turn the
    // ⚠ warnings (gateway down / ANTHROPIC_BASE_URL unset) into a non-zero exit
    // so CI / scripted health checks can gate on it (audit P2-8).
    let dir = tempfile::tempdir().unwrap();
    let config_dir = dir.path().join(".config");
    std::fs::create_dir_all(&config_dir).unwrap();
    std::fs::write(
        config_dir.join("trimwire.toml"),
        "profile = \"default\"\n[server]\nlisten = \"127.0.0.1:8765\"\nupstream = \"https://api.anthropic.com\"\n",
    )
    .unwrap();
    let out = Command::new(bin())
        .args(["doctor", "--strict"])
        // Offline + deterministic update-advisory check (see note above).
        .env("TRIMWIRE_UPDATE_API_BASE", "http://127.0.0.1:1")
        .env("HOME", dir.path())
        // Override the config's listen to a free (closed) port so the gateway
        // probe deterministically sees "down" under parallel test runs, even if
        // another test transiently binds the config's :8765.
        .env(
            "TRIMWIRE_SERVER__LISTEN",
            format!("127.0.0.1:{}", free_port()),
        )
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("ANTHROPIC_BASE_URL")
        .output()
        .expect("spawn doctor --strict");
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(
        !out.status.success(),
        "doctor --strict must exit non-zero on advisory warnings. got: {s}"
    );
}

/// `trimwire doctor` surfaces the update-advisory bullet when a newer stable
/// release exists — wired to a LOCAL fake GitHub (never the real one). This is
/// the one doctor test that exercises the advisory path on purpose; every other
/// doctor test forces the check offline (refused localhost) so it stays
/// deterministic.
#[test]
fn doctor_reports_update_available_advisory() {
    let dir = tempfile::tempdir().unwrap();
    // The update advisory runs unconditionally (before doctor's install-state
    // branch); the config just keeps doctor in a realistic "installed" state.
    let config_dir = dir.path().join(".config");
    std::fs::create_dir_all(&config_dir).unwrap();
    std::fs::write(
        config_dir.join("trimwire.toml"),
        "profile = \"default\"\n[server]\nlisten = \"127.0.0.1:8765\"\nupstream = \"https://api.anthropic.com\"\n",
    )
    .unwrap();
    // Fake GitHub advertises a far-newer stable tag → the advisory must appear.
    let gh = FakeGitHub::start("v999.0.0");
    let out = Command::new(bin())
        .arg("doctor")
        .env("HOME", dir.path())
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("ANTHROPIC_BASE_URL")
        .env("TRIMWIRE_UPDATE_API_BASE", gh.base())
        .output()
        .expect("spawn doctor");
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "doctor stays exit 0 (the update check is advisory only). got: {s}"
    );
    assert!(
        s.contains("999.0.0") && s.contains("available") && s.contains("trimwire upgrade"),
        "doctor should surface the update-advisory bullet, got: {s}"
    );
}

/// Inverse of the advisory test: when the latest stable release is NOT newer than
/// the running version, doctor surfaces NO update bullet (don't nag users to
/// "upgrade" to an older/equal release). Wired to a LOCAL fake GitHub.
#[test]
fn doctor_no_advisory_when_not_newer() {
    let dir = tempfile::tempdir().unwrap();
    let gh = FakeGitHub::start("v0.0.1"); // older than this build
    let out = Command::new(bin())
        .arg("doctor")
        .env("HOME", dir.path())
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("ANTHROPIC_BASE_URL")
        .env("TRIMWIRE_UPDATE_API_BASE", gh.base())
        .output()
        .expect("spawn doctor");
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(
        !s.contains("available (you have"),
        "no update-advisory bullet when the latest release is not newer, got: {s}"
    );
}

/// A non-stable latest tag (prerelease) must NOT raise the advisory either —
/// `newer_available` gates on `is_stable_release_tag`, so a stray `-rc` tag is
/// never surfaced as "available". Wired to a LOCAL fake GitHub.
#[test]
fn doctor_no_advisory_for_non_stable_latest_tag() {
    let dir = tempfile::tempdir().unwrap();
    let gh = FakeGitHub::start("v999.0.0-rc.1"); // newer number, but not stable
    let out = Command::new(bin())
        .arg("doctor")
        .env("HOME", dir.path())
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("ANTHROPIC_BASE_URL")
        .env("TRIMWIRE_UPDATE_API_BASE", gh.base())
        .output()
        .expect("spawn doctor");
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(
        !s.contains("available (you have"),
        "a non-stable (prerelease) latest tag must not raise the advisory, got: {s}"
    );
}

/// `summarizer setup` with stdin closed (EOF) must cancel cleanly — never spin
/// forever re-prompting on an empty answer — and write no config.
#[test]
fn summarizer_setup_cancels_on_stdin_eof() {
    let dir = tempfile::tempdir().unwrap();
    let out = Command::new(bin())
        .args(["summarizer", "setup"])
        .env("HOME", dir.path())
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("TRIMWIRE_OLLAMA_ENDPOINT") // isolate from a dev's host env
        .stdin(std::process::Stdio::null())
        .output()
        .expect("spawn summarizer setup");
    assert!(!out.status.success(), "EOF cancels with a non-zero exit");
    let all = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(all.contains("setup cancelled"), "got: {all}");
    // Real path is $HOME/.config/trimwire.toml (global_config_path). The old
    // assertion checked .config/trimwire/config.toml — a path the wizard never
    // writes — so it passed vacuously. This now actually fails if a cancel writes.
    assert!(
        !dir.path().join(".config/trimwire.toml").exists(),
        "cancel must write no config"
    );
}

/// `summarizer setup` model-free happy path: pick model-free (`m`) then confirm
/// the write (`y`). Asserts the config is written with `engine = "model-free"`,
/// stores no API key, and PRESERVES a pre-existing non-summarizer section.
/// Environment-independent: `m` selects model-free regardless of what the entry
/// ollama probe finds, so this is stable whether or not ollama is running.
#[test]
fn summarizer_setup_model_free_writes_config_and_preserves_other_sections() {
    use std::io::Write;
    use std::process::Stdio;

    let dir = tempfile::tempdir().unwrap();
    // global_config_path() with HOME set + XDG removed = $HOME/.config/trimwire.toml.
    let cfg_path = dir.path().join(".config/trimwire.toml");
    fs::create_dir_all(cfg_path.parent().unwrap()).unwrap();
    // Pre-existing, NON-summarizer config that must survive the wizard.
    fs::write(&cfg_path, "[server]\nlisten = \"127.0.0.1:9999\"\n").unwrap();

    let mut child = Command::new(bin())
        .args(["summarizer", "setup"])
        .env("HOME", dir.path())
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("TRIMWIRE_OLLAMA_ENDPOINT") // isolate from a dev's host env
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn summarizer setup");
    // "m" = model-free at the primary picker; "y" = confirm the write.
    child.stdin.take().unwrap().write_all(b"m\ny\n").unwrap();
    let out = child.wait_with_output().expect("wait");
    let all = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        out.status.success(),
        "model-free setup should succeed; got: {all}"
    );

    let cfg = fs::read_to_string(&cfg_path).expect("config written");
    assert!(
        cfg.contains("engine = \"model-free\""),
        "engine must be model-free; got:\n{cfg}"
    );
    assert!(
        cfg.contains("listen = \"127.0.0.1:9999\""),
        "pre-existing [server] section must be preserved; got:\n{cfg}"
    );
    assert!(
        !cfg.to_lowercase().contains("api_key ="),
        "model-free must store no API key value; got:\n{cfg}"
    );
}

/// `sweep all` must REFUSE (not hang, not destroy) when stdin is not a terminal
/// and `--yes` is absent — the non-interactive safeguard. The transcript on disk
/// must be left byte-for-byte intact with no backup created.
#[test]
fn sweep_all_refuses_without_yes_when_noninteractive() {
    let dir = tempfile::tempdir().unwrap();
    let sess_dir = dir.path().join("projects/-home-user-x");
    fs::create_dir_all(&sess_dir).unwrap();
    let sess = sess_dir.join("s.jsonl");
    let body = "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"hi\"}}\n";
    fs::write(&sess, body).unwrap();

    let out = Command::new(bin())
        .args(["sweep", "all"])
        .env("CLAUDE_CONFIG_DIR", dir.path())
        .env("HOME", dir.path())
        .env_remove("XDG_CONFIG_HOME")
        .stdin(std::process::Stdio::null())
        .output()
        .expect("spawn sweep all");

    let all = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        out.status.success(),
        "refusal is a clean exit, not an error; got: {all}"
    );
    assert!(
        all.contains("refusing to sweep") && all.contains("--yes"),
        "must explain it refused and point at --yes; got: {all}"
    );
    // File untouched, and no `.bak.*` backup created.
    assert_eq!(
        fs::read_to_string(&sess).unwrap(),
        body,
        "transcript must be unchanged"
    );
    let made_backup = fs::read_dir(&sess_dir)
        .unwrap()
        .flatten()
        .any(|e| e.file_name().to_string_lossy().contains(".bak."));
    assert!(
        !made_backup,
        "no backup should be created when sweep refuses"
    );
}

/// A tiny fake ollama server: answers `GET /api/tags` with the given model list
/// so the `summarizer setup` probe is deterministic and offline (no real ollama,
/// no model inference). Stops + joins on drop. Pointed at via the
/// `TRIMWIRE_OLLAMA_ENDPOINT` test seam. Declare it BEFORE the child and let it
/// drop at end of scope, so it outlives `wait_with_output()` (join-on-drop only
/// blocks briefly once the child has exited and closed the socket).
struct FakeOllama {
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
    port: u16,
}

impl FakeOllama {
    fn start(models: &[&str]) -> Self {
        use std::io::{Read, Write};
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        listener.set_nonblocking(true).unwrap();

        let models_json = models
            .iter()
            .map(|m| format!("{{\"name\":\"{m}\"}}"))
            .collect::<Vec<_>>()
            .join(",");
        let body = format!("{{\"models\":[{models_json}]}}");

        let stop = Arc::new(AtomicBool::new(false));
        let stop2 = stop.clone();
        let handle = std::thread::spawn(move || {
            while !stop2.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((mut s, _)) => {
                        let _ = s.set_read_timeout(Some(std::time::Duration::from_millis(50)));
                        let mut buf = [0u8; 2048];
                        let _ = s.read(&mut buf); // best-effort drain the request line
                        let resp = format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\
                             Content-Length: {}\r\nConnection: close\r\n\r\n{}",
                            body.len(),
                            body
                        );
                        let _ = s.write_all(resp.as_bytes());
                        let _ = s.flush();
                    }
                    Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(std::time::Duration::from_millis(5));
                    }
                    Err(_) => break,
                }
            }
        });
        FakeOllama {
            stop,
            handle: Some(handle),
            port,
        }
    }

    fn endpoint(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }
}

impl Drop for FakeOllama {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

/// `summarizer setup` API-provider happy path. The entry ollama probe is pointed
/// at a dead port (no local models) so the added provider is item 1 —
/// deterministic regardless of any real ollama. Asserts the provider block is
/// written with the env-var NAME (never a key value), the engine is the provider
/// id, and a pre-existing unrelated section survives.
#[test]
fn summarizer_setup_api_provider_writes_provider_block_without_key() {
    use std::io::Write;
    use std::process::Stdio;

    let dir = tempfile::tempdir().unwrap();
    let cfg_path = dir.path().join(".config/trimwire.toml");
    fs::create_dir_all(cfg_path.parent().unwrap()).unwrap();
    fs::write(&cfg_path, "[server]\nlisten = \"127.0.0.1:9999\"\n").unwrap();

    // A fake ollama with NO models → probe reachable but empty → no local items in
    // the picker → the added provider is item 1 (deterministic). Using a held-open
    // socket avoids the dead-port TOCTOU a bind-then-drop free_port() would risk.
    let fake = FakeOllama::start(&[]);
    let mut child = Command::new(bin())
        .args(["summarizer", "setup"])
        .env("HOME", dir.path())
        .env_remove("XDG_CONFIG_HOME")
        .env("TRIMWIRE_OLLAMA_ENDPOINT", fake.endpoint())
        .env_remove("TESTPROV_KEY")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn summarizer setup");
    // a=add provider; id; style=anthropic; base_url=default(empty); model;
    // key FILE (blank=skip, prompted first); key ENV-VAR NAME; y=add;
    // 1=pick as primary; n=no fallback; y=write.
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"a\ntestprov\nanthropic\n\ntest-model\n\nTESTPROV_KEY\ny\n1\nn\ny\n")
        .unwrap();
    let out = child.wait_with_output().expect("wait");
    let all = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(out.status.success(), "api setup should succeed; got: {all}");

    let cfg = fs::read_to_string(&cfg_path).expect("config written");
    assert!(
        cfg.contains("engine = \"testprov\""),
        "engine = provider id; got:\n{cfg}"
    );
    assert!(
        cfg.contains("[[summarizer.providers]]"),
        "provider block written; got:\n{cfg}"
    );
    assert!(cfg.contains("\"test-model\""), "model written; got:\n{cfg}");
    assert!(
        cfg.contains("https://api.anthropic.com"),
        "default base_url written; got:\n{cfg}"
    );
    assert!(
        cfg.contains("api_key_env") && cfg.contains("\"TESTPROV_KEY\""),
        "stores the env-var NAME; got:\n{cfg}"
    );
    assert!(
        !cfg.to_lowercase().contains("sk-"),
        "must not store a key VALUE; got:\n{cfg}"
    );
    assert!(
        cfg.contains("listen = \"127.0.0.1:9999\""),
        "unrelated [server] preserved; got:\n{cfg}"
    );
}

/// `summarizer setup` — issue #118 regression: after adding a provider while
/// local models are present, the primary picker must (a) mark the new provider
/// row `← your new provider` and (b) DEFAULT the selection to it, not to a local
/// model. Two local models exist (rows 1–2); the added provider is row 3. We
/// accept the default (blank line) at the primary pick and assert the written
/// engine is the provider id — proving the default pointed at the new provider.
#[test]
fn summarizer_setup_defaults_primary_to_newly_added_provider() {
    use std::io::Write;
    use std::process::Stdio;

    let dir = tempfile::tempdir().unwrap();
    let cfg_path = dir.path().join(".config/trimwire.toml");
    fs::create_dir_all(cfg_path.parent().unwrap()).unwrap();
    fs::write(&cfg_path, "[server]\nlisten = \"127.0.0.1:9999\"\n").unwrap();

    // Two local models → the added provider is item 3 (a local model is item 1,
    // which is the OLD buggy default the fix must move off of).
    let fake = FakeOllama::start(&["qwen3.5:4b", "llama3"]);
    let mut child = Command::new(bin())
        .args(["summarizer", "setup"])
        .env("HOME", dir.path())
        .env_remove("XDG_CONFIG_HOME")
        .env("TRIMWIRE_OLLAMA_ENDPOINT", fake.endpoint())
        .env_remove("NEWPROV_KEY")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn summarizer setup");
    // a=add; id; style; base_url(default); model; key file(skip); env NAME; y=add;
    // <blank>=accept the primary DEFAULT (must be the new provider, row 3);
    // n=no fallback; y=write.
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"a\nnewprov\nanthropic\n\ntest-model\n\nNEWPROV_KEY\ny\n\nn\ny\n")
        .unwrap();
    let out = child.wait_with_output().expect("wait");
    let all = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(out.status.success(), "setup should succeed; got: {all}");
    // The re-displayed picker marks the new provider row.
    assert!(
        all.contains("your new provider"),
        "picker must mark the newly-added provider; got: {all}"
    );
    let cfg = fs::read_to_string(&cfg_path).expect("config written");
    // The accepted default selected the provider, not a local model.
    assert!(
        cfg.contains("engine = \"newprov\""),
        "accepting the default must pick the new provider as primary; got:\n{cfg}"
    );
}

/// `summarizer setup` local-ollama happy path against a FAKE ollama server (no
/// real ollama, no inference). The `TRIMWIRE_OLLAMA_ENDPOINT` seam points the
/// probe at the fake, which reports an approved model — so picking it as primary
/// is deterministic. Asserts the `[summarizer.local]` block + preserved config.
#[test]
fn summarizer_setup_local_path_writes_local_block() {
    use std::io::Write;
    use std::process::Stdio;

    let fake = FakeOllama::start(&["qwen3.5:4b"]);
    let dir = tempfile::tempdir().unwrap();
    let cfg_path = dir.path().join(".config/trimwire.toml");
    fs::create_dir_all(cfg_path.parent().unwrap()).unwrap();
    fs::write(&cfg_path, "[server]\nlisten = \"127.0.0.1:9999\"\n").unwrap();

    let mut child = Command::new(bin())
        .args(["summarizer", "setup"])
        .env("HOME", dir.path())
        .env_remove("XDG_CONFIG_HOME")
        .env("TRIMWIRE_OLLAMA_ENDPOINT", fake.endpoint())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn summarizer setup");
    // 1=pick the local model; endpoint=default(empty, resolves to the fake);
    // model=default(empty → qwen3.5:4b); n=no fallback; y=write.
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"1\n\n\nn\ny\n")
        .unwrap();
    let out = child.wait_with_output().expect("wait");
    let all = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        out.status.success(),
        "local setup should succeed; got: {all}"
    );

    let cfg = fs::read_to_string(&cfg_path).expect("config written");
    assert!(
        cfg.contains("engine = \"local\""),
        "engine = local; got:\n{cfg}"
    );
    assert!(
        cfg.contains("[summarizer.local]"),
        "local block written; got:\n{cfg}"
    );
    assert!(cfg.contains("qwen3.5:4b"), "model written; got:\n{cfg}");
    assert!(
        cfg.contains(&fake.endpoint()),
        "endpoint (the fake) written; got:\n{cfg}"
    );
    assert!(
        cfg.contains("listen = \"127.0.0.1:9999\""),
        "unrelated [server] preserved; got:\n{cfg}"
    );
}

// ── P2 cheap CLI smokes (offline, deterministic) ────────────────────────────

/// `trimwire completions bash` emits a non-empty shell completion script.
#[test]
fn completions_emits_a_bash_script() {
    let out = Command::new(bin())
        .args(["completions", "bash"])
        .output()
        .expect("spawn completions");
    assert!(out.status.success(), "completions exits 0");
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(
        !s.trim().is_empty() && s.contains("trimwire"),
        "completion script should mention the binary; got {} bytes",
        s.len()
    );
}

/// `trimwire man --out <dir>` writes at least one man page (`.1`) into the dir.
#[test]
fn man_out_writes_man_pages() {
    let dir = tempfile::tempdir().unwrap();
    let out_dir = dir.path().join("man");
    let out = Command::new(bin())
        .args(["man", "--out"])
        .arg(&out_dir)
        .output()
        .expect("spawn man --out");
    assert!(
        out.status.success(),
        "man --out exits 0; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let n = fs::read_dir(&out_dir)
        .expect("man dir created")
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "1"))
        .count();
    assert!(n >= 1, "expected at least one .1 man page, got {n}");
}

/// `trimwire config edit` ensures the config exists and opens it in `$EDITOR`.
/// A fake editor records the path it was handed, proving the right file is opened.
#[test]
fn config_edit_opens_editor_on_the_config_file() {
    let dir = tempfile::tempdir().unwrap();
    let bindir = dir.path().join("bin");
    fs::create_dir_all(&bindir).unwrap();
    let marker = dir.path().join("editor_ran.txt");
    let editor = bindir.join("fakeeditor");
    fs::write(
        &editor,
        format!("#!/bin/sh\nprintf '%s' \"$1\" > \"{}\"\n", marker.display()),
    )
    .unwrap();
    fs::set_permissions(&editor, fs::Permissions::from_mode(0o755)).unwrap();

    let out = Command::new(bin())
        .args(["config", "edit"])
        .env("HOME", dir.path())
        .env_remove("XDG_CONFIG_HOME")
        .env("EDITOR", editor.to_str().unwrap())
        .output()
        .expect("spawn config edit");
    assert!(
        out.status.success(),
        "config edit exits 0; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let recorded = fs::read_to_string(&marker).expect("fake editor ran");
    assert!(
        recorded.ends_with("trimwire.toml"),
        "editor opened the config file; got: {recorded}"
    );
    assert!(
        dir.path().join(".config/trimwire.toml").exists(),
        "config edit ensures the file exists"
    );
}

/// `trimwire sweep file --dry-run <path>` reports without writing — the file is
/// left byte-for-byte intact and no `.bak.*` backup is created.
#[test]
fn sweep_file_dry_run_reports_without_writing() {
    let dir = tempfile::tempdir().unwrap();
    let sess = dir.path().join("s.jsonl");
    // An empty thinking block makes the session sweepable (thinking_strip).
    let body = "{\"type\":\"assistant\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"thinking\",\"thinking\":\"\",\"signature\":\"x\"},{\"type\":\"text\",\"text\":\"hi\"}]}}\n";
    fs::write(&sess, body).unwrap();

    let out = Command::new(bin())
        .args(["sweep", "file", "--dry-run"])
        .arg(&sess)
        .env("HOME", dir.path())
        .env_remove("XDG_CONFIG_HOME")
        .output()
        .expect("spawn sweep file --dry-run");
    assert!(
        out.status.success(),
        "dry-run exits 0; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        fs::read_to_string(&sess).unwrap(),
        body,
        "dry-run must not modify the file"
    );
    let made_backup = fs::read_dir(dir.path())
        .unwrap()
        .flatten()
        .any(|e| e.file_name().to_string_lossy().contains(".bak."));
    assert!(!made_backup, "dry-run makes no backup");
}

// ── P2 batch 2: share / statusline / sweep-undo / summarizer-status (offline) ──

/// `trimwire share enable` / `disable` toggle `[share] enabled` in the config.
/// Pure local file writes — no network.
#[test]
fn share_enable_then_disable_toggles_config_flag() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join(".config/trimwire.toml");
    let run = |arg: &str| {
        Command::new(bin())
            .args(["share", arg])
            .env("HOME", dir.path())
            .env_remove("XDG_CONFIG_HOME")
            .output()
            .expect("spawn share")
    };
    let e = run("enable");
    assert!(
        e.status.success(),
        "share enable ok: {}",
        String::from_utf8_lossy(&e.stderr)
    );
    assert!(
        fs::read_to_string(&cfg).unwrap().contains("enabled = true"),
        "enable writes the flag"
    );
    let d = run("disable");
    assert!(d.status.success(), "share disable ok");
    assert!(
        fs::read_to_string(&cfg)
            .unwrap()
            .contains("enabled = false"),
        "disable writes the flag"
    );
}

/// `trimwire share stats` WITHOUT `--yes` and with the ledger disabled must be a
/// safe no-op: a friendly message, exit 0, and NO upload (no `--yes` → no POST).
#[test]
fn share_stats_without_yes_is_offline() {
    let dir = tempfile::tempdir().unwrap();
    let out = Command::new(bin())
        .args(["share", "stats"]) // no --yes ⇒ never POSTs
        .env("HOME", dir.path())
        .env("TRIMWIRE_LEDGER__ENABLED", "false")
        .env_remove("XDG_CONFIG_HOME")
        .output()
        .expect("spawn share stats");
    assert!(out.status.success(), "share stats exits 0");
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(
        s.contains("nothing to share") || s.contains("ledger is disabled"),
        "ledger-disabled path is a safe no-op (no upload); got: {s}"
    );
}

/// `trimwire statusline add` wires trimwire into `~/.claude/settings.json`, and
/// `remove` unwires it — a clean round trip. File-only, no network.
#[test]
fn statusline_add_then_remove_round_trips_settings() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join(".claude")).unwrap();
    let settings = dir.path().join(".claude/settings.json");
    let run = |a: &str| {
        Command::new(bin())
            .args(["statusline", a])
            .env("HOME", dir.path())
            .env_remove("XDG_CONFIG_HOME")
            .output()
            .expect("spawn statusline")
    };
    let add = run("add");
    assert!(
        add.status.success(),
        "statusline add ok: {}",
        String::from_utf8_lossy(&add.stderr)
    );
    let s = fs::read_to_string(&settings).expect("settings.json created");
    assert!(
        s.contains("statusLine") && s.contains("trimwire"),
        "trimwire statusLine wired; got: {s}"
    );
    let rm = run("remove");
    assert!(rm.status.success(), "statusline remove ok");
    let s2 = fs::read_to_string(&settings).unwrap_or_default();
    assert!(
        !s2.contains("trimwire"),
        "remove unwires trimwire; got: {s2}"
    );
}

/// `trimwire sweep file <path>` (mutating) creates a `.bak.*` backup, and
/// `sweep undo <path>` restores the original bytes — the data-safety round trip.
#[test]
fn sweep_file_then_undo_restores_original_bytes() {
    let dir = tempfile::tempdir().unwrap();
    let sess = dir.path().join("s.jsonl");
    // Empty thinking block → sweep actually mutates (thinking_strip), so a backup
    // is made and there is something for undo to restore.
    let body = "{\"type\":\"assistant\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"thinking\",\"thinking\":\"\",\"signature\":\"x\"},{\"type\":\"text\",\"text\":\"hi\"}]}}\n";
    fs::write(&sess, body).unwrap();

    let sw = Command::new(bin())
        .args(["sweep", "file"])
        .arg(&sess)
        .env("HOME", dir.path())
        .env_remove("XDG_CONFIG_HOME")
        .output()
        .expect("spawn sweep file");
    assert!(
        sw.status.success(),
        "sweep file ok: {}",
        String::from_utf8_lossy(&sw.stderr)
    );
    let made_backup = fs::read_dir(dir.path())
        .unwrap()
        .flatten()
        .any(|e| e.file_name().to_string_lossy().contains(".bak."));
    assert!(made_backup, "sweep file creates a backup before rewriting");
    assert_ne!(
        fs::read_to_string(&sess).unwrap(),
        body,
        "sweep actually modified the file (empty thinking dropped)"
    );

    let un = Command::new(bin())
        .args(["sweep", "undo"])
        .arg(&sess)
        .env("HOME", dir.path())
        .env_remove("XDG_CONFIG_HOME")
        .output()
        .expect("spawn sweep undo");
    assert!(
        un.status.success(),
        "sweep undo ok: {}",
        String::from_utf8_lossy(&un.stderr)
    );
    assert_eq!(
        fs::read_to_string(&sess).unwrap(),
        body,
        "undo restores the original bytes"
    );
}

/// `trimwire summarizer status` on a fresh config reports the model-free
/// (unconfigured) state cleanly. Offline.
#[test]
fn summarizer_status_reports_model_free_on_fresh_config() {
    let dir = tempfile::tempdir().unwrap();
    let out = Command::new(bin())
        .args(["summarizer", "status"])
        .env("HOME", dir.path())
        .env_remove("XDG_CONFIG_HOME")
        .output()
        .expect("spawn summarizer status");
    assert!(out.status.success(), "summarizer status exits 0");
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(
        s.contains("model-free") || s.contains("not configured"),
        "fresh config = model-free; got: {s}"
    );
}

// ── P2 batch 3: recall / preview / dashboard / statusline-wrap / fallback ─────

/// `trimwire recall` degrades gracefully with no ledger: `--json` reports
/// availability=false (ledger disabled), and the human path on a fresh HOME says
/// the ledger isn't created yet. Offline — no network, no model.
#[test]
fn recall_degrades_gracefully_without_a_ledger() {
    let dir = tempfile::tempdir().unwrap();
    // JSON, ledger explicitly disabled → available:false.
    let j = Command::new(bin())
        .args(["recall", "--json"])
        .env("HOME", dir.path())
        .env("TRIMWIRE_LEDGER__ENABLED", "false")
        .env_remove("XDG_CONFIG_HOME")
        .output()
        .expect("spawn recall --json");
    assert!(j.status.success(), "recall --json exits 0");
    let v: serde_json::Value = serde_json::from_slice(&j.stdout).expect("valid json");
    assert_eq!(v["available"], false, "no ledger → available:false");

    // Human path, fresh HOME (default config, no ledger db yet).
    let h = Command::new(bin())
        .args(["recall"])
        .env("HOME", dir.path())
        .env_remove("XDG_CONFIG_HOME")
        .output()
        .expect("spawn recall");
    assert!(h.status.success(), "recall exits 0 on a fresh HOME");
    let s = String::from_utf8_lossy(&h.stdout);
    assert!(
        s.contains("ledger") && (s.contains("not yet created") || s.contains("disabled")),
        "friendly no-ledger message; got: {s}"
    );
}

/// `trimwire preview <path> --json` reconstructs a session transcript and emits
/// valid estimate JSON. Reads the explicit `.jsonl` path (no ledger, no network).
#[test]
fn preview_json_reconstructs_a_temp_session() {
    let dir = tempfile::tempdir().unwrap();
    let sess = dir.path().join("s.jsonl");
    let body = concat!(
        "{\"type\":\"user\",\"uuid\":\"u1\",\"message\":{\"role\":\"user\",\"content\":[{\"type\":\"text\",\"text\":\"hi\"}]}}\n",
        "{\"type\":\"assistant\",\"uuid\":\"a1\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"ok\"}]}}\n",
    );
    fs::write(&sess, body).unwrap();

    let out = Command::new(bin())
        .args(["preview"])
        .arg(&sess)
        .arg("--json")
        .env("HOME", dir.path())
        .env_remove("XDG_CONFIG_HOME")
        .output()
        .expect("spawn preview --json");
    assert!(
        out.status.success(),
        "preview --json exits 0; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).expect("valid json");
    assert_eq!(v["messages"], 2, "two reconstructed turns");
    assert!(
        v.get("in_bytes").is_some() && v.get("out_bytes").is_some(),
        "estimate fields present"
    );
}

/// `preview --json` on an empty/invalid session emits a JSON ERROR object on
/// stdout (not a plain-text anyhow message) and exits non-zero — so a `--json`
/// consumer always gets parseable output, never a stray non-JSON line.
#[test]
fn preview_json_error_is_json() {
    let dir = tempfile::tempdir().unwrap();
    let empty = dir.path().join("empty.jsonl"); // 0 records → "no user/assistant turns"
    fs::write(&empty, "").unwrap();

    let out = Command::new(bin())
        .args(["preview"])
        .arg(&empty)
        .arg("--json")
        .env("HOME", dir.path())
        .env_remove("XDG_CONFIG_HOME")
        .output()
        .expect("spawn preview --json");
    assert!(
        !out.status.success(),
        "preview --json on an empty session must exit non-zero"
    );
    // stdout must be a valid JSON object carrying the error (NOT plain text).
    let v: serde_json::Value =
        serde_json::from_slice(&out.stdout).expect("stdout is valid JSON in --json mode");
    let msg = v["error"].as_str().unwrap_or("");
    assert!(
        msg.contains("no user/assistant turns") || msg.contains("preview"),
        "JSON error carries the reason; got: {v}"
    );
}

/// `trimwire dashboard --out <file>` degrades gracefully with no ledger data:
/// exits 0 (consistent with `stats`/`recall`) and — crucially — says EXPLICITLY
/// that no file was written, so a `--out` caller isn't left wondering why the
/// file never appeared. Covers both no-data branches: ledger disabled, and
/// ledger enabled but not yet created.
#[test]
fn dashboard_degrades_gracefully_without_a_ledger() {
    // (a) ledger disabled in config.
    let dir = tempfile::tempdir().unwrap();
    let html = dir.path().join("report.html");
    let out = Command::new(bin())
        .args(["dashboard", "--out"])
        .arg(&html)
        .env("HOME", dir.path())
        .env("TRIMWIRE_LEDGER__ENABLED", "false")
        .env_remove("XDG_CONFIG_HOME")
        .output()
        .expect("spawn dashboard --out");
    assert!(out.status.success(), "dashboard exits 0 with no ledger");
    let s = String::from_utf8_lossy(&out.stdout);
    // `TRIMWIRE_LEDGER__ENABLED=false` forces the *disabled* branch specifically
    // (not the not-created one) — assert both the branch marker and the no-file note.
    assert!(
        s.contains("disabled") && s.contains("no dashboard file written"),
        "disabled branch must say explicitly that no file was written; got: {s}"
    );
    assert!(
        !html.exists(),
        "no HTML written when the ledger is disabled"
    );

    // (b) ledger enabled but never created (fresh HOME, gateway never ran).
    let dir2 = tempfile::tempdir().unwrap();
    let html2 = dir2.path().join("report.html");
    let out2 = Command::new(bin())
        .args(["dashboard", "--out"])
        .arg(&html2)
        .env("HOME", dir2.path())
        .env_remove("XDG_CONFIG_HOME")
        .output()
        .expect("spawn dashboard --out");
    assert!(
        out2.status.success(),
        "dashboard exits 0 when ledger not created"
    );
    let s2 = String::from_utf8_lossy(&out2.stdout);
    assert!(
        s2.contains("not yet created") && s2.contains("no dashboard file written"),
        "explicit not-created + no-file message; got: {s2}"
    );
    assert!(
        !html2.exists(),
        "no HTML written when the ledger isn't created yet"
    );
}

/// `trimwire dashboard --out <file>` against a POPULATED ledger writes the
/// self-contained HTML: it runs the ledger report + session queries, embeds the
/// content-free payload, and writes the file. Seeds a real on-disk ledger via the
/// public `Ledger::open`/`record` API (the same write path the gateway uses), then
/// drives the binary as a subprocess pointed at it. Complements
/// `dashboard_degrades_gracefully_without_a_ledger` (which covers the empty path).
/// Offline — no network, no model, no live call.
#[test]
fn dashboard_writes_html_from_a_populated_ledger() {
    use trimwire::ledger::{Ledger, Record};

    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("ledger.db");
    let db_str = db.to_str().unwrap().to_owned();
    let session_id = "dash-smoke-sess-7f3a";

    // One representative request row (content-free metadata only — byte counts,
    // hashes, timings, model name; never message text).
    let rec = |ts: i64, in_b: i64, out_b: i64| Record {
        ts,
        session_id: Some(session_id.to_owned()),
        model: Some("claude-sonnet-4-6".to_owned()),
        in_bytes: in_b,
        out_bytes: out_b,
        strategies: "bloat_cap".to_owned(),
        strategy_bytes: format!("bloat_cap:{}", in_b - out_b),
        prefix_hash_in: "hashin".to_owned(),
        prefix_hash_out: "hashout".to_owned(),
        ttft_us: 12_000,
        input_tokens: 100,
        cache_read_input_tokens: 40,
        cache_creation_input_tokens: 10,
        output_tokens: 25,
        applied_edits_cleared_thinking_turns: 0,
        applied_edits_cleared_tool_uses: 0,
        applied_edits_cleared_input_tokens: 0,
        response_status: 0,
        rolled_back: false,
    };

    // Seed two rows through the REAL recorder (fire-and-forget on the tokio
    // blocking pool — `record` needs a runtime context). Use `now` timestamps so
    // open-time pruning (retain_days) never drops them.
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let ledger = Ledger::open(&db_str, 365);
        ledger.record(rec(now, 1000, 400));
        ledger.record(rec(now, 1000, 400));
    });
    // The inserts run on the blocking pool; WAL + per-statement autocommit means a
    // fresh read-only connection sees them once committed. Poll the read side
    // (kept inside the runtime's lifetime) until both rows land — deterministic,
    // with a generous timeout instead of a fixed sleep.
    let mut total = 0;
    for _ in 0..100 {
        total = Ledger::report(&db_str)
            .map(|r| r.total_requests)
            .unwrap_or(0);
        if total >= 2 {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    assert_eq!(
        total, 2,
        "seed must land 2 request rows before the smoke runs"
    );
    drop(rt);

    // Drive the binary against the seeded ledger.
    let html = dir.path().join("report.html");
    let out = Command::new(bin())
        .args(["dashboard", "--out"])
        .arg(&html)
        .current_dir(dir.path())
        .env("HOME", dir.path())
        .env("TRIMWIRE_LEDGER__ENABLED", "true")
        .env("TRIMWIRE_LEDGER__DB_PATH", &db_str)
        .env_remove("XDG_CONFIG_HOME")
        .output()
        .expect("spawn dashboard --out");
    assert!(
        out.status.success(),
        "dashboard exits 0 with a populated ledger; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // The HTML file must be written and be the fully-spliced dashboard, carrying a
    // value DERIVED from the seeded ledger (so this can't pass on the empty path).
    assert!(
        html.exists(),
        "HTML file written for the populated-ledger path"
    );
    let body = fs::read_to_string(&html).expect("read written html");
    assert!(
        !body.contains("__TRIMWIRE_DATA__"),
        "data token must be replaced (template was spliced)"
    );
    assert!(
        body.contains("const DATA ="),
        "renders into the dashboard DATA literal"
    );
    assert!(
        body.contains("\"total_requests\":2"),
        "report aggregated from the 2 seeded rows is embedded"
    );
    assert!(
        body.contains(session_id),
        "the seeded session row is embedded in the dashboard"
    );
}

/// `trimwire statusline wrap` over a PRE-EXISTING (non-trimwire) statusLine must
/// preserve the original (wrap it, not clobber it), and `remove` must restore the
/// original losslessly. Pins wrap-over-existing + round-trip. File-only.
#[test]
fn statusline_wrap_preserves_existing_then_remove_restores() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join(".claude")).unwrap();
    let settings = dir.path().join(".claude/settings.json");
    // A pre-existing, NON-trimwire statusLine the user already has.
    fs::write(
        &settings,
        "{\"statusLine\":{\"type\":\"command\",\"command\":\"my-custom-bar\"}}",
    )
    .unwrap();
    let run = |a: &str| {
        Command::new(bin())
            .args(["statusline", a])
            .env("HOME", dir.path())
            .env_remove("XDG_CONFIG_HOME")
            .output()
            .expect("spawn statusline")
    };

    let w = run("wrap");
    assert!(
        w.status.success(),
        "statusline wrap ok: {}",
        String::from_utf8_lossy(&w.stderr)
    );
    let wrapped = fs::read_to_string(&settings).unwrap();
    assert!(
        wrapped.contains("trimwire") && wrapped.contains("--wrap-file"),
        "wrap wires trimwire as a wrapper; got: {wrapped}"
    );

    // remove must restore the ORIGINAL command losslessly.
    let r = run("remove");
    assert!(r.status.success(), "statusline remove ok");
    let restored = fs::read_to_string(&settings).unwrap();
    assert!(
        restored.contains("my-custom-bar"),
        "remove restores the original statusLine; got: {restored}"
    );
    assert!(
        !restored.contains("trimwire"),
        "remove unwires trimwire; got: {restored}"
    );
}

/// `summarizer setup` multi-engine chain: API provider as PRIMARY + local as
/// FALLBACK, against the fake ollama (no real model/network/key). Asserts the
/// fallback chain, both engine blocks, the env-var NAME (no key), and preserved
/// config. The deferred multi-engine wizard coverage.
#[test]
fn summarizer_setup_api_primary_with_local_fallback() {
    use std::io::Write;
    use std::process::Stdio;

    let fake = FakeOllama::start(&["qwen3.5:4b"]);
    let dir = tempfile::tempdir().unwrap();
    let cfg_path = dir.path().join(".config/trimwire.toml");
    fs::create_dir_all(cfg_path.parent().unwrap()).unwrap();
    fs::write(&cfg_path, "[server]\nlisten = \"127.0.0.1:9999\"\n").unwrap();

    let mut child = Command::new(bin())
        .args(["summarizer", "setup"])
        .env("HOME", dir.path())
        .env_remove("XDG_CONFIG_HOME")
        .env("TRIMWIRE_OLLAMA_ENDPOINT", fake.endpoint())
        .env_remove("TESTPROV_KEY")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn summarizer setup");
    // Items at the primary picker: 1) qwen3.5:4b (fake local). We:
    //   a            → add an API provider
    //   testprov / anthropic / "" / test-model / "" / TESTPROV_KEY / y  → provider fields
    //                  (the "" before TESTPROV_KEY skips the key-FILE prompt, asked first)
    //   2            → pick the provider (now item 2) as PRIMARY
    //   y            → add a fallback
    //   1            → pick the local model as the fallback
    //   "" / ""      → local endpoint default (the fake) / model default (qwen3.5:4b)
    //   n            → no more fallbacks
    //   y            → write
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"a\ntestprov\nanthropic\n\ntest-model\n\nTESTPROV_KEY\ny\n2\ny\n1\n\n\nn\ny\n")
        .unwrap();
    let out = child.wait_with_output().expect("wait");
    let all = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        out.status.success(),
        "api+local setup should succeed; got: {all}"
    );

    let cfg = fs::read_to_string(&cfg_path).expect("config written");
    assert!(
        cfg.contains("engine = \"testprov\""),
        "primary engine = provider id; got:\n{cfg}"
    );
    assert!(
        cfg.contains("fallback = [\"local\"]"),
        "fallback chain = [local]; got:\n{cfg}"
    );
    assert!(
        cfg.contains("[[summarizer.providers]]") && cfg.contains("\"test-model\""),
        "provider block written; got:\n{cfg}"
    );
    assert!(
        cfg.contains("[summarizer.local]") && cfg.contains("qwen3.5:4b"),
        "local fallback block written; got:\n{cfg}"
    );
    assert!(
        cfg.contains("api_key_env") && cfg.contains("\"TESTPROV_KEY\""),
        "stores the env-var NAME; got:\n{cfg}"
    );
    assert!(
        !cfg.to_lowercase().contains("sk-"),
        "no key VALUE stored; got:\n{cfg}"
    );
    assert!(
        cfg.contains("listen = \"127.0.0.1:9999\""),
        "unrelated [server] preserved; got:\n{cfg}"
    );
}

// ---- `trimwire update` (read-only check, phase 4a) -------------------------

/// A signed release the fake server can hand out (4b/4c). `minisig = None` lets a
/// test simulate a MISSING signature (the `.minisig` route 404s).
struct Release {
    asset: String,
    archive: Vec<u8>,
    sha256: String,
    minisig: Option<String>,
}

/// Fake GitHub for the updater tests. Routes by request path: `…/releases/latest`
/// → `{"tag_name":"<tag>"}`; `…/<asset>.minisig` → the detached signature (404 if
/// `minisig = None`); `…/<asset>.sha256` → the checksum file; `…/<asset>` → the
/// archive bytes; anything else → 404. One base serves BOTH the API
/// (`TRIMWIRE_UPDATE_API_BASE`) and the download host (`TRIMWIRE_UPDATE_DL_BASE`).
/// Mirrors `FakeOllama` (listener + thread).
struct FakeGitHub {
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
    port: u16,
    /// Per-artifact request counters (archive, .sha256, .minisig) so a test can
    /// prove e.g. that `upgrade --yes` re-downloads after `upgrade --dry-run`
    /// rather than trusting any staged state.
    hits: std::sync::Arc<[std::sync::atomic::AtomicUsize; 3]>,
}

/// Indices into `FakeGitHub::hits`.
const HIT_ARCHIVE: usize = 0;
const HIT_SHA: usize = 1;
const HIT_SIG: usize = 2;

/// How the fake server reports the archive's `Content-Length` — to exercise the
/// downloader's two size-cap defenses.
#[derive(Clone, Copy)]
enum ArchiveCl {
    /// Honest `Content-Length` = body length (normal).
    Auto,
    /// No `Content-Length` header → forces the streaming accumulation path.
    Omit,
    /// A (possibly lying/oversized) fixed `Content-Length` → exercises the
    /// pre-read header check.
    Fixed(u64),
}

fn http_resp_cl(status: &str, ctype: &str, body: &[u8], content_length: Option<u64>) -> Vec<u8> {
    let mut head = format!("HTTP/1.1 {status}\r\nContent-Type: {ctype}\r\n");
    if let Some(n) = content_length {
        head.push_str(&format!("Content-Length: {n}\r\n"));
    }
    head.push_str("Connection: close\r\n\r\n");
    let mut v = head.into_bytes();
    v.extend_from_slice(body);
    v
}

fn http_resp(status: &str, ctype: &str, body: &[u8]) -> Vec<u8> {
    http_resp_cl(status, ctype, body, Some(body.len() as u64))
}

impl FakeGitHub {
    /// Releases API only (returns `tag` for `/releases/latest`); no downloadable
    /// artifacts — used by the read-only check tests.
    fn start(tag: &str) -> Self {
        Self::start_inner(tag, None, ArchiveCl::Auto)
    }

    /// Full release: API + downloadable archive/.sha256/.minisig.
    fn with_release(tag: &str, rel: Release) -> Self {
        Self::start_inner(tag, Some(rel), ArchiveCl::Auto)
    }

    /// Full release with a chosen archive `Content-Length` behavior (size-cap tests).
    fn with_release_cl(tag: &str, rel: Release, cl: ArchiveCl) -> Self {
        Self::start_inner(tag, Some(rel), cl)
    }

    fn start_inner(tag: &str, rel: Option<Release>, archive_cl: ArchiveCl) -> Self {
        use std::io::{Read, Write};
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        listener.set_nonblocking(true).unwrap();
        let tag_json = format!("{{\"tag_name\":\"{tag}\"}}");

        let stop = Arc::new(AtomicBool::new(false));
        let stop2 = stop.clone();
        let hits: Arc<[AtomicUsize; 3]> = Arc::new([
            AtomicUsize::new(0),
            AtomicUsize::new(0),
            AtomicUsize::new(0),
        ]);
        let hits2 = hits.clone();
        let handle = std::thread::spawn(move || {
            while !stop2.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((mut s, _)) => {
                        // The listener is non-blocking (so `accept` can poll the
                        // `stop` flag). On macOS/BSD the accepted socket INHERITS
                        // that non-blocking flag, which makes the `set_read_timeout`
                        // below a no-op and lets the first `read()` return
                        // `WouldBlock` before the request bytes arrive — routing on
                        // an empty request and 404-ing. That is the macOS-only,
                        // intermittent CI flake (#131). Force the stream back to
                        // blocking so the read timeout actually applies. (Linux
                        // already clears the flag on `accept`, so this is a no-op
                        // there.)
                        let _ = s.set_nonblocking(false);
                        // Read the request until the end of headers (GET requests
                        // carry no body). A single `read()` is NOT enough: on macOS
                        // the request line + headers can arrive across multiple TCP
                        // segments, so a one-shot read would route on a truncated
                        // (or empty) request and 404 — the source of cross-platform
                        // flakiness. Accumulate until `\r\n\r\n`, with a generous
                        // per-read timeout so a slow first segment doesn't truncate.
                        let _ = s.set_read_timeout(Some(std::time::Duration::from_secs(2)));
                        let mut data: Vec<u8> = Vec::new();
                        let mut buf = [0u8; 4096];
                        loop {
                            match s.read(&mut buf) {
                                Ok(0) => break,
                                Ok(k) => {
                                    data.extend_from_slice(&buf[..k]);
                                    if data.windows(4).any(|w| w == b"\r\n\r\n") {
                                        break;
                                    }
                                    if data.len() > 64 * 1024 {
                                        break; // guard: never buffer unbounded
                                    }
                                }
                                Err(_) => break,
                            }
                        }
                        let req = String::from_utf8_lossy(&data);
                        let path = req
                            .lines()
                            .next()
                            .and_then(|l| l.split_whitespace().nth(1))
                            .unwrap_or("");
                        let resp = if path.ends_with("/releases/latest") {
                            http_resp("200 OK", "application/json", tag_json.as_bytes())
                        } else if let Some(r) = &rel {
                            // Count BEFORE responding (and even on a 404, so a
                            // missing-sig test still records the attempt).
                            if path.ends_with(".minisig") {
                                hits2[HIT_SIG].fetch_add(1, Ordering::Relaxed);
                                match &r.minisig {
                                    Some(sig) => http_resp("200 OK", "text/plain", sig.as_bytes()),
                                    None => http_resp("404 Not Found", "text/plain", b"no sig"),
                                }
                            } else if path.ends_with(".sha256") {
                                hits2[HIT_SHA].fetch_add(1, Ordering::Relaxed);
                                http_resp("200 OK", "text/plain", r.sha256.as_bytes())
                            } else if path.ends_with(&r.asset) {
                                hits2[HIT_ARCHIVE].fetch_add(1, Ordering::Relaxed);
                                let cl = match archive_cl {
                                    ArchiveCl::Auto => Some(r.archive.len() as u64),
                                    ArchiveCl::Omit => None,
                                    ArchiveCl::Fixed(n) => Some(n),
                                };
                                http_resp_cl("200 OK", "application/octet-stream", &r.archive, cl)
                            } else {
                                http_resp("404 Not Found", "text/plain", b"not found")
                            }
                        } else {
                            http_resp("404 Not Found", "text/plain", b"not found")
                        };
                        let _ = s.write_all(&resp);
                        let _ = s.flush();
                    }
                    Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(std::time::Duration::from_millis(5));
                    }
                    Err(_) => break,
                }
            }
        });
        FakeGitHub {
            stop,
            handle: Some(handle),
            port,
            hits,
        }
    }

    fn base(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    /// (archive, .sha256, .minisig) request counts so far.
    fn hits(&self) -> (usize, usize, usize) {
        use std::sync::atomic::Ordering;
        (
            self.hits[HIT_ARCHIVE].load(Ordering::Relaxed),
            self.hits[HIT_SHA].load(Ordering::Relaxed),
            self.hits[HIT_SIG].load(Ordering::Relaxed),
        )
    }
}

impl Drop for FakeGitHub {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

/// A real minisign-signed release fixture for this platform's asset name, plus
/// the base64 public key to pin via `TRIMWIRE_UPDATE_PUBKEY`. Signs with the
/// `minisign` dev-dep (the prehashed `-H` form the release workflow emits).
struct SignedFixture {
    release: Release,
    pubkey: String,
}

fn signed_fixture(archive: &[u8]) -> SignedFixture {
    use minisign::{KeyPair, sign};
    use sha2::{Digest, Sha256};

    let asset = trimwire::update::asset_name(env!("TRIMWIRE_TARGET"));
    let kp = KeyPair::generate_unencrypted_keypair().expect("keypair");
    let sig_box = sign(
        Some(&kp.pk),
        &kp.sk,
        std::io::Cursor::new(archive),
        Some("trusted comment: trimwire integration test"),
        None,
    )
    .expect("sign");
    let minisig: String = sig_box.into();
    let sha = hex::encode(Sha256::digest(archive));
    SignedFixture {
        release: Release {
            sha256: format!("{sha}  {asset}\n"),
            asset,
            archive: archive.to_vec(),
            minisig: Some(minisig),
        },
        pubkey: kp.pk.to_base64(),
    }
}

/// Write a `method="script"` receipt pointing at the test binary, so the running
/// binary is treated as a managed (self-updatable) install.
fn write_script_receipt(data_home: &std::path::Path) {
    let dir = data_home.join("trimwire");
    fs::create_dir_all(&dir).unwrap();
    let exe = fs::canonicalize(bin()).unwrap();
    let receipt = format!(
        "{{\"schema_version\":1,\"method\":\"script\",\"binary_path\":\"{}\",\"version\":\"0.0.0\",\"target\":\"{}\",\"installed_at\":0}}",
        exe.display(),
        env!("TRIMWIRE_TARGET")
    );
    fs::write(dir.join("install-receipt.json"), receipt).unwrap();
}

fn run_update(
    home: &std::path::Path,
    data_home: &std::path::Path,
    api_base: &str,
) -> std::process::Output {
    Command::new(bin())
        .arg("update")
        .env("HOME", home)
        .env("XDG_DATA_HOME", data_home)
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("TRIMWIRE_UPDATE_API_BASE", api_base)
        .output()
        .expect("spawn trimwire update")
}

/// No receipt → refuse (exit 2) with the per-method guidance, never a check.
#[test]
fn update_refuses_without_receipt() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    let out = run_update(dir.path(), &data, "http://127.0.0.1:1");
    assert_eq!(out.status.code(), Some(2), "no-receipt refuses with exit 2");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("no install receipt"), "got: {err}");
    assert!(
        err.contains("install.sh"),
        "reuses per-method guidance: {err}"
    );
    assert!(out.stdout.is_empty());
}

/// cargo/manual (`method="unknown"`) → refuse (exit 2), not self-updatable.
#[test]
fn update_refuses_non_script_install() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    let rcpt_dir = data.join("trimwire");
    fs::create_dir_all(&rcpt_dir).unwrap();
    let exe = fs::canonicalize(bin()).unwrap();
    fs::write(
        rcpt_dir.join("install-receipt.json"),
        format!(
            "{{\"schema_version\":1,\"method\":\"unknown\",\"binary_path\":\"{}\",\"version\":\"0.0.0\",\"target\":\"{}\",\"installed_at\":0}}",
            exe.display(),
            env!("TRIMWIRE_TARGET")
        ),
    )
    .unwrap();
    let out = run_update(dir.path(), &data, "http://127.0.0.1:1");
    assert_eq!(out.status.code(), Some(2));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("wasn't installed by the curl|sh installer"),
        "got: {err}"
    );
}

/// Eligible install + a newer release → exit 0, reports availability.
#[test]
fn update_reports_available_for_eligible_install() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    write_script_receipt(&data);
    let gh = FakeGitHub::start("v999.0.0");
    let out = run_update(dir.path(), &data, &gh.base());
    assert_eq!(out.status.code(), Some(0), "available check exits 0");
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(
        s.contains("999.0.0") && s.contains("is available"),
        "got: {s}"
    );
    assert!(
        s.contains("trimwire upgrade --dry-run") && s.contains("trimwire upgrade"),
        "check-only output points at the upgrade command: {s}"
    );
    assert!(
        !s.contains("--apply"),
        "the removed --apply flag must not be advertised: {s}"
    );
}

/// Eligible install + an older/equal release → exit 0, "already up to date".
#[test]
fn update_reports_current_when_not_newer() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    write_script_receipt(&data);
    let gh = FakeGitHub::start("v0.0.1"); // < the test binary's version
    let out = run_update(dir.path(), &data, &gh.base());
    assert_eq!(out.status.code(), Some(0));
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains("already up to date"), "got: {s}");
}

/// MIGRATION: a legacy poisoned receipt (v0.3.13 wrote `binary_path` ending in
/// " (deleted)") must self-heal — `trimwire update` repairs it in place and
/// proceeds, instead of refusing with PathMismatch (exit 2). End-to-end proof of
/// the `resolve_eligibility` wiring (the unit tests cover the pure decision).
#[test]
fn update_heals_legacy_deleted_receipt() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    let rcpt_dir = data.join("trimwire");
    fs::create_dir_all(&rcpt_dir).unwrap();
    let exe = fs::canonicalize(bin()).unwrap();
    let receipt_file = rcpt_dir.join("install-receipt.json");
    // Exactly what the pre-fix updater serialized: a "(deleted)"-suffixed path.
    fs::write(
        &receipt_file,
        format!(
            "{{\"schema_version\":1,\"method\":\"script\",\"binary_path\":\"{} (deleted)\",\"version\":\"0.3.13\",\"target\":\"{}\",\"installed_at\":0}}",
            exe.display(),
            env!("TRIMWIRE_TARGET")
        ),
    )
    .unwrap();

    // Older release → "already up to date" iff eligibility PASSED (i.e. it
    // healed). A non-healed poisoned receipt would refuse with exit 2 instead.
    let gh = FakeGitHub::start("v0.0.1");
    let out = run_update(dir.path(), &data, &gh.base());
    assert_eq!(
        out.status.code(),
        Some(0),
        "healed receipt proceeds (not a PathMismatch refusal); stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("already up to date"),
        "got: {}",
        String::from_utf8_lossy(&out.stdout)
    );

    // The on-disk receipt was repaired: canonical path, no " (deleted)" suffix.
    let healed = fs::read_to_string(&receipt_file).unwrap();
    assert!(
        !healed.contains(" (deleted)"),
        "receipt still poisoned after heal: {healed}"
    );
    assert!(
        healed.contains(&format!("\"binary_path\": \"{}\"", exe.display())),
        "receipt should record the canonical path: {healed}"
    );
}

/// Eligible install but the check can't reach GitHub → exit 0, clear message,
/// no partial-update state.
#[test]
fn update_network_failure_is_nonfatal() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    write_script_receipt(&data);
    // Port 1 is not listening → connection fails fast.
    let out = run_update(dir.path(), &data, "http://127.0.0.1:1");
    assert_eq!(out.status.code(), Some(0), "network failure is non-fatal");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("couldn't check for updates"), "got: {err}");
}

// ---- `trimwire update` deprecated apply/verify flags → redirect to upgrade ----

/// Build a `<verb>` Command (update|upgrade) wired to a fake server for both API
/// + downloads, with stdin nulled (so `is_terminal()` is false — non-interactive).
fn upd_cmd(
    verb: &str,
    home: &std::path::Path,
    data_home: &std::path::Path,
    base: &str,
    args: &[&str],
) -> Command {
    let mut c = Command::new(bin());
    c.arg(verb)
        .args(args)
        .env("HOME", home)
        .env("XDG_DATA_HOME", data_home)
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("TRIMWIRE_UPDATE_API_BASE", base)
        .env("TRIMWIRE_UPDATE_DL_BASE", base)
        .stdin(std::process::Stdio::null());
    c
}

/// The verify/apply flags were removed from `update` (they live on `upgrade`):
/// passing one is a clean exit-2 redirect, never a download or apply.
#[test]
fn update_apply_verify_flags_redirect_to_upgrade() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    for (flag, want) in [
        ("--dry-run", "trimwire upgrade --dry-run"),
        ("--apply", "trimwire upgrade"),
        ("--yes", "trimwire upgrade --yes"),
    ] {
        let out = upd_cmd("update", dir.path(), &data, "http://127.0.0.1:1", &[flag])
            .output()
            .expect("spawn");
        assert_eq!(out.status.code(), Some(2), "update {flag} → exit 2");
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(
            err.contains(want),
            "update {flag} redirects to `{want}`: {err}"
        );
    }
}

// ---- `trimwire upgrade --dry-run` (4b: verify) ----------------------------

/// `upgrade --dry-run` on a correctly-signed release → exit 0, "verified".
#[test]
fn upgrade_dry_run_verifies_a_signed_release() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    let fx = signed_fixture(b"trimwire fake archive payload");
    let gh = FakeGitHub::with_release("v999.0.0", fx.release);
    let out = upd_cmd("upgrade", dir.path(), &data, &gh.base(), &["--dry-run"])
        .env("TRIMWIRE_UPDATE_PUBKEY", &fx.pubkey)
        .output()
        .expect("spawn");
    let s = String::from_utf8_lossy(&out.stdout);
    assert_eq!(out.status.code(), Some(0), "verified → exit 0; stdout: {s}");
    assert!(s.contains("verified"), "stdout: {s}");
}

/// `upgrade --dry-run` with no pinned key → fail closed (exit 1), never "verified".
#[test]
fn upgrade_dry_run_fails_closed_without_pinned_key() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    let fx = signed_fixture(b"trimwire fake archive payload");
    let gh = FakeGitHub::with_release("v999.0.0", fx.release);
    // Empty override simulates a build with no pinned key (the shipped build now
    // embeds a real one, so force the no-key state via the localhost seam).
    let out = upd_cmd("upgrade", dir.path(), &data, &gh.base(), &["--dry-run"])
        .env("TRIMWIRE_UPDATE_PUBKEY", "")
        .output()
        .expect("spawn");
    assert_eq!(out.status.code(), Some(1), "no key → fail closed");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("NOT verified"), "got: {err}");
}

/// `upgrade --dry-run` where the archive doesn't match the signed digest → fail
/// closed at the checksum gate (exit 1).
#[test]
fn upgrade_dry_run_fails_closed_on_tampered_artifact() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    let mut fx = signed_fixture(b"trimwire fake archive payload");
    // Tamper the served archive AFTER signing/checksumming.
    fx.release.archive = b"a different, malicious payload".to_vec();
    let gh = FakeGitHub::with_release("v999.0.0", fx.release);
    let out = upd_cmd("upgrade", dir.path(), &data, &gh.base(), &["--dry-run"])
        .env("TRIMWIRE_UPDATE_PUBKEY", &fx.pubkey)
        .output()
        .expect("spawn");
    assert_eq!(out.status.code(), Some(1), "tampered → fail closed");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("NOT verified"), "got: {err}");
}

/// `upgrade --dry-run` with a MISSING `.minisig` (server 404s it) → fail closed
/// (exit 1), never silently skip the signature.
#[test]
fn upgrade_dry_run_fails_closed_on_missing_signature() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    let mut fx = signed_fixture(b"trimwire fake archive payload");
    fx.release.minisig = None; // server returns 404 for the .minisig
    let gh = FakeGitHub::with_release("v999.0.0", fx.release);
    let out = upd_cmd("upgrade", dir.path(), &data, &gh.base(), &["--dry-run"])
        .env("TRIMWIRE_UPDATE_PUBKEY", &fx.pubkey)
        .output()
        .expect("spawn");
    assert_eq!(out.status.code(), Some(1), "missing sig → fail closed");
}

/// `upgrade --dry-run` with the server unreachable → fail closed (exit 1).
#[test]
fn upgrade_dry_run_network_failure_fails_closed() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    let out = upd_cmd(
        "upgrade",
        dir.path(),
        &data,
        "http://127.0.0.1:1",
        &["--dry-run"],
    )
    .env(
        "TRIMWIRE_UPDATE_PUBKEY",
        "RWQf6LRCGA9i53mlYecO4IzT51TGPpvWucNSCh1CBM0QTaLn73Y7GFO3",
    )
    .output()
    .expect("spawn");
    assert_eq!(out.status.code(), Some(1), "network failure → fail closed");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("NOT verified"), "got: {err}");
}

// ---- `trimwire upgrade` (4c: apply, draft) --------------------------------

/// No receipt → upgrade refuses (exit 2) before any download, nothing changed.
// Linux-only: the apply path (`run_apply`) is `#[cfg(target_os = "linux")]`; on
// other platforms `upgrade` refuses with an OS-unsupported message before these
// receipt/key/version checks, so the asserted Linux-path messages don't apply.
#[cfg(target_os = "linux")]
#[test]
fn upgrade_refuses_without_receipt() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    let out = upd_cmd(
        "upgrade",
        dir.path(),
        &data,
        "http://127.0.0.1:1",
        &["--yes"],
    )
    .output()
    .expect("spawn");
    assert_eq!(out.status.code(), Some(2), "no receipt → refuse");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("no install receipt"), "got: {err}");
}

/// Eligible install but a non-interactive shell with bare `upgrade` (no `--yes`)
/// → refuse (exit 2) rather than apply unattended. (stdin is nulled, not a TTY.)
#[cfg(target_os = "linux")] // apply path is Linux-only (see note above)
#[test]
fn upgrade_non_interactive_requires_yes() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    write_script_receipt(&data);
    let fx = signed_fixture(b"trimwire fake archive payload");
    let gh = FakeGitHub::with_release("v999.0.0", fx.release);
    let out = upd_cmd("upgrade", dir.path(), &data, &gh.base(), &[])
        .env("TRIMWIRE_UPDATE_PUBKEY", &fx.pubkey)
        .output()
        .expect("spawn");
    assert_eq!(
        out.status.code(),
        Some(2),
        "non-interactive without --yes → refuse"
    );
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("without confirmation") && err.contains("--yes"),
        "got: {err}"
    );
}

/// Eligible + `--yes`, but the latest release is NOT newer → no-op (exit 0),
/// nothing applied. Proves `upgrade --yes` never blindly reinstalls/downgrades.
#[cfg(target_os = "linux")] // apply path is Linux-only (see note above)
#[test]
fn upgrade_yes_is_noop_when_current() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    write_script_receipt(&data);
    let gh = FakeGitHub::start("v0.0.1"); // older than the test binary
    let out = upd_cmd("upgrade", dir.path(), &data, &gh.base(), &["--yes"])
        .output()
        .expect("spawn");
    assert_eq!(out.status.code(), Some(0), "current → no-op exit 0");
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains("already up to date"), "got: {s}");
}

/// Eligible + `--yes` but no pinned key → refuse (exit 2): can't verify, won't
/// apply (fail closed), even though a newer version exists.
#[cfg(target_os = "linux")] // apply path is Linux-only (see note above)
#[test]
fn upgrade_refuses_without_pinned_key() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    write_script_receipt(&data);
    let gh = FakeGitHub::start("v999.0.0");
    // Empty override simulates a build with no pinned key (force the no-key state
    // via the localhost seam, since the shipped build embeds a real key).
    let out = upd_cmd("upgrade", dir.path(), &data, &gh.base(), &["--yes"])
        .env("TRIMWIRE_UPDATE_PUBKEY", "")
        .output()
        .expect("spawn");
    assert_eq!(out.status.code(), Some(2), "no key → refuse");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("no embedded update-signing key"), "got: {err}");
}

/// FULL apply path, end to end, with the localhost-only test seam that stops
/// right before the binary swap: eligible + newer + verified + `--yes` →
/// reaches the apply stage (exit 0, "would replace"), WITHOUT overwriting the
/// running test binary. Exercises every gate the real apply runs.
#[cfg(target_os = "linux")]
#[test]
fn upgrade_reaches_replace_after_verification() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    write_script_receipt(&data);
    let fx = signed_fixture(b"trimwire fake archive payload");
    let gh = FakeGitHub::with_release("v999.0.0", fx.release);
    let out = upd_cmd("upgrade", dir.path(), &data, &gh.base(), &["--yes"])
        .env("TRIMWIRE_UPDATE_PUBKEY", &fx.pubkey)
        .env("TRIMWIRE_UPDATE_DRYRUN_APPLY", "1") // test seam: stop before swap
        .output()
        .expect("spawn");
    let s = String::from_utf8_lossy(&out.stdout);
    let err = String::from_utf8_lossy(&out.stderr);
    assert_eq!(
        out.status.code(),
        Some(0),
        "verified apply reaches the swap stage; stdout: {s} stderr: {err}"
    );
    assert!(s.contains("would replace"), "stdout: {s}");
}

// ---- download semantics: dry-run is verify-only (no staging); apply re-fetches ----

/// `upgrade --dry-run` downloads + verifies entirely in memory: it hits each
/// artifact exactly once and leaves NO staged file behind — not in the temp dir
/// (pointed at a dir we control via TMPDIR) nor in the data dir (only the
/// install receipt). Proves dry-run is verification-only, not a staging step.
#[test]
fn upgrade_dry_run_leaves_no_staged_artifact() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    write_script_receipt(&data);
    let tmp = dir.path().join("tmp");
    fs::create_dir_all(&tmp).unwrap();

    let fx = signed_fixture(b"trimwire fake archive payload");
    let pubkey = fx.pubkey.clone();
    let gh = FakeGitHub::with_release("v999.0.0", fx.release);

    let out = upd_cmd("upgrade", dir.path(), &data, &gh.base(), &["--dry-run"])
        .env("TRIMWIRE_UPDATE_PUBKEY", &pubkey)
        .env("TMPDIR", &tmp) // std::env::temp_dir() honors this on Unix
        .output()
        .expect("spawn");
    assert_eq!(out.status.code(), Some(0), "verified → exit 0");

    // Each artifact fetched exactly once (one full verification pass).
    assert_eq!(
        gh.hits(),
        (1, 1, 1),
        "dry-run fetches archive/.sha256/.minisig once each"
    );

    // No staged artifact in the temp dir — dry-run wrote nothing to disk.
    let tmp_entries: Vec<_> = fs::read_dir(&tmp).unwrap().flatten().collect();
    assert!(
        tmp_entries.is_empty(),
        "dry-run must leave no staged file in TMPDIR, found: {:?}",
        tmp_entries
            .iter()
            .map(|e| e.file_name())
            .collect::<Vec<_>>()
    );

    // The data dir holds only the install receipt — no cached archive/.sha256/.minisig.
    let rcpt_dir = data.join("trimwire");
    let staged: Vec<_> = fs::read_dir(&rcpt_dir)
        .unwrap()
        .flatten()
        .filter(|e| e.file_name() != std::ffi::OsStr::new("install-receipt.json"))
        .collect();
    assert!(
        staged.is_empty(),
        "dry-run must not cache any artifact in the data dir, found: {:?}",
        staged.iter().map(|e| e.file_name()).collect::<Vec<_>>()
    );
}

/// A later `upgrade --yes` performs its OWN fresh download + checksum + minisign
/// verification before applying — it does not trust any prior `--dry-run` state.
/// Proven by counting requests on the same server across both runs: the apply
/// run adds a second full archive/.sha256/.minisig fetch.
#[cfg(target_os = "linux")]
#[test]
fn upgrade_apply_redownloads_and_reverifies_after_dry_run() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    write_script_receipt(&data);
    let fx = signed_fixture(b"trimwire fake archive payload");
    let pubkey = fx.pubkey.clone();
    let gh = FakeGitHub::with_release("v999.0.0", fx.release);

    // 1) Dry-run: one full fetch + verify, nothing applied.
    let dry = upd_cmd("upgrade", dir.path(), &data, &gh.base(), &["--dry-run"])
        .env("TRIMWIRE_UPDATE_PUBKEY", &pubkey)
        .output()
        .expect("spawn");
    assert_eq!(dry.status.code(), Some(0), "dry-run verified");
    assert_eq!(gh.hits(), (1, 1, 1), "dry-run = one fetch of each artifact");

    // 2) Apply (stopped at the test seam right before the swap): must fetch +
    //    verify AGAIN, not reuse the dry-run download.
    let apply = upd_cmd("upgrade", dir.path(), &data, &gh.base(), &["--yes"])
        .env("TRIMWIRE_UPDATE_PUBKEY", &pubkey)
        .env("TRIMWIRE_UPDATE_DRYRUN_APPLY", "1") // stop before the binary swap
        .output()
        .expect("spawn");
    let s = String::from_utf8_lossy(&apply.stdout);
    assert_eq!(
        apply.status.code(),
        Some(0),
        "apply reached the swap stage: {s}"
    );
    assert!(s.contains("would replace"), "stdout: {s}");

    // The apply run did its OWN full download + verification (counts doubled),
    // proving it never trusts prior dry-run state.
    assert_eq!(
        gh.hits(),
        (2, 2, 2),
        "apply re-downloads + re-verifies every artifact (no reuse of dry-run state)"
    );
}

// ---- download size cap (Content-Length pre-check + streaming accumulation) ----

/// An oversized download with an honest `Content-Length` is rejected before the
/// body is read (pre-check). Simulated by capping at a few bytes via the
/// localhost-only `TRIMWIRE_UPDATE_MAX_BYTES` seam; the signed fixture archive
/// exceeds it. Fail-closed (exit 1).
#[test]
fn upgrade_dry_run_rejects_oversized_content_length() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    let fx = signed_fixture(b"trimwire fake archive payload well over eight bytes");
    let gh = FakeGitHub::with_release("v999.0.0", fx.release); // ArchiveCl::Auto
    let out = upd_cmd("upgrade", dir.path(), &data, &gh.base(), &["--dry-run"])
        .env("TRIMWIRE_UPDATE_PUBKEY", &fx.pubkey)
        .env("TRIMWIRE_UPDATE_MAX_BYTES", "8")
        .output()
        .expect("spawn");
    assert_eq!(out.status.code(), Some(1), "oversized (CL) → fail closed");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("NOT verified") && err.contains("size cap"),
        "must fail on the size cap specifically: {err}"
    );
}

/// An oversized download with NO `Content-Length` (chunked/close-delimited) is
/// still rejected by the streaming accumulation limit, not just the header
/// pre-check. Fail-closed (exit 1).
#[test]
fn upgrade_dry_run_rejects_oversized_without_content_length() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    let fx = signed_fixture(b"trimwire fake archive payload well over eight bytes");
    let gh = FakeGitHub::with_release_cl("v999.0.0", fx.release, ArchiveCl::Omit);
    let out = upd_cmd("upgrade", dir.path(), &data, &gh.base(), &["--dry-run"])
        .env("TRIMWIRE_UPDATE_PUBKEY", &fx.pubkey)
        .env("TRIMWIRE_UPDATE_MAX_BYTES", "8")
        .output()
        .expect("spawn");
    assert_eq!(
        out.status.code(),
        Some(1),
        "oversized (no CL) → fail closed"
    );
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("NOT verified") && err.contains("size cap"),
        "must fail on the streaming size cap specifically: {err}"
    );
}

/// A server that DECLARES a huge `Content-Length` (≫ the real 200 MB cap) while
/// sending a tiny body is rejected by the header pre-check — at the default cap,
/// no env override — so we never start reading an "oversized" body. Fail-closed.
#[test]
fn upgrade_dry_run_rejects_inflated_content_length_at_default_cap() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    let fx = signed_fixture(b"trimwire fake archive payload");
    // Declare ~10 GB; the real body is a few bytes.
    let gh = FakeGitHub::with_release_cl("v999.0.0", fx.release, ArchiveCl::Fixed(10_000_000_000));
    let out = upd_cmd("upgrade", dir.path(), &data, &gh.base(), &["--dry-run"])
        .env("TRIMWIRE_UPDATE_PUBKEY", &fx.pubkey)
        .output()
        .expect("spawn");
    assert_eq!(out.status.code(), Some(1), "inflated CL → fail closed");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("NOT verified") && err.contains("size cap"),
        "must fail on the Content-Length size cap specifically: {err}"
    );
}

/// A normal-sized signed release still verifies under the default cap (sanity:
/// the cap doesn't reject legitimate downloads).
#[test]
fn upgrade_dry_run_verifies_under_default_cap() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    let fx = signed_fixture(b"trimwire fake archive payload");
    let gh = FakeGitHub::with_release("v999.0.0", fx.release);
    let out = upd_cmd("upgrade", dir.path(), &data, &gh.base(), &["--dry-run"])
        .env("TRIMWIRE_UPDATE_PUBKEY", &fx.pubkey)
        .output()
        .expect("spawn");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(
        out.status.code(),
        Some(0),
        "verified under default cap. stdout: {stdout}\nstderr: {stderr}"
    );
    assert!(stdout.contains("verified"), "stdout: {stdout}");
}

// ---- strict self-update tag validation ----

/// A non-stable latest tag (prerelease) must be refused by `upgrade --dry-run`
/// before any asset URL is built — fail-closed (exit 1), even with a valid key.
#[test]
fn upgrade_dry_run_rejects_non_stable_tag() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    let gh = FakeGitHub::start("v9.9.9-rc.1");
    let out = upd_cmd("upgrade", dir.path(), &data, &gh.base(), &["--dry-run"])
        .env(
            "TRIMWIRE_UPDATE_PUBKEY",
            "RWQf6LRCGA9i53mlYecO4IzT51TGPpvWucNSCh1CBM0QTaLn73Y7GFO3",
        )
        .output()
        .expect("spawn");
    assert_eq!(out.status.code(), Some(1), "non-stable tag → fail closed");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("NOT verified") && err.contains("stable"),
        "got: {err}"
    );
}

/// `upgrade --yes` on an eligible install must also refuse a non-stable latest
/// tag before downloading/applying — fail-closed (exit 1).
#[cfg(target_os = "linux")]
#[test]
fn upgrade_apply_rejects_non_stable_tag() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    write_script_receipt(&data);
    let gh = FakeGitHub::start("v9.9.9-rc.1");
    let out = upd_cmd("upgrade", dir.path(), &data, &gh.base(), &["--yes"])
        .env(
            "TRIMWIRE_UPDATE_PUBKEY",
            "RWQf6LRCGA9i53mlYecO4IzT51TGPpvWucNSCh1CBM0QTaLn73Y7GFO3",
        )
        .output()
        .expect("spawn");
    assert_eq!(out.status.code(), Some(1), "non-stable tag → fail closed");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("not a stable"), "got: {err}");
}

/// `trimwire off` (default) is a true BYPASS, not a kill switch: it writes the
/// `~/.trimwire/bypass` sentinel (keeping the gateway serving) rather than
/// stopping the service, and `trimwire status` then reports pruning OFF. This is
/// the fix for the "off leaves a dead ANTHROPIC_BASE_URL" footgun.
///
/// Also asserts the honesty guard: when the gateway isn't actually serving,
/// `off` warns that bypass has nothing to connect to (rather than implying Claude
/// will keep working) — proven here by pointing at a free port nothing serves.
#[test]
fn off_bypasses_and_status_reflects_it() {
    let dir = tempfile::tempdir().unwrap();
    let sentinel = dir.path().join(".trimwire").join("bypass");
    // A free port nothing listens on, so the gateway health probe deterministically
    // fails and the "gateway isn't running" warning must fire.
    let listen = format!("127.0.0.1:{}", free_port());

    // `off` writes the sentinel and prints the one-line bypass message.
    let out = Command::new(bin())
        .arg("off")
        .env("HOME", dir.path())
        .env("TRIMWIRE_SERVER__LISTEN", &listen)
        .env_remove("XDG_CONFIG_HOME")
        .output()
        .expect("spawn trimwire off");
    assert!(out.status.success(), "off exits 0");
    assert!(sentinel.exists(), "off creates the bypass sentinel");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("straight to Anthropic") && stdout.contains("no pruning"),
        "off prints the bypass message: {stdout}"
    );
    assert!(
        stdout.contains("gateway isn't running"),
        "off warns when the gateway isn't serving: {stdout}"
    );

    // `status` surfaces the bypass state on its `pruning:` line.
    let st = Command::new(bin())
        .arg("status")
        .env("HOME", dir.path())
        .env("TRIMWIRE_SERVER__LISTEN", &listen)
        .env_remove("XDG_CONFIG_HOME")
        .output()
        .expect("spawn trimwire status");
    let sout = String::from_utf8_lossy(&st.stdout);
    assert!(
        sout.contains("pruning: OFF") && sout.contains("bypass"),
        "status shows bypass: {sout}"
    );
}

/// `trimwire run --bypass` runs ONE session with trimwire out of the loop: it
/// launches `claude` pointed straight at the configured upstream (not the local
/// gateway), leaving global state untouched.
#[test]
fn run_bypass_points_claude_at_upstream() {
    let dir = tempfile::tempdir().unwrap();
    let bindir = dir.path().join("bin");
    fs::create_dir_all(&bindir).unwrap();

    // Fake `claude`: record the base URL it was handed, then exit 0.
    let fake = bindir.join("claude");
    fs::write(
        &fake,
        "#!/bin/sh\nprintf 'BASE=%s TS=%s' \"$ANTHROPIC_BASE_URL\" \"$ENABLE_TOOL_SEARCH\" > \"$CLAUDE_OUT\"\nexit 0\n",
    )
    .unwrap();
    fs::set_permissions(&fake, fs::Permissions::from_mode(0o755)).unwrap();

    let out_file = dir.path().join("out.txt");
    let path = format!(
        "{}:{}",
        bindir.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    // Pin the gateway listen port so we can prove --bypass never bound it.
    let port = free_port();

    let status = Command::new(bin())
        .args(["run", "--bypass", "--print", "hi"])
        .env("HOME", dir.path())
        .env("PATH", path)
        .env("CLAUDE_OUT", &out_file)
        .env("TRIMWIRE_SERVER__LISTEN", format!("127.0.0.1:{port}"))
        // A distinctive upstream so we can prove the child was pointed at IT,
        // not at any local gateway URL.
        .env(
            "TRIMWIRE_SERVER__UPSTREAM",
            "https://upstream.example.invalid",
        )
        .env("TRIMWIRE_LEDGER__ENABLED", "false")
        .env_remove("XDG_CONFIG_HOME")
        .status()
        .expect("spawn trimwire run --bypass");

    assert_eq!(status.code(), Some(0), "claude's exit code propagates");
    let recorded = fs::read_to_string(&out_file).expect("fake claude wrote output");
    assert!(
        recorded.contains("BASE=https://upstream.example.invalid"),
        "child points straight at the upstream, not the gateway: {recorded}"
    );
    assert!(
        recorded.contains("TS=true"),
        "ENABLE_TOOL_SEARCH still set so web search works: {recorded}"
    );
    // Prove the "no gateway in the loop" property: nothing is listening on the
    // configured gateway port — `--bypass` returned before starting one.
    assert!(
        std::net::TcpStream::connect(("127.0.0.1", port)).is_err(),
        "run --bypass must NOT start a gateway on the configured port {port}"
    );
}
