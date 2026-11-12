//! Configuration — TOML file with env var override.
//!
//! Priority: env var > config file > default.
//! When spawned by openab, env vars are sufficient (no config file needed).
//! For standalone deployment, use a config file.

use serde::Deserialize;
use std::path::Path;
use tracing::{info, warn};

use crate::llm::LlmConfig;

/// On-disk config file structure.
#[derive(Debug, Deserialize, Default)]
pub struct ConfigFile {
    #[serde(default)]
    pub llm: LlmSection,
    #[serde(default)]
    pub mcp: McpSection,
}

/// `[mcp]` — relaying the Client's MCP servers (e.g. Xcode's tools).
#[derive(Debug, Deserialize, Default, Clone)]
pub struct McpSection {
    /// Honour `mcpServers` from `session/new`. Mirrors `LLM_MCP`.
    pub enabled: Option<bool>,
    /// `auto` | `on` | `off`. Mirrors `LLM_XCODE_TOOLS`.
    pub xcode_tools: Option<String>,
    /// Only expose these MCP tools (`*` wildcard). Mirrors `LLM_MCP_TOOLS`.
    pub tools: Option<Vec<String>>,
    /// Never expose these MCP tools. Mirrors `LLM_MCP_EXCLUDE_TOOLS`.
    pub exclude_tools: Option<Vec<String>>,
    /// Timeout for one MCP tool call (`0` = none). Mirrors `LLM_MCP_TOOL_TIMEOUT`.
    pub tool_timeout_secs: Option<u64>,
    /// Max time for an MCP server's handshake (`0` = none). Mirrors
    /// `LLM_MCP_CONNECT_TIMEOUT`.
    pub connect_timeout_secs: Option<u64>,
}

/// An env var lookup: `Fn(name) -> Option<String>`, `None` when unset.
///
/// Config merging takes this instead of reading [`std::env`] directly so
/// tests can supply values from a map instead of mutating the process
/// environment — env mutation races other tests running on parallel
/// threads. Production goes through [`mcp_config`] /
/// [`ConfigFile::into_llm_config`], which pass `|k| std::env::var(k).ok()`.
pub type EnvLookup<'a> = &'a dyn Fn(&str) -> Option<String>;

fn process_env(name: &str) -> Option<String> {
    std::env::var(name).ok()
}

/// Build the MCP relay config: env var > `[mcp]` section > default.
pub fn mcp_config(file: Option<&McpSection>) -> crate::mcp::McpConfig {
    mcp_config_with_env(file, &process_env)
}

