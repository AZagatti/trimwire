//! Cloud API summarizer backend (Anthropic Messages API + OpenAI Chat Completions API).
//!
//! Provides [`call_api`] — a best-effort one-shot summarizer call to a cloud provider.
//! Mirrors [`super::call_model`] in structure: same hyper client, same timeout approach,
//! same `CompactorError` taxonomy, same "never load-bearing" contract.
//!
//! ## base_url join rule
//!
//! `api.base_url` is the **API root** the user configures in their TOML.  The path
//! segment for each style is appended to it (after stripping any trailing `/`):
//!
//! - `style = "anthropic"` → `{base_url}/v1/messages`
//!   Example: `base_url = "https://api.anthropic.com"` → `https://api.anthropic.com/v1/messages`
//!
//! - `style = "openai"` → `{base_url}/v1/chat/completions`
//!   Example: `base_url = "https://api.openai.com"` → `https://api.openai.com/v1/chat/completions`
//!   Example (OpenRouter): `base_url = "https://openrouter.ai/api"` → `https://openrouter.ai/api/v1/chat/completions`
//!   Example (already has /v1): `base_url = "https://openrouter.ai/api/v1"` → `https://openrouter.ai/api/v1/v1/chat/completions` — **WRONG**.
//!   To avoid the double-/v1 trap, set `base_url` to the root *before* any `/v1`
//!   and let trimwire append `/v1/chat/completions`.  E.g. for OpenRouter use
//!   `"https://openrouter.ai/api"`, not `"https://openrouter.ai/api/v1"`.
//!
//! ## full_url override
//!
//! When `api.full_url` is set it is used verbatim as the POST URL — the `base_url` +
//! `/v1/...` convention is bypassed (for providers on a non-standard path, e.g. Z.ai's
//! OpenAI endpoint `…/paas/v4/chat/completions` or Azure deployment URLs). `style` still
//! selects the auth header (`x-api-key` vs `Bearer`) and the request/response shape.
//!
//! **Never** forward ollama-only options (`keep_alive`, `num_ctx`, `think`, `options`
//! sub-object) in API requests — only standard fields for each style are sent.
//!
//! ## Security
//!
//! The API key is read from the environment variable *named by* `api.api_key_env`.
//! trimwire **never** reads or forwards the Claude Code subscription / OAuth token.
//! If `api_key_env` is empty or the named env var is unset/empty the function
//! returns a `CompactorError::Unreachable` so the cascade skips this engine.

use std::time::Duration;

use http_body_util::{BodyExt, Full, Limited};
use hyper::body::Bytes;
use serde_json::Value;

use crate::config::SummarizerProviderConfig;
use crate::summarizer::{CompactorError, SUMMARY_SYSTEM_PROMPT};

/// Max tokens we ask a cloud API to produce.  Cloud models are fast, but we
/// still cap this to ~1/4 of the local num_ctx-equivalent budget —
/// a summary that costs thousands of output tokens is the wrong tool.
const API_MAX_TOKENS: u64 = 4_096;

/// Hard cap on the summarizer API *response* body we'll buffer (8 MiB). The
/// request asks for ≤4096 output tokens (a few hundred KB at most), so this is
/// generous — its job is to stop a slow/hostile/misconfigured endpoint from
/// streaming an unbounded body into memory (the call runs in a background task).
/// Exceeding it aborts the read (treated as "skip compaction", like any I/O error).
const API_MAX_RESPONSE_BYTES: usize = 8 * 1024 * 1024;

