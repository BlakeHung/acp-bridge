//! Configuration — TOML file with env var override.
//!
//! Priority: env var > config file > default.
//! When spawned by openab, env vars are sufficient (no config file needed).
//! For standalone deployment, use a config file.

use serde::Deserialize;
use std::path::Path;
use tracing::info;

use crate::llm::{env_parse, env_parse_bool, ConfigError, LlmConfig};
use reqwest::Client;
use std::time::Duration;

/// On-disk config file structure.
///
/// `deny_unknown_fields` is deliberate: a config that names a section or
/// field this binary doesn't understand (e.g. `[permissions]` written for
/// a newer build) must fail loudly instead of running with the setting
/// silently dropped — see review B7.
#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct ConfigFile {
    #[serde(default)]
    pub llm: LlmSection,
}

#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
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
    /// Whether the backend accepts image content blocks. Mirrors the
    /// `LLM_SUPPORTS_IMAGE` env var; the env var takes precedence.
    #[serde(default)]
    pub supports_image: Option<bool>,
    /// Model context window in tokens. Mirrors the `LLM_MODEL_CONTEXT` env
    /// var; the env var takes precedence. Reported as `size` in
    /// `usage_update` notifications.
    #[serde(default)]
    pub model_context: Option<u64>,
    /// Max model/tool round-trips per turn. Mirrors `LLM_MAX_TOOL_ROUNDS`.
    #[serde(default)]
    pub max_tool_rounds: Option<usize>,
    /// Per-tool wall-clock timeout in seconds. Mirrors `LLM_TOOL_TIMEOUT`.
    #[serde(default)]
    pub tool_timeout_secs: Option<u64>,
    /// Whole-turn deadline in seconds (0 = off). Mirrors `LLM_TURN_TIMEOUT`.
    #[serde(default)]
    pub turn_timeout_secs: Option<u64>,
    /// Abort after N consecutive identical tool failures. Mirrors
    /// `LLM_TOOL_FAILURE_STREAK`.
    #[serde(default)]
    pub tool_failure_streak: Option<usize>,
}

