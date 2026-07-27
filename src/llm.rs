//! Local AI HTTP client — streams chat completions via SSE or NDJSON.
//! Supports Ollama native API (/api/chat) and any OpenAI-compatible API.

use reqwest::Client;
use serde_json::{json, Value};
use std::time::Duration;
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

/// Collect the string values at `name_key` from each object in the JSON array
/// stored under `array_key`. A missing array or missing keys yield an empty vec.
fn json_names(val: &Value, array_key: &str, name_key: &str) -> Vec<String> {
    val[array_key]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|m| m[name_key].as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

/// Probe the backend on startup: check connectivity and list available models.
/// Returns Ok(model_list) on success, Err(reason) on failure.
/// Non-fatal — callers should log the result but not abort.
pub async fn probe_backend(config: &LlmConfig) -> Result<Vec<String>, String> {
    let client = &config.client;

    // Try Ollama-native /api/tags first (works on localhost:11434)
    let tags_url = format!("{}/api/tags", config.ollama_base());

    if let Ok(resp) = client.get(&tags_url).send().await {
        if resp.status().is_success() {
            if let Ok(val) = resp.json::<Value>().await {
                return Ok(json_names(&val, "models", "name"));
            }
        }
    }

    // Fallback: try /v1/models (OpenAI-compatible)
    let models_url = format!("{}/models", config.base_url);
    match client
        .get(&models_url)
        .header("Authorization", format!("Bearer {}", config.api_key))
        .send()
        .await
    {
        Ok(resp) if resp.status().is_success() => {
            if let Ok(val) = resp.json::<Value>().await {
                return Ok(json_names(&val, "data", "id"));
            }
            Ok(vec![])
        }
        Ok(resp) => Err(format!("HTTP {}", resp.status())),
        Err(e) => Err(format!("{e}")),
    }
}

/// Query Ollama /api/show for model metadata (context length, etc.).
/// Returns None if not an Ollama backend or request fails.
pub async fn query_model_info(config: &LlmConfig) -> Option<ModelInfo> {
    if !config.is_ollama_native() {
        return None;
    }
    let url = format!("{}/api/show", config.base_url);
    let resp = config
        .client
        .post(&url)
        .json(&json!({"name": config.model}))
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let val: Value = resp.json().await.ok()?;

    // Extract context length from model_info
    let model_info = val.get("model_info")?;
    let context_length = model_info
        .as_object()?
        .iter()
        .find(|(k, _)| k.ends_with(".context_length"))
        .and_then(|(_, v)| v.as_u64())
        .unwrap_or(0);

    Some(ModelInfo { context_length })
}

/// Query Ollama /api/ps to check if a model is loaded in VRAM.
pub async fn query_running_models(config: &LlmConfig) -> Option<Vec<String>> {
    let url = format!("{}/api/ps", config.ollama_base());
    let resp = config.client.get(&url).send().await.ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let val: Value = resp.json().await.ok()?;
    Some(json_names(&val, "models", "name"))
}

pub struct ModelInfo {
    pub context_length: u64,
}

/// Maximum number of retry attempts for transient LLM HTTP errors.
const MAX_RETRIES: u32 = 3;
/// Initial backoff delay in milliseconds (doubles each retry).
const INITIAL_BACKOFF_MS: u64 = 500;

pub struct LlmConfig {
    pub base_url: String,
    pub model: String,
    pub api_key: String,
    pub system_prompt: Option<String>,
    pub temperature: Option<f64>,
    pub max_tokens: Option<u64>,
    pub timeout_secs: u64,
    /// Maximum conversation turns to keep (0 = unlimited).
    pub max_history_turns: usize,
    /// Maximum number of concurrent sessions (0 = unlimited).
    pub max_sessions: usize,
    /// Session idle timeout in seconds (0 = no timeout).
    pub session_idle_timeout_secs: u64,
    /// Shared HTTP client for connection pooling.
    pub client: Client,
}

impl LlmConfig {
    /// Returns true if the base_url points to an Ollama native API (no /v1 suffix).
    pub fn is_ollama_native(&self) -> bool {
        !self.base_url.ends_with("/v1")
    }

    fn ollama_base(&self) -> &str {
        self.base_url.trim_end_matches("/v1").trim_end_matches('/')
    }

    fn authenticated_post(&self, url: &str) -> reqwest::RequestBuilder {
        self.client
            .post(url)
            .header("Content-Type", "application/json")
            .bearer_auth(&self.api_key)
    }

    /// Returns the chat completion URL based on backend type.
    fn chat_url(&self) -> String {
        if self.is_ollama_native() {
            format!("{}/api/chat", self.base_url)
        } else {
            format!("{}/chat/completions", self.base_url)
        }
    }

    pub fn from_env() -> Self {
        let timeout_secs = std::env::var("LLM_TIMEOUT")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(300);

        let client = Client::builder()
            .timeout(Duration::from_secs(timeout_secs))
            .pool_max_idle_per_host(4)
            .build()
            .expect("Failed to create HTTP client");

        Self {
            base_url: std::env::var("LLM_BASE_URL")
                .or_else(|_| std::env::var("OLLAMA_BASE_URL"))
                .unwrap_or_else(|_| "http://localhost:11434/v1".into()),
            model: std::env::var("LLM_MODEL")
                .or_else(|_| std::env::var("OLLAMA_MODEL"))
                .unwrap_or_else(|_| "gemma4:26b".into()),
            api_key: std::env::var("LLM_API_KEY")
                .or_else(|_| std::env::var("OLLAMA_API_KEY"))
                .unwrap_or_else(|_| "local-ai".into()),
            system_prompt: std::env::var("LLM_SYSTEM_PROMPT").ok(),
            temperature: std::env::var("LLM_TEMPERATURE")
                .ok()
                .and_then(|v| v.parse::<f64>().ok())
                .filter(|t| t.is_finite()),
            max_tokens: std::env::var("LLM_MAX_TOKENS")
                .ok()
                .and_then(|v| v.parse().ok()),
            timeout_secs,
            max_history_turns: std::env::var("LLM_MAX_HISTORY_TURNS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(50),
            max_sessions: std::env::var("LLM_MAX_SESSIONS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(0),
            session_idle_timeout_secs: std::env::var("LLM_SESSION_IDLE_TIMEOUT")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(0),
            client,
        }
    }
}

#[derive(Debug)]
pub enum StreamChunk {
    Content(String),
    Error(String),
    Done,
}

const MAX_STREAM_BUFFER_SIZE: usize = 10 * 1024 * 1024;

#[derive(Default)]
struct LineBuffer {
    data: String,
}

impl LineBuffer {
    fn push(&mut self, chunk: &[u8]) -> bool {
        let chunk = String::from_utf8_lossy(chunk);
        if self.data.len() + chunk.len() > MAX_STREAM_BUFFER_SIZE {
            return false;
        }
        self.data.push_str(&chunk);
        true
    }

    fn next_line(&mut self) -> Option<String> {
        let newline_pos = self.data.find('\n').or_else(|| self.data.find('\r'))?;
        let skip = if self.data[newline_pos..].starts_with("\r\n") {
            2
        } else {
            1
        };
        let line = self.data[..newline_pos].trim_end().to_string();
        self.data.drain(..newline_pos + skip);
        Some(line)
    }
}

/// Returns true if the HTTP status code is transient and worth retrying.
fn is_retryable(status: reqwest::StatusCode) -> bool {
    matches!(status.as_u16(), 408 | 429 | 500 | 502 | 503 | 504)
}

async fn send_with_retry(
    config: &LlmConfig,
    url: &str,
    body: &Value,
    operation: &str,
) -> Result<reqwest::Response, String> {
    let mut last_err = String::new();

    for attempt in 0..=MAX_RETRIES {
        if attempt > 0 {
            let delay = INITIAL_BACKOFF_MS * 2u64.pow(attempt - 1);
            warn!(attempt, delay_ms = delay, operation, "Retrying LLM request");
            tokio::time::sleep(Duration::from_millis(delay)).await;
        }

        match config.authenticated_post(url).json(body).send().await {
            Ok(response) if response.status().is_success() => return Ok(response),
            Ok(response) if is_retryable(response.status()) => {
                last_err = format!(
                    "LLM HTTP {}: {}",
                    response.status(),
                    response.status().canonical_reason().unwrap_or("error")
                );
                warn!(status = %response.status(), operation, "Transient LLM error");
            }
            Ok(response) => {
                return Err(format!(
                    "LLM HTTP {}: {}",
                    response.status(),
                    response
                        .status()
                        .canonical_reason()
                        .unwrap_or("Unknown error")
                ));
            }
            Err(e) if e.is_timeout() || e.is_connect() => {
                last_err = format!("HTTP request failed: {e}");
                warn!(error = %e, operation, "Transient connection error");
            }
            Err(e) => return Err(format!("HTTP request failed: {e}")),
        }
    }

    error!(error = %last_err, operation, "All retry attempts exhausted");
    Err(last_err)
}

/// Build the JSON body for a chat completion request.
fn build_body(
    config: &LlmConfig,
    messages: &[Value],
    model: &str,
    stream: bool,
    tools: Option<&[Value]>,
) -> Value {
    let mut body = json!({
        "model": model,
        "messages": messages,
        "stream": stream,
    });
    if let Some(temp) = config.temperature {
        // Clamp to valid range 0.0–2.0
        body["temperature"] = json!(temp.clamp(0.0, 2.0));
    }
    if let Some(max) = config.max_tokens {
        body["max_tokens"] = json!(max);
    }
    if let Some(tools) = tools {
        body["tools"] = json!(tools);
    }
    body
}

/// Non-streaming chat completion — returns full response as Value.
pub async fn chat(
    config: &LlmConfig,
    messages: &[Value],
    model_override: Option<&str>,
    tools: Option<&[Value]>,
) -> Result<Value, String> {
    let url = config.chat_url();
    let model = model_override.unwrap_or(&config.model);
    let body = build_body(config, messages, model, false, tools);
    let response = send_with_retry(config, &url, &body, "chat").await?;
    response
        .json()
        .await
        .map_err(|e| format!("Failed to parse response: {e}"))
}

/// Stream chat completion — auto-detects backend and uses appropriate parser.
pub async fn stream_chat(
    config: &LlmConfig,
    messages: &[Value],
    model_override: Option<&str>,
) -> Result<mpsc::Receiver<StreamChunk>, String> {
    let url = config.chat_url();
    let model = model_override.unwrap_or(&config.model);
    let is_native = config.is_ollama_native();

    let body = build_body(config, messages, model, true, None);
    let response = send_with_retry(config, &url, &body, "stream_chat").await?;

    let (tx, rx) = mpsc::channel(256);

    if is_native {
        tokio::spawn(parse_ollama_native_stream(response, tx));
    } else {
        tokio::spawn(parse_openai_sse_stream(response, tx));
    }

    info!(model, native = is_native, "Streaming started");
    Ok(rx)
}

/// Parse Ollama native NDJSON streaming response.
/// Each line is a complete JSON object: {"message":{"content":"..."},"done":false}
async fn parse_ollama_native_stream(
    mut response: reqwest::Response,
    tx: mpsc::Sender<StreamChunk>,
) {
    let mut buffer = LineBuffer::default();

    loop {
        let chunk_result: Result<Option<bytes::Bytes>, reqwest::Error> = response.chunk().await;
        match chunk_result {
            Ok(Some(bytes)) => {
                if !buffer.push(&bytes) {
                    error!("Stream buffer exceeded limit, aborting");
                    let _ = tx
                        .send(StreamChunk::Error("Stream buffer overflow".into()))
                        .await;
                    return;
                }

                while let Some(line) = buffer.next_line() {
                    if line.is_empty() {
                        continue;
                    }

                    if let Ok(parsed) = serde_json::from_str::<Value>(&line) {
                        // Check if done
                        if parsed.get("done").and_then(|d| d.as_bool()) == Some(true) {
                            let _ = tx.send(StreamChunk::Done).await;
                            return;
                        }

                        // Extract content from message.content
                        if let Some(text) = parsed
                            .get("message")
                            .and_then(|m| m.get("content"))
                            .and_then(|c| c.as_str())
                        {
                            if !text.is_empty() {
                                let _ = tx.send(StreamChunk::Content(text.to_string())).await;
                            }
                        }
                    }
                }
            }
            Ok(None) => {
                debug!("Ollama native stream ended");
                break;
            }
            Err(e) => {
                error!(error = %e, "Stream chunk error");
                let _ = tx.send(StreamChunk::Error(e.to_string())).await;
                break;
            }
        }
    }

    let _ = tx.send(StreamChunk::Done).await;
}

/// Parse OpenAI-compatible SSE streaming response.
/// Each line: "data: {json}" or "data: [DONE]"
async fn parse_openai_sse_stream(mut response: reqwest::Response, tx: mpsc::Sender<StreamChunk>) {
    let mut buffer = LineBuffer::default();

    loop {
        let chunk_result: Result<Option<bytes::Bytes>, reqwest::Error> = response.chunk().await;
        match chunk_result {
            Ok(Some(bytes)) => {
                if !buffer.push(&bytes) {
                    error!("Stream buffer exceeded limit, aborting");
                    let _ = tx
                        .send(StreamChunk::Error("Stream buffer overflow".into()))
                        .await;
                    return;
                }

                while let Some(line) = buffer.next_line() {
                    if line.is_empty() || !line.starts_with("data: ") {
                        continue;
                    }

                    let data = &line[6..];
                    if data == "[DONE]" {
                        let _ = tx.send(StreamChunk::Done).await;
                        return;
                    }

                    if let Ok(parsed) = serde_json::from_str::<Value>(data) {
                        if let Some(text) = parsed
                            .get("choices")
                            .and_then(|c| c.get(0))
                            .and_then(|c| c.get("delta"))
                            .and_then(|d| d.get("content"))
                            .and_then(|t| t.as_str())
                        {
                            if !text.is_empty() {
                                let _ = tx.send(StreamChunk::Content(text.to_string())).await;
                            }
                        }
                    }
                }
            }
            Ok(None) => {
                debug!("SSE stream ended");
                break;
            }
            Err(e) => {
                error!(error = %e, "Stream chunk error");
                let _ = tx.send(StreamChunk::Error(e.to_string())).await;
                break;
            }
        }
    }

    let _ = tx.send(StreamChunk::Done).await;
}
#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::response::{IntoResponse, Response};
    use axum::routing::{get, post};
    use axum::{Json, Router};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    /// Build an `LlmConfig` pointing at `base_url` with a short timeout so
    /// error-path tests don't hang.
    fn test_config(base_url: &str) -> LlmConfig {
        LlmConfig {
            base_url: base_url.to_string(),
            model: "test-model".into(),
            api_key: "test-key".into(),
            system_prompt: None,
            temperature: None,
            max_tokens: None,
            timeout_secs: 5,
            max_history_turns: 50,
            max_sessions: 0,
            session_idle_timeout_secs: 0,
            client: Client::builder()
                .timeout(Duration::from_secs(5))
                .build()
                .expect("client"),
        }
    }

    /// Bind an ephemeral port, serve `router` in the background, and return the
    /// base URL (e.g. `http://127.0.0.1:54321`).
    async fn serve(router: Router) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        format!("http://{addr}")
    }

    // -- pure helpers -------------------------------------------------------

    #[test]
    fn is_ollama_native_detects_v1_suffix() {
        assert!(test_config("http://localhost:11434").is_ollama_native());
        assert!(!test_config("http://localhost:11434/v1").is_ollama_native());
    }

    #[test]
    fn chat_url_switches_on_backend() {
        assert_eq!(
            test_config("http://host:11434").chat_url(),
            "http://host:11434/api/chat"
        );
        assert_eq!(
            test_config("http://host:8000/v1").chat_url(),
            "http://host:8000/v1/chat/completions"
        );
    }

    #[test]
    fn is_retryable_matches_transient_codes() {
        for code in [408u16, 429, 500, 502, 503, 504] {
            assert!(
                is_retryable(reqwest::StatusCode::from_u16(code).unwrap()),
                "{code} should be retryable"
            );
        }
        for code in [200u16, 400, 401, 403, 404, 501] {
            assert!(
                !is_retryable(reqwest::StatusCode::from_u16(code).unwrap()),
                "{code} should not be retryable"
            );
        }
    }

    #[test]
    fn json_names_collects_present_string_fields() {
        let value = json!({
            "models": [
                {"name": "qwen"},
                {"name": 42},
                {},
                {"name": "gemma"}
            ]
        });

        assert_eq!(json_names(&value, "models", "name"), vec!["qwen", "gemma"]);
        assert!(json_names(&value, "data", "id").is_empty());
    }

    #[test]
    fn line_buffer_handles_chunked_and_crlf_lines() {
        let mut buffer = LineBuffer::default();

        assert!(buffer.push(b"first\r"));
        assert_eq!(buffer.next_line().as_deref(), Some("first"));
        assert!(buffer.push(b"\nsecond"));
        assert_eq!(buffer.next_line().as_deref(), Some(""));
        assert!(buffer.push(b"\n"));
        assert_eq!(buffer.next_line().as_deref(), Some("second"));
        assert!(buffer.next_line().is_none());
    }

    #[test]
    fn build_body_includes_core_fields_only_by_default() {
        let cfg = test_config("http://host/v1");
        let messages = vec![json!({"role": "user", "content": "hi"})];
        let body = build_body(&cfg, &messages, "m", true, None);
        assert_eq!(body["model"], "m");
        assert_eq!(body["stream"], true);
        assert_eq!(body["messages"], json!(messages));
        assert!(body.get("temperature").is_none());
        assert!(body.get("max_tokens").is_none());
        assert!(body.get("tools").is_none());
    }

    #[test]
    fn build_body_clamps_temperature() {
        let mut cfg = test_config("http://host/v1");
        cfg.temperature = Some(5.0);
        let hi = build_body(&cfg, &[], "m", false, None);
        assert_eq!(hi["temperature"], json!(2.0));

        cfg.temperature = Some(-1.0);
        let lo = build_body(&cfg, &[], "m", false, None);
        assert_eq!(lo["temperature"], json!(0.0));

        cfg.temperature = Some(0.7);
        let ok = build_body(&cfg, &[], "m", false, None);
        assert_eq!(ok["temperature"], json!(0.7));
    }

    #[test]
    fn build_body_adds_max_tokens_and_tools() {
        let mut cfg = test_config("http://host/v1");
        cfg.max_tokens = Some(256);
        let tools = vec![json!({"type": "function", "function": {"name": "read"}})];
        let body = build_body(&cfg, &[], "m", false, Some(&tools));
        assert_eq!(body["max_tokens"], json!(256));
        assert_eq!(body["tools"], json!(tools));
    }

    // -- probe_backend ------------------------------------------------------

    #[tokio::test]
    async fn probe_backend_reads_ollama_tags() {
        async fn tags() -> impl IntoResponse {
            Json(json!({"models": [{"name": "llama3:8b"}, {"name": "qwen2:7b"}]}))
        }
        let url = serve(Router::new().route("/api/tags", get(tags))).await;
        let cfg = test_config(&format!("{url}/v1"));
        let models = probe_backend(&cfg).await.unwrap();
        assert_eq!(models, vec!["llama3:8b", "qwen2:7b"]);
    }

    #[tokio::test]
    async fn probe_backend_falls_back_to_openai_models() {
        async fn models() -> impl IntoResponse {
            Json(json!({"data": [{"id": "gpt-local"}]}))
        }
        // No /api/tags route → Ollama probe fails, falls back to /v1/models.
        let url = serve(Router::new().route("/v1/models", get(models))).await;
        let cfg = test_config(&format!("{url}/v1"));
        let found = probe_backend(&cfg).await.unwrap();
        assert_eq!(found, vec!["gpt-local"]);
    }

    #[tokio::test]
    async fn probe_backend_reports_http_error() {
        async fn err() -> impl IntoResponse {
            axum::http::StatusCode::INTERNAL_SERVER_ERROR
        }
        let url = serve(Router::new().route("/v1/models", get(err))).await;
        let cfg = test_config(&format!("{url}/v1"));
        let res = probe_backend(&cfg).await;
        assert!(res.is_err(), "expected Err, got {res:?}");
    }

    // -- query_model_info / query_running_models ----------------------------

    #[tokio::test]
    async fn query_model_info_none_for_openai_backend() {
        let cfg = test_config("http://host/v1");
        assert!(query_model_info(&cfg).await.is_none());
    }

    #[tokio::test]
    async fn query_model_info_reads_context_length() {
        async fn show() -> impl IntoResponse {
            Json(json!({"model_info": {"llama.context_length": 8192}}))
        }
        let url = serve(Router::new().route("/api/show", post(show))).await;
        // No /v1 suffix → treated as Ollama native.
        let cfg = test_config(&url);
        let info = query_model_info(&cfg).await.expect("model info");
        assert_eq!(info.context_length, 8192);
    }

    #[tokio::test]
    async fn query_running_models_lists_loaded() {
        async fn ps() -> impl IntoResponse {
            Json(json!({"models": [{"name": "loaded:latest"}]}))
        }
        let url = serve(Router::new().route("/api/ps", get(ps))).await;
        let cfg = test_config(&format!("{url}/v1"));
        let models = query_running_models(&cfg).await.expect("running models");
        assert_eq!(models, vec!["loaded:latest"]);
    }

    // -- chat (retry + errors) ---------------------------------------------

    #[tokio::test]
    async fn chat_returns_response_on_success() {
        async fn ok() -> impl IntoResponse {
            Json(json!({"choices": [{"message": {"content": "hi"}}]}))
        }
        let url = serve(Router::new().route("/v1/chat/completions", post(ok))).await;
        let cfg = test_config(&format!("{url}/v1"));
        let val = chat(&cfg, &[], None, None).await.unwrap();
        assert_eq!(val["choices"][0]["message"]["content"], "hi");
    }

    #[tokio::test]
    async fn chat_retries_transient_then_succeeds() {
        let counter = Arc::new(AtomicUsize::new(0));
        async fn handler(State(c): State<Arc<AtomicUsize>>) -> Response {
            let n = c.fetch_add(1, Ordering::SeqCst);
            if n == 0 {
                axum::http::StatusCode::SERVICE_UNAVAILABLE.into_response()
            } else {
                Json(json!({"choices": [{"message": {"content": "ok"}}]})).into_response()
            }
        }
        use axum::extract::State;
        let router = Router::new()
            .route("/v1/chat/completions", post(handler))
            .with_state(counter.clone());
        let url = serve(router).await;
        let cfg = test_config(&format!("{url}/v1"));
        let val = chat(&cfg, &[], None, None).await.unwrap();
        assert_eq!(val["choices"][0]["message"]["content"], "ok");
        assert_eq!(counter.load(Ordering::SeqCst), 2, "should retry once");
    }

    #[tokio::test]
    async fn chat_returns_err_on_non_retryable_status() {
        async fn bad() -> impl IntoResponse {
            axum::http::StatusCode::BAD_REQUEST
        }
        let url = serve(Router::new().route("/v1/chat/completions", post(bad))).await;
        let cfg = test_config(&format!("{url}/v1"));
        let err = chat(&cfg, &[], None, None).await.unwrap_err();
        assert!(err.contains("400"), "err was: {err}");
    }

    // -- streaming ----------------------------------------------------------

    async fn collect_stream(mut rx: mpsc::Receiver<StreamChunk>) -> (String, bool) {
        let mut text = String::new();
        let mut done = false;
        while let Some(chunk) = rx.recv().await {
            match chunk {
                StreamChunk::Content(c) => text.push_str(&c),
                StreamChunk::Done => {
                    done = true;
                    break;
                }
                StreamChunk::Error(e) => panic!("unexpected stream error: {e}"),
            }
        }
        (text, done)
    }

    #[tokio::test]
    async fn stream_chat_parses_openai_sse() {
        async fn sse() -> impl IntoResponse {
            let chunks = vec![
                format!(
                    "data: {}\n\n",
                    json!({"choices": [{"delta": {"content": "Hello"}}]})
                ),
                format!(
                    "data: {}\n\n",
                    json!({"choices": [{"delta": {"content": " world"}}]})
                ),
                "data: [DONE]\n\n".to_string(),
            ];
            let stream = futures_lite::stream::iter(
                chunks.into_iter().map(Ok::<_, std::convert::Infallible>),
            );
            Response::builder()
                .header("content-type", "text/event-stream")
                .body(Body::from_stream(stream))
                .unwrap()
        }
        let url = serve(Router::new().route("/v1/chat/completions", post(sse))).await;
        let cfg = test_config(&format!("{url}/v1"));
        let rx = stream_chat(&cfg, &[], None).await.unwrap();
        let (text, done) = collect_stream(rx).await;
        assert_eq!(text, "Hello world");
        assert!(done);
    }

    #[tokio::test]
    async fn stream_chat_parses_ollama_ndjson() {
        async fn ndjson() -> impl IntoResponse {
            let chunks = vec![
                format!(
                    "{}\n",
                    json!({"message": {"content": "foo"}, "done": false})
                ),
                format!(
                    "{}\n",
                    json!({"message": {"content": "bar"}, "done": false})
                ),
                format!("{}\n", json!({"done": true})),
            ];
            let stream = futures_lite::stream::iter(
                chunks.into_iter().map(Ok::<_, std::convert::Infallible>),
            );
            Response::builder().body(Body::from_stream(stream)).unwrap()
        }
        // No /v1 suffix → Ollama native NDJSON parser.
        let url = serve(Router::new().route("/api/chat", post(ndjson))).await;
        let cfg = test_config(&url);
        let rx = stream_chat(&cfg, &[], None).await.unwrap();
        let (text, done) = collect_stream(rx).await;
        assert_eq!(text, "foobar");
        assert!(done);
    }

    #[tokio::test]
    async fn stream_chat_errors_on_non_retryable_status() {
        async fn bad() -> impl IntoResponse {
            axum::http::StatusCode::UNAUTHORIZED
        }
        let url = serve(Router::new().route("/v1/chat/completions", post(bad))).await;
        let cfg = test_config(&format!("{url}/v1"));
        let err = stream_chat(&cfg, &[], None).await.unwrap_err();
        assert!(err.contains("401"), "err was: {err}");
    }
}
