//! trimwire CLI entry point. Thin wiring only — command bodies live in `cli`.

mod cli;

use anyhow::{Context, Result};
use clap::{CommandFactory, Parser, Subcommand};

// ---- Statusline sub-actions ------------------------------------------------

/// `trimwire statusline …` actions.
#[derive(Subcommand)]
enum StatuslineAction {
    /// (internal) The command Claude Code runs each refresh; you never type it.
    #[command(hide = true)]
    Render {
        /// Internal: file holding the wrapped statusline command to run first.
        #[arg(long, hide = true)]
        wrap_file: Option<std::path::PathBuf>,
    },
    /// Make trimwire your Claude Code statusline (errors if you already have one — use `wrap`).
    Add,
    /// Keep your existing statusline and add a trimwire row beneath it (reversible).
    Wrap,
    /// Remove trimwire from the statusline (restores any wrapped original).
    Remove,
}

// ---- Config sub-actions ----------------------------------------------------

/// `trimwire config …` actions.
#[derive(Subcommand)]
enum ConfigAction {
    /// Print the effective config after the profile + global/project/env merge.
    Show {
        /// Emit JSON instead of TOML.
        #[arg(long)]
        json: bool,
    },
    /// Open the config file in $EDITOR (the default with no subcommand).
    Edit,
}

// ---- Sweep sub-actions -----------------------------------------------------

/// `trimwire sweep …` actions.
#[derive(Subcommand)]
enum SweepAction {
    /// List the session transcripts trimwire can clean (no need to find paths).
    List,
    /// Clean every discovered session (active ones safely abort).
    All {
        /// Report what would change without writing anything.
        #[arg(long)]
        dry_run: bool,
        /// Skip the confirmation prompt (required in non-interactive use).
        #[arg(long)]
        yes: bool,
    },
    /// Clean one session file.
    File {
        /// Path to the session `.jsonl` file.
        path: std::path::PathBuf,
        /// Only validate the file; don't modify it.
        #[arg(long, conflicts_with = "dry_run")]
        validate_only: bool,
        /// Report what would change without writing anything.
        #[arg(long)]
        dry_run: bool,
    },
    /// Restore a session from its latest backup.
    Undo {
        /// Path to the session `.jsonl` file to restore.
        path: std::path::PathBuf,
    },
}

// ---- Summarizer sub-actions ------------------------------------------------

/// `trimwire summarizer …` actions.
#[derive(Subcommand)]
enum SummarizerAction {
    /// Interactive wizard to configure the summarizer backend (local ollama or
    /// a cloud API provider). Writes the `[summarizer]` config section.
    Setup,
    /// Show the current summarizer engine, model, and config state.
    Status,
    /// Score a summarizer model against the bundled quality corpus
    /// (directional sanity-check).
    ///
    /// LOCAL ollama models score directly. An API provider (a `--model` matching
    /// a configured `[[summarizer.providers]]` id) makes real, PAID API calls on
    /// your own key, so it only runs with `--yes`; without it you get a dry-run
    /// cost preview. API scores are directional only — not comparable to local.
    Benchmark {
        /// Model to score (repeatable): a local ollama tag, or a configured
        /// API provider id. Omit to use your configured model / default.
        #[arg(long)]
        model: Vec<String>,
        /// Score every model installed in ollama (disqualified ones are skipped).
        #[arg(long)]
        all_installed: bool,
        /// Directory to save each produced summary into (skim them — scores can't judge prose).
        #[arg(long)]
        out: Option<std::path::PathBuf>,
        /// Emit machine-readable JSON.
        #[arg(long)]
        json: bool,
        /// One line per model (model + FCS).
        #[arg(long, short = 'q')]
        quiet: bool,
        /// Confirm real, paid API calls when scoring an API provider (no upload).
        /// Local models ignore this. Without it, an API provider is a dry-run.
        #[arg(long)]
        yes: bool,
        /// Cap how many corpus slices an API provider is scored on (default: all).
        /// Spend-control for paid providers; local models ignore it.
        #[arg(long)]
        max_calls: Option<usize>,
    },
    /// Probe whether a model holds your slice budget: plant distinctive facts
    /// across a synthetic OLD slice at the configured `slice_char_budget`,
    /// summarize it, and report fact retention by position (start/mid/end).
    ///
    /// The installed-user counterpart of the `api_harm` example. A `--model`
    /// matching a configured provider id makes ONE real, PAID call (needs
    /// `--yes`); `local` / an ollama tag runs locally. Exits non-zero if
    /// retention is below the 90% gate.
    Probe {
        /// Model to probe: a configured provider id, `local`, or a local ollama
        /// tag. Omit to use your configured engine.
        #[arg(long)]
        model: Option<String>,
        /// Slice budget in bytes (default: the engine's effective slice_char_budget).
        #[arg(long)]
        bytes: Option<usize>,
        /// Repeat the probe N times and report the retention distribution
        /// (pass-rate / p50 / min). Model summaries are non-deterministic, so a
        /// single run is unreliable near the 90% gate. PASS requires ALL N to pass.
        /// For an API provider, cost scales with N.
        #[arg(long, default_value_t = 1)]
        runs: usize,
        /// How many runs to fire in PARALLEL (API only — the local engine is forced
        /// serial). Speeds up a big `--runs` sweep; mind provider rate limits.
        #[arg(long, default_value_t = 1)]
        concurrency: usize,
        /// Confirm real, paid API call(s) when probing an API provider.
        #[arg(long)]
        yes: bool,
    },
}

