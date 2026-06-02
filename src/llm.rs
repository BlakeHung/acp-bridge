//! Local AI HTTP client — streams chat completions via SSE or NDJSON.
//! Supports Ollama native API (/api/chat) and any OpenAI-compatible API.

use reqwest::Client;
use serde_json::{json, Value};
use std::time::Duration;
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

/// Probe the backend on startup: check connectivity and list available models.
/// Returns Ok(model_list) on success, Err(reason) on failure.
/// Non-fatal — callers should log the result but not abort.
pub async fn probe_backend(config: &LlmConfig) -> Result<Vec<String>, String> {
    let client = &config.client;

    // Try Ollama-native /api/tags first (works on localhost:11434)
    let base = config
        .base_url
        .trim_end_matches("/v1")
        .trim_end_matches('/');
    let tags_url = format!("{base}/api/tags");

    if let Ok(resp) = client.get(&tags_url).send().await {
        if resp.status().is_success() {
            if let Ok(val) = resp.json::<Value>().await {
                let models: Vec<String> = val["models"]
                    .as_array()
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|m| m["name"].as_str().map(String::from))
                            .collect()
                    })
                    .unwrap_or_default();
                return Ok(models);
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
                let models: Vec<String> = val["data"]
                    .as_array()
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|m| m["id"].as_str().map(String::from))
                            .collect()
                    })
                    .unwrap_or_default();
                return Ok(models);
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
    let base = config
        .base_url
        .trim_end_matches("/v1")
        .trim_end_matches('/');
    let url = format!("{base}/api/ps");
    let resp = config.client.get(&url).send().await.ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let val: Value = resp.json().await.ok()?;
    let models: Vec<String> = val["models"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|m| m["name"].as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    Some(models)
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

/// Returns true if the HTTP status code is transient and worth retrying.
fn is_retryable(status: reqwest::StatusCode) -> bool {
    matches!(status.as_u16(), 408 | 429 | 500 | 502 | 503 | 504)
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
    let client = &config.client;

    let body = build_body(config, messages, model, false, tools);

    let mut last_err = String::new();
    for attempt in 0..=MAX_RETRIES {
        if attempt > 0 {
            let delay = INITIAL_BACKOFF_MS * 2u64.pow(attempt - 1);
            warn!(attempt, delay_ms = delay, "Retrying LLM request");
            tokio::time::sleep(Duration::from_millis(delay)).await;
        }

        let result = client
            .post(&url)
            .header("Content-Type", "application/json")
            .header("Authorization", format!("Bearer {}", config.api_key))
            .json(&body)
            .send()
            .await;

        match result {
            Ok(response) if response.status().is_success() => {
                let val: Value = response
                    .json()
                    .await
                    .map_err(|e| format!("Failed to parse response: {e}"))?;
                return Ok(val);
            }
            Ok(response) if is_retryable(response.status()) => {
                last_err = format!(
                    "LLM HTTP {}: {}",
                    response.status(),
                    response.status().canonical_reason().unwrap_or("error")
                );
                warn!(status = %response.status(), "Transient LLM error");
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
                warn!(error = %e, "Transient connection error");
            }
            Err(e) => return Err(format!("HTTP request failed: {e}")),
        }
    }

    error!(error = %last_err, "All retry attempts exhausted");
    Err(last_err)
}

/// Stream chat completion — auto-detects backend and uses appropriate parser.
pub async fn stream_chat(
    config: &LlmConfig,
    messages: &[Value],
    model_override: Option<&str>,
) -> Result<mpsc::Receiver<StreamChunk>, String> {
    let url = config.chat_url();
    let model = model_override.unwrap_or(&config.model);
    let client = &config.client;
    let is_native = config.is_ollama_native();

    let body = build_body(config, messages, model, true, None);

    // Retry loop for the initial connection
    let mut last_err = String::new();
    let mut response_ok = None;

    for attempt in 0..=MAX_RETRIES {
        if attempt > 0 {
            let delay = INITIAL_BACKOFF_MS * 2u64.pow(attempt - 1);
            warn!(attempt, delay_ms = delay, "Retrying streaming LLM request");
            tokio::time::sleep(Duration::from_millis(delay)).await;
        }

        let result = client
            .post(&url)
            .header("Content-Type", "application/json")
            .header("Authorization", format!("Bearer {}", config.api_key))
            .json(&body)
            .send()
            .await;

        match result {
            Ok(resp) if resp.status().is_success() => {
                response_ok = Some(resp);
                break;
            }
            Ok(resp) if is_retryable(resp.status()) => {
                last_err = format!(
                    "LLM HTTP {}: {}",
                    resp.status(),
                    resp.status().canonical_reason().unwrap_or("error")
                );
                warn!(status = %resp.status(), "Transient LLM error");
            }
            Ok(resp) => {
                return Err(format!(
                    "LLM HTTP {}: {}",
                    resp.status(),
                    resp.status().canonical_reason().unwrap_or("Unknown error")
                ));
            }
            Err(e) if e.is_timeout() || e.is_connect() => {
                last_err = format!("HTTP request failed: {e}");
                warn!(error = %e, "Transient connection error");
            }
            Err(e) => return Err(format!("HTTP request failed: {e}")),
        }
    }

    let response = match response_ok {
        Some(r) => r,
        None => {
            error!(error = %last_err, "All retry attempts exhausted (stream)");
            return Err(last_err);
        }
    };

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
    let mut buffer = String::new();
    const MAX_BUFFER_SIZE: usize = 10 * 1024 * 1024;

    loop {
        let chunk_result: Result<Option<bytes::Bytes>, reqwest::Error> = response.chunk().await;
        match chunk_result {
            Ok(Some(bytes)) => {
                let chunk_str = String::from_utf8_lossy(&bytes);
                if buffer.len() + chunk_str.len() > MAX_BUFFER_SIZE {
                    error!("Stream buffer exceeded limit, aborting");
                    let _ = tx
                        .send(StreamChunk::Error("Stream buffer overflow".into()))
                        .await;
                    return;
                }
                buffer.push_str(&chunk_str);

                while let Some(newline_pos) = buffer.find('\n').or_else(|| buffer.find('\r')) {
                    let line = buffer[..newline_pos].trim_end().to_string();
                    let skip = if buffer[newline_pos..].starts_with("\r\n") {
                        2
                    } else {
                        1
                    };
                    buffer = buffer[newline_pos + skip..].to_string();

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
    let mut buffer = String::new();
    const MAX_BUFFER_SIZE: usize = 10 * 1024 * 1024;

    loop {
        let chunk_result: Result<Option<bytes::Bytes>, reqwest::Error> = response.chunk().await;
        match chunk_result {
            Ok(Some(bytes)) => {
                let chunk_str = String::from_utf8_lossy(&bytes);
                if buffer.len() + chunk_str.len() > MAX_BUFFER_SIZE {
                    error!("Stream buffer exceeded limit, aborting");
                    let _ = tx
                        .send(StreamChunk::Error("Stream buffer overflow".into()))
                        .await;
                    return;
                }
                buffer.push_str(&chunk_str);

                while let Some(newline_pos) = buffer.find('\n').or_else(|| buffer.find('\r')) {
                    let line = buffer[..newline_pos].trim_end().to_string();
                    let skip = if buffer[newline_pos..].starts_with("\r\n") {
                        2
                    } else {
                        1
                    };
                    buffer = buffer[newline_pos + skip..].to_string();

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
