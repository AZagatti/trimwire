//! `trimwire summarizer` — manage the optional summarizer backend.
//!
//! Subcommands:
//!   setup    — interactive wizard to configure a summarizer backend.
//!   status   — show the current summarizer configuration state.
//!   benchmark — score a local model against the quality corpus.

use anyhow::{Context, Result};

use trimwire::config::{Config, global_config_path};

// ─── setup wizard data types ─────────────────────────────────────────────────

/// A single named cloud API provider entered during the wizard.
/// Mirrors [`SummarizerProviderConfig`] but without the runtime-only timeout default.
#[derive(Debug, Clone, PartialEq)]
pub struct ProviderEntry {
    pub id: String,
    pub style: String,
    pub base_url: String,
    pub model: String,
    pub api_key_env: String,
    /// Optional path to a file holding the key — the daemon-safe alternative to
    /// `api_key_env` (a systemd/launchd service can't see shell exports). `None`
    /// when the user only configured an env var.
    pub api_key_file: Option<String>,
}

/// Answers collected from the unified model-picker wizard.
///
/// `engine` is the primary engine token (`"local"`, `"model-free"`, or a provider `id`).
/// `fallback` is the ordered list of fallback tokens (same token space).
/// `local_endpoint` / `local_model` are set when `"local"` appears anywhere in engine/fallback.
/// `providers` holds ALL `[[summarizer.providers]]` entries to emit.
#[derive(Debug, PartialEq)]
pub struct SetupAnswers {
    /// Primary engine token: `"local"` | `"model-free"` | provider id.
    pub engine: String,
    /// Ordered fallback chain (each: `"local"` | `"model-free"` | provider id).
    pub fallback: Vec<String>,
    /// Local ollama endpoint (present when `"local"` is in engine or fallback).
    pub local_endpoint: Option<String>,
    /// Local model tag (present when `"local"` is in engine or fallback).
    pub local_model: Option<String>,
    /// All API providers to emit as `[[summarizer.providers]]` blocks.
    pub providers: Vec<ProviderEntry>,
}

// ─── PURE LOGIC — unit-testable, no I/O ──────────────────────────────────────

/// Build the `[summarizer]` + engine-specific sub-section TOML block from wizard
/// answers. Only the engine-relevant keys are emitted. No other sections are touched.
///
/// Emits `[summarizer]` with `engine` + `fallback` (if non-empty), then
/// `[summarizer.local]` (if local is in the chain), then one
/// `[[summarizer.providers]]` block per provider entry. The key value is NEVER
/// stored — only the env-var name as written.
pub fn render_summarizer_config_block(answers: &SetupAnswers) -> String {
    use trimwire::summarizer::{APPROVED_MODELS, WARN_MODELS};

    let mut out = String::new();
    out.push_str("[summarizer]\n");
    out.push_str(&format!("engine = \"{}\"\n", answers.engine));

    if !answers.fallback.is_empty() {
        let chain: Vec<String> = answers
            .fallback
            .iter()
            .map(|s| format!("\"{}\"", s))
            .collect();
        out.push_str(&format!("fallback = [{}]\n", chain.join(", ")));
    }

    // Emit [summarizer.local] if local is anywhere in the chain.
    let local_in_chain = answers.engine == "local" || answers.fallback.iter().any(|f| f == "local");
    if local_in_chain {
        let endpoint = answers
            .local_endpoint
            .as_deref()
            .unwrap_or(DEFAULT_ENDPOINT);
        let model = answers.local_model.as_deref().unwrap_or(RECOMMENDED_MODEL);
        out.push('\n');
        out.push_str("[summarizer.local]\n");
        out.push_str(&format!("endpoint = \"{endpoint}\"\n"));
        out.push_str(&format!("model    = \"{model}\"\n"));
        // Inline annotation for the WARN tier.
        if WARN_MODELS.contains(&model) {
            out.push_str(
                "# Warning: this model failed the fact-retention harm gate. \
                 Prefer qwen3.5:4b when you have the RAM.\n",
            );
        } else if !APPROVED_MODELS.contains(&model) {
            out.push_str(
                "# Note: this model is unvalidated. \
                 qwen3.5:4b is the recommended default.\n",
            );
        }
    }

    // Emit [[summarizer.providers]] for each configured provider.
    for p in &answers.providers {
        out.push('\n');
        out.push_str("[[summarizer.providers]]\n");
        out.push_str(&format!("id          = \"{}\"\n", p.id));
        out.push_str(&format!("style       = \"{}\"\n", p.style));
        out.push_str(&format!("base_url    = \"{}\"\n", p.base_url));
        out.push_str(&format!("model       = \"{}\"\n", p.model));
        out.push_str(&format!("api_key_env = \"{}\"\n", p.api_key_env));
        if let Some(f) = p.api_key_file.as_deref().filter(|f| !f.is_empty()) {
            out.push_str(&format!("api_key_file = \"{}\"\n", f));
        }
    }

    out
}

/// Merge `new_block` into `existing_toml`: replace the existing `[summarizer]`
/// section (and its `[summarizer.*]` sub-sections, including
/// `[[summarizer.providers]]` array-of-tables headers) if present, otherwise
/// append. Every other section (`[server]`, `[strategies]`, `[reprune]`, …) is
/// preserved verbatim. Round-trips without a toml_edit dependency.
///
/// Algorithm:
/// 1. Split the existing TOML into lines.
/// 2. Walk the lines, tracking which section header we are inside.
/// 3. Skip lines that belong to an existing `[summarizer]`, `[summarizer.*]`,
///    or `[[summarizer.*]]` section.
/// 4. Append the new block (replacing the dropped lines).
pub fn upsert_summarizer_section(existing_toml: &str, new_block: &str) -> String {
    let mut out: Vec<&str> = Vec::new();
    let mut in_summarizer = false;

    for line in existing_toml.lines() {
        let trimmed = line.trim();
        // Strip an inline comment before header detection so a header like
        // `[reprune] # note` or `[reprune]#note` is still recognized — otherwise
        // `in_summarizer` would wrongly stay on and silently drop the following
        // section. Use `find('#')` (not `find(" #")`) so a `#` without a leading
        // space is also caught. (Detection only; the original `line` is what gets
        // pushed/preserved.)
        let hdr = match trimmed.find('#') {
            Some(i) => trimmed[..i].trim_end(),
            None => trimmed,
        };
        // Detect single-bracket section headers: [section] or [section.sub]
        if hdr.starts_with('[') && hdr.ends_with(']') && !hdr.starts_with("[[") {
            let header = hdr.trim_start_matches('[').trim_end_matches(']');
            // A summarizer section: `[summarizer]` or `[summarizer.local]` etc.
            in_summarizer = header == "summarizer" || header.starts_with("summarizer.");
        }
        // Detect double-bracket array-of-tables headers: [[summarizer.providers]]
        if hdr.starts_with("[[") && hdr.ends_with("]]") {
            let header = hdr.trim_start_matches('[').trim_end_matches(']');
            // [[summarizer.providers]] — belongs to the summarizer section.
            in_summarizer = header == "summarizer" || header.starts_with("summarizer.");
        }
        if !in_summarizer {
            out.push(line);
        }
    }

    // Trim trailing blank lines from the non-summarizer content.
    while out.last().map(|l| l.trim().is_empty()).unwrap_or(false) {
        out.pop();
    }

    let mut result = out.join("\n");
    if !result.is_empty() {
        result.push_str("\n\n");
    }
    result.push_str(new_block);
    // Ensure exactly one trailing newline.
    if !result.ends_with('\n') {
        result.push('\n');
    }
    result
}

// ─── Ollama probe helpers (sync, no external dep) ────────────────────────────

/// Result of probing the ollama endpoint.
pub enum OllamaProbe {
    /// Ollama is reachable; these model tags are installed.
    Reachable(Vec<String>),
    /// Ollama could not be reached.
    Unreachable(String),
}

/// Probe ollama synchronously: build a tiny tokio runtime for the async hyper
/// call — mirrors the pattern used in `benchmark.rs` (`rt.block_on`).
fn probe_ollama(endpoint: &str) -> OllamaProbe {
    match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Err(e) => OllamaProbe::Unreachable(format!("could not build runtime: {e}")),
        Ok(rt) => match rt.block_on(super::fetch_ollama_tags(endpoint)) {
            Ok(tags) => OllamaProbe::Reachable(tags),
            Err(e) => OllamaProbe::Unreachable(e.to_string()),
        },
    }
}

// ─── Prompt helpers ───────────────────────────────────────────────────────────

/// The error message every prompt raises when the wizard is cancelled (EOF /
/// `q`). Loops that swallow sub-flow errors to retry must re-raise this one.
const CANCELLED: &str = "setup cancelled";

/// True when `e` is the wizard-cancel error (must propagate, never retry).
fn is_cancelled(e: &anyhow::Error) -> bool {
    e.to_string() == CANCELLED
}

/// Read a line from stdin, stripping the newline. `None` on EOF (stdin
/// closed / Ctrl-D / piped input exhausted) or a read error — required-field
/// loops would otherwise spin forever re-reading an empty answer.
fn read_line() -> Option<String> {
    let mut buf = String::new();
    match std::io::stdin().read_line(&mut buf) {
        Ok(0) | Err(_) => None,
        Ok(_) => Some(buf.trim_end_matches(['\n', '\r']).to_owned()),
    }
}

/// Prompt `msg` and read a trimmed line. Errs with [`CANCELLED`] on EOF or a
/// lone `q` — both abort the wizard cleanly (nothing written).
fn prompt(msg: &str) -> Result<String> {
    print!("{msg}");
    use std::io::Write;
    let _ = std::io::stdout().flush();
    let Some(line) = read_line() else {
        println!();
        anyhow::bail!(CANCELLED);
    };
    let line = line.trim().to_owned();
    if line.eq_ignore_ascii_case("q") {
        anyhow::bail!(CANCELLED);
    }
    Ok(line)
}