// ---- Share sub-actions -----------------------------------------------------

/// `trimwire share …` actions.
#[derive(Subcommand)]
enum ShareAction {
    /// Opt in: persist consent so `share stats` uploads (anonymous, content-free).
    Enable,
    /// Opt out: stop uploading. Reverses `share enable`.
    Disable,
    /// OPT-IN: upload an anonymous, content-free aggregate of your ledger to
    /// the community dashboard. Dry-run until you `share enable` (or pass `--yes`
    /// once). See docs/TELEMETRY.md.
    Stats {
        /// Confirm the upload (without it, this is a dry run).
        #[arg(long)]
        yes: bool,
        /// Bypass the once-per-day throttle.
        #[arg(long)]
        force: bool,
    },
    /// OPT-IN: score your summarizer model and upload the anonymous,
    /// content-free per-model row to the community benchmark endpoint.
    /// Dry-run unless `--yes`.
    Benchmark {
        /// Model tag to score (repeatable). Omit to use your configured summarizer model.
        #[arg(long)]
        model: Vec<String>,
        /// Score every model installed in ollama (disqualified ones are skipped).
        #[arg(long)]
        all_installed: bool,
        /// Confirm the upload (without it, this is a dry run).
        #[arg(long)]
        yes: bool,
    },
}

// ---- Top-level command enum ------------------------------------------------
//
// clap 4 does not natively group subcommands under separate headings in the
// flat `Commands:` help section. We use `display_order` to sort commands into
// logical groups (LIFECYCLE 10-15, INSPECT 20-23, SUMMARIZER 30, SHARE 40,
// MAINTENANCE 50-51, SHELL 60-62) and document the groupings in `after_help`.

