# CLI Reference

trimwire's command tree is grouped into six areas. The new-user flow is:

```sh
trimwire install             # wire trimwire into Claude Code
source ~/.zshrc              # pick up ANTHROPIC_BASE_URL (or open a new terminal)
trimwire doctor              # verify the setup
claude                       # use Claude Code as normal
trimwire stats               # see savings after your first session
```

Tab-completion is available for bash, zsh, fish, elvish, and PowerShell. See `trimwire completions --help` for per-shell one-liners.

---

## LIFECYCLE

Commands to install, start, stop, and verify the gateway.

### `trimwire install`

Write a starter config, add the `ANTHROPIC_BASE_URL` env export, and register the always-up service.

| Flag | Description |
|---|---|
| `--boot` | Enable lingering (systemd) so the service survives logout and starts before login |

After install, source your shell rc (`source ~/.zshrc` or `source ~/.bashrc`) or open a new terminal to pick up the new env export, then run `trimwire doctor` to verify.

```sh
trimwire install            # standard install
trimwire install --boot     # also survive logout (systemd lingering)
```

### `trimwire uninstall`

Remove the service, the GUI/login env hooks, and lingering that `install` set up. **It does not edit your shell rc** — the `# >>> trimwire >>>` block that exports `ANTHROPIC_BASE_URL` is left in place (rewriting a user's rc is risky); the command prints a reminder to delete that block by hand and restart your shell.

### `trimwire on`

Start the gateway service.

### `trimwire off`

Stop the gateway service. Your shell still exports `ANTHROPIC_BASE_URL` (from `install`), so Claude Code keeps pointing at the now-stopped local gateway and its calls **fail** until you either `trimwire on` again **or** `unset ANTHROPIC_BASE_URL` (or open a fresh shell after `trimwire uninstall`) to go straight to Anthropic.

### `trimwire run [<claude args>…]`

Launch `claude` through a one-shot gateway, without installing the always-on service: trimwire starts the gateway in the background, points it at `claude` via `ANTHROPIC_BASE_URL`, runs `claude`, then tears the gateway down on exit. The command is always `claude` — any positional args are forwarded to it (so it's `trimwire run`, *not* `trimwire run claude`). Good for trying trimwire once. `--audit FILE` (or `TRIMWIRE_AUDIT=FILE`) writes a metadata-only wire audit (JSONL — shape/counts only, never message content).

### `trimwire status`

Show whether the gateway is running and serving.

### `trimwire doctor`

Diagnose the setup: config, active profile, gateway health, `ANTHROPIC_BASE_URL` wiring, ledger, and summarizer state. Run this first whenever something looks wrong.

---

## INSPECT

Commands to view savings, browse sessions, and test pruning without network I/O.

### `trimwire stats`

Show the savings ledger: bytes pruned, reduction %, estimated tokens, per-strategy breakdown, and cache health.

| Flag | Description |
|---|---|
| `--json` | Emit machine-readable JSON (`bytes_saved`, `reduction_pct`, `est_tokens_removed`, `per_day`, `per_strategy`, …) |
| `-q`, `--quiet` | One-line headline only — for scripts, prompts, and a quick glance |
| `-v`, `--verbose` | Full response instrumentation and a longer day history |
| `--session [ID]` | Per-session, per-model cache/token report. Omit the value (`--session`) to show the most recent session. Pass a session id from `trimwire recall` for a specific one. Conflicts with `--since`/`--until` |
| `--since YYYY-MM-DD` | Count only requests on/after this UTC date. Conflicts with `--session` |
| `--until YYYY-MM-DD` | Count only requests up to and including this UTC date. Conflicts with `--session` |

```sh
trimwire stats                          # summary: savings, reduction %, cache health
trimwire stats -v                       # full instrumentation and longer day history
trimwire stats --session                # most recent session detail
trimwire stats --session abc123         # a specific session from `trimwire recall`
trimwire stats --since 2026-06-01       # savings since a date
trimwire stats --json | jq .reduction_pct   # scripting
```

### `trimwire recall`

List recent sessions (content-free metadata: id, model, start time, request count) so you can find a session id to pass to `stats --session`.

| Flag | Description |
|---|---|
| `[QUERY]` | Filter: keep sessions whose id or model contains this substring (positional, optional) |
| `--json` | Machine-readable JSON |
| `--limit N` | Max sessions to list, newest first (default: 20) |
| `--since YYYY-MM-DD` | Only sessions active on/after this UTC date |
| `--until YYYY-MM-DD` | Only sessions active up to and including this UTC date |

```sh
trimwire recall                           # 20 most recent sessions
trimwire recall sonnet                    # filter by model name
trimwire recall --since 2026-06-01        # sessions from a date window
trimwire recall --limit 5 --json          # scripting
```

### `trimwire preview`

What-if: estimate what pruning would trim from a recorded session transcript, without touching the file or the network. Safe to run on an active session.

| Flag | Description |
|---|---|
| `[PATH]` | Path to a Claude Code session transcript (`~/.claude/projects/**/*.jsonl`). Omit with `--last` |
| `--last` | Auto-pick the most recently modified session — no path needed. Conflicts with `PATH` |
| `--profile NAME` | Pruning profile to measure against (`default` or `gentle`; default: `default`) |
| `--include-sidechains` | Include sub-agent (isSidechain) turns — off by default since they are never part of the parent request's `messages[]` |
| `--with-summarizer` | Also estimate the configured summarizer's extra reduction on this session (off by default; the base preview is offline/instant). Directional, single-slice estimate |
| `--yes` | Confirm a real, paid API call when `--with-summarizer` uses an API engine. Without it, an API engine shows a cost preview and is skipped; `local` needs no confirmation |
| `--json` | Machine-readable JSON |

```sh
trimwire preview --last                                   # most recent session
trimwire preview ~/.claude/projects/foo/bar.jsonl         # a specific transcript
trimwire preview --last --profile gentle                  # compare against gentle profile
trimwire preview --last --with-summarizer                 # also estimate a local summarizer
trimwire preview --last --with-summarizer --yes           # ...incl. a paid API engine (real call)
```

### `trimwire dashboard`

Write a self-contained, content-free local stats dashboard to an HTML file. Open the file in any browser — no server needed.

| Flag | Description |
|---|---|
| `--out PATH` | Output path (default: `trimwire-report.html` in the current directory) |

```sh
trimwire dashboard                              # writes trimwire-report.html
trimwire dashboard --out ~/tmp/report.html      # custom path
```

---

## SUMMARIZER

Manage the optional model-based summarizer backend. Off by default — `engine = "model-free"`. See [Summarizer](SUMMARIZER.md) for full setup and privacy details.

### `trimwire summarizer setup`

Interactive wizard: asks which engine (`local`, a cloud API provider, or `model-free`), which model, and (for API engines) which API-key environment variable, then writes the config block.

### `trimwire summarizer status`

Show the current summarizer engine, model, and whether the endpoint is reachable.

### `trimwire summarizer benchmark`

Score a summarizer model against the bundled quality corpus. A directional sanity-check — not an authoritative ranking. See [Benchmark a local model](BENCHMARK.md) for full guidance.

Local ollama models score directly. An API provider (a `--model` matching a configured `[[summarizer.providers]]` id) makes real, paid API calls on your key, so it only runs with `--yes`; without it you get a dry-run cost preview.

| Flag | Description |
|---|---|
| `--model TAG_OR_ID` | Model to score (repeatable): a local ollama tag, or a configured API provider id. Omit to use your configured model |
| `--all-installed` | Score every model installed in ollama (disqualified ones are skipped) |
| `--out DIR` | Directory to save each produced summary (skim them — scores cannot judge prose) |
| `--json` | Machine-readable JSON |
| `-q`, `--quiet` | One line per model (model + score) |
| `--yes` | Confirm real, paid API calls for an API provider. Local models ignore this; without it an API provider is a dry run |
| `--max-calls N` | Cap how many corpus slices an API provider is scored on. Spend control for paid providers; local models ignore it |

```sh
trimwire summarizer benchmark                           # configured model
trimwire summarizer benchmark --model qwen3.5:4b        # a specific local model
trimwire summarizer benchmark --all-installed           # every installed ollama model
trimwire summarizer benchmark --model anthropic         # dry run (cost preview)
trimwire summarizer benchmark --model anthropic --yes   # real paid calls on your key
```

### `trimwire summarizer probe`

Slice-ceiling fact gate: plant distinctive facts across a synthetic OLD slice at your `slice_char_budget` (or `--bytes`), summarize it with your model, and report fact retention by position (start/mid/end). Exits non-zero below 90%. The installed-user counterpart of the `api_harm` example — validate **your** model at **your** budget before trusting large-budget summaries.

| Flag | Description |
|---|---|
| `--model TAG_OR_ID` | Model to probe: a configured provider id, `local`, or a local ollama tag. Omit to use your configured engine |
| `--bytes N` | Slice budget in bytes (default: the engine's effective `slice_char_budget`) |
| `--runs N` | Repeat N times and report the retention distribution (pass-rate / p50 / min). Model summaries are non-deterministic, so a single run near the 90% gate is unreliable. **PASS requires ALL N runs ≥90%.** For an API provider, cost scales with N |
| `--concurrency K` | Fire up to K of the `--runs` in PARALLEL (API only; the local engine is forced serial — one model). Speeds up a big sweep; mind provider rate limits |
| `--yes` | Confirm real, paid API call(s) when probing an API provider; without it you get a dry-run notice. Local models ignore it |

```sh
trimwire summarizer probe --model qwen3.5:4b --runs 3   # local, default budget, 3 runs
trimwire summarizer probe --model qwen3.5:4b --bytes 60000
trimwire summarizer probe --model openrouter --runs 10 --concurrency 5 --yes   # 10 paid calls, 5 at a time
```

Single-run rankings are unreliable for non-deterministic models — `--runs 5+` is the honest way to tell whether *your* model holds *your* budget. See [Model compatibility](MODEL-COMPATIBILITY.md).

---

## SHARE

Opt-in anonymous telemetry uploads to the community dashboard. Everything is a dry run until you `share enable` (or pass `--yes` once). Content-free — no prompts, code, or session text. See [Telemetry](TELEMETRY.md) for the exact payload.

### `trimwire share enable`

Opt in: persist consent so future `share stats` runs upload without `--yes`.

### `trimwire share disable`

Opt out: stop uploading. Reverses `share enable`.

### `trimwire share stats`

Upload an anonymous, content-free aggregate of your ledger to the community dashboard. Dry run until you `share enable` (or pass `--yes`).

| Flag | Description |
|---|---|
| `--yes` | Confirm the upload for this run (one-off; does not persist consent) |
| `--force` | Bypass the once-per-day throttle. Does not bypass consent |

```sh
trimwire share stats              # dry run (shows payload, sends nothing)
trimwire share stats --yes        # one-off upload (does not persist consent)
trimwire share enable             # persist consent — future runs upload automatically
trimwire share stats --force      # re-upload today (bypasses the daily throttle)
```

### `trimwire share benchmark`

Score your summarizer model and upload the anonymous, content-free per-model result to the community benchmark leaderboard. Dry run unless `--yes`. The model is scored on a bundled synthetic corpus — never your session content.

| Flag | Description |
|---|---|
| `--model TAG_OR_ID` | Model tag to score (repeatable). Omit to use your configured summarizer model |
| `--all-installed` | Score every model installed in ollama (disqualified ones are skipped) |
| `--yes` | Confirm the upload (without it, this is a dry run) |

```sh
trimwire share benchmark                          # dry run (prints the row)
trimwire share benchmark --model qwen3.5:4b --yes # score + upload
```

---

## MAINTENANCE

Commands to manage on-disk session transcripts and config.

### `trimwire sweep`

Clean Claude Code session transcripts on disk. Atomic (backed up before any write). Safe to run; active sessions abort cleanly and leave the file untouched.

**Subcommands:**

#### `trimwire sweep list`

List all session transcripts trimwire can find (no need to locate paths manually).

#### `trimwire sweep all`

Clean every discovered session. Active ones safely abort (file untouched).

| Flag | Description |
|---|---|
| `--dry-run` | Report what would change without writing anything |
| `--yes` | Skip the confirmation prompt (required in non-interactive use) |

```sh
trimwire sweep all --dry-run        # preview without writing
trimwire sweep all --yes            # run without prompting (scripting)
```

#### `trimwire sweep file`

Clean a single session file by path.

| Flag | Description |
|---|---|
| `PATH` | Path to the session `.jsonl` file (required positional) |
| `--dry-run` | Report what would change without writing anything |
| `--validate-only` | Only validate the file format; do not modify it. Conflicts with `--dry-run` |

```sh
trimwire sweep file ~/.claude/projects/foo/bar.jsonl
trimwire sweep file ~/.claude/projects/foo/bar.jsonl --dry-run
```

#### `trimwire sweep undo`

Restore a session from its latest backup (the `.bak.<timestamp>` file written by a previous sweep).

| Flag | Description |
|---|---|
| `PATH` | Path to the session `.jsonl` file to restore (required positional) |

```sh
trimwire sweep undo ~/.claude/projects/foo/bar.jsonl
```

### `trimwire config`

With no subcommand (or `edit`): open `~/.config/trimwire.toml` in `$EDITOR`.

**Subcommands:**

#### `trimwire config show`

Print the effective resolved config — after the profile + global/project/env merge.

| Flag | Description |
|---|---|
| `--json` | Emit JSON instead of TOML |

```sh
trimwire config show           # resolved TOML
trimwire config show --json    # resolved JSON (scripting)
```

#### `trimwire config edit`

Open `~/.config/trimwire.toml` in `$EDITOR`. Same as running `trimwire config` with no subcommand.

---

## SHELL

Commands for shell integration: statusline, tab-completion, and man pages.

### `trimwire statusline`

Manage trimwire's Claude Code statusline bar. The statusline shows live gateway + savings data inside the Claude Code terminal.

**Subcommands:**

| Subcommand | Description |
|---|---|
| `add` | Make trimwire your Claude Code statusline (errors if you already have one — use `wrap` instead) |
| `wrap` | Keep your existing statusline and add a trimwire row beneath it (reversible) |
| `remove` | Remove trimwire from the statusline (restores any wrapped original) |

```sh
trimwire statusline add       # set as statusline (fresh install)
trimwire statusline wrap      # add beneath an existing statusline
trimwire statusline remove    # remove and restore original
```

### `trimwire completions`

Print a shell completion script to stdout. Pipe or redirect it to your shell's standard location — one-time setup, then restart your shell.

| Argument | Description |
|---|---|
| `SHELL` | Target shell: `bash`, `zsh`, `fish`, `elvish`, `powershell` (required positional) |

```sh
# bash
trimwire completions bash > ~/.local/share/bash-completion/completions/trimwire

# zsh — simplest: eval inline (add to ~/.zshrc, then restart shell)
echo 'eval "$(trimwire completions zsh)"' >> ~/.zshrc
# or write to a file on $fpath:
trimwire completions zsh > ~/.zfunc/_trimwire
# (requires: fpath=(~/.zfunc $fpath) and autoload -Uz compinit in ~/.zshrc)

# fish — drop into the completions dir, fish picks it up automatically
trimwire completions fish > ~/.config/fish/completions/trimwire.fish

# powershell — append to your profile so it loads each session
trimwire completions powershell >> $PROFILE

# elvish — source inline from your rc
echo 'eval (trimwire completions elvish | slurp)' >> ~/.config/elvish/rc.elv
```

### `trimwire man`

Generate man pages. With no `--out`, prints the top-level page to stdout. With `--out`, writes one page per command (for packagers).

| Flag | Description |
|---|---|
| `--out DIR` | Directory to write the generated man pages into (for packagers) |

```sh
trimwire man | man -l -        # browse in man
trimwire man --out ./man/      # write all pages for packaging
```

---

## Environment variables

| Variable | Description |
|---|---|
| `ANTHROPIC_BASE_URL` | Points Claude Code at the trimwire gateway. Set automatically by `trimwire install`; unset it (or run `trimwire off`) to bypass the proxy |
| `TRIMWIRE_LOG` | Log verbosity for the gateway: `warn` (default), `info`, `debug`. Logs go to stderr. Example: `TRIMWIRE_LOG=info trimwire run` (the foreground gateway picks up the env). |
| `TRIMWIRE_AUDIT` | Opt-in metadata-only wire audit: append one JSONL line per request describing its *shape* (counts/flags + cache-prefix structure, never content) to `<file>`. Same as `--audit <file>`. See [CONFIGURATION.md](../CONFIGURATION.md). Off when unset |

---

*Configuration reference: `trimwire config show`. Setup diagnosis: `trimwire doctor`. Troubleshooting: [Troubleshooting](TROUBLESHOOTING.md).*