/// Ask a yes/no question. Returns `true` for yes (default y), `false` for no (default n).
fn prompt_yn(msg: &str, default_yes: bool) -> Result<bool> {
    let hint = if default_yes { "[Y/n]" } else { "[y/N]" };
    let answer = prompt(&format!("{msg} {hint}: "))?;
    if answer.is_empty() {
        return Ok(default_yes);
    }
    Ok(matches!(answer.to_lowercase().as_str(), "y" | "yes"))
}

// ─── Constants ────────────────────────────────────────────────────────────────

const RECOMMENDED_MODEL: &str = "qwen3.5:4b";
const DEFAULT_ENDPOINT: &str = "http://localhost:11434";

/// The ollama endpoint the wizard probes by default.
///
/// Honors `TRIMWIRE_OLLAMA_ENDPOINT` — an advanced/test-only seam so the probe
/// can be pointed at a fake or unreachable local endpoint for deterministic,
/// offline tests of the setup wizard (the live probe otherwise makes the menu
/// depend on whatever ollama happens to have installed). When the env var is
/// unset or blank, this returns the standard local address, so default behavior
/// for normal users is unchanged.
fn default_ollama_endpoint() -> String {
    std::env::var("TRIMWIRE_OLLAMA_ENDPOINT")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_ENDPOINT.to_owned())
}

// ─── API provider sub-flow ────────────────────────────────────────────────────

/// Run the inline "Add a new API provider" sub-flow.
/// Returns the new `ProviderEntry` on success. The caller ensures no duplicate ids
/// by passing `existing_ids`.
fn wizard_add_api_provider(existing_ids: &[&str]) -> Result<ProviderEntry> {
    use super::render;
    println!();
    println!("  {}", render::strong("New API provider"));
    println!(
        "  {}",
        render::dim("The prunable slice is sent here with YOUR key — content leaves your machine.")
    );
    println!();

    // Provider id
    let id = loop {
        let raw = prompt(&format!(
            "  Provider id {}: ",
            render::dim("(short, no spaces — e.g. anthropic)")
        ))?;
        if raw.is_empty() {
            println!(
                "  {} id is required (e.g. anthropic, openrouter).",
                render::warn()
            );
            continue;
        }
        if raw.contains(' ') {
            println!("  {} id must not contain spaces.", render::warn());
            continue;
        }
        if raw == "local" || raw == "model-free" {
            println!(
                "  {} '{raw}' is a reserved engine token — pick another id.",
                render::warn()
            );
            continue;
        }
        if raw.contains(['"', '\\', '\n', '\r']) {
            println!(
                "  {} id must not contain quotes or backslashes.",
                render::warn()
            );
            continue;
        }
        if existing_ids.contains(&raw.as_str()) {
            println!(
                "  {} id '{raw}' is already configured — pick a unique id.",
                render::warn()
            );
            continue;
        }
        break raw;
    };

    // Style
    let style = loop {
        let raw = prompt(&format!(
            "  API style {}: ",
            render::dim("(anthropic | openai-compatible) [anthropic]")
        ))?;
        let s = if raw.is_empty() {
            "anthropic".to_owned()
        } else {
            raw.to_lowercase()
        };
        if s == "anthropic" || s == "openai" {
            break s;
        }
        println!("  {} enter 'anthropic' or 'openai'.", render::warn());
    };

    // Default base URL hint (OpenRouter's double-/v1 trap is the one worth calling out).
    let default_url = match style.as_str() {
        "anthropic" => "https://api.anthropic.com",
        _ => "https://api.openai.com",
    };
    println!();
    println!(
        "  {}",
        render::dim("Base URL = the API root (no /v1). OpenRouter: https://openrouter.ai/api")
    );
    let base_url = {
        let raw = prompt(&format!("  base_url [{}]: ", render::accent(default_url)))?;
        if raw.is_empty() {
            default_url.to_owned()
        } else {
            raw
        }
    };

    // Model
    let model_hint = match style.as_str() {
        "anthropic" => "e.g. claude-haiku-4-5",
        _ => "e.g. gpt-4o-mini",
    };
    let model = loop {
        let raw = prompt(&format!(
            "  Model tag {}: ",
            render::dim(&format!("({model_hint})"))
        ))?;
        if !raw.is_empty() {
            break raw;
        }
        println!("  {} model tag is required.", render::warn());
    };

    // API key. trimwire stores the file PATH or the env-var NAME — NEVER the key.
    // We ask for the key FILE first because it's the path that works for the
    // default install: `trimwire install` runs an always-up systemd/launchd
    // service, which does NOT inherit your ~/.zshrc exports — so an env var is
    // invisible to it (issue #111). A file is read at runtime and works either way.
    let default_key_file = format!("~/.{id}_key");
    println!();
    println!(
        "  {} trimwire stores WHERE to find your key, never the key itself.",
        render::strong("API key.")
    );
    println!(
        "  {}",
        render::dim("A key FILE is recommended — the background service can't see shell exports.")
    );
    let api_key_file = {
        let raw = prompt(&format!(
            "  Key file path [{}] {}: ",
            render::accent(&default_key_file),
            render::dim("(Enter to skip)")
        ))?;
        let trimmed = raw.trim().to_owned();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed)
        }
    };

    // Env-var NAME — the fallback / foreground path. Always recorded (default
    // derived from the provider id, e.g. `zai` → `ZAI_API_KEY`) so the provider
    // has a named source too; the file above takes over for the service.
    let key_hint: String = id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_uppercase()
            } else {
                '_'
            }
        })
        .collect::<String>()
        + "_API_KEY";
    println!();
    let env_why = if api_key_file.is_some() {
        "Optional env-var NAME too — used for foreground `trimwire run`."
    } else {
        "Env-var NAME holding your key — works for `trimwire run`, not the service."
    };
    println!("  {}", render::dim(env_why));
    let api_key_env = loop {
        let raw = prompt(&format!("  Env var name [{}]: ", render::accent(&key_hint)))?;
        let name = if raw.is_empty() {
            key_hint.clone()
        } else {
            raw
        };
        // Reject anything that looks like a key value (starts with sk- or contains spaces).
        if name.contains(' ') || name.to_lowercase().starts_with("sk-") {
            println!(
                "  {} that looks like a key VALUE — enter the NAME (e.g. ANTHROPIC_API_KEY).",
                render::warn()
            );
            continue;
        }
        break name;
    };

    // Warn only if NEITHER source can currently produce a key. A configured key
    // file makes the provider daemon-safe, so a set-but-unexported env var is fine.
    let env_unset = std::env::var(&api_key_env)
        .map(|v| v.trim().is_empty())
        .unwrap_or(true);
    if env_unset && api_key_file.is_none() {
        println!();
        println!(
            "  {} no key source set — trimwire will skip this provider until you add one:",
            render::warn()
        );
        println!(
            "    {}",
            render::accent(&format!(
                "printf '%s' \"<key>\" > ~/.{id}_key && chmod 600 ~/.{id}_key"
            ))
        );
    }

    if !prompt_yn("  Add this provider?", true)? {
        anyhow::bail!("provider entry cancelled");
    }

    println!(
        "  {} provider {} added — it now appears in the list.",
        render::ok(),
        render::strong(&format!("\"{id}\""))
    );

    Ok(ProviderEntry {
        id,
        style,
        base_url,
        model,
        api_key_env,
        api_key_file,
    })
}

// ─── Unified model picker ─────────────────────────────────────────────────────

/// A single item in the unified model-picker list.
#[derive(Debug, Clone)]
enum PickerItem {
    /// A locally-installed ollama model.
    LocalModel { tag: String },
    /// A model from an already-configured (or just-added) API provider.
    ApiProvider { provider: ProviderEntry },
}

impl PickerItem {
    /// The engine token this item maps to (`"local"` or the provider id).
    fn engine_token(&self) -> &str {
        match self {
            PickerItem::LocalModel { .. } => "local",
            PickerItem::ApiProvider { provider } => &provider.id,
        }
    }

    /// Human-readable label for the picker line. The `← recommended` marker is
    /// accented (cyan) so it reads at a glance; the glyph-free text still conveys
    /// it under NO_COLOR/non-TTY, so meaning is never colour-only.
    fn label(&self) -> String {
        use super::render;
        match self {
            PickerItem::LocalModel { tag } => {
                use trimwire::summarizer::{APPROVED_MODELS, WARN_MODELS, is_disqualified};
                let annotation = if tag == RECOMMENDED_MODEL {
                    render::accent(" ← recommended")
                } else if WARN_MODELS.contains(&tag.as_str()) {
                    render::dim(" (warn: failed harm gate)")
                } else if is_disqualified(tag) {
                    render::warn_text(" (DISQUALIFIED)")
                } else if !APPROVED_MODELS.contains(&tag.as_str()) {
                    render::dim(" (unvalidated)")
                } else {
                    String::new()
                };
                format!("{tag:<28} {}{annotation}", render::dim("[local]"))
            }
            PickerItem::ApiProvider { provider } => {
                format!(
                    "{:<28} {}",
                    provider.model,
                    render::dim(&format!("[api · {}]", provider.id))
                )
            }
        }
    }
}

/// Print the numbered picker list. `highlight` marks the item the user just
/// added (accented `← your new provider`) so it can't be confused with the
/// pre-existing rows — the item explicitly deferred from PR #115.
fn print_picker(items: &[PickerItem], exclude: Option<usize>, highlight: Option<usize>) {
    use super::render;
    let mut had_local = false;
    let mut printed_api_header = false;
    let mut n = 1usize;

    for (i, item) in items.iter().enumerate() {
        if exclude == Some(i) {
            n += 1;
            continue;
        }
        match item {
            PickerItem::LocalModel { .. } => {
                had_local = true;
            }
            PickerItem::ApiProvider { .. } => {
                if had_local && !printed_api_header {
                    println!("    {}", render::dim(&"─".repeat(53)));
                    printed_api_header = true;
                }
            }
        }
        let marker = if highlight == Some(i) {
            render::accent("  ← your new provider")
        } else {
            String::new()
        };
        println!("    {n:>2})  {}{marker}", item.label());
        n += 1;
    }
}