/// [`mcp_config`] with an injected env lookup (see [`EnvLookup`]).
pub fn mcp_config_with_env(file: Option<&McpSection>, env: EnvLookup) -> crate::mcp::McpConfig {
    use crate::mcp::{parse_list, McpConfig, XcodeMode};
    let file = file.cloned().unwrap_or_default();
    let defaults = McpConfig::default();
    let xcode = env("LLM_XCODE_TOOLS")
        .or(file.xcode_tools)
        .map(|v| {
            XcodeMode::parse(&v).unwrap_or_else(|| {
                warn!(value = %v, "Invalid LLM_XCODE_TOOLS (expected auto|on|off), using auto");
                XcodeMode::Auto
            })
        })
        .unwrap_or(defaults.xcode);
    McpConfig {
        enabled: env("LLM_MCP")
            .as_deref()
            .and_then(crate::llm::parse_bool_value)
            .or(file.enabled)
            .unwrap_or(defaults.enabled),
        xcode,
        include: env("LLM_MCP_TOOLS")
            .map(|v| parse_list(&v))
            .or(file.tools)
            .unwrap_or_default(),
        exclude: env("LLM_MCP_EXCLUDE_TOOLS")
            .map(|v| parse_list(&v))
            .or(file.exclude_tools)
            .unwrap_or_default(),
        tool_timeout_secs: env("LLM_MCP_TOOL_TIMEOUT")
            .and_then(|v| v.parse().ok())
            .or(file.tool_timeout_secs)
            .unwrap_or(defaults.tool_timeout_secs),
        connect_timeout_secs: env("LLM_MCP_CONNECT_TIMEOUT")
            .and_then(|v| v.parse().ok())
            .or(file.connect_timeout_secs)
            .unwrap_or(defaults.connect_timeout_secs),
    }
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
    /// Whether the backend accepts image content blocks. Mirrors the
    /// `LLM_SUPPORTS_IMAGE` env var; the env var takes precedence.
    #[serde(default)]
    pub supports_image: Option<bool>,
    /// Model context window in tokens. Mirrors the `LLM_MODEL_CONTEXT` env
    /// var; the env var takes precedence. Reported as `size` in
    /// `usage_update` notifications.
    #[serde(default)]
    pub model_context: Option<u64>,
    /// Max tool-call rounds per turn (`0` = unlimited). Mirrors
    /// `LLM_MAX_TOOL_ROUNDS`.
    #[serde(default)]
    pub max_tool_rounds: Option<usize>,
    /// Timeout in seconds for one `bash` tool call (`0` = none). Mirrors
    /// `LLM_BASH_TIMEOUT`.
    #[serde(default)]
    pub bash_timeout_secs: Option<u64>,
    /// Stream agent-loop responses. Mirrors `LLM_STREAM`.
    #[serde(default)]
    pub stream: Option<bool>,
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
        self.into_llm_config_with_env(&process_env)
    }

    /// [`ConfigFile::into_llm_config`] with an injected env lookup (see
    /// [`EnvLookup`]). Tests use this to avoid mutating the process
    /// environment, which races with other tests.
    pub fn into_llm_config_with_env(self, env: EnvLookup) -> LlmConfig {
        let file = self.llm;

        // Helper: env var wins, then config file, then default
        let base_url = env("LLM_BASE_URL")
            .or_else(|| env("OLLAMA_BASE_URL"))
            .or(file.base_url)
            .unwrap_or_else(|| "http://localhost:11434/v1".into());

        let model = env("LLM_MODEL")
            .or_else(|| env("OLLAMA_MODEL"))
            .or(file.model)
            .unwrap_or_else(|| "gemma4:26b".into());

        let api_key = env("LLM_API_KEY")
            .or_else(|| env("OLLAMA_API_KEY"))
            .or(file.api_key)
            .unwrap_or_else(|| "local-ai".into());

        let system_prompt = env("LLM_SYSTEM_PROMPT").or(file.system_prompt);

        let temperature = env("LLM_TEMPERATURE")
            .and_then(|v| v.parse::<f64>().ok())
            .or(file.temperature)
            .filter(|t| t.is_finite());

        let max_tokens = env("LLM_MAX_TOKENS")
            .and_then(|v| v.parse().ok())
            .or(file.max_tokens);

        let timeout_secs = env("LLM_TIMEOUT")
            .and_then(|v| v.parse().ok())
            .or(file.timeout_secs)
            .unwrap_or(300);

        let max_history_turns = env("LLM_MAX_HISTORY_TURNS")
            .and_then(|v| v.parse().ok())
            .or(file.max_history_turns)
            .unwrap_or(50);

        let max_sessions = env("LLM_MAX_SESSIONS")
            .and_then(|v| v.parse().ok())
            .or(file.max_sessions)
            .unwrap_or(0);

        let session_idle_timeout_secs = env("LLM_SESSION_IDLE_TIMEOUT")
            .and_then(|v| v.parse().ok())
            .or(file.session_idle_timeout_secs)
            .unwrap_or(0);

        let prompt_supports_image = match env("LLM_SUPPORTS_IMAGE").as_deref() {
            Some("1") | Some("true") | Some("yes") | Some("on") => true,
            Some("0") | Some("false") | Some("no") | Some("off") => false,
            _ => file.supports_image.unwrap_or(false),
        };

        let context_size = env("LLM_MODEL_CONTEXT")
            .and_then(|v| v.parse().ok())
            .or(file.model_context)
            .unwrap_or(32768);

        let max_tool_rounds = env("LLM_MAX_TOOL_ROUNDS")
            .and_then(|v| v.parse().ok())
            .or(file.max_tool_rounds)
            .unwrap_or(crate::llm::DEFAULT_MAX_TOOL_ROUNDS);

        let bash_timeout_secs = env("LLM_BASH_TIMEOUT")
            .and_then(|v| v.parse().ok())
            .or(file.bash_timeout_secs)
            .unwrap_or(crate::llm::DEFAULT_BASH_TIMEOUT_SECS);

        let stream = env("LLM_STREAM")
            .as_deref()
            .and_then(crate::llm::parse_bool_value)
            .or(file.stream)
            .unwrap_or(true);

        let client = crate::llm::build_http_client(timeout_secs);

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
            prompt_supports_image,
            context_size,
            max_tool_rounds,
            bash_timeout_secs,
            stream,
            client,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{ConfigFile, LlmSection};
    use std::collections::HashMap;

    /// A config file that sets only `system_prompt`, for precedence tests.
    fn cfg_with_file_prompt() -> ConfigFile {
        ConfigFile {
            llm: LlmSection {
                system_prompt: Some("from file".into()),
                ..LlmSection::default()
            },
            ..ConfigFile::default()
        }
    }

    /// Build an env lookup from a map so tests never touch the process
    /// environment (`set_var`/`remove_var` race other tests).
    fn env_from(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn llm_config_uses_system_prompt_from_file_when_env_missing() {
        let env = env_from(&[]);
        let llm = cfg_with_file_prompt().into_llm_config_with_env(&|k| env.get(k).cloned());
        assert_eq!(llm.system_prompt.as_deref(), Some("from file"));
    }

    #[test]
    fn llm_config_env_system_prompt_overrides_file() {
        let env = env_from(&[("LLM_SYSTEM_PROMPT", "from env")]);
        let llm = cfg_with_file_prompt().into_llm_config_with_env(&|k| env.get(k).cloned());
        assert_eq!(llm.system_prompt.as_deref(), Some("from env"));
    }

    #[test]
    fn llm_config_ollama_fallback_vars_used_when_llm_vars_missing() {
        let env = env_from(&[
            ("OLLAMA_BASE_URL", "http://ollama:11434/v1"),
            ("OLLAMA_MODEL", "qwen2:7b"),
        ]);
        let llm = ConfigFile::default().into_llm_config_with_env(&|k| env.get(k).cloned());
        assert_eq!(llm.base_url, "http://ollama:11434/v1");
        assert_eq!(llm.model, "qwen2:7b");
    }

    #[test]
    fn llm_config_llm_vars_take_precedence_over_ollama_vars() {
        let env = env_from(&[
            ("LLM_BASE_URL", "http://vllm:8000/v1"),
            ("OLLAMA_BASE_URL", "http://ollama:11434/v1"),
        ]);
        let llm = ConfigFile::default().into_llm_config_with_env(&|k| env.get(k).cloned());
        assert_eq!(llm.base_url, "http://vllm:8000/v1");
    }
}