/// Call the cloud API backend once (non-streaming) and return the summary text.
///
/// Takes a [`SummarizerProviderConfig`] — the `id` field is not used by the HTTP
/// logic; only `style`, `base_url`, `model`, `api_key_env`, and `timeout_secs`
/// matter here.
///
/// Returns `Err(CompactorError)` for any failure; the cascade treats every error
/// as "skip this engine, try the next".  On `Ok` the caller checks
/// `summary_is_smaller` before installing.
///
/// See the module-level doc for the `base_url` join rule.
pub async fn call_api(
    api: &SummarizerProviderConfig,
    prompt: String,
) -> Result<String, CompactorError> {
    // Resolve the API key from the user's own env var — never from a hard-coded
    // value and never from CC's subscription/OAuth token.
    let api_key = if api.api_key_env.is_empty() {
        return Err(CompactorError::Unreachable(
            "summarizer provider api_key_env is not set".to_owned(),
        ));
    } else {
        match std::env::var(&api.api_key_env) {
            Ok(k) if !k.trim().is_empty() => k,
            Ok(_) => {
                return Err(CompactorError::Unreachable(format!(
                    "env var {} is set but empty",
                    api.api_key_env
                )));
            }
            Err(_) => {
                return Err(CompactorError::Unreachable(format!(
                    "env var {} is not set",
                    api.api_key_env
                )));
            }
        }
    };

    // Fail fast on the empty defaults (a provider configured without filling these
    // in): a clearer skip-to-fallback than an opaque relative-URI build error, and
    // it avoids a wasted billable request with model="".
    let has_full_url = api
        .full_url
        .as_deref()
        .is_some_and(|u| !u.trim().is_empty());
    if api.base_url.trim().is_empty() && !has_full_url {
        return Err(CompactorError::Unreachable(format!(
            "summarizer provider {:?}: base_url is not set (or set full_url)",
            api.id
        )));
    }
    if api.model.trim().is_empty() {
        return Err(CompactorError::Unreachable(format!(
            "summarizer provider {:?}: model is not set",
            api.id
        )));
    }

    let base = api.base_url.trim_end_matches('/');

    match api.style.as_str() {
        "anthropic" => call_anthropic(api, base, &api_key, prompt).await,
        "openai" => call_openai(api, base, &api_key, prompt).await,
        other => Err(CompactorError::Unreachable(format!(
            "summarizer provider {:?}: unknown style {other:?} \
             (expected \"anthropic\" or \"openai\")",
            api.id
        ))),
    }
}

// ── Anthropic Messages API ────────────────────────────────────────────────────

/// context7-verified (anthropic-sdk-typescript, High reputation):
/// - POST `{base_url}/v1/messages`
/// - Required headers: `x-api-key`, `anthropic-version: 2023-06-01`, `content-type: application/json`
/// - Body: `{ model, max_tokens, system, messages: [{role:"user", content}], stream: false }`
/// - Response (non-streaming): `{ content: [{type:"text", text:"..."}], ... }`
///   `content` is an array of content blocks; concatenate all `text`-typed blocks.
async fn call_anthropic(
    api: &SummarizerProviderConfig,
    base: &str,
    api_key: &str,
    prompt: String,
) -> Result<String, CompactorError> {
    let url = api
        .full_url
        .clone()
        .filter(|u| !u.trim().is_empty())
        .unwrap_or_else(|| format!("{base}/v1/messages"));

    let payload = serde_json::json!({
        "model": api.model,
        "max_tokens": API_MAX_TOKENS,
        "system": SUMMARY_SYSTEM_PROMPT,
        "messages": [{"role": "user", "content": prompt}],
        "stream": false,
    });

    let body =
        serde_json::to_vec(&payload).map_err(|e| CompactorError::Malformed(e.to_string()))?;

    let req = hyper::Request::builder()
        .method(hyper::Method::POST)
        .uri(&url)
        .header("x-api-key", api_key)
        .header("anthropic-version", "2023-06-01")
        .header(hyper::header::CONTENT_TYPE, "application/json")
        .body(Full::new(Bytes::from(body)))
        .map_err(|e| CompactorError::Unreachable(e.to_string()))?;

    let collected = execute_request(req, api.timeout_secs).await?;

    // Non-streaming Anthropic response: `{ "content": [{"type":"text","text":"..."}, ...] }`
    // Concatenate all text-typed blocks defensively.
    let v: Value =
        serde_json::from_slice(&collected).map_err(|e| CompactorError::Malformed(e.to_string()))?;

    let text = extract_anthropic_text(&v)?;
    finalize_text(text)
}

/// Extract and concatenate text blocks from an Anthropic Messages API response.
fn extract_anthropic_text(v: &Value) -> Result<String, CompactorError> {
    let content = v.get("content").and_then(Value::as_array).ok_or_else(|| {
        CompactorError::Malformed("missing or non-array 'content' field".to_owned())
    })?;

    let mut out = String::new();
    for block in content {
        if block.get("type").and_then(Value::as_str) == Some("text") {
            if let Some(t) = block.get("text").and_then(Value::as_str) {
                out.push_str(t);
            }
        }
    }
    Ok(out)
}

// ── OpenAI Chat Completions API ───────────────────────────────────────────────