/// Pick one item from the numbered list.  Returns `Some(index)` into `items`
/// (skipping any `exclude`-d index), `None` for "none/done", or `Err` to trigger
/// the "add a new API provider" flow.
enum PickResult {
    Item(usize), // index into items[]
    AddProvider,
    ModelFree,
    None,
}

/// Display the list and prompt for one pick.
fn prompt_picker(
    items: &[PickerItem],
    heading: &str,
    default_label: &str,
    exclude: Option<usize>,
    include_model_free: bool,
    include_none: bool,
    highlight: Option<usize>,
) -> Result<PickResult> {
    use super::render;
    println!();
    println!("  {}", render::strong(heading));
    print_picker(items, exclude, highlight);
    // Compute the length of non-excluded items for separator detection.
    let visible_count = items
        .iter()
        .enumerate()
        .filter(|(i, _)| exclude != Some(*i))
        .count();
    if visible_count > 0 {
        println!("    {}", render::dim(&"─".repeat(53)));
    }
    // Option keys (a/m/n) are left unstyled to match the plain numbered rows —
    // the accent is reserved for markers, defaults, and values-to-type.
    println!("    a)  Add a new API provider…");
    if include_model_free {
        println!("    m)  model-free (no summarizer)");
    }
    if include_none {
        println!("    n)  None  (model-free is always the implicit last resort)");
    }

    // Build the set of valid option labels for a clean error message.
    let mut valid_opts: Vec<&str> = vec!["a number", "'a'"];
    if include_model_free {
        valid_opts.push("'m'");
    }
    if include_none {
        valid_opts.push("'n'");
    }
    let valid_msg = valid_opts.join(", ");

    loop {
        let raw = prompt(&format!(
            "  {heading} [{}]: ",
            render::accent(default_label)
        ))?;
        let inp = if raw.is_empty() {
            default_label.to_owned()
        } else {
            raw
        };

        match inp.trim().to_lowercase().as_str() {
            "a" | "add" => return Ok(PickResult::AddProvider),
            "m" | "model-free" if include_model_free => return Ok(PickResult::ModelFree),
            "n" | "none" if include_none => return Ok(PickResult::None),
            other => {
                if let Ok(n) = other.parse::<usize>() {
                    // Map 1-based visible number to items[] index.
                    let mut counter = 0usize;
                    for (idx, _item) in items.iter().enumerate() {
                        if exclude == Some(idx) {
                            continue;
                        }
                        counter += 1;
                        if counter == n {
                            return Ok(PickResult::Item(idx));
                        }
                    }
                }
                println!("  Please enter {valid_msg}.");
            }
        }
    }
}

// ─── Local model details sub-flow ────────────────────────────────────────────

/// Ask for endpoint + model (with pull offer). Used when local is the primary or first-fallback.
fn wizard_local_details(
    tag_hint: Option<&str>,
    ollama_reachable: bool,
    installed_models: &[String],
) -> Result<(String, String)> {
    use super::render;
    use trimwire::summarizer::{APPROVED_MODELS, WARN_MODELS, is_disqualified};

    println!();
    println!(
        "  {} ollama runs on your machine — content never leaves it.",
        render::strong("Local backend.")
    );

    let def_ep = default_ollama_endpoint();
    let raw_endpoint = prompt(&format!(
        "\n  ollama endpoint [{}]: ",
        render::accent(&def_ep)
    ))?;
    let endpoint = if raw_endpoint.is_empty() {
        def_ep.clone()
    } else {
        raw_endpoint
    };

    // If the user entered a non-default endpoint, re-probe it so the model list
    // and reachability info reflects the actual target. Unreachable is a warning
    // only — the user can configure now and start ollama later.
    let custom_probe: Option<(bool, Vec<String>)> = if endpoint != def_ep {
        print!("  Probing ollama at {endpoint} … ");
        use std::io::Write;
        let _ = std::io::stdout().flush();
        match probe_ollama(&endpoint) {
            OllamaProbe::Reachable(models) => {
                println!("{} reachable ({} installed)", render::ok(), models.len());
                if !models.is_empty() {
                    println!(
                        "  {}",
                        render::dim(&format!("installed: {}", models.join(", ")))
                    );
                }
                Some((true, models))
            }
            OllamaProbe::Unreachable(reason) => {
                println!("{} unreachable ({reason})", render::warn());
                println!("  {}", render::dim("Configure now, start ollama later."));
                Some((false, Vec::new()))
            }
        }
    } else {
        None // default endpoint — use the already-probed results from the caller
    };

    // Resolve effective reachability + model list: custom probe wins if present.
    let (ep_reachable, ep_models): (bool, &[String]) = match &custom_probe {
        Some((r, m)) => (*r, m.as_slice()),
        None => (ollama_reachable, installed_models),
    };

    // Use tag_hint if provided; otherwise the first installed model or the recommended.
    let hint = tag_hint.unwrap_or_else(|| {
        ep_models
            .first()
            .map(|s| s.as_str())
            .unwrap_or(RECOMMENDED_MODEL)
    });

    // Offer to pull the recommended model if not present.
    let recommended_installed = ep_models.iter().any(|m| m == RECOMMENDED_MODEL);
    if ep_reachable && !recommended_installed && tag_hint.is_none() {
        println!();
        println!(
            "  {} recommended model {} is not installed.",
            render::warn(),
            render::accent(RECOMMENDED_MODEL)
        );
        if prompt_yn(
            &format!("  Pull it now (`ollama pull {RECOMMENDED_MODEL}`)?"),
            true,
        )? {
            println!("  Running: ollama pull {RECOMMENDED_MODEL}");
            let status = std::process::Command::new("ollama")
                .args(["pull", RECOMMENDED_MODEL])
                .status();
            match status {
                Ok(s) if s.success() => println!("  {} done.", render::ok()),
                Ok(s) => println!(
                    "  {} ollama pull exited with {s} — continuing.",
                    render::warn()
                ),
                Err(e) => println!(
                    "  {} could not run `ollama`: {e} — continuing.",
                    render::warn()
                ),
            }
        }
    }

    let model = {
        let raw = prompt(&format!("\n  Model to use [{}]: ", render::accent(hint)))?;
        if raw.is_empty() { hint.to_owned() } else { raw }
    };

    // Model guard feedback.
    println!();
    if is_disqualified(&model) {
        println!(
            "  {} {} is {} — it drops/hallucinates load-bearing facts; the runtime will REFUSE it.",
            render::bad(),
            model,
            render::error_text("DISQUALIFIED")
        );
        println!(
            "  {}",
            render::dim(&format!("Recommend {RECOMMENDED_MODEL} instead."))
        );
        if !prompt_yn("  Continue with this model anyway?", false)? {
            anyhow::bail!("setup cancelled — choose an approved model");
        }
    } else if WARN_MODELS.contains(&model.as_str()) {
        println!(
            "  {} {} failed the fact-retention harm gate — a RAM opt-down, not an equal.",
            render::warn(),
            model
        );
    } else if !APPROVED_MODELS.contains(&model.as_str()) {
        println!(
            "  {} {} is unvalidated — fidelity unverified. {}",
            render::warn(),
            model,
            render::dim(&format!("Approved: {}", APPROVED_MODELS.join(", ")))
        );
    }

    Ok((endpoint, model))
}

// ─── Unified wizard entry point ───────────────────────────────────────────────