impl ConfigFile {
    /// Load from a TOML file path. Any failure — unreadable file, invalid
    /// TOML, unknown field — is a startup error, never a silent default.
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let content = std::fs::read_to_string(path).map_err(|e| ConfigError::Read {
            path: path.display().to_string(),
            reason: e.to_string(),
        })?;
        let cfg: Self = toml::from_str(&content).map_err(|e| ConfigError::Parse {
            path: path.display().to_string(),
            reason: e.to_string(),
        })?;
        info!(path = %path.display(), "Loaded config file");
        Ok(cfg)
    }

    /// Merge config file values into LlmConfig. Env vars always take precedence.
    /// A present-but-invalid env var is a startup error, not a fallback.
    pub fn into_llm_config(self) -> Result<LlmConfig, ConfigError> {
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

        let system_prompt = std::env::var("LLM_SYSTEM_PROMPT")
            .ok()
            .or(file.system_prompt);

        let temperature = env_parse::<f64>("LLM_TEMPERATURE")?.or(file.temperature);
        if let Some(t) = temperature {
            if !t.is_finite() {
                return Err(ConfigError::InvalidEnv {
                    var: "LLM_TEMPERATURE",
                    value: t.to_string(),
                    reason: "must be finite".into(),
                });
            }
        }

        let max_tokens = env_parse("LLM_MAX_TOKENS")?.or(file.max_tokens);

        let timeout_secs = env_parse("LLM_TIMEOUT")?
            .or(file.timeout_secs)
            .unwrap_or(300);

        let max_history_turns = env_parse("LLM_MAX_HISTORY_TURNS")?
            .or(file.max_history_turns)
            .unwrap_or(50);

        let max_sessions = env_parse("LLM_MAX_SESSIONS")?
            .or(file.max_sessions)
            .unwrap_or(0);

        let session_idle_timeout_secs = env_parse("LLM_SESSION_IDLE_TIMEOUT")?
            .or(file.session_idle_timeout_secs)
            .unwrap_or(0);

        let prompt_supports_image = env_parse_bool("LLM_SUPPORTS_IMAGE")?
            .or(file.supports_image)
            .unwrap_or(false);

        let context_size = env_parse("LLM_MODEL_CONTEXT")?
            .or(file.model_context)
            .unwrap_or(32768);

        let max_tool_rounds = env_parse("LLM_MAX_TOOL_ROUNDS")?
            .or(file.max_tool_rounds)
            .unwrap_or(5);

        let tool_timeout_secs = env_parse("LLM_TOOL_TIMEOUT")?
            .or(file.tool_timeout_secs)
            .unwrap_or(120);

        let turn_timeout_secs = env_parse("LLM_TURN_TIMEOUT")?
            .or(file.turn_timeout_secs)
            .unwrap_or(0);

        let tool_failure_streak = env_parse("LLM_TOOL_FAILURE_STREAK")?
            .or(file.tool_failure_streak)
            .unwrap_or(3);

        let client = Client::builder()
            .timeout(Duration::from_secs(timeout_secs))
            .pool_max_idle_per_host(4)
            .build()
            .expect("Failed to create HTTP client");

        Ok(LlmConfig {
            base_url,
            model,
            api_key,
            system_prompt,
            temperature,
            max_tokens,
            timeout_secs,
            max_history_turns,
            max_sessions,
            session_idle_timeout_secs,
            prompt_supports_image,
            context_size,
            max_tool_rounds,
            tool_timeout_secs,
            turn_timeout_secs,
            tool_failure_streak,
            client,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{ConfigFile, LlmSection};

    #[test]
    fn llm_config_uses_system_prompt_from_file_when_env_missing() {
        std::env::remove_var("LLM_SYSTEM_PROMPT");

        let cfg = ConfigFile {
            llm: LlmSection {
                system_prompt: Some("from file".into()),
                ..LlmSection::default()
            },
        };

        let llm = cfg.into_llm_config().expect("config should merge");
        assert_eq!(llm.system_prompt.as_deref(), Some("from file"));
    }

    #[test]
    fn llm_config_env_system_prompt_overrides_file() {
        std::env::set_var("LLM_SYSTEM_PROMPT", "from env");

        let cfg = ConfigFile {
            llm: LlmSection {
                system_prompt: Some("from file".into()),
                ..LlmSection::default()
            },
        };

        let llm = cfg.into_llm_config().expect("config should merge");
        assert_eq!(llm.system_prompt.as_deref(), Some("from env"));

        std::env::remove_var("LLM_SYSTEM_PROMPT");
    }

    #[test]
    fn invalid_numeric_env_var_is_an_error_not_a_fallback() {
        std::env::set_var("LLM_TIMEOUT", "abc");

        let cfg = ConfigFile::default();
        let err = cfg
            .into_llm_config()
            .err()
            .expect("unparseable LLM_TIMEOUT must fail startup");
        assert!(err.to_string().contains("LLM_TIMEOUT"));

        std::env::remove_var("LLM_TIMEOUT");
    }

    #[test]
    fn invalid_bool_env_var_is_an_error() {
        std::env::set_var("LLM_SUPPORTS_IMAGE", "2");

        let cfg = ConfigFile::default();
        assert!(cfg.into_llm_config().is_err());

        std::env::remove_var("LLM_SUPPORTS_IMAGE");
    }

    #[test]
    fn unknown_toml_fields_are_rejected() {
        let bad = "[permissions]\nallow_all = true\n";
        assert!(toml::from_str::<ConfigFile>(bad).is_err());

        let bad_field = "[llm]\npermisssions = \"all\"\n";
        assert!(toml::from_str::<ConfigFile>(bad_field).is_err());
    }
}