/// Returns `true` for OpenAI reasoning / o-series models that reject `max_tokens`
/// and require `max_completion_tokens` instead.  The match is intentionally
/// conservative (prefix patterns from the public naming convention) — an unknown
/// future model falls back to the safe `max_tokens` field.
fn is_openai_reasoning_model(model: &str) -> bool {
    // o1/o3/o4 families and gpt-5 reasoning variants as documented by OpenAI.
    // Prefixes: "o1", "o3", "o4", "gpt-5" with an optional "-reasoning" suffix.
    let m = model.trim().to_lowercase();
    m.starts_with("o1") || m.starts_with("o3") || m.starts_with("o4") || m.starts_with("gpt-5")
}

/// context7-verified (OpenAI API Reference, High reputation, Benchmark 93.8):
/// - POST `{base_url}/v1/chat/completions`
/// - Required headers: `Authorization: Bearer <key>`, `content-type: application/json`
/// - Body: `{ model, messages: [{role:"system",...},{role:"user",...}], stream: false }`
/// - Response: `{ "choices": [{"message": {"role":"assistant","content":"..."}}], ... }`
///   Parse `.choices[0].message.content`.
///
/// **Token cap field:** most models use `max_tokens`; OpenAI o1/o3/o4/gpt-5-reasoning
/// models reject `max_tokens` and require `max_completion_tokens` instead.
/// [`is_openai_reasoning_model`] selects the correct field name.
async fn call_openai(
    api: &SummarizerProviderConfig,
    base: &str,
    api_key: &str,
    prompt: String,
) -> Result<String, CompactorError> {
    let url = api
        .full_url
        .clone()
        .filter(|u| !u.trim().is_empty())
        .unwrap_or_else(|| format!("{base}/v1/chat/completions"));

    // Reasoning / o-series models (o1, o3, o4, gpt-5*) reject `max_tokens` and
    // require `max_completion_tokens`. Use the correct field for each model family.
    let token_cap_key = if is_openai_reasoning_model(&api.model) {
        "max_completion_tokens"
    } else {
        "max_tokens"
    };

    let mut payload = serde_json::json!({
        "model": api.model,
        "messages": [
            {"role": "system", "content": SUMMARY_SYSTEM_PROMPT},
            {"role": "user",   "content": prompt},
        ],
        "stream": false,
    });
    payload[token_cap_key] = serde_json::json!(API_MAX_TOKENS);

    let body =
        serde_json::to_vec(&payload).map_err(|e| CompactorError::Malformed(e.to_string()))?;

    let mut builder = hyper::Request::builder()
        .method(hyper::Method::POST)
        .uri(&url)
        .header(hyper::header::AUTHORIZATION, format!("Bearer {api_key}"))
        .header(hyper::header::CONTENT_TYPE, "application/json");
    // OpenRouter attributes usage by app via these headers; without them our calls
    // show up under "Other". Only sent to OpenRouter (other OpenAI-compatible APIs
    // ignore unknown headers, but there's no reason to send them elsewhere).
    if url.contains("openrouter.ai") {
        builder = builder
            .header("HTTP-Referer", "https://github.com/AZagatti/trimwire")
            .header("X-Title", "trimwire");
    }
    let req = builder
        .body(Full::new(Bytes::from(body)))
        .map_err(|e| CompactorError::Unreachable(e.to_string()))?;

    let collected = execute_request(req, api.timeout_secs).await?;

    let v: Value =
        serde_json::from_slice(&collected).map_err(|e| CompactorError::Malformed(e.to_string()))?;

    // Detect a null/missing content field with a non-"stop" finish_reason — this
    // is typically a content-filter refusal or a context-length truncation (finish
    // reasons: "content_filter", "length", etc.). Return a distinct diagnostic
    // error rather than the generic EmptyResponse so it's diagnosable in logs.
    let first_choice = v.get("choices").and_then(|c| c.get(0));
    let finish_reason = first_choice
        .and_then(|c| c.get("finish_reason"))
        .and_then(Value::as_str)
        .unwrap_or("stop");
    let content = first_choice
        .and_then(|c| c.get("message"))
        .and_then(|m| m.get("content"));
    if content.map(|v| v.is_null()).unwrap_or(true) && finish_reason != "stop" {
        return Err(CompactorError::Malformed(format!(
            "OpenAI response: choices[0].message.content is null/missing \
             (finish_reason = {finish_reason:?}); likely a content-filter refusal or \
             context-length truncation — check model, prompt, or max_tokens"
        )));
    }

    let text = content.and_then(Value::as_str).unwrap_or("").to_owned();

    finalize_text(text)
}

