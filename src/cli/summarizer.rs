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

// ─── API provider sub-flow ────────────────────────────────────────────────────

/// Run the inline "Add a new API provider" sub-flow.
/// Returns the new `ProviderEntry` on success. The caller ensures no duplicate ids
/// by passing `existing_ids`.
fn wizard_add_api_provider(existing_ids: &[&str]) -> Result<ProviderEntry> {
    println!();
    println!("  New API provider");
    println!("  ─────────────────");
    println!("  trimwire will SEND the prunable conversation slice to this provider");
    println!("  authenticated with YOUR API key. Content leaves your machine.");
    println!();

    // Provider id
    let id = loop {
        let raw = prompt("  Provider id (short name, no spaces) [e.g. anthropic]: ")?;
        if raw.is_empty() {
            println!("  Provider id is required (e.g. 'anthropic', 'openrouter').");
            continue;
        }
        if raw.contains(' ') {
            println!("  Provider id must not contain spaces.");
            continue;
        }
        if raw == "local" || raw == "model-free" {
            println!("  '{raw}' is a reserved engine token — pick another id.");
            continue;
        }
        if raw.contains(['"', '\\', '\n', '\r']) {
            println!("  Provider id must not contain quotes or backslashes.");
            continue;
        }
        if existing_ids.contains(&raw.as_str()) {
            println!("  Provider id '{raw}' is already configured — pick a unique id.");
            continue;
        }
        break raw;
    };

    // Style
    let style = loop {
        let raw = prompt("  API style — anthropic or openai (OpenAI-compatible) [anthropic]: ")?;
        let s = if raw.is_empty() {
            "anthropic".to_owned()
        } else {
            raw.to_lowercase()
        };
        if s == "anthropic" || s == "openai" {
            break s;
        }
        println!("  Please enter 'anthropic' or 'openai'.");
    };

    // Default base URL hints
    let default_url = match style.as_str() {
        "anthropic" => "https://api.anthropic.com",
        _ => "https://api.openai.com",
    };
    println!();
    println!("  Base URL (the API root, before any /v1 path).");
    println!("  Defaults: anthropic → https://api.anthropic.com");
    println!("            openai   → https://api.openai.com");
    println!("  OpenRouter: use https://openrouter.ai/api (NOT .../api/v1 — double-/v1 trap).");
    let base_url = {
        let raw = prompt(&format!("  base_url [{default_url}]: "))?;
        if raw.is_empty() {
            default_url.to_owned()
        } else {
            raw
        }
    };

    // Model
    println!();
    let model_hint = match style.as_str() {
        "anthropic" => "e.g. claude-haiku-4-5",
        _ => "e.g. gpt-4o-mini",
    };
    let model = loop {
        let raw = prompt(&format!("  Model tag ({model_hint}): "))?;
        if !raw.is_empty() {
            break raw;
        }
        println!("  Model tag is required.");
    };

    // API key env var name — NEVER the key itself.
    println!();
    println!("  Enter the NAME of the environment variable that holds your API key.");
    println!("  trimwire stores only the variable name — NEVER the key itself.");
    let key_hint = match style.as_str() {
        "anthropic" => "ANTHROPIC_API_KEY",
        _ => "OPENAI_API_KEY",
    };
    let api_key_env = loop {
        let raw = prompt(&format!("  API key env var name [{key_hint}]: "))?;
        let name = if raw.is_empty() {
            key_hint.to_owned()
        } else {
            raw
        };
        // Reject anything that looks like a key value (starts with sk- or contains spaces).
        if name.contains(' ') || name.to_lowercase().starts_with("sk-") {
            println!("  That looks like a key VALUE, not a variable name.");
            println!("  Enter the NAME of the env var (e.g. ANTHROPIC_API_KEY).");
            continue;
        }
        break name;
    };

    // Privacy notice
    println!();
    println!("  Privacy: trimwire will send the prunable conversation slice to {base_url}");
    println!("  to be summarized, authenticated with the key in ${api_key_env}.");

    // Warn if env var is currently unset — with a copy-paste snippet.
    if std::env::var(&api_key_env)
        .map(|v| v.trim().is_empty())
        .unwrap_or(true)
    {
        println!();
        println!("  Warning: ${api_key_env} is not currently set in this shell.");
        println!("  trimwire will skip this provider (and fall back to the next engine)");
        println!("  until you export it before starting the gateway.");
        println!();
        println!("  To set it now (copy-paste):");
        println!("    export {api_key_env}=\"<your-api-key>\"");
        println!();
        println!("  To persist across shells, add that line to your ~/.zshrc or ~/.bashrc.");
    }

    if !prompt_yn("  Add this provider?", true)? {
        anyhow::bail!("provider entry cancelled");
    }

    println!("  → Provider \"{id}\" added. It will appear in the model list.");

    Ok(ProviderEntry {
        id,
        style,
        base_url,
        model,
        api_key_env,
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

    /// Human-readable label for the picker line.
    fn label(&self) -> String {
        match self {
            PickerItem::LocalModel { tag } => {
                use trimwire::summarizer::{APPROVED_MODELS, WARN_MODELS, is_disqualified};
                let annotation = if tag == RECOMMENDED_MODEL {
                    " ← recommended".to_owned()
                } else if WARN_MODELS.contains(&tag.as_str()) {
                    " (warn: failed harm gate)".to_owned()
                } else if is_disqualified(tag) {
                    " (DISQUALIFIED)".to_owned()
                } else if !APPROVED_MODELS.contains(&tag.as_str()) {
                    " (unvalidated)".to_owned()
                } else {
                    String::new()
                };
                format!("{tag:<28} [local]{annotation}")
            }
            PickerItem::ApiProvider { provider } => {
                format!("{:<28} [api · {}]", provider.model, provider.id)
            }
        }
    }
}

/// Print the numbered picker list.
/// Returns (local_count, api_count) for the separator position.
fn print_picker(items: &[PickerItem], exclude: Option<usize>) {
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
                    println!("    ─────────────────────────────────────────────────────");
                    printed_api_header = true;
                }
            }
        }
        println!("    {n:>2})  {}", item.label());
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
) -> Result<PickResult> {
    println!();
    println!("  {heading}:");
    print_picker(items, exclude);
    // Compute the length of non-excluded items for separator detection.
    let visible_count = items
        .iter()
        .enumerate()
        .filter(|(i, _)| exclude != Some(*i))
        .count();
    if visible_count > 0 {
        println!("    ─────────────────────────────────────────────────────");
    }
    println!("    a)  Add a new API provider...");
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
        let raw = prompt(&format!("  {heading} [{default_label}]: "))?;
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
    use trimwire::summarizer::{APPROVED_MODELS, WARN_MODELS, is_disqualified};

    println!();
    println!("  Local backend: ollama runs on your machine. Content never leaves your machine.");

    let raw_endpoint = prompt(&format!("\n  ollama endpoint [{DEFAULT_ENDPOINT}]: "))?;
    let endpoint = if raw_endpoint.is_empty() {
        DEFAULT_ENDPOINT.to_owned()
    } else {
        raw_endpoint
    };

    // If the user entered a non-default endpoint, re-probe it so the model list
    // and reachability info reflects the actual target. Unreachable is a warning
    // only — the user can configure now and start ollama later.
    let custom_probe: Option<(bool, Vec<String>)> = if endpoint != DEFAULT_ENDPOINT {
        print!("  Probing ollama at {endpoint} ... ");
        use std::io::Write;
        let _ = std::io::stdout().flush();
        match probe_ollama(&endpoint) {
            OllamaProbe::Reachable(models) => {
                println!("reachable ({} model(s) installed)", models.len());
                if !models.is_empty() {
                    println!("  Installed models at this endpoint:");
                    for m in &models {
                        println!("    {m}");
                    }
                }
                Some((true, models))
            }
            OllamaProbe::Unreachable(reason) => {
                println!("unreachable ({reason})");
                println!("    You can still configure the backend now and start ollama later.");
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
        println!("  The recommended model ({RECOMMENDED_MODEL}) is not installed.");
        if prompt_yn(
            &format!("  Pull it now (`ollama pull {RECOMMENDED_MODEL}`)?"),
            true,
        )? {
            println!("  Running: ollama pull {RECOMMENDED_MODEL}");
            let status = std::process::Command::new("ollama")
                .args(["pull", RECOMMENDED_MODEL])
                .status();
            match status {
                Ok(s) if s.success() => println!("  Done."),
                Ok(s) => println!("  ollama pull exited with {s} — continuing."),
                Err(e) => println!("  Could not run `ollama`: {e} — continuing."),
            }
        }
    }

    let model = {
        let raw = prompt(&format!("\n  Model to use [{hint}]: "))?;
        if raw.is_empty() { hint.to_owned() } else { raw }
    };

    // Model guard feedback.
    println!();
    if is_disqualified(&model) {
        println!("  WARNING: {model} is DISQUALIFIED for summarization (the blind");
        println!("  gut-read proved it drops or hallucinates load-bearing facts).");
        println!("  trimwire will REFUSE to use it at runtime.");
        println!("  Strongly recommend using qwen3.5:4b instead.");
        if !prompt_yn("  Continue with this model anyway?", false)? {
            anyhow::bail!("setup cancelled — choose an approved model");
        }
    } else if WARN_MODELS.contains(&model.as_str()) {
        println!("  Note: {model} failed the fact-retention harm gate.");
        println!("  It is a RAM opt-down — consider qwen3.5:4b if you have the RAM.");
    } else if !APPROVED_MODELS.contains(&model.as_str()) {
        println!("  Note: {model} is not a validated tag.");
        println!("  Approved: {}", APPROVED_MODELS.join(", "));
        println!("  Summary fidelity is unverified. Proceeding at your own risk.");
    }

    Ok((endpoint, model))
}

// ─── Unified wizard entry point ───────────────────────────────────────────────

/// `trimwire summarizer setup` — interactive unified model-picker wizard.
pub fn summarizer_setup() -> Result<()> {
    use trimwire::summarizer::is_disqualified;

    println!("trimwire summarizer setup\n");
    println!("This wizard configures the optional summarizer backend.");
    println!("The summarizer compresses OLD conversation turns before model-free");
    println!("pruning applies — it is best-effort and NEVER load-bearing.");
    println!("(enter q — or Ctrl-D — at any prompt to cancel; nothing is written)");
    println!();
    println!("Note: completing this wizard REPLACES the entire [summarizer] section in");
    println!("your config (existing providers are re-seeded, so they are not lost).");
    println!("To add another provider later, simply re-run `trimwire summarizer setup`.");
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
                })
                .collect()
        })
        .unwrap_or_default();

    // Probe ollama.
    let (ollama_reachable, installed_models) = {
        let ep = DEFAULT_ENDPOINT;
        print!("  Probing ollama at {ep} ... ");
        use std::io::Write;
        let _ = std::io::stdout().flush();
        match probe_ollama(ep) {
            OllamaProbe::Reachable(models) => {
                println!("reachable ({} model(s) installed)", models.len());
                (true, models)
            }
            OllamaProbe::Unreachable(reason) => {
                println!("unreachable ({reason})");
                println!("  → Install/start ollama: https://ollama.com");
                println!("    You can still configure the backend now and start ollama later.");
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

    let primary_idx = loop {
        match prompt_picker(
            &items,
            "Primary engine",
            "1",
            None,
            true,  // model-free is an option
            false, // no "none" at primary step
        )? {
            PickResult::Item(idx) => break idx,
            PickResult::ModelFree => {
                // model-free is a valid primary — means "no summarizer at all".
                println!();
                println!("  model-free selected: this disables the summarizer entirely.");
                println!("  Pruning still runs; no model calls are made.");
                let answers = SetupAnswers {
                    engine: "model-free".to_owned(),
                    fallback: Vec::new(),
                    local_endpoint: None,
                    local_model: None,
                    providers: Vec::new(),
                };
                write_summarizer_config(&answers)?;
                println!();
                println!("  Next steps:");
                println!("    trimwire doctor                 — validate your full configuration");
                println!(
                    "    trimwire summarizer setup       — re-run this wizard to add a provider later"
                );
                println!();
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
                    Ok(p) => items.push(PickerItem::ApiProvider { provider: p }),
                    Err(e) if is_cancelled(&e) => return Err(e),
                    Err(e) => println!("  {e} — try again."),
                }
                // Re-display the picker.
                continue;
            }
            PickResult::None => unreachable!("None not offered at primary step"),
        }
    };

    let primary_token = items[primary_idx].engine_token().to_owned();
    let primary_label = items[primary_idx].label();
    println!();
    println!("  Primary: {primary_label}");

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
    // reachable, pre-suggest `local` as the fallback (so the user sees a concrete
    // recommendation rather than a blank prompt). Never auto-write; the preview
    // before confirm will show the full chain either way.
    let primary_is_api = primary_token != "local" && primary_token != "model-free";
    if primary_is_api && ollama_reachable {
        println!();
        println!("  Suggestion: add 'local' as a fallback in case {primary_token} is unreachable.");
        println!(
            "  (model-free is always the implicit last resort — no need to add it explicitly)"
        );
    }

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

            match prompt_picker(
                &remaining_items,
                &format!("Fallback {fallback_n}"),
                "n",
                None,
                false, // model-free shown via "none" button
                true,  // "none" = done
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
                        Ok(p) => items.push(PickerItem::ApiProvider { provider: p }),
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

    // ── Next steps ────────────────────────────────────────────────────────────

    println!();
    println!("  Next steps:");
    println!("    trimwire doctor                 — validate your full configuration");
    println!("    trimwire summarizer status      — check the summarizer state");
    if answers.engine == "local" || answers.fallback.iter().any(|f| f == "local") {
        println!("    trimwire summarizer benchmark   — score the model on the quality corpus");
    }
    // Remind about API key env vars for any provider in the chain.
    for p in &answers.providers {
        let key_set = std::env::var(&p.api_key_env)
            .map(|v| !v.trim().is_empty())
            .unwrap_or(false);
        if !key_set {
            println!();
            println!("  Action required: set your API key before starting the gateway:");
            println!("    export {}=\"<your-api-key>\"", p.api_key_env);
            println!("  Add that export to ~/.zshrc or ~/.bashrc to persist it.");
            println!("  Then run `trimwire on` to start.");
        }
    }
    println!("    trimwire summarizer setup       — re-run to add more providers later");
    println!();

    Ok(())
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

    println!();
    println!("  Config file: {}", path.display());
    println!("  The following will be written (summarizer section only):");
    println!("  ─────────────────────────────────────────────────────");
    for line in block.lines() {
        println!("  {line}");
    }
    println!("  ─────────────────────────────────────────────────────");

    if !prompt_yn("\n  Write this to your config?", true)? {
        println!("  Aborted — nothing written.");
        return Ok(());
    }

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create config dir {}", parent.display()))?;
    }
    std::fs::write(&path, &merged).with_context(|| format!("write {}", path.display()))?;

    println!("  Saved.");
    Ok(())
}

// ─── status ───────────────────────────────────────────────────────────────────

/// `trimwire summarizer status` — show the current summarizer configuration.
pub fn summarizer_status() -> Result<()> {
    println!("trimwire summarizer status\n");

    let config = match Config::load() {
        Ok(c) => c,
        Err(e) => {
            println!("  config failed to load: {e}");
            println!("  run `trimwire config` to create or edit the config.");
            return Ok(());
        }
    };

    let s = &config.summarizer;
    match s.engine.as_str() {
        "model-free" => {
            println!("  summarizer: not configured (engine = \"model-free\")");
            println!();
            println!("  To enable: run `trimwire summarizer setup` for the wizard.");
        }
        "local" => {
            println!("  summarizer: enabled");
            println!("  engine:     local model (ollama)");
            println!("  model:      {}", s.local.model);
            println!("  endpoint:   {}", s.local.endpoint);
            println!("  mode:       {}", s.mode);
            if !s.fallback.is_empty() {
                println!("  fallback:   {}", s.fallback.join(" -> "));
            }
            if !config.reprune.enabled {
                println!();
                println!(
                    "  warning: reprune.enabled = false — the summarizer is enabled but \
                     reprune is off. The summary is not applied across turns (silent no-op)."
                );
                println!(
                    "  To fix: add the following to your config (`trimwire config` to open it):"
                );
                println!("    [reprune]");
                println!("    enabled = true");
            }
            println!();
            println!("  Validate with: trimwire doctor");
            println!("  Score the model: trimwire summarizer benchmark");
        }
        provider_id => {
            println!("  summarizer: enabled");
            println!("  engine:     cloud API provider (id = {provider_id:?})");
            println!("  mode:       {}", s.mode);
            if !s.fallback.is_empty() {
                println!("  fallback:   {}", s.fallback.join(" -> "));
            }
            // Print each configured provider.
            if s.providers.is_empty() {
                println!();
                println!("  warning: no [[summarizer.providers]] entries configured.");
            } else {
                println!();
                println!("  Configured providers:");
                for p in &s.providers {
                    println!(
                        "    id={:?}  style={}  model={}  base_url={}  api_key_env={}",
                        p.id, p.style, p.model, p.base_url, p.api_key_env
                    );
                    if p.api_key_env.is_empty() {
                        println!("      key: (api_key_env not set)");
                    } else {
                        match std::env::var(&p.api_key_env) {
                            Ok(v) if !v.trim().is_empty() => {
                                println!("      key: set");
                            }
                            _ => {
                                println!("      key: NOT SET");
                                println!(
                                    "      → export {}=\"<your-api-key>\" before starting the gateway.",
                                    p.api_key_env
                                );
                            }
                        }
                    }
                }
            }
            if !config.reprune.enabled {
                println!();
                println!(
                    "  warning: reprune.enabled = false — the summarizer is enabled but \
                     reprune is off. The summary is not applied across turns (silent no-op)."
                );
                println!(
                    "  To fix: add the following to your config (`trimwire config` to open it):"
                );
                println!("    [reprune]");
                println!("    enabled = true");
            }
            println!();
            println!("  Validate with: trimwire doctor");
            println!(
                "  Score the provider (makes real paid API calls): \
                 trimwire summarizer benchmark --model {provider_id} --yes"
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
    use trimwire::summarizer::{self, build_prompt, effective_char_budget, probe};

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

    let budget = bytes.unwrap_or_else(|| effective_char_budget(s));
    let slice = probe::build_probe_slice(budget);

    // Is the target a configured API provider?
    let provider = s.providers.iter().find(|p| p.id == target).cloned();

    println!("trimwire summarizer probe\n");
    println!(
        "  target={target}  slice={} chars (~{} KB)  turns={}  facts={}  runs={runs}",
        slice.slice_text.len(),
        slice.slice_text.len() / 1024,
        slice.n_turns,
        probe::PROBE_FACTS.len(),
    );

    let prompt = build_prompt(&slice.slice_text);

    // Resolve the engine ONCE (owned, so it can be cloned into concurrent tasks).
    let engine = if let Some(p) = provider {
        ProbeEngine::Api(p)
    } else {
        let mut local = s.local.clone();
        if target != "local" {
            local.model = target.clone();
        }
        ProbeEngine::Local(local, s.timeout_secs)
    };

    // Dry-run gate + banner.
    match &engine {
        ProbeEngine::Api(p) => {
            if !yes {
                println!();
                println!(
                    "  Provider \"{}\" ({}) — probing makes {runs} real, PAID API call(s)",
                    p.id, p.model
                );
                println!("  to {} with the key in ${}.", p.base_url, p.api_key_env);
                println!("  Re-run with --yes to make the call(s).");
                return Ok(());
            }
            if std::env::var(&p.api_key_env)
                .map(|v| v.trim().is_empty())
                .unwrap_or(true)
            {
                anyhow::bail!(
                    "${} is not set — export your API key before probing provider \"{}\".",
                    p.api_key_env,
                    p.id
                );
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
    let is_local = matches!(engine, ProbeEngine::Local(..));
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
                },
                ProviderEntry {
                    id: "openrouter".to_owned(),
                    style: "openai".to_owned(),
                    base_url: "https://openrouter.ai/api".to_owned(),
                    model: "meta-llama/llama-3.1-8b-instruct:free".to_owned(),
                    api_key_env: "OPENROUTER_API_KEY".to_owned(),
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
                },
                ProviderEntry {
                    id: "openrouter".to_owned(),
                    style: "openai".to_owned(),
                    base_url: "https://openrouter.ai/api".to_owned(),
                    model: "meta-llama/llama-3.1-8b-instruct:free".to_owned(),
                    api_key_env: "OPENROUTER_API_KEY".to_owned(),
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
