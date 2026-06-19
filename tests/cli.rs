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
    // key ENV-VAR NAME; y=add; 1=pick as primary; n=no fallback; y=write.
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"a\ntestprov\nanthropic\n\ntest-model\nTESTPROV_KEY\ny\n1\nn\ny\n")
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
