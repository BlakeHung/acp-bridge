//! Configuration — TOML file with env var override.
//!
//! Priority: env var > config file > default.
//! When spawned by openab, env vars are sufficient (no config file needed).
//! For standalone deployment, use a config file.

use serde::Deserialize;
use std::path::Path;
use tracing::{info, warn};

use crate::a2a::A2aConfig;
use crate::llm::LlmConfig;
use reqwest::Client;
use std::collections::HashMap;
use std::time::Duration;

/// On-disk config file structure.
#[derive(Debug, Deserialize, Default)]
pub struct ConfigFile {
    #[serde(default)]
    pub llm: LlmSection,
    #[serde(default)]
    pub a2a: A2aSection,
    #[serde(default)]
    pub agent: Option<AgentSection>,
}

#[derive(Debug, Deserialize, Default)]
pub struct A2aSection {
    pub host: Option<String>,
    pub port: Option<u16>,
    pub agent_name: Option<String>,
    pub agent_description: Option<String>,
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

/// Config for an external ACP agent to spawn (in client mode).
#[derive(Debug, Deserialize, Default)]
pub struct AgentSection {
    pub command: Option<String>,
    pub args: Option<Vec<String>>,
    pub working_dir: Option<String>,
    #[serde(default)]
    pub env: Option<HashMap<String, String>>,
}

/// Runtime config for spawning an external ACP agent.
#[derive(Debug, Clone)]
pub struct AgentConfig {
    pub command: String,
    pub args: Vec<String>,
    pub working_dir: String,
    pub env: HashMap<String, String>,
}

impl AgentConfig {
    /// Build from environment variables only (no config file).
    pub fn from_env() -> Option<Self> {
        let command = std::env::var("AGENT_COMMAND").ok()?;
        Some(Self {
            command,
            args: std::env::var("AGENT_ARGS")
                .ok()
                .map(|a| a.split_whitespace().map(String::from).collect())
                .unwrap_or_default(),
            working_dir: std::env::var("AGENT_WORKING_DIR").unwrap_or_else(|_| "/tmp".into()),
            env: HashMap::new(),
        })
    }
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

    /// Build AgentConfig from file + env vars. Returns None if no agent configured.
    pub fn agent_config(&self) -> Option<AgentConfig> {
        let section = self.agent.as_ref();

        let command = std::env::var("AGENT_COMMAND")
            .ok()
            .or_else(|| section.and_then(|s| s.command.clone()))?;

        let args = std::env::var("AGENT_ARGS")
            .ok()
            .map(|a| a.split_whitespace().map(String::from).collect())
            .or_else(|| section.and_then(|s| s.args.clone()))
            .unwrap_or_default();

        let working_dir = std::env::var("AGENT_WORKING_DIR")
            .ok()
            .or_else(|| section.and_then(|s| s.working_dir.clone()))
            .unwrap_or_else(|| "/tmp".into());

        let env = section.and_then(|s| s.env.clone()).unwrap_or_default();

        Some(AgentConfig {
            command,
            args,
            working_dir,
            env,
        })
    }

    /// Build A2aConfig from file + env vars.
    pub fn a2a_config(&self) -> A2aConfig {
        let defaults = A2aConfig::default();
        A2aConfig {
            host: std::env::var("A2A_HOST")
                .ok()
                .or_else(|| self.a2a.host.clone())
                .unwrap_or(defaults.host),
            port: std::env::var("A2A_PORT")
                .ok()
                .and_then(|v| v.parse().ok())
                .or(self.a2a.port)
                .unwrap_or(defaults.port),
            agent_name: std::env::var("A2A_AGENT_NAME")
                .ok()
                .or_else(|| self.a2a.agent_name.clone())
                .unwrap_or(defaults.agent_name),
            agent_description: std::env::var("A2A_AGENT_DESCRIPTION")
                .ok()
                .or_else(|| self.a2a.agent_description.clone())
                .unwrap_or(defaults.agent_description),
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

        let system_prompt = std::env::var("LLM_SYSTEM_PROMPT")
            .ok()
            .or(file.system_prompt);

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
            system_prompt,
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
            ..ConfigFile::default()
        };

        let llm = cfg.into_llm_config();
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
            ..ConfigFile::default()
        };

        let llm = cfg.into_llm_config();
        assert_eq!(llm.system_prompt.as_deref(), Some("from env"));

        std::env::remove_var("LLM_SYSTEM_PROMPT");
    }
}