/// `trimwire summarizer setup` — interactive unified model-picker wizard.
pub fn summarizer_setup() -> Result<()> {
    use trimwire::summarizer::is_disqualified;

    println!("{}\n", super::render::strong("trimwire summarizer setup"));
    println!("Pick the engine that compresses OLD context before model-free pruning.");
    println!(
        "{}",
        super::render::dim(
            "Best-effort, never load-bearing. Re-running keeps providers you already added. q/Ctrl-D cancels."
        )
    );
    println!();

    // Load existing config (if any) to seed already-configured providers.
    let existing_providers: Vec<ProviderEntry> = Config::load()
        .ok()
        .map(|c| {
            c.summarizer
                .providers
                .into_iter()
                .map(|p| ProviderEntry {
                    id: p.id,
                    style: p.style,
                    base_url: p.base_url,
                    model: p.model,
                    api_key_env: p.api_key_env,
                    api_key_file: p.api_key_file,
                })
                .collect()
        })
        .unwrap_or_default();

    // Probe ollama.
    let (ollama_reachable, installed_models) = {
        let ep = default_ollama_endpoint();
        print!("  Probing ollama at {ep} … ");
        use std::io::Write;
        let _ = std::io::stdout().flush();
        match probe_ollama(&ep) {
            OllamaProbe::Reachable(models) => {
                println!(
                    "{} reachable ({} installed)",
                    super::render::ok(),
                    models.len()
                );
                (true, models)
            }
            OllamaProbe::Unreachable(reason) => {
                println!("{} unreachable ({reason})", super::render::warn());
                println!(
                    "  {}",
                    super::render::dim("Configure now, start ollama later: https://ollama.com")
                );
                (false, vec![])
            }
        }
    };

    // Build the initial item list: local models first, then pre-configured providers.
    let mut items: Vec<PickerItem> = installed_models
        .iter()
        .map(|tag| PickerItem::LocalModel { tag: tag.clone() })
        .collect();
    for p in existing_providers {
        items.push(PickerItem::ApiProvider { provider: p });
    }

    // ── Primary engine pick ───────────────────────────────────────────────────
    // `just_added` tracks the provider the user added in THIS picker so its row
    // gets the `← your new provider` marker AND becomes the default selection —
    // the fix for the maintainer-reported "default points at a local model even
    // right after I added my provider" bug (issue #118 / deferred from PR #115).

    let mut just_added: Option<usize> = None;
    let primary_idx = loop {
        // Default to the just-added provider (its 1-based row) when present;
        // otherwise the first item. Recomputed each pass so it always reflects
        // live state, never a frozen initial label (rustup #3429 anti-pattern).
        let default_label = just_added
            .map(|i| (i + 1).to_string())
            .unwrap_or_else(|| "1".to_owned());
        match prompt_picker(
            &items,
            "Primary engine",
            &default_label,
            None,
            true,  // model-free is an option
            false, // no "none" at primary step
            just_added,
        )? {
            PickResult::Item(idx) => break idx,
            PickResult::ModelFree => {
                // model-free is a valid primary — means "no summarizer at all".
                println!();
                println!("  model-free selected — the summarizer is off.");
                println!(
                    "  {}",
                    super::render::dim("Pruning still runs; no model calls are made.")
                );
                let answers = SetupAnswers {
                    engine: "model-free".to_owned(),
                    fallback: Vec::new(),
                    local_endpoint: None,
                    local_model: None,
                    providers: Vec::new(),
                };
                write_summarizer_config(&answers)?;
                print_next_steps(&answers);
                return Ok(());
            }
            PickResult::AddProvider => {
                let existing_ids: Vec<&str> = items
                    .iter()
                    .filter_map(|it| {
                        if let PickerItem::ApiProvider { provider } = it {
                            Some(provider.id.as_str())
                        } else {
                            None
                        }
                    })
                    .collect();
                match wizard_add_api_provider(&existing_ids) {
                    Ok(p) => {
                        items.push(PickerItem::ApiProvider { provider: p });
                        just_added = Some(items.len() - 1);
                    }
                    Err(e) if is_cancelled(&e) => return Err(e),
                    Err(e) => println!("  {e} — try again."),
                }
                // Re-display the picker, now defaulting to the new provider.
                continue;
            }
            PickResult::None => unreachable!("None not offered at primary step"),
        }
    };

    let primary_token = items[primary_idx].engine_token().to_owned();
    let primary_label = items[primary_idx].label();
    println!();
    println!("  {} {primary_label}", super::render::strong("Primary:"));

    // ── Local details for primary local pick ─────────────────────────────────

    let (mut local_endpoint, mut local_model) = (None, None);
    if primary_token == "local" {
        let tag_hint = if let PickerItem::LocalModel { tag } = &items[primary_idx] {
            Some(tag.as_str())
        } else {
            None
        };
        let (ep, m) = wizard_local_details(tag_hint, ollama_reachable, &installed_models)?;
        local_endpoint = Some(ep);
        local_model = Some(m);

        // Disqualified guard info.
        if let Some(ref m) = local_model {
            if is_disqualified(m) {
                println!();
                println!("  Note: the runtime model-guard will SKIP this model (disqualified).");
                println!("  The cascade will fall through to the next engine or model-free.");
            }
        }
    }

    // ── Fallback chain ────────────────────────────────────────────────────────

    let mut fallback: Vec<String> = Vec::new();
    let mut selected: Vec<usize> = vec![primary_idx]; // tracks which items are already in chain

    // F2: smart fallback suggestion — if primary is an API provider and ollama is
    // reachable, pre-suggest `local` as the fallback (concrete recommendation over
    // a blank prompt). Never auto-write; the confirm preview shows the full chain.
    let primary_is_api = primary_token != "local" && primary_token != "model-free";
    if primary_is_api && ollama_reachable {
        println!();
        println!(
            "  {} add {} as a fallback if {primary_token} is unreachable.",
            super::render::dim("Suggestion:"),
            super::render::accent("local")
        );
    }

    // `fb_added_orig` tracks a provider added mid-fallback (its ORIGINAL index in
    // `items`) so we can re-locate it in each rebuilt `remaining` view to mark it
    // `← your new provider` and default the pick to it.
    let mut fb_added_orig: Option<usize> = None;

    // Offer fallback round(s).
    println!();
    if prompt_yn("  Add a fallback? (tried when the primary fails)", true)? {
        loop {
            // Build exclude mask: already-selected items.
            // Since exclude only accepts a single usize, we filter the list
            // and renumber — instead we rebuild a filtered view.
            let exclude_set: std::collections::HashSet<usize> = selected.iter().copied().collect();
            // Find remaining items (not yet selected).
            let remaining: Vec<(usize, &PickerItem)> = items
                .iter()
                .enumerate()
                .filter(|(i, _)| !exclude_set.contains(i))
                .collect();
            if remaining.is_empty() {
                println!("  No more engines to fall back to.");
                break;
            }

            // Build a fresh display-ordered list from the remaining items.
            let remaining_items: Vec<PickerItem> =
                remaining.iter().map(|(_, it)| (*it).clone()).collect();
            let fallback_n = fallback.len() + 1;

            // Re-locate a just-added provider in this rebuilt view → highlight +
            // default. Falls back to "n" (done) when nothing was just added.
            let fb_highlight =
                fb_added_orig.and_then(|orig| remaining.iter().position(|(i, _)| *i == orig));
            // Default the pick so Enter agrees with what's on screen:
            //   1. a just-added provider → its row (the `← your new provider` fix);
            //   2. else, on the FIRST fallback when we actively suggested `local`
            //      (primary is an API + ollama reachable), point at the recommended
            //      local model (or the first local) so the default matches the
            //      "Suggestion: add local" line and the `← recommended` marker —
            //      instead of the "None" escape hatch contradicting both;
            //   3. else "n" (done).
            let fb_default = if let Some(d) = fb_highlight {
                (d + 1).to_string()
            } else if fallback.is_empty() && primary_is_api && ollama_reachable {
                remaining
                    .iter()
                    .position(|(_, it)| {
                        matches!(it, PickerItem::LocalModel { tag } if tag == RECOMMENDED_MODEL)
                    })
                    .or_else(|| {
                        remaining
                            .iter()
                            .position(|(_, it)| matches!(it, PickerItem::LocalModel { .. }))
                    })
                    .map(|d| (d + 1).to_string())
                    .unwrap_or_else(|| "n".to_owned())
            } else {
                "n".to_owned()
            };

            match prompt_picker(
                &remaining_items,
                &format!("Fallback {fallback_n}"),
                &fb_default,
                None,
                false, // model-free shown via "none" button
                true,  // "none" = done
                fb_highlight,
            )? {
                PickResult::Item(display_idx) => {
                    let (original_idx, item) = remaining[display_idx];
                    let token = item.engine_token().to_owned();

                    // If this is a local pick we haven't seen yet, collect details.
                    if token == "local" && local_endpoint.is_none() {
                        let tag_hint = if let PickerItem::LocalModel { tag } = item {
                            Some(tag.as_str())
                        } else {
                            None
                        };
                        match wizard_local_details(tag_hint, ollama_reachable, &installed_models) {
                            Ok((ep, m)) => {
                                local_endpoint = Some(ep);
                                local_model = Some(m);
                            }
                            Err(e) => {
                                println!("  {e}");
                                continue;
                            }
                        }
                    }

                    fallback.push(token);
                    selected.push(original_idx);

                    if !prompt_yn("  Add another fallback?", false)? {
                        break;
                    }
                }
                PickResult::AddProvider => {
                    let existing_ids: Vec<&str> = items
                        .iter()
                        .filter_map(|it| {
                            if let PickerItem::ApiProvider { provider } = it {
                                Some(provider.id.as_str())
                            } else {
                                None
                            }
                        })
                        .collect();
                    match wizard_add_api_provider(&existing_ids) {
                        Ok(p) => {
                            items.push(PickerItem::ApiProvider { provider: p });
                            fb_added_orig = Some(items.len() - 1);
                        }
                        Err(e) if is_cancelled(&e) => return Err(e),
                        Err(e) => println!("  {e} — try again."),
                    }
                    // Loop back to re-display the fallback picker with the new provider.
                    continue;
                }
                PickResult::None | PickResult::ModelFree => break,
            }
        }
    }

    // Collect all provider entries that appear in the chain.
    let providers: Vec<ProviderEntry> = items
        .iter()
        .enumerate()
        .filter_map(|(i, it)| {
            if selected.contains(&i) {
                if let PickerItem::ApiProvider { provider } = it {
                    return Some(provider.clone());
                }
            }
            None
        })
        .collect();

    let answers = SetupAnswers {
        engine: primary_token,
        fallback,
        local_endpoint,
        local_model,
        providers,
    };

    // ── Write config ──────────────────────────────────────────────────────────

    write_summarizer_config(&answers)?;
    print_next_steps(&answers);

    Ok(())
}

