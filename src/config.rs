//! Configuration — TOML file with env var override.
//!
//! Priority: env var > config file > default.
//! When spawned by openab, env vars are sufficient (no config file needed).
//! For standalone deployment, use a config file.

use serde::Deserialize;
use std::path::Path;
use tracing::{info, warn};

use crate::llm::LlmConfig;
use reqwest::Client;
use std::time::Duration;

/// On-disk config file structure.
#[derive(Debug, Deserialize, Default)]
pub struct ConfigFile {
    #[serde(default)]
    pub llm: LlmSection,
}

#[derive(Debug, Deserialize, Default)]
pub struct LlmSection {
    pub base_url: Option<String>,
    pub model: Option<String>,
    pub api_key: Option<String>,
    pub system_prompt: Option<String>,
    pub temperature: Option<f64>,
    pub max_tokens: Option<u64>,
    pub timeout_secs: Option<u64>,
    pub max_history_turns: Option<usize>,
    pub max_sessions: Option<usize>,
    pub session_idle_timeout_secs: Option<u64>,
}

impl ConfigFile {
    /// Try to load from a TOML file path. Returns default if file doesn't exist.
    pub fn load(path: &Path) -> Self {
        match std::fs::read_to_string(path) {
            Ok(content) => match toml::from_str(&content) {
                Ok(cfg) => {
                    info!(path = %path.display(), "Loaded config file");
                    cfg
                }
                Err(e) => {
                    warn!(path = %path.display(), error = %e, "Failed to parse config file, using defaults");
                    Self::default()
                }
            },
            Err(_) => Self::default(),
        }
    }

    /// Merge config file values into LlmConfig. Env vars always take precedence.
    pub fn into_llm_config(self) -> LlmConfig {
        let file = self.llm;

        // Helper: env var wins, then config file, then default
        let base_url = std::env::var("LLM_BASE_URL")
            .or_else(|_| std::env::var("OLLAMA_BASE_URL"))
            .ok()
            .or(file.base_url)
            .unwrap_or_else(|| "http://localhost:11434/v1".into());

        let model = std::env::var("LLM_MODEL")
            .or_else(|_| std::env::var("OLLAMA_MODEL"))
            .ok()
            .or(file.model)
            .unwrap_or_else(|| "gemma4:26b".into());

        let api_key = std::env::var("LLM_API_KEY")
            .or_else(|_| std::env::var("OLLAMA_API_KEY"))
            .ok()
            .or(file.api_key)
            .unwrap_or_else(|| "local-ai".into());

        let temperature = std::env::var("LLM_TEMPERATURE")
            .ok()
            .and_then(|v| v.parse::<f64>().ok())
            .or(file.temperature)
            .filter(|t| t.is_finite());

        let max_tokens = std::env::var("LLM_MAX_TOKENS")
            .ok()
            .and_then(|v| v.parse().ok())
            .or(file.max_tokens);

        let timeout_secs = std::env::var("LLM_TIMEOUT")
            .ok()
            .and_then(|v| v.parse().ok())
            .or(file.timeout_secs)
            .unwrap_or(300);

        let max_history_turns = std::env::var("LLM_MAX_HISTORY_TURNS")
            .ok()
            .and_then(|v| v.parse().ok())
            .or(file.max_history_turns)
            .unwrap_or(50);

        let max_sessions = std::env::var("LLM_MAX_SESSIONS")
            .ok()
            .and_then(|v| v.parse().ok())
            .or(file.max_sessions)
            .unwrap_or(0);

        let session_idle_timeout_secs = std::env::var("LLM_SESSION_IDLE_TIMEOUT")
            .ok()
            .and_then(|v| v.parse().ok())
            .or(file.session_idle_timeout_secs)
            .unwrap_or(0);

        let client = Client::builder()
            .timeout(Duration::from_secs(timeout_secs))
            .pool_max_idle_per_host(4)
            .build()
            .expect("Failed to create HTTP client");

        LlmConfig {
            base_url,
            model,
            api_key,
            temperature,
            max_tokens,
            timeout_secs,
            max_history_turns,
            max_sessions,
            session_idle_timeout_secs,
            client,
        }
    }
}
