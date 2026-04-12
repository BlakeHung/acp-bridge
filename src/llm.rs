//! Local AI HTTP client — streams chat completions via SSE.
//! Supports any OpenAI-compatible API: Ollama, LocalAI, vLLM, llama.cpp, LM Studio, etc.

use reqwest::Client;
use serde_json::{json, Value};
use std::time::Duration;
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

/// Probe the backend on startup: check connectivity and list available models.
/// Returns Ok(model_list) on success, Err(reason) on failure.
/// Non-fatal — callers should log the result but not abort.
pub async fn probe_backend(config: &LlmConfig) -> Result<Vec<String>, String> {
    let client = Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .map_err(|e| format!("HTTP client error: {e}"))?;

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

/// Maximum number of retry attempts for transient LLM HTTP errors.
const MAX_RETRIES: u32 = 3;
/// Initial backoff delay in milliseconds (doubles each retry).
const INITIAL_BACKOFF_MS: u64 = 500;

pub struct LlmConfig {
    pub base_url: String,
    pub model: String,
    pub api_key: String,
    pub temperature: Option<f64>,
    pub max_tokens: Option<u64>,
    pub timeout_secs: u64,
    /// Maximum conversation turns to keep (0 = unlimited).
    pub max_history_turns: usize,
}

impl LlmConfig {
    pub fn from_env() -> Self {
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
            temperature: std::env::var("LLM_TEMPERATURE")
                .ok()
                .and_then(|v| v.parse().ok()),
            max_tokens: std::env::var("LLM_MAX_TOKENS")
                .ok()
                .and_then(|v| v.parse().ok()),
            timeout_secs: std::env::var("LLM_TIMEOUT")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(300),
            max_history_turns: std::env::var("LLM_MAX_HISTORY_TURNS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(50),
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
        body["temperature"] = json!(temp);
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
    let url = format!("{}/chat/completions", config.base_url);
    let model = model_override.unwrap_or(&config.model);
    let client = Client::builder()
        .timeout(Duration::from_secs(config.timeout_secs))
        .build()
        .map_err(|e| format!("Failed to create HTTP client: {e}"))?;

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
                // Non-retryable HTTP error
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

/// Stream chat completion from any OpenAI-compatible endpoint.
pub async fn stream_chat(
    config: &LlmConfig,
    messages: &[Value],
    model_override: Option<&str>,
) -> Result<mpsc::Receiver<StreamChunk>, String> {
    let url = format!("{}/chat/completions", config.base_url);
    let model = model_override.unwrap_or(&config.model);
    let client = Client::builder()
        .timeout(Duration::from_secs(config.timeout_secs))
        .build()
        .map_err(|e| format!("Failed to create HTTP client: {e}"))?;

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

    tokio::spawn(async move {
        let mut response = response;
        let mut buffer = String::new();
        /// Maximum stream buffer size (10 MB) to prevent unbounded memory growth.
        const MAX_BUFFER_SIZE: usize = 10 * 1024 * 1024;

        loop {
            let chunk_result: Result<Option<bytes::Bytes>, reqwest::Error> = response.chunk().await;
            match chunk_result {
                Ok(Some(bytes)) => {
                    let chunk_str = String::from_utf8_lossy(&bytes);
                    if buffer.len() + chunk_str.len() > MAX_BUFFER_SIZE {
                        error!("Stream buffer exceeded {}MB limit, aborting", MAX_BUFFER_SIZE / 1024 / 1024);
                        let _ = tx.send(StreamChunk::Error("Stream buffer overflow".into())).await;
                        return;
                    }
                    buffer.push_str(&chunk_str);

                    while let Some(newline_pos) = buffer.find('\n') {
                        let line = buffer[..newline_pos].trim().to_string();
                        buffer = buffer[newline_pos + 1..].to_string();

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
                    debug!("Stream ended (no more chunks)");
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
    });

    info!(model, "Streaming started");
    Ok(rx)
}