// ── Shared transport ──────────────────────────────────────────────────────────

/// Execute a single request with the full-exchange timeout and return the
/// collected body bytes.  Error taxonomy mirrors `call_model`:
/// - connect/DNS/I/O → `Unreachable`
/// - timeout         → `Timeout`
/// - non-2xx HTTP    → `HttpStatus`
/// - body collect    → `Unreachable`
async fn execute_request(
    req: hyper::Request<Full<Bytes>>,
    timeout_secs: u64,
) -> Result<Bytes, CompactorError> {
    let client = crate::proxy::upstream::build_client();

    let exchange = async {
        let resp = client
            .request(req)
            .await
            .map_err(|e| CompactorError::Unreachable(e.to_string()))?;
        let status = resp.status();
        if !status.is_success() {
            return Err(CompactorError::HttpStatus(status.as_u16()));
        }
        // Bound the buffered response so a hostile/misconfigured endpoint can't
        // stream an unbounded body into the background task's memory.
        Limited::new(resp.into_body(), API_MAX_RESPONSE_BYTES)
            .collect()
            .await
            .map_err(|e| CompactorError::Unreachable(e.to_string()))
            .map(|c| c.to_bytes())
    };

    match tokio::time::timeout(Duration::from_secs(timeout_secs), exchange).await {
        Err(_) => Err(CompactorError::Timeout),
        Ok(r) => r,
    }
}