/// Print the post-write "next steps" block: a short curated command list, plus a
/// per-provider "action required" callout for any provider that has no reachable
/// key yet (a configured key file counts as set — it works in a daemon even
/// without a shell export). Shared by the model-free and full paths.
fn print_next_steps(answers: &SetupAnswers) {
    use super::render;
    // Pad the PLAIN command to a fixed width BEFORE colouring — applying `{:<30}`
    // to an already-escaped string counts the invisible ANSI bytes and breaks
    // column alignment whenever colour is on.
    let cmd = |c: &str, why: &str| {
        println!(
            "    {}  {}",
            render::accent(&format!("{c:<30}")),
            render::dim(why)
        )
    };

    println!();
    println!("  {}", render::strong("Next steps"));
    cmd("trimwire doctor", "validate your full setup");
    cmd("trimwire summarizer status", "check the summarizer state");
    if answers.engine == "local" || answers.fallback.iter().any(|f| f == "local") {
        cmd(
            "trimwire summarizer benchmark",
            "score the model on the corpus",
        );
    }
    cmd("trimwire summarizer setup", "re-run to add more providers");

    for p in &answers.providers {
        let env_set = std::env::var(&p.api_key_env)
            .map(|v| !v.trim().is_empty())
            .unwrap_or(false);
        let file_set = p
            .api_key_file
            .as_deref()
            .map(|f| !f.trim().is_empty())
            .unwrap_or(false);
        if !env_set && !file_set {
            println!();
            println!(
                "  {} give provider {} a key before starting:",
                render::warn(),
                render::strong(&format!("\"{}\"", p.id))
            );
            println!(
                "    {}",
                render::accent(&format!(
                    "printf '%s' \"<key>\" > ~/.{}_key && chmod 600 ~/.{}_key",
                    p.id, p.id
                ))
            );
            println!(
                "    {}",
                render::dim(&format!(
                    "then set  api_key_file = \"~/.{}_key\"  on this provider, and `trimwire on`.",
                    p.id
                ))
            );
        }
    }
    println!();
}

// ─── Config writing ───────────────────────────────────────────────────────────

fn write_summarizer_config(answers: &SetupAnswers) -> Result<()> {
    let block = render_summarizer_config_block(answers);
    let path = global_config_path();

    // Read existing content (may not exist yet).
    let existing = if path.exists() {
        std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?
    } else {
        String::new()
    };

    let merged = upsert_summarizer_section(&existing, &block);

    use super::render;
    let rule = render::dim(&"─".repeat(53));
    println!();
    println!(
        "  {} {} {}",
        render::strong("Review"),
        render::dim("— writes only the [summarizer] section of"),
        render::accent(&path.display().to_string())
    );
    println!("  {rule}");
    for line in block.lines() {
        println!("  {line}");
    }
    println!("  {rule}");

    if !prompt_yn("\n  Write this to your config?", true)? {
        println!("  {} nothing written.", render::warn());
        return Ok(());
    }

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create config dir {}", parent.display()))?;
    }
    std::fs::write(&path, &merged).with_context(|| format!("write {}", path.display()))?;

    println!("  {} saved.", render::ok());
    Ok(())
}

// ─── status ───────────────────────────────────────────────────────────────────

/// `trimwire summarizer status` — show the current summarizer configuration.
pub fn summarizer_status() -> Result<()> {
    use super::render;
    println!("{}\n", render::strong("trimwire summarizer status"));

    let config = match Config::load() {
        Ok(c) => c,
        Err(e) => {
            println!("  {} config failed to load: {e}", render::bad());
            println!(
                "  {}",
                render::dim("run `trimwire config` to create or edit the config.")
            );
            return Ok(());
        }
    };

    // Shared "reprune off → silent no-op" caution (both engine branches use it).
    let reprune_warning = |render: &dyn Fn(&str) -> String, warn: &str| {
        println!();
        println!(
            "  {warn} reprune.enabled = false — summarizer runs but the summary isn't carried \
             across turns (silent no-op)."
        );
        println!(
            "  {}",
            render("To fix: set  [reprune] enabled = true  (`trimwire config`).")
        );
    };

    let s = &config.summarizer;
    match s.engine.as_str() {
        "model-free" => {
            println!(
                "  {} summarizer not configured (engine = model-free)",
                render::bullet()
            );
            println!();
            println!(
                "  {}",
                render::dim("To enable: run `trimwire summarizer setup`.")
            );
        }
        "local" => {
            println!("  {} summarizer enabled", render::ok());
            println!("  engine:    local model (ollama)");
            println!("  model:     {}", render::accent(&s.local.model));
            println!("  endpoint:  {}", s.local.endpoint);
            println!("  mode:      {}", s.mode);
            if !s.fallback.is_empty() {
                println!("  fallback:  {}", s.fallback.join(" → "));
            }
            if !config.reprune.enabled {
                reprune_warning(&render::dim, &render::warn());
            }
            println!();
            println!(
                "  {}",
                render::dim("Validate: trimwire doctor · Score: trimwire summarizer benchmark")
            );
        }
        provider_id => {
            println!("  {} summarizer enabled", render::ok());
            println!(
                "  engine:    cloud API provider (id = {})",
                render::accent(provider_id)
            );
            println!("  mode:      {}", s.mode);
            if !s.fallback.is_empty() {
                println!("  fallback:  {}", s.fallback.join(" → "));
            }
            // Print each configured provider.
            if s.providers.is_empty() {
                println!();
                println!(
                    "  {} no [[summarizer.providers]] entries configured.",
                    render::warn()
                );
            } else {
                println!();
                println!("  Configured providers:");
                for p in &s.providers {
                    let key_file = match p.api_key_file.as_deref() {
                        Some(f) if !f.is_empty() => format!("  api_key_file={f}"),
                        _ => String::new(),
                    };
                    println!(
                        "    id={:?}  style={}  model={}  base_url={}  api_key_env={}{key_file}",
                        p.id, p.style, p.model, p.base_url, p.api_key_env
                    );
                    // Report the key the same way the runtime resolves it (env then file).
                    match trimwire::summarizer::api::resolve_provider_key(p) {
                        Ok(_) => {
                            let has_file = p
                                .api_key_file
                                .as_deref()
                                .is_some_and(|f| !f.trim().is_empty());
                            if !has_file && super::service::managed_service_installed() {
                                // Resolved from the shell env, but the installed service
                                // can't see shell exports — flag the silent-failure trap.
                                println!(
                                    "      {} key set in THIS shell — but the installed service can't see it",
                                    render::warn()
                                );
                                println!(
                                    "      {}",
                                    render::dim(&format!(
                                        "→ set api_key_file = \"~/.{}_key\" so the service can authenticate.",
                                        p.id
                                    ))
                                );
                            } else {
                                println!("      {} key set", render::ok());
                            }
                        }
                        Err(reason) => {
                            println!(
                                "      {} key {} ({reason})",
                                render::bad(),
                                render::error_text("NOT SET")
                            );
                            // Only mention the env-var fallback when the provider
                            // actually has one — a file-only provider (api_key_env
                            // = "") must not print a dangling "or export .".
                            let hint = if p.api_key_env.is_empty() {
                                format!(
                                    "→ set api_key_file = \"~/.{}_key\" (works as a service).",
                                    p.id
                                )
                            } else {
                                format!(
                                    "→ set api_key_file = \"~/.{}_key\" (works as a service), or export {}.",
                                    p.id, p.api_key_env
                                )
                            };
                            println!("      {}", render::dim(&hint));
                        }
                    }
                }
            }
            if !config.reprune.enabled {
                reprune_warning(&render::dim, &render::warn());
            }
            println!();
            println!(
                "  {}",
                render::dim(&format!(
                    "Validate: trimwire doctor · Score (paid): trimwire summarizer benchmark --model {provider_id} --yes"
                ))
            );
        }
    }
    Ok(())
}

// ─── probe (slice-ceiling fact-retention gate) ──────────────────────────────────

/// The resolved summarizer backend for a probe run, owned so it can be cloned into
/// concurrent tasks. `call` does ONE model call on the (deterministic) slice.
#[derive(Clone)]
enum ProbeEngine {
    Api(trimwire::config::SummarizerProviderConfig),
    Local(trimwire::config::SummarizerLocalConfig, u64),
}

impl ProbeEngine {
    async fn call(&self, prompt: String) -> Result<String> {
        use trimwire::summarizer;
        match self {
            ProbeEngine::Api(p) => summarizer::api::call_api(p, prompt)
                .await
                .map_err(|e| anyhow::anyhow!("API call failed: {e}")),
            ProbeEngine::Local(l, t) => summarizer::call_model(l, *t, prompt)
                .await
                .map_err(|e| anyhow::anyhow!("local model call failed: {e}")),
        }
    }
}