#[derive(Parser)]
#[command(
    name = "trimwire",
    version = env!("TRIMWIRE_VERSION"),
    about = "Local gateway that prunes Claude Code context on every API call.",
    after_help = "\
Commands by group:\n\
\x20 LIFECYCLE    install · uninstall · on · off · status · doctor\n\
\x20 INSPECT      stats · recall · preview · dashboard\n\
\x20 SUMMARIZER   summarizer  (setup · status · benchmark · probe)\n\
\x20 SHARE        share  (enable · disable · stats · benchmark)   — opt-in, content-free\n\
\x20 MAINTENANCE  sweep (list/all/file/undo) · config (show/edit)\n\
\x20 SHELL        statusline (add/wrap/remove) · completions · man\n\
\n\
New-user flow:\n\
\x20  trimwire install   →  source ~/.zshrc (or open a new terminal)   →  trimwire doctor   →  claude\n\
\x20  trimwire stats     (see savings after your first session)\n\
\x20  trimwire summarizer setup   (optional: enable model-based compression)\n\
\n\
Docs & issues: https://github.com/AZagatti/trimwire"
)]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    // ---- LIFECYCLE (display_order 10-15) -----------------------------------
    /// Write a starter config, add the env exports, and install the always-up service.
    #[command(
        display_order = 10,
        after_help = "See also: trimwire on / off / status, trimwire doctor, trimwire uninstall"
    )]
    Install {
        /// Also start before login / survive logout (systemd lingering).
        #[arg(long)]
        boot: bool,
    },

    /// Remove the service, GUI env hooks, and lingering that `install` set up.
    #[command(display_order = 11)]
    Uninstall,

    /// Start the always-up gateway service.
    #[command(display_order = 12)]
    On,

    /// Stop the gateway service (Claude Code goes straight to Anthropic again).
    #[command(display_order = 13)]
    Off,

    /// Show whether the gateway is running and serving.
    #[command(display_order = 14)]
    Status,

    /// Diagnose the setup: config, gateway health, env wiring, ledger.
    #[command(display_order = 15)]
    Doctor,

    // ---- INSPECT (display_order 20-23) -------------------------------------
    /// Show the savings ledger.
    #[command(
        display_order = 20,
        after_help = "See also: trimwire recall (find a session), trimwire dashboard \
                            (HTML report), trimwire preview (what-if, no network)"
    )]
    Stats {
        /// Emit machine-readable JSON (totals, per-day, per-strategy, estimates).
        #[arg(long)]
        json: bool,
        /// One-line headline only — for scripts, prompts, and a quick glance.
        #[arg(long, short = 'q', conflicts_with_all = ["json", "session"])]
        quiet: bool,
        /// Show the full response instrumentation and a longer day history.
        #[arg(long, short = 'v', conflicts_with_all = ["json", "quiet"])]
        verbose: bool,
        /// Show a per-session, per-model cache/token report. Pass a session id
        /// (from `trimwire recall`), or use `--session` with no value to
        /// automatically select the most-recent session.
        #[arg(long, num_args = 0..=1, default_missing_value = "last")]
        session: Option<String>,
        /// Only count requests on/after this UTC date (YYYY-MM-DD).
        #[arg(long, value_name = "YYYY-MM-DD", conflicts_with = "session")]
        since: Option<String>,
        /// Only count requests up to and including this UTC date (YYYY-MM-DD).
        #[arg(long, value_name = "YYYY-MM-DD", conflicts_with = "session")]
        until: Option<String>,
    },

    /// List recent sessions (content-free metadata) to find one for `stats --session`.
    #[command(display_order = 21)]
    Recall {
        /// Optional filter: keep sessions whose id or model contains this substring.
        query: Option<String>,
        /// Emit machine-readable JSON.
        #[arg(long)]
        json: bool,
        /// Max sessions to list (newest first).
        #[arg(long, default_value_t = 20)]
        limit: usize,
        /// Only sessions active on/after this UTC date (YYYY-MM-DD).
        #[arg(long, value_name = "YYYY-MM-DD")]
        since: Option<String>,
        /// Only sessions active up to and including this UTC date (YYYY-MM-DD).
        #[arg(long, value_name = "YYYY-MM-DD")]
        until: Option<String>,
    },

    /// What-if: estimate what pruning would trim from a recorded session
    /// transcript (.jsonl), without touching the file or the network.
    #[command(
        display_order = 22,
        after_help = "Example: trimwire preview --last   (the most recent session — no path needed)\n\
                      Summarizer: trimwire preview --last --with-summarizer [--yes]"
    )]
    Preview {
        /// Path to a Claude Code session transcript (~/.claude/projects/**/*.jsonl).
        /// Omit it and pass --last to auto-pick the most recent session.
        path: Option<std::path::PathBuf>,
        /// Preview the most recently modified session — no path needed.
        #[arg(long, conflicts_with = "path")]
        last: bool,
        /// Pruning profile to measure against (default/gentle; default).
        #[arg(long)]
        profile: Option<String>,
        /// Include sub-agent (isSidechain) turns — off by default, since they
        /// are never part of the parent request's messages[].
        #[arg(long)]
        include_sidechains: bool,
        /// Emit machine-readable JSON.
        #[arg(long)]
        json: bool,
        /// Also estimate the additional reduction the configured summarizer engine would
        /// contribute on this session. Requires a non-model-free summarizer in config.
        /// For local (ollama) engines: runs the model directly (no paid call).
        /// For API engines: makes ONE real paid call — confirm with --yes, or it's a dry run.
        #[arg(long)]
        with_summarizer: bool,
        /// Confirm a paid API call when --with-summarizer resolves to an API engine.
        /// Ignored for the local (ollama) engine. Without it the deterministic preview
        /// still prints, but the summarizer call is skipped.
        #[arg(long)]
        yes: bool,
    },

    /// Write a self-contained local stats dashboard (content-free) to an HTML file.
    #[command(display_order = 23)]
    Dashboard {
        /// Output path (default: trimwire-report.html in the current directory).
        #[arg(long)]
        out: Option<std::path::PathBuf>,
    },

    // ---- SUMMARIZER (display_order 30) -------------------------------------
    /// Manage and query the optional summarizer backend (local or API provider).
    #[command(display_order = 30)]
    Summarizer {
        #[command(subcommand)]
        action: SummarizerAction,
    },

    // ---- SHARE (display_order 40) ------------------------------------------
    /// OPT-IN anonymous telemetry uploads to the community dashboard (dry-run
    /// unless --yes; content-free — see docs/TELEMETRY.md).
    #[command(display_order = 40)]
    Share {
        #[command(subcommand)]
        action: ShareAction,
    },

    // ---- MAINTENANCE (display_order 50-51) ---------------------------------
    /// Clean Claude Code session transcripts on disk (atomic, backed up).
    #[command(display_order = 50)]
    Sweep {
        #[command(subcommand)]
        action: SweepAction,
    },

    /// Show the resolved config, or (with no subcommand) open it in $EDITOR.
    #[command(display_order = 51)]
    Config {
        #[command(subcommand)]
        action: Option<ConfigAction>,
    },

    // ---- SHELL (display_order 60-62) ----------------------------------------
    /// Manage / render trimwire's Claude Code statusline bar.
    #[command(display_order = 60)]
    Statusline {
        #[command(subcommand)]
        action: StatuslineAction,
    },

    /// Print a shell completion script to stdout. Pipe or redirect it to your
    /// shell's standard location — one-time setup, then restart your shell.
    #[command(
        display_order = 61,
        long_about = "\
Print a shell completion script to stdout. Pipe or redirect it to your \
shell's standard location — one-time setup, then restart your shell.\n\
\n\
bash\n\
\x20 trimwire completions bash > ~/.local/share/bash-completion/completions/trimwire\n\
\x20 # then: source ~/.bashrc  (or open a new terminal)\n\
\n\
zsh  — simplest (add to ~/.zshrc, then restart your shell):\n\
\x20 echo 'eval \"$(trimwire completions zsh)\"' >> ~/.zshrc\n\
\x20 # Or write a file to a dir on $fpath:\n\
\x20 trimwire completions zsh > ~/.zfunc/_trimwire\n\
\x20 # (requires: fpath=(~/.zfunc $fpath) + autoload -Uz compinit in ~/.zshrc)\n\
\n\
fish  — fish picks it up automatically:\n\
\x20 trimwire completions fish > ~/.config/fish/completions/trimwire.fish\n\
\n\
powershell  — append to your profile so it loads each session:\n\
\x20 trimwire completions powershell >> $PROFILE\n\
\n\
elvish  — source inline from rc.elv:\n\
\x20 echo 'eval (trimwire completions elvish | slurp)' >> ~/.config/elvish/rc.elv"
    )]
    Completions {
        /// Target shell.
        shell: clap_complete::Shell,
    },

    /// Generate man pages. With `--out <DIR>` writes one page per command there
    /// (for packagers); with no `--out` prints the top-level page to stdout
    /// (`trimwire man | man -l -`).
    #[command(display_order = 62)]
    Man {
        /// Directory to write the generated man pages into.
        #[arg(long, value_name = "DIR")]
        out: Option<std::path::PathBuf>,
    },

    // ---- HIDDEN (internal / advanced) --------------------------------------
    /// Start the gateway in the foreground (internal — use `trimwire on` instead).
    /// `daemon` is a hidden alias (historical name; also used by the CI smoke test).
    #[command(hide = true, alias = "daemon")]
    Serve {
        /// Address:port to listen on. Overrides `[server] listen` in config.
        #[arg(long)]
        listen: Option<String>,
        /// Upstream URL. Overrides `[server] upstream` in config.
        #[arg(long)]
        upstream: Option<String>,
        /// Write a metadata-only wire audit to <file> (JSONL; shape/counts only,
        /// never message content). Same as TRIMWIRE_AUDIT=<file>.
        #[arg(long, value_name = "FILE")]
        audit: Option<String>,
    },

    /// Start the gateway in the background, then launch `claude` pointed at it.
    #[command(hide = true)]
    Run {
        /// Args forwarded to `claude`.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
        /// Write a metadata-only wire audit to <file> (JSONL; shape/counts only,
        /// never message content). Same as TRIMWIRE_AUDIT=<file>.
        #[arg(long, value_name = "FILE")]
        audit: Option<String>,
    },

    /// Claude Code hook: warn in-session if trimwire is set but not serving (reads JSON on stdin).
    #[command(hide = true)]
    Hook,
}