/// Trim and validate the extracted summary text; return `EmptyResponse` if blank.
fn finalize_text(text: String) -> Result<String, CompactorError> {
    let text = text.trim().to_owned();
    if text.is_empty() {
        Err(CompactorError::EmptyResponse)
    } else {
        Ok(text)
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
// set_var/remove_var are unsafe in Rust 2024; test-only, unique env var names per test.
#[allow(unsafe_code)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};
    use wiremock::matchers::{header, header_exists, method, path};
    use wiremock::{Mock, MockServer, Request, ResponseTemplate};

    /// A process-unique env-var name per call. The env block is global, so two
    /// tests sharing a name would race `set_var`/`remove_var` under the parallel
    /// test runner (a flaky missing-Authorization 404). A unique name per cfg keeps
    /// every test's key isolated — which is what the SAFETY comments assume.
    fn unique_key_env(prefix: &str) -> String {
        static SEQ: AtomicU32 = AtomicU32::new(0);
        format!("{prefix}_{}", SEQ.fetch_add(1, Ordering::Relaxed))
    }

    fn anthropic_cfg(base_url: String) -> SummarizerProviderConfig {
        SummarizerProviderConfig {
            id: "test-anthropic".to_owned(),
            style: "anthropic".to_owned(),
            base_url,
            full_url: None,
            model: "claude-haiku-4-20250514".to_owned(),
            api_key_env: unique_key_env("TEST_ANTHROPIC_API_KEY"),
            timeout_secs: 5,
        }
    }

    fn openai_cfg(base_url: String) -> SummarizerProviderConfig {
        SummarizerProviderConfig {
            id: "test-openai".to_owned(),
            style: "openai".to_owned(),
            base_url,
            full_url: None,
            model: "gpt-4o-mini".to_owned(),
            api_key_env: unique_key_env("TEST_OPENAI_API_KEY"),
            timeout_secs: 5,
        }
    }

    fn with_key(cfg: SummarizerProviderConfig, key: &str) -> SummarizerProviderConfig {
        // SAFETY: tests are single-threaded per-test runtime (tokio::test) and
        // each test uses a unique env var name, so concurrent modification is not
        // a concern within a single test run.
        unsafe { std::env::set_var(&cfg.api_key_env, key) };
        cfg
    }

    fn without_key(cfg: SummarizerProviderConfig) -> SummarizerProviderConfig {
        // SAFETY: see with_key above.
        unsafe { std::env::remove_var(&cfg.api_key_env) };
        cfg
    }

    // ── Anthropic happy path ──────────────────────────────────────────────────

    #[tokio::test]
    async fn anthropic_happy_path_parses_content_blocks() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .and(header("anthropic-version", "2023-06-01"))
            .and(header_exists("x-api-key"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "msg_01",
                "type": "message",
                "role": "assistant",
                "content": [
                    {"type": "text", "text": "GOAL: approach B chosen\nNEXT: wire the writer"},
                ],
                "stop_reason": "end_turn",
                "usage": {"input_tokens": 10, "output_tokens": 20}
            })))
            .mount(&server)
            .await;

        let cfg = with_key(anthropic_cfg(server.uri()), "test-key-anthropic");
        let out = call_api(&cfg, "prompt text".to_owned()).await.expect("ok");
        assert_eq!(out, "GOAL: approach B chosen\nNEXT: wire the writer");
    }

    #[tokio::test]
    async fn anthropic_concatenates_multiple_text_blocks() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "content": [
                    {"type": "text", "text": "GOAL: first"},
                    {"type": "text", "text": " NEXT: second"},
                ]
            })))
            .mount(&server)
            .await;

        let cfg = with_key(anthropic_cfg(server.uri()), "key");
        let out = call_api(&cfg, "p".to_owned()).await.expect("ok");
        assert_eq!(out, "GOAL: first NEXT: second");
    }

    /// Custom body inspector: asserts the Anthropic request has the mandatory fields
    /// and does NOT contain ollama-only options (keep_alive, num_ctx, think, options).
    struct AnthropicBodyChecker;
    impl wiremock::Match for AnthropicBodyChecker {
        fn matches(&self, req: &Request) -> bool {
            let Ok(v) = serde_json::from_slice::<serde_json::Value>(&req.body) else {
                return false;
            };
            // Required fields must be present.
            let has_required = v.get("model").is_some()
                && v.get("max_tokens").is_some()
                && v.get("system").is_some()
                && v.get("messages").is_some();
            // Ollama-only fields must be absent.
            let no_ollama = v.get("keep_alive").is_none()
                && v.get("num_ctx").is_none()
                && v.get("think").is_none()
                && v.get("options").is_none();
            has_required && no_ollama
        }
    }

    #[tokio::test]
    async fn anthropic_request_has_correct_headers_and_no_ollama_fields() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .and(header("x-api-key", "sk-ant-test"))
            .and(header("anthropic-version", "2023-06-01"))
            .and(header("content-type", "application/json"))
            .and(AnthropicBodyChecker)
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "content": [{"type": "text", "text": "ok summary"}]
            })))
            .mount(&server)
            .await;

        let cfg = with_key(anthropic_cfg(server.uri()), "sk-ant-test");
        call_api(&cfg, "p".to_owned()).await.expect("ok");
        server.verify().await;
    }

    // ── OpenAI happy path ─────────────────────────────────────────────────────

    #[tokio::test]
    async fn openai_happy_path_parses_choices_message_content() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .and(header_exists("authorization"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "chatcmpl-1",
                "object": "chat.completion",
                "choices": [
                    {
                        "index": 0,
                        "message": {
                            "role": "assistant",
                            "content": "GOAL: approach B chosen\nNEXT: wire the writer"
                        },
                        "finish_reason": "stop"
                    }
                ],
                "usage": {"prompt_tokens": 10, "completion_tokens": 20, "total_tokens": 30}
            })))
            .mount(&server)
            .await;

        let cfg = with_key(openai_cfg(server.uri()), "sk-openai-test");
        let out = call_api(&cfg, "prompt".to_owned()).await.expect("ok");
        assert_eq!(out, "GOAL: approach B chosen\nNEXT: wire the writer");
    }

    #[tokio::test]
    async fn openai_request_has_bearer_and_no_ollama_fields() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .and(header("authorization", "Bearer sk-openai-test"))
            .and(header("content-type", "application/json"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "choices": [{"message": {"content": "summary"}}]
            })))
            .mount(&server)
            .await;

        let cfg = with_key(openai_cfg(server.uri()), "sk-openai-test");
        call_api(&cfg, "p".to_owned()).await.expect("ok");
        server.verify().await;
    }

    #[tokio::test]
    async fn full_url_overrides_the_v1_path() {
        // A provider whose real endpoint is NOT {base}/v1/chat/completions (e.g. Z.ai's
        // OpenAI path /paas/v4). full_url is used verbatim; base_url is ignored; style
        // still selects the Bearer auth + openai payload/response shape.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/paas/v4/chat/completions"))
            .and(header("authorization", "Bearer sk-z"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "choices": [{"message": {"content": "summary"}}]
            })))
            .mount(&server)
            .await;
        let mut cfg = openai_cfg("https://unused.example".to_owned());
        cfg.full_url = Some(format!("{}/paas/v4/chat/completions", server.uri()));
        let cfg = with_key(cfg, "sk-z");
        let out = call_api(&cfg, "p".to_owned()).await.expect("ok");
        assert_eq!(out, "summary");
        server.verify().await;
    }

    // ── Error taxonomy ────────────────────────────────────────────────────────

    #[tokio::test]
    async fn anthropic_non_2xx_is_http_status_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(ResponseTemplate::new(429))
            .mount(&server)
            .await;

        let cfg = with_key(anthropic_cfg(server.uri()), "key");
        let err = call_api(&cfg, "p".to_owned()).await.unwrap_err();
        assert!(matches!(err, CompactorError::HttpStatus(429)));
    }

    #[tokio::test]
    async fn openai_non_2xx_is_http_status_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;

        let cfg = with_key(openai_cfg(server.uri()), "key");
        let err = call_api(&cfg, "p".to_owned()).await.unwrap_err();
        assert!(matches!(err, CompactorError::HttpStatus(500)));
    }

    #[tokio::test]
    async fn anthropic_empty_content_array_is_empty_response_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "content": []
            })))
            .mount(&server)
            .await;

        let cfg = with_key(anthropic_cfg(server.uri()), "key");
        let err = call_api(&cfg, "p".to_owned()).await.unwrap_err();
        assert!(matches!(err, CompactorError::EmptyResponse));
    }

    #[tokio::test]
    async fn openai_empty_content_is_empty_response_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "choices": [{"message": {"content": "   "}}]
            })))
            .mount(&server)
            .await;

        let cfg = with_key(openai_cfg(server.uri()), "key");
        let err = call_api(&cfg, "p".to_owned()).await.unwrap_err();
        assert!(matches!(err, CompactorError::EmptyResponse));
    }

    #[tokio::test]
    async fn anthropic_malformed_json_is_malformed_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_string("not json"))
            .mount(&server)
            .await;

        let cfg = with_key(anthropic_cfg(server.uri()), "key");
        let err = call_api(&cfg, "p".to_owned()).await.unwrap_err();
        assert!(matches!(err, CompactorError::Malformed(_)));
    }

    #[tokio::test]
    async fn openai_malformed_json_is_malformed_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_string("not json"))
            .mount(&server)
            .await;

        let cfg = with_key(openai_cfg(server.uri()), "key");
        let err = call_api(&cfg, "p".to_owned()).await.unwrap_err();
        assert!(matches!(err, CompactorError::Malformed(_)));
    }

    #[tokio::test]
    async fn anthropic_unreachable_endpoint_errors() {
        let cfg = with_key(anthropic_cfg("http://127.0.0.1:1".to_owned()), "key");
        let err = call_api(&cfg, "p".to_owned()).await.unwrap_err();
        assert!(
            matches!(
                err,
                CompactorError::Unreachable(_) | CompactorError::Timeout
            ),
            "unreachable must error, got {err:?}"
        );
    }

    #[tokio::test]
    async fn openai_unreachable_endpoint_errors() {
        let cfg = with_key(openai_cfg("http://127.0.0.1:1".to_owned()), "key");
        let err = call_api(&cfg, "p".to_owned()).await.unwrap_err();
        assert!(
            matches!(
                err,
                CompactorError::Unreachable(_) | CompactorError::Timeout
            ),
            "unreachable must error, got {err:?}"
        );
    }

    #[tokio::test]
    async fn anthropic_slow_response_times_out() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_delay(Duration::from_secs(3))
                    .set_body_json(serde_json::json!({"content": [{"type":"text","text":"late"}]})),
            )
            .mount(&server)
            .await;

        let mut cfg = with_key(anthropic_cfg(server.uri()), "key");
        cfg.timeout_secs = 1;
        let err = call_api(&cfg, "p".to_owned()).await.unwrap_err();
        assert!(matches!(err, CompactorError::Timeout));
    }

    #[tokio::test]
    async fn openai_slow_response_times_out() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_delay(Duration::from_secs(3))
                    .set_body_json(serde_json::json!({"choices":[{"message":{"content":"late"}}]})),
            )
            .mount(&server)
            .await;

        let mut cfg = with_key(openai_cfg(server.uri()), "key");
        cfg.timeout_secs = 1;
        let err = call_api(&cfg, "p".to_owned()).await.unwrap_err();
        assert!(matches!(err, CompactorError::Timeout));
    }

    // ── Missing / empty api_key_env ───────────────────────────────────────────

    #[tokio::test]
    async fn missing_api_key_env_name_errors_without_network() {
        let mut cfg = anthropic_cfg("http://127.0.0.1:1".to_owned());
        cfg.api_key_env = String::new(); // api_key_env not configured at all
        let err = call_api(&cfg, "p".to_owned()).await.unwrap_err();
        assert!(
            matches!(err, CompactorError::Unreachable(_)),
            "empty api_key_env must error: {err:?}"
        );
    }

    #[tokio::test]
    async fn provider_id_not_sent_in_request() {
        // The `id` field is internal and must never appear in the HTTP body.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "content": [{"type": "text", "text": "ok"}]
            })))
            .mount(&server)
            .await;
        let cfg = with_key(
            SummarizerProviderConfig {
                id: "my-provider-id".to_owned(),
                style: "anthropic".to_owned(),
                base_url: server.uri(),
                full_url: None,
                model: "claude-haiku-4-20250514".to_owned(),
                api_key_env: "TEST_PROVIDER_ID_KEY".to_owned(),
                timeout_secs: 5,
            },
            "sk-test",
        );
        call_api(&cfg, "p".to_owned()).await.expect("ok");
        // Verify via wiremock's request log that body has no "id" / "my-provider-id".
        let reqs = server.received_requests().await.unwrap();
        assert_eq!(reqs.len(), 1);
        let body_str = String::from_utf8_lossy(&reqs[0].body);
        assert!(
            !body_str.contains("my-provider-id"),
            "provider id must not appear in the HTTP body"
        );
    }

    #[tokio::test]
    async fn unset_api_key_env_var_errors_without_network() {
        let cfg = without_key(anthropic_cfg("http://127.0.0.1:1".to_owned()));
        let err = call_api(&cfg, "p".to_owned()).await.unwrap_err();
        assert!(
            matches!(err, CompactorError::Unreachable(_)),
            "unset env var must error: {err:?}"
        );
    }

    #[tokio::test]
    async fn empty_api_key_env_var_errors_without_network() {
        // Use a DEDICATED env var name (not the shared TEST_ANTHROPIC_API_KEY that
        // anthropic_cfg uses) so setting it to "" can't race other tests, and clean
        // it up afterward. SAFETY: test-only, unique name.
        let var = "TEST_ANTHROPIC_EMPTY_KEY";
        unsafe { std::env::set_var(var, "") };
        let cfg = SummarizerProviderConfig {
            api_key_env: var.to_owned(),
            ..anthropic_cfg("http://127.0.0.1:1".to_owned())
        };
        let err = call_api(&cfg, "p".to_owned()).await.unwrap_err();
        unsafe { std::env::remove_var(var) };
        assert!(
            matches!(err, CompactorError::Unreachable(_)),
            "empty env var value must error: {err:?}"
        );
    }

    // ── base_url trailing-slash handling ──────────────────────────────────────

    #[tokio::test]
    async fn anthropic_trailing_slash_stripped_from_base_url() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "content": [{"type": "text", "text": "ok"}]
            })))
            .expect(1)
            .mount(&server)
            .await;

        // Append trailing slash to base_url — must still hit /v1/messages once.
        let mut cfg = with_key(anthropic_cfg(server.uri()), "key");
        cfg.base_url = format!("{}/", server.uri());
        call_api(&cfg, "p".to_owned()).await.expect("ok");
        server.verify().await;
    }

    // ── Anthropic content block with non-text types skipped ───────────────────

    #[tokio::test]
    async fn anthropic_non_text_blocks_are_skipped() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "content": [
                    {"type": "tool_use", "id": "tu_1", "name": "Bash", "input": {}},
                    {"type": "text", "text": "GOAL: ok"},
                ]
            })))
            .mount(&server)
            .await;

        let cfg = with_key(anthropic_cfg(server.uri()), "key");
        let out = call_api(&cfg, "p".to_owned()).await.expect("ok");
        assert_eq!(out, "GOAL: ok", "non-text blocks must be skipped");
    }

    // ── is_openai_reasoning_model ─────────────────────────────────────────────

    #[test]
    fn reasoning_model_detection_covers_o_series_and_gpt5() {
        // o1 / o3 / o4 family
        assert!(is_openai_reasoning_model("o1"));
        assert!(is_openai_reasoning_model("o1-mini"));
        assert!(is_openai_reasoning_model("o1-preview"));
        assert!(is_openai_reasoning_model("o3"));
        assert!(is_openai_reasoning_model("o3-mini"));
        assert!(is_openai_reasoning_model("o4-mini"));
        // gpt-5 reasoning variants
        assert!(is_openai_reasoning_model("gpt-5"));
        assert!(is_openai_reasoning_model("gpt-5-reasoning"));
        // Standard models — must NOT be flagged.
        assert!(!is_openai_reasoning_model("gpt-4o"));
        assert!(!is_openai_reasoning_model("gpt-4o-mini"));
        assert!(!is_openai_reasoning_model("gpt-4-turbo"));
        assert!(!is_openai_reasoning_model("gpt-3.5-turbo"));
        assert!(!is_openai_reasoning_model("claude-haiku-4-20250514"));
    }

    #[tokio::test]
    async fn openai_reasoning_model_sends_max_completion_tokens() {
        // An o-series model request must include `max_completion_tokens`, not `max_tokens`.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "choices": [{"message": {"content": "GOAL: ok"}, "finish_reason": "stop"}]
            })))
            .mount(&server)
            .await;

        let mut cfg = with_key(openai_cfg(server.uri()), "key");
        cfg.model = "o3-mini".to_owned();
        call_api(&cfg, "p".to_owned()).await.expect("ok");

        let reqs = server.received_requests().await.unwrap();
        assert_eq!(reqs.len(), 1);
        let body: serde_json::Value = serde_json::from_slice(&reqs[0].body).expect("body is JSON");
        assert!(
            body.get("max_completion_tokens").is_some(),
            "reasoning model must send max_completion_tokens"
        );
        assert!(
            body.get("max_tokens").is_none(),
            "reasoning model must NOT send max_tokens"
        );
    }

    #[tokio::test]
    async fn openai_standard_model_sends_max_tokens() {
        // A standard model (gpt-4o-mini) must use the legacy `max_tokens` field.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "choices": [{"message": {"content": "GOAL: ok"}, "finish_reason": "stop"}]
            })))
            .mount(&server)
            .await;

        let cfg = with_key(openai_cfg(server.uri()), "key");
        call_api(&cfg, "p".to_owned()).await.expect("ok");

        let reqs = server.received_requests().await.unwrap();
        assert_eq!(reqs.len(), 1);
        let body: serde_json::Value = serde_json::from_slice(&reqs[0].body).expect("body is JSON");
        assert!(
            body.get("max_tokens").is_some(),
            "standard model must send max_tokens"
        );
        assert!(
            body.get("max_completion_tokens").is_none(),
            "standard model must NOT send max_completion_tokens"
        );
    }

    // ── content_filter / null content diagnostic ──────────────────────────────

    #[tokio::test]
    async fn openai_null_content_with_content_filter_reason_gives_diagnostic_error() {
        // When choices[0].message.content is null and finish_reason != "stop", the
        // caller gets a Malformed error describing the content-filter/truncation, not
        // the generic EmptyResponse.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "choices": [{"message": {"role": "assistant", "content": null},
                             "finish_reason": "content_filter"}]
            })))
            .mount(&server)
            .await;

        let cfg = with_key(openai_cfg(server.uri()), "key");
        let err = call_api(&cfg, "p".to_owned()).await.unwrap_err();
        match &err {
            CompactorError::Malformed(msg) => {
                assert!(
                    msg.contains("content_filter"),
                    "error must mention the finish_reason: {msg}"
                );
                assert!(
                    msg.contains("null"),
                    "error must mention null content: {msg}"
                );
            }
            other => panic!("expected Malformed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn openai_null_content_with_stop_reason_still_gives_empty_response_error() {
        // finish_reason="stop" + null content is unusual but not a content-filter:
        // fall through to the normal empty-response path.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "choices": [{"message": {"role": "assistant", "content": null},
                             "finish_reason": "stop"}]
            })))
            .mount(&server)
            .await;

        let cfg = with_key(openai_cfg(server.uri()), "key");
        let err = call_api(&cfg, "p".to_owned()).await.unwrap_err();
        assert!(
            matches!(err, CompactorError::EmptyResponse),
            "null content with stop finish_reason must give EmptyResponse, got {err:?}"
        );
    }
}