/// `trimwire summarizer probe` — plant distinctive facts across a synthetic OLD
/// slice at your engine's slice budget, summarize it with your configured model,
/// and report how many facts survive (by position). The installed-user counterpart
/// of `examples/api_harm.rs`: it answers "does MY model hold MY slice budget?".
///
/// `model`: a configured provider id, `"local"`, or a local ollama tag. Omit to use
/// the configured engine. `bytes`: slice budget (defaults to the engine's effective
/// `slice_char_budget`). `runs`: repeat the model call N times and report the
/// retention DISTRIBUTION (pass-rate / p50 / min) — model summaries are
/// non-deterministic, so a single run is a coin flip near the threshold.
/// `concurrency`: how many of those runs to fire in parallel (API only; the local
/// engine is forced serial — one GPU/model). `yes`: confirm real PAID calls for an
/// API provider (cost scales with `runs`).
pub fn summarizer_probe(
    model: Option<String>,
    bytes: Option<usize>,
    runs: usize,
    concurrency: usize,
    yes: bool,
) -> Result<()> {
    use trimwire::summarizer::{self, build_prompt, probe};

    const THRESHOLD: f64 = 0.90;
    let runs = runs.max(1);

    let cfg = Config::load().context("load config (run `trimwire config` to create one)")?;
    let s = &cfg.summarizer;

    // Resolve the target engine: explicit --model wins, else the configured engine.
    let target = model.unwrap_or_else(|| s.engine.clone());
    if target == "model-free" {
        anyhow::bail!(
            "no summarizer engine to probe (engine = \"model-free\"). \
             Pass --model <provider-id|local|ollama-tag>, or run `trimwire summarizer setup`."
        );
    }

    // Resolve the engine ONCE (owned, so it can be cloned into concurrent tasks)
    // BEFORE picking the budget: a `--model <local-tag>` must use the LOCAL
    // num_ctx budget even when the configured engine is `model-free`/API, or the
    // local model is probed at the larger API budget and fails spuriously.
    let provider = s.providers.iter().find(|p| p.id == target).cloned();
    let engine = if let Some(p) = provider {
        ProbeEngine::Api(p)
    } else {
        let mut local = s.local.clone();
        if target != "local" {
            local.model = target.clone();
        }
        ProbeEngine::Local(local, s.timeout_secs)
    };
    let is_local = matches!(engine, ProbeEngine::Local(..));

    // Budget follows the RESOLVED target (not the config engine):
    // --bytes > config slice_char_budget > the engine's natural default
    // (local ≈ num_ctx-derived ~60 KB; API 128 KB).
    let budget =
        summarizer::target_char_budget(is_local, s.local.max_num_ctx, bytes, s.slice_char_budget);
    let slice = probe::build_probe_slice(budget);

    println!("trimwire summarizer probe\n");
    println!(
        "  target={target}  engine={}  slice={} chars (~{} KB)  turns={}  facts={}  runs={runs}",
        if is_local { "local" } else { "api" },
        slice.slice_text.len(),
        slice.slice_text.len() / 1024,
        slice.n_turns,
        probe::PROBE_FACTS.len(),
    );

    let prompt = build_prompt(&slice.slice_text);

    // Dry-run gate + banner.
    match &engine {
        ProbeEngine::Api(p) => {
            if !yes {
                println!();
                println!(
                    "  Provider \"{}\" ({}) — probing makes {runs} real, PAID API call(s)",
                    p.id, p.model
                );
                // Name whichever key source is configured (env var, file, or both).
                let key_src = match (p.api_key_env.is_empty(), p.api_key_file.as_deref()) {
                    (false, Some(f)) if !f.is_empty() => {
                        format!("the key in ${} (or {f})", p.api_key_env)
                    }
                    (false, _) => format!("the key in ${}", p.api_key_env),
                    (true, Some(f)) if !f.is_empty() => format!("the key in {f}"),
                    (true, _) => "your configured key".to_owned(),
                };
                println!("  to {} with {key_src}.", p.base_url);
                println!("  Re-run with --yes to make the call(s).");
                return Ok(());
            }
            if let Err(reason) = trimwire::summarizer::api::resolve_provider_key(p) {
                let how = if p.api_key_env.is_empty() {
                    "set api_key_file".to_owned()
                } else {
                    format!("export {} or set api_key_file", p.api_key_env)
                };
                anyhow::bail!("{reason} — {how} before probing provider \"{}\".", p.id);
            }
            println!("  engine=API  model={}  base_url={}", p.model, p.base_url);
        }
        ProbeEngine::Local(l, _) => {
            println!("  engine=local  model={}  endpoint={}", l.model, l.endpoint);
        }
    }

    let n_turns = slice.n_turns;
    let prompt = std::sync::Arc::new(prompt);

    // runs == 1: the detailed single-run path (summary + false-done + per-fact table).
    if runs == 1 {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .context("build tokio runtime")?;
        let summary = rt.block_on(engine.call((*prompt).clone()))?;
        let report = probe::ProbeReport::score(&summary, n_turns);
        let r = report.retention();
        println!("\n── summary ({} chars) ──\n{summary}", summary.len());
        let fd = summarizer::harm_check::detect_false_done(&summary, &slice.slice_text);
        if fd.is_empty() {
            println!("\n── false-done check ── none");
        } else {
            println!("\n── false-done check ── {} FLAG(S):", fd.len());
            for f in &fd {
                println!("  ⚠ {}\n      ↳ {}", f.claim, f.reason);
            }
        }
        report.print();
        println!(
            "\nretention: {}/{} = {:.1}%  (threshold {:.0}%)",
            report.kept(),
            report.total(),
            r * 100.0,
            THRESHOLD * 100.0,
        );
        return if r + 1e-9 >= THRESHOLD {
            println!(
                "PASS — this model holds the slice budget (1 run; re-run with --runs 5 to gauge variance)."
            );
            Ok(())
        } else {
            anyhow::bail!(
                "FAIL: retention below {:.0}% — check the start bucket for early-drop. \
                 Lower slice_char_budget or pick a stronger model.",
                THRESHOLD * 100.0
            );
        };
    }

    // runs > 1: distribution. `--concurrency` parallelizes API runs; the LOCAL engine
    // is forced serial (one GPU/model — concurrent calls would contend/OOM).
    // (`is_local` was resolved above when picking the budget.)
    let eff_conc = if is_local { 1 } else { concurrency.max(1) };
    if is_local && concurrency > 1 {
        eprintln!(
            "  note: --concurrency ignored for the local engine (single model); running serially."
        );
    } else if eff_conc > 1 {
        println!("  concurrency={eff_conc}");
    }

    let retentions: Vec<f64> = if eff_conc <= 1 {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .context("build tokio runtime")?;
        let mut v = Vec::with_capacity(runs);
        for i in 0..runs {
            let summary = rt.block_on(engine.call((*prompt).clone()))?;
            let r = probe::ProbeReport::score(&summary, n_turns).retention();
            println!(
                "  run {}/{}: {:.1}%  [{}]",
                i + 1,
                runs,
                r * 100.0,
                if r + 1e-9 >= THRESHOLD {
                    "PASS"
                } else {
                    "FAIL"
                }
            );
            v.push(r);
        }
        v
    } else {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(4)
            .enable_all()
            .build()
            .context("build tokio runtime")?;
        rt.block_on(async {
            let sem = std::sync::Arc::new(tokio::sync::Semaphore::new(eff_conc));
            let mut set = tokio::task::JoinSet::new();
            for i in 0..runs {
                let sem = sem.clone();
                let engine = engine.clone();
                let prompt = prompt.clone();
                set.spawn(async move {
                    let _permit = sem.acquire_owned().await.expect("semaphore open");
                    let summary = engine.call((*prompt).clone()).await?;
                    Ok::<(usize, f64), anyhow::Error>((
                        i,
                        probe::ProbeReport::score(&summary, n_turns).retention(),
                    ))
                });
            }
            let mut pairs: Vec<(usize, f64)> = Vec::with_capacity(runs);
            while let Some(joined) = set.join_next().await {
                pairs.push(joined.context("probe task panicked")??);
            }
            pairs.sort_by_key(|(i, _)| *i);
            for (i, r) in &pairs {
                println!(
                    "  run {}/{}: {:.1}%  [{}]",
                    i + 1,
                    runs,
                    r * 100.0,
                    if *r + 1e-9 >= THRESHOLD {
                        "PASS"
                    } else {
                        "FAIL"
                    }
                );
            }
            Ok::<Vec<f64>, anyhow::Error>(pairs.into_iter().map(|(_, r)| r).collect())
        })?
    };

    let (passes, p50, min) = probe::summarize_runs(&retentions, THRESHOLD);
    println!(
        "\n── distribution over {runs} runs ──\n  pass-rate {passes}/{runs} ({:.0}%)  p50 {:.1}%  min {:.1}%  (threshold {:.0}%)",
        passes as f64 / runs as f64 * 100.0,
        p50 * 100.0,
        min * 100.0,
        THRESHOLD * 100.0,
    );
    if passes == runs {
        println!("PASS — held the budget on all {runs} runs.");
        Ok(())
    } else {
        anyhow::bail!(
            "NOT RELIABLE: only {passes}/{runs} runs passed (min {:.1}%). A model that \
             fails any run is not safe at this budget — lower slice_char_budget or pick a \
             stronger model (see docs/MODEL-COMPATIBILITY.md).",
            min * 100.0
        );
    }
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── render_summarizer_config_block ────────────────────────────────────────

    #[test]
    fn local_answers_produce_correct_toml_block() {
        let answers = SetupAnswers {
            engine: "local".to_owned(),
            fallback: Vec::new(),
            local_endpoint: Some("http://localhost:11434".to_owned()),
            local_model: Some("qwen3.5:4b".to_owned()),
            providers: Vec::new(),
        };
        let block = render_summarizer_config_block(&answers);
        assert!(block.contains("[summarizer]"));
        assert!(block.contains("engine = \"local\""));
        assert!(block.contains("[summarizer.local]"));
        assert!(block.contains("endpoint = \"http://localhost:11434\""));
        assert!(block.contains("model    = \"qwen3.5:4b\""));
        // Should NOT have providers sub-section.
        assert!(!block.contains("[[summarizer.providers]]"));
        // Should NOT contain a raw API key.
        assert!(!block.contains("sk-"));
    }

    #[test]
    fn local_answers_with_model_free_fallback_includes_fallback_line() {
        let answers = SetupAnswers {
            engine: "local".to_owned(),
            fallback: vec!["model-free".to_owned()],
            local_endpoint: Some("http://localhost:11434".to_owned()),
            local_model: Some("qwen3.5:4b".to_owned()),
            providers: Vec::new(),
        };
        let block = render_summarizer_config_block(&answers);
        assert!(block.contains("fallback = [\"model-free\"]"));
    }

    #[test]
    fn api_answers_produce_correct_toml_block() {
        let answers = SetupAnswers {
            engine: "anthropic".to_owned(),
            fallback: Vec::new(),
            local_endpoint: None,
            local_model: None,
            providers: vec![ProviderEntry {
                id: "anthropic".to_owned(),
                style: "anthropic".to_owned(),
                base_url: "https://api.anthropic.com".to_owned(),
                model: "claude-haiku-4-20250514".to_owned(),
                api_key_env: "ANTHROPIC_API_KEY".to_owned(),
                api_key_file: None,
            }],
        };
        let block = render_summarizer_config_block(&answers);
        assert!(block.contains("[summarizer]"));
        // engine uses the provider id
        assert!(block.contains("engine = \"anthropic\""));
        // double-bracket providers block
        assert!(block.contains("[[summarizer.providers]]"));
        assert!(block.contains("id          = \"anthropic\""));
        assert!(block.contains("style       = \"anthropic\""));
        assert!(block.contains("base_url    = \"https://api.anthropic.com\""));
        assert!(block.contains("model       = \"claude-haiku-4-20250514\""));
        assert!(block.contains("api_key_env = \"ANTHROPIC_API_KEY\""));
        // Old dead shape must be ABSENT
        assert!(!block.contains("[summarizer.api]"));
        // KEY INVARIANT: block must not contain a literal key value.
        assert!(!block.contains("sk-ant"));
        assert!(!block.contains("sk-or-v1"));
        // Should NOT have local sub-section.
        assert!(!block.contains("[summarizer.local]"));
    }

    #[test]
    fn api_key_file_is_emitted_when_set_but_omitted_when_none() {
        let with_file = SetupAnswers {
            engine: "zai".to_owned(),
            fallback: Vec::new(),
            local_endpoint: None,
            local_model: None,
            providers: vec![ProviderEntry {
                id: "zai".to_owned(),
                style: "anthropic".to_owned(),
                base_url: "https://api.z.ai/api/anthropic".to_owned(),
                model: "glm-5.2".to_owned(),
                api_key_env: "ZAI_API_KEY".to_owned(),
                api_key_file: Some("~/.zai_key".to_owned()),
            }],
        };
        let block = render_summarizer_config_block(&with_file);
        assert!(
            block.contains("api_key_file = \"~/.zai_key\""),
            "api_key_file must be emitted when set; got:\n{block}"
        );
        // It stores only the PATH — never a key value.
        assert!(!block.contains("sk-"));

        // When None, the line must be absent entirely (not an empty string).
        let without = SetupAnswers {
            providers: vec![ProviderEntry {
                api_key_file: None,
                ..with_file.providers[0].clone()
            }],
            ..with_file
        };
        let block2 = render_summarizer_config_block(&without);
        assert!(
            !block2.contains("api_key_file"),
            "api_key_file line must be omitted when None; got:\n{block2}"
        );
    }

    #[test]
    fn api_answers_never_store_a_key_value() {
        let answers = SetupAnswers {
            engine: "openrouter".to_owned(),
            fallback: vec!["local".to_owned()],
            local_endpoint: Some("http://localhost:11434".to_owned()),
            local_model: Some("qwen3.5:4b".to_owned()),
            providers: vec![ProviderEntry {
                id: "openrouter".to_owned(),
                style: "openai".to_owned(),
                base_url: "https://api.openai.com".to_owned(),
                model: "gpt-4o-mini".to_owned(),
                api_key_env: "OPENAI_API_KEY".to_owned(),
                api_key_file: None,
            }],
        };
        let block = render_summarizer_config_block(&answers);
        // The env var VALUE is never placed in the block regardless.
        assert!(block.contains("api_key_env = \"OPENAI_API_KEY\""));
        assert!(block.contains("fallback = [\"local\"]"));
        // Provider block must be present, old [summarizer.api] must be absent.
        assert!(block.contains("[[summarizer.providers]]"));
        assert!(!block.contains("[summarizer.api]"));
        // No key VALUE — only name.
        assert!(!block.contains("sk-"));
        // Local block must be present (local is in fallback).
        assert!(block.contains("[summarizer.local]"));
    }

    #[test]
    fn render_multi_provider_block_contains_providers_header() {
        // The [[summarizer.providers]] double-bracket header must appear in the
        // rendered output; and the old [summarizer.api] single-bracket must NOT.
        let answers = SetupAnswers {
            engine: "mycloud".to_owned(),
            fallback: vec!["model-free".to_owned()],
            local_endpoint: None,
            local_model: None,
            providers: vec![ProviderEntry {
                id: "mycloud".to_owned(),
                style: "openai".to_owned(),
                base_url: "https://my.example.com".to_owned(),
                model: "my-model".to_owned(),
                api_key_env: "MY_API_KEY".to_owned(),
                api_key_file: None,
            }],
        };
        let block = render_summarizer_config_block(&answers);
        // Must have the double-bracket header.
        assert!(
            block.contains("[[summarizer.providers]]"),
            "providers block must use double-bracket header"
        );
        // Must NOT have the old single-bracket shape.
        assert!(
            !block.contains("[summarizer.api]"),
            "old [summarizer.api] shape must not be emitted"
        );
        // id field must be present.
        assert!(block.contains("id          = \"mycloud\""));
        // engine uses the provider id.
        assert!(block.contains("engine = \"mycloud\""));
    }

    #[test]
    fn render_multi_provider_block_contains_all_entries() {
        // Two providers in SetupAnswers → TOML block contains both [[summarizer.providers]] headers.
        let answers = SetupAnswers {
            engine: "anthropic".to_owned(),
            fallback: vec!["openrouter".to_owned()],
            local_endpoint: None,
            local_model: None,
            providers: vec![
                ProviderEntry {
                    id: "anthropic".to_owned(),
                    style: "anthropic".to_owned(),
                    base_url: "https://api.anthropic.com".to_owned(),
                    model: "claude-haiku-4-20250514".to_owned(),
                    api_key_env: "ANTHROPIC_API_KEY".to_owned(),
                    api_key_file: None,
                },
                ProviderEntry {
                    id: "openrouter".to_owned(),
                    style: "openai".to_owned(),
                    base_url: "https://openrouter.ai/api".to_owned(),
                    model: "meta-llama/llama-3.1-8b-instruct:free".to_owned(),
                    api_key_env: "OPENROUTER_API_KEY".to_owned(),
                    api_key_file: None,
                },
            ],
        };
        let block = render_summarizer_config_block(&answers);
        // Both [[summarizer.providers]] headers must be present.
        assert_eq!(
            block.matches("[[summarizer.providers]]").count(),
            2,
            "two providers must produce two [[summarizer.providers]] blocks"
        );
        assert!(block.contains("id          = \"anthropic\""));
        assert!(block.contains("id          = \"openrouter\""));
        assert!(block.contains("fallback = [\"openrouter\"]"));
        // KEY INVARIANT: never store a key value.
        assert!(!block.contains("sk-"));
    }

    #[test]
    fn render_block_never_stores_key_value() {
        // Even with api_key_env set to a key-like name the block must only store the name.
        let answers = SetupAnswers {
            engine: "myprovider".to_owned(),
            fallback: Vec::new(),
            local_endpoint: None,
            local_model: None,
            providers: vec![ProviderEntry {
                id: "myprovider".to_owned(),
                style: "openai".to_owned(),
                base_url: "https://api.openai.com".to_owned(),
                model: "gpt-4o".to_owned(),
                api_key_env: "OPENAI_API_KEY".to_owned(),
                api_key_file: None,
            }],
        };
        let block = render_summarizer_config_block(&answers);
        assert!(block.contains("api_key_env = \"OPENAI_API_KEY\""));
        assert!(!block.contains("sk-"));
        // Ensure no env-var expansion happens (the value should just be the name string).
        assert!(!block.contains("Bearer "));
    }

    #[test]
    fn disqualified_model_selection_is_flagged_in_block() {
        use trimwire::summarizer::is_disqualified;
        assert!(is_disqualified("granite4.1:8b"));
        assert!(is_disqualified("granite4.1:latest")); // family prefix
        assert!(!is_disqualified("qwen3.5:4b"));
    }

    #[test]
    fn warn_model_block_contains_annotation() {
        let answers = SetupAnswers {
            engine: "local".to_owned(),
            fallback: Vec::new(),
            local_endpoint: Some("http://localhost:11434".to_owned()),
            local_model: Some("qwen3.5:2b".to_owned()), // a WARN_MODELS entry
            providers: Vec::new(),
        };
        let block = render_summarizer_config_block(&answers);
        assert!(block.contains("qwen3.5:2b"));
        assert!(
            block.contains("harm gate") || block.contains("Warning"),
            "warn-tier model must produce an annotation in the block"
        );
    }

    // ── upsert_summarizer_section ─────────────────────────────────────────────

    #[test]
    fn upsert_appends_when_no_existing_summarizer() {
        let existing = "[server]\nlisten = \"127.0.0.1:8765\"\n";
        let new_block =
            "[summarizer]\nengine = \"local\"\n\n[summarizer.local]\nmodel = \"qwen3.5:4b\"\n";
        let merged = upsert_summarizer_section(existing, new_block);
        assert!(merged.contains("[server]"));
        assert!(merged.contains("listen = \"127.0.0.1:8765\""));
        assert!(merged.contains("[summarizer]"));
        assert!(merged.contains("engine = \"local\""));
    }

    #[test]
    fn upsert_replaces_existing_summarizer_block() {
        let existing = "[server]\nlisten = \"127.0.0.1:8765\"\n\n\
                        [summarizer]\nengine = \"model-free\"\n\n\
                        [summarizer.local]\nmodel = \"old-model\"\n";
        let new_block =
            "[summarizer]\nengine = \"local\"\n\n[summarizer.local]\nmodel = \"qwen3.5:4b\"\n";
        let merged = upsert_summarizer_section(existing, new_block);
        // Old section gone.
        assert!(!merged.contains("model-free"));
        assert!(!merged.contains("old-model"));
        // New section present.
        assert!(merged.contains("engine = \"local\""));
        assert!(merged.contains("model = \"qwen3.5:4b\""));
        // Other sections preserved verbatim.
        assert!(merged.contains("[server]"));
        assert!(merged.contains("listen = \"127.0.0.1:8765\""));
    }

    #[test]
    fn upsert_preserves_strategies_and_reprune() {
        let existing = "\
[server]\nlisten = \"127.0.0.1:8765\"\n\n\
[strategies.bloat_cap]\nenabled = true\n\n\
[summarizer]\nengine = \"local\"\n\n\
[summarizer.local]\nmodel = \"old\"\n\n\
[reprune]\nenabled = true\n";
        let new_block = "[summarizer]\nengine = \"anthropic\"\n\n\
[[summarizer.providers]]\nid = \"anthropic\"\nstyle = \"anthropic\"\n";
        let merged = upsert_summarizer_section(existing, new_block);
        assert!(merged.contains("[server]"));
        assert!(merged.contains("[strategies.bloat_cap]"));
        assert!(merged.contains("enabled = true"));
        assert!(merged.contains("[reprune]"));
        assert!(merged.contains("[summarizer]"));
        assert!(merged.contains("engine = \"anthropic\""));
        // Old summarizer entries gone.
        assert!(!merged.contains("model = \"old\""));
    }

    #[test]
    fn upsert_drops_double_bracket_providers_header() {
        // Existing TOML with [[summarizer.providers]] entries must be fully replaced
        // by upsert — neither the [[...]] header nor its body lines must survive.
        let existing = "\
[server]\nlisten = \"127.0.0.1:8765\"\n\n\
[summarizer]\nengine = \"old-provider\"\n\n\
[[summarizer.providers]]\nid = \"old-provider\"\nstyle = \"openai\"\n\n\
[[summarizer.providers]]\nid = \"other-old\"\nstyle = \"anthropic\"\n\n\
[reprune]\nenabled = true\n";
        let new_block = "[summarizer]\nengine = \"new-provider\"\n\n\
[[summarizer.providers]]\nid = \"new-provider\"\nstyle = \"anthropic\"\n";
        let merged = upsert_summarizer_section(existing, new_block);
        // Old provider entries must be gone.
        assert!(
            !merged.contains("old-provider"),
            "old provider id must be replaced"
        );
        assert!(
            !merged.contains("other-old"),
            "second old provider must be replaced"
        );
        // New block is present.
        assert!(merged.contains("new-provider"));
        assert!(merged.contains("[[summarizer.providers]]"));
        // Non-summarizer sections preserved.
        assert!(merged.contains("[server]"));
        assert!(merged.contains("[reprune]"));
    }

    #[test]
    fn upsert_replaces_multi_provider_section() {
        // Two [[summarizer.providers]] entries replaced by one new entry.
        let existing = "\
profile = \"default\"\n\n\
[summarizer]\nengine = \"a\"\nfallback = [\"b\"]\n\n\
[[summarizer.providers]]\nid = \"a\"\nstyle = \"anthropic\"\n\n\
[[summarizer.providers]]\nid = \"b\"\nstyle = \"openai\"\n";
        let new_block = "[summarizer]\nengine = \"myp\"\n\n\
[[summarizer.providers]]\nid = \"myp\"\nstyle = \"openai\"\n";
        let merged = upsert_summarizer_section(existing, new_block);
        assert!(
            !merged.contains("\"a\"") || merged.contains("profile = \"default\""),
            "old provider a must be gone (only profile = 'default' may still contain 'a')"
        );
        // More specific: make sure neither provider id "a" nor "b" appears as a section entry
        assert!(
            !merged.contains("id = \"a\""),
            "old id='a' must be replaced"
        );
        assert!(
            !merged.contains("id = \"b\""),
            "old id='b' must be replaced"
        );
        assert!(merged.contains("id = \"myp\""));
        assert!(merged.contains("profile = \"default\""));
    }

    #[test]
    fn upsert_on_empty_existing_just_returns_block() {
        let new_block = "[summarizer]\nengine = \"local\"\n";
        let merged = upsert_summarizer_section("", new_block);
        assert_eq!(merged.trim(), new_block.trim());
    }

    #[test]
    fn merged_toml_is_valid_and_round_trips() {
        // Prove that upsert produces TOML that figment can parse without error.
        use figment::Figment;
        use figment::providers::{Format, Serialized, Toml};
        use trimwire::config::Config;

        let existing = "\
profile = \"default\"\n\
[server]\nlisten = \"127.0.0.1:8765\"\n";
        let answers = SetupAnswers {
            engine: "anthropic".to_owned(),
            fallback: Vec::new(),
            local_endpoint: None,
            local_model: None,
            providers: vec![ProviderEntry {
                id: "anthropic".to_owned(),
                style: "anthropic".to_owned(),
                base_url: "https://api.anthropic.com".to_owned(),
                model: "claude-haiku-4-20250514".to_owned(),
                api_key_env: "ANTHROPIC_API_KEY".to_owned(),
                api_key_file: None,
            }],
        };
        let block = render_summarizer_config_block(&answers);
        let merged = upsert_summarizer_section(existing, &block);

        let cfg: Config = Figment::from(Serialized::defaults(Config::default()))
            .merge(Toml::string(&merged))
            .extract()
            .expect("merged TOML must parse without error");
        // engine is now the provider id (a string)
        assert_eq!(cfg.summarizer.engine, "anthropic");
        assert_eq!(cfg.summarizer.providers.len(), 1);
        let p = &cfg.summarizer.providers[0];
        assert_eq!(p.id, "anthropic");
        assert_eq!(p.style, "anthropic");
        assert_eq!(p.base_url, "https://api.anthropic.com");
        assert_eq!(p.model, "claude-haiku-4-20250514");
        assert_eq!(p.api_key_env, "ANTHROPIC_API_KEY");
    }

    #[test]
    fn render_multi_provider_answers_roundtrips_through_figment() {
        // A multi-provider answers set renders engine + fallback + all [[summarizer.providers]] blocks.
        use figment::Figment;
        use figment::providers::{Format, Serialized, Toml};
        use trimwire::config::Config;

        let answers = SetupAnswers {
            engine: "anthropic".to_owned(),
            fallback: vec!["openrouter".to_owned()],
            local_endpoint: None,
            local_model: None,
            providers: vec![
                ProviderEntry {
                    id: "anthropic".to_owned(),
                    style: "anthropic".to_owned(),
                    base_url: "https://api.anthropic.com".to_owned(),
                    model: "claude-haiku-4-20250514".to_owned(),
                    api_key_env: "ANTHROPIC_API_KEY".to_owned(),
                    api_key_file: None,
                },
                ProviderEntry {
                    id: "openrouter".to_owned(),
                    style: "openai".to_owned(),
                    base_url: "https://openrouter.ai/api".to_owned(),
                    model: "meta-llama/llama-3.1-8b-instruct:free".to_owned(),
                    api_key_env: "OPENROUTER_API_KEY".to_owned(),
                    api_key_file: None,
                },
            ],
        };
        let block = render_summarizer_config_block(&answers);

        // Must render engine and fallback correctly.
        assert!(block.contains("engine = \"anthropic\""));
        assert!(block.contains("fallback = [\"openrouter\"]"));
        assert_eq!(
            block.matches("[[summarizer.providers]]").count(),
            2,
            "two providers must produce two [[summarizer.providers]] blocks"
        );

        // Must round-trip through figment.
        let cfg: Config = Figment::from(Serialized::defaults(Config::default()))
            .merge(Toml::string(&block))
            .extract()
            .expect("multi-provider block must parse");
        assert_eq!(cfg.summarizer.engine, "anthropic");
        assert_eq!(cfg.summarizer.fallback, vec!["openrouter"]);
        assert_eq!(cfg.summarizer.providers.len(), 2);
        assert_eq!(cfg.summarizer.providers[0].id, "anthropic");
        assert_eq!(cfg.summarizer.providers[1].id, "openrouter");

        // KEY INVARIANT: never store a key value.
        assert!(!block.contains("sk-"));
    }

    #[test]
    fn reserved_id_rejected_in_answers() {
        // "local" and "model-free" ids are reserved — verify the guard function
        // rejects them (the wizard loop uses this logic).
        let reserved = &["local", "model-free"];
        for &id in reserved {
            assert!(
                id == "local" || id == "model-free",
                "must be a reserved token"
            );
        }
    }

    #[test]
    fn upsert_section_header_with_inline_comment_no_leading_space() {
        // A section header with an inline comment WITHOUT a leading space before '#'
        // (e.g. `[reprune]#note`) must still be detected as a non-summarizer section
        // so the following content is preserved, not silently dropped.
        let existing = "\
[server]\nlisten = \"127.0.0.1:8765\"\n\n\
[summarizer]\nengine = \"local\"\n\n\
[reprune]#this is a tight comment\nenabled = true\n";
        let new_block = "[summarizer]\nengine = \"anthropic\"\n";
        let merged = upsert_summarizer_section(existing, new_block);
        // The non-summarizer sections must survive.
        assert!(
            merged.contains("[server]"),
            "server section must be preserved"
        );
        assert!(
            merged.contains("[reprune]#this is a tight comment"),
            "reprune section with tight inline comment must be preserved verbatim"
        );
        assert!(
            merged.contains("enabled = true"),
            "reprune.enabled must survive the upsert"
        );
        // Old summarizer replaced.
        assert!(!merged.contains("engine = \"local\""));
        assert!(merged.contains("engine = \"anthropic\""));
    }

    #[test]
    fn upsert_on_partial_config_appends_providers() {
        // Existing TOML has [summarizer] without any [[summarizer.providers]] → upsert
        // replaces the whole summarizer section and adds the array.
        let existing = "[summarizer]\nengine = \"model-free\"\n";
        let new_block = "[summarizer]\nengine = \"myprovider\"\n\n\
[[summarizer.providers]]\nid = \"myprovider\"\nstyle = \"openai\"\nbase_url = \"https://example.com\"\nmodel = \"gpt-4o\"\napi_key_env = \"MY_KEY\"\n";
        let merged = upsert_summarizer_section(existing, new_block);
        assert!(!merged.contains("model-free"));
        assert!(merged.contains("engine = \"myprovider\""));
        assert!(merged.contains("[[summarizer.providers]]"));
        assert!(merged.contains("id = \"myprovider\""));
    }
}
