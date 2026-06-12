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
        .env("HOME", dir.path())
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
        .env("HOME", dir.path())
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

/// `summarizer setup` with stdin closed (EOF) must cancel cleanly — never spin
/// forever re-prompting on an empty answer — and write no config.
#[test]
fn summarizer_setup_cancels_on_stdin_eof() {
    let dir = tempfile::tempdir().unwrap();
    let out = Command::new(bin())
        .args(["summarizer", "setup"])
        .env("HOME", dir.path())
        .env_remove("XDG_CONFIG_HOME")
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
    assert!(
        !dir.path().join(".config/trimwire/config.toml").exists(),
        "cancel writes nothing"
    );
}