// ---- main ------------------------------------------------------------------

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("TRIMWIRE_LOG")
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();
    match cli.command {
        // LIFECYCLE
        Cmd::Install { boot } => cli::install(boot),
        Cmd::Uninstall => cli::uninstall(),
        Cmd::On => cli::on(),
        Cmd::Off => cli::off(),
        Cmd::Status => cli::status(),
        Cmd::Doctor => cli::doctor(),

        // INSPECT
        Cmd::Stats {
            json,
            quiet,
            verbose,
            session,
            since,
            until,
        } => cli::stats(json, quiet, verbose, session, since, until),
        Cmd::Recall {
            query,
            json,
            limit,
            since,
            until,
        } => cli::recall(query, json, limit, since, until),
        Cmd::Preview {
            path,
            last,
            profile,
            include_sidechains,
            json,
            with_summarizer,
            yes,
        } => cli::preview(
            path,
            last,
            profile,
            include_sidechains,
            json,
            with_summarizer,
            yes,
        ),
        Cmd::Dashboard { out } => cli::dashboard(out),

        // SUMMARIZER
        Cmd::Summarizer { action } => match action {
            SummarizerAction::Setup => cli::summarizer_setup(),
            SummarizerAction::Status => cli::summarizer_status(),
            SummarizerAction::Benchmark {
                model,
                all_installed,
                out,
                json,
                quiet,
                yes,
                max_calls,
            } => cli::benchmark(
                model,
                all_installed,
                out,
                json,
                quiet,
                false,
                yes,
                max_calls,
            ),
            SummarizerAction::Probe {
                model,
                bytes,
                runs,
                concurrency,
                yes,
            } => cli::summarizer_probe(model, bytes, runs, concurrency, yes),
        },

        // SHARE
        Cmd::Share { action } => match action {
            ShareAction::Enable => cli::share_enable(),
            ShareAction::Disable => cli::share_disable(),
            ShareAction::Stats { yes, force } => cli::share_stats(yes, force),
            ShareAction::Benchmark {
                model,
                all_installed,
                yes,
            } => cli::share_benchmark(model, all_installed, yes),
        },

        // MAINTENANCE
        Cmd::Sweep { action } => match action {
            SweepAction::List => cli::sweep_list(),
            SweepAction::All { dry_run, yes } => cli::sweep_all(dry_run, yes),
            SweepAction::File {
                path,
                validate_only,
                dry_run,
            } => cli::sweep_file(path, validate_only, dry_run),
            SweepAction::Undo { path } => cli::sweep_undo(path),
        },
        Cmd::Config { action } => match action {
            None | Some(ConfigAction::Edit) => cli::config_edit(),
            Some(ConfigAction::Show { json }) => cli::config_show(json),
        },

        // SHELL
        Cmd::Statusline { action } => match action {
            StatuslineAction::Render { wrap_file } => cli::statusline(wrap_file),
            StatuslineAction::Add => cli::statusline_add(false),
            StatuslineAction::Wrap => cli::statusline_add(true),
            StatuslineAction::Remove => cli::statusline_remove(),
        },
        Cmd::Completions { shell } => {
            clap_complete::generate(
                shell,
                &mut Cli::command(),
                "trimwire",
                &mut std::io::stdout(),
            );
            Ok(())
        }
        Cmd::Man { out } => {
            let cmd = Cli::command();
            match out {
                Some(dir) => {
                    std::fs::create_dir_all(&dir)
                        .with_context(|| format!("create {}", dir.display()))?;
                    clap_mangen::generate_to(cmd, &dir)
                        .with_context(|| format!("write man pages to {}", dir.display()))?;
                    eprintln!("wrote man pages to {}", dir.display());
                }
                None => clap_mangen::Man::new(cmd)
                    .render(&mut std::io::stdout())
                    .context("render man page")?,
            }
            Ok(())
        }

        // HIDDEN
        Cmd::Serve {
            listen,
            upstream,
            audit,
        } => cli::serve(listen, upstream, audit),
        Cmd::Run { args, audit } => cli::run(&args, audit),
        Cmd::Hook => cli::hook(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap_complete::Shell;

    /// clap's own structural validation — catches arg/subcommand conflicts,
    /// duplicate flags, bad value parsers, etc. (the recommended CLI test).
    #[test]
    fn cli_definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn completions_generate_for_every_shell() {
        for shell in [
            Shell::Bash,
            Shell::Zsh,
            Shell::Fish,
            Shell::Elvish,
            Shell::PowerShell,
        ] {
            let mut buf = Vec::new();
            clap_complete::generate(shell, &mut Cli::command(), "trimwire", &mut buf);
            assert!(!buf.is_empty(), "empty completion script for {shell:?}");
        }
    }

    #[test]
    fn man_page_renders() {
        let mut buf = Vec::new();
        clap_mangen::Man::new(Cli::command())
            .render(&mut buf)
            .expect("man render");
        assert!(!buf.is_empty());
        assert!(String::from_utf8_lossy(&buf).contains("trimwire"));
    }

    #[test]
    fn version_string_carries_build_metadata() {
        // env set by build.rs; always starts with the plain Cargo version.
        assert!(env!("TRIMWIRE_VERSION").starts_with(env!("CARGO_PKG_VERSION")));
    }

    // ---- v1 command-tree reorg (clean break — no aliases) ------------------

    /// The new noun-grouped tree parses: `share {stats}`, `summarizer {setup,status}`,
    /// and the hidden-but-functional `serve`. These are present in every build
    /// (the `benchmark` subcommands are feature-gated and covered separately).
    #[test]
    fn new_command_tree_parses() {
        assert!(matches!(
            Cli::try_parse_from(["trimwire", "share", "stats"]).map(|c| c.command),
            Ok(Cmd::Share {
                action: ShareAction::Stats { .. }
            })
        ));
        assert!(matches!(
            Cli::try_parse_from(["trimwire", "summarizer", "setup"]).map(|c| c.command),
            Ok(Cmd::Summarizer {
                action: SummarizerAction::Setup
            })
        ));
        assert!(matches!(
            Cli::try_parse_from(["trimwire", "summarizer", "status"]).map(|c| c.command),
            Ok(Cmd::Summarizer {
                action: SummarizerAction::Status
            })
        ));
        // `serve` is hidden in --help but still a valid invocation.
        assert!(matches!(
            Cli::try_parse_from(["trimwire", "serve"]).map(|c| c.command),
            Ok(Cmd::Serve { .. })
        ));
    }

    /// Clean break: the old spellings no longer exist (no aliases, no shims).
    #[test]
    fn old_command_spellings_are_gone() {
        // `--share`/`--yes`/`--force` were lifted off `stats` onto `share stats`.
        assert!(Cli::try_parse_from(["trimwire", "stats", "--share"]).is_err());
        assert!(Cli::try_parse_from(["trimwire", "stats", "--yes"]).is_err());
        // `daemon` is retained as a hidden alias for `serve` (historical name +
        // the CI smoke test uses it).
        assert!(Cli::try_parse_from(["trimwire", "daemon", "--listen", "127.0.0.1:1"]).is_ok());
        // `stats` still keeps its non-share flags.
        assert!(Cli::try_parse_from(["trimwire", "stats", "--json"]).is_ok());
        assert!(Cli::try_parse_from(["trimwire", "stats", "--since", "2026-01-01"]).is_ok());
    }
}
