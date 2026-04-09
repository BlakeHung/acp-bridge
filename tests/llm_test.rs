use acp_bridge::llm::LlmConfig;

#[test]
fn config_defaults() {
    // Clear any env vars that might interfere
    std::env::remove_var("LLM_BASE_URL");
    std::env::remove_var("OLLAMA_BASE_URL");
    std::env::remove_var("LLM_MODEL");
    std::env::remove_var("OLLAMA_MODEL");
    std::env::remove_var("LLM_API_KEY");
    std::env::remove_var("OLLAMA_API_KEY");
    std::env::remove_var("LLM_TEMPERATURE");
    std::env::remove_var("LLM_MAX_TOKENS");
    std::env::remove_var("LLM_TIMEOUT");
    std::env::remove_var("LLM_MAX_HISTORY_TURNS");

    let config = LlmConfig::from_env();

    assert_eq!(config.base_url, "http://localhost:11434/v1");
    assert_eq!(config.model, "gemma4:26b");
    assert_eq!(config.api_key, "local-ai");
    assert!(config.temperature.is_none());
    assert!(config.max_tokens.is_none());
    assert_eq!(config.timeout_secs, 300);
    assert_eq!(config.max_history_turns, 50);
}

#[test]
fn config_from_llm_env_vars() {
    std::env::set_var("LLM_BASE_URL", "http://gpu-server:8000/v1");
    std::env::set_var("LLM_MODEL", "llama3:8b");
    std::env::set_var("LLM_API_KEY", "secret-key");
    std::env::set_var("LLM_TEMPERATURE", "0.7");
    std::env::set_var("LLM_MAX_TOKENS", "4096");
    std::env::set_var("LLM_TIMEOUT", "120");
    std::env::set_var("LLM_MAX_HISTORY_TURNS", "20");

    let config = LlmConfig::from_env();

    assert_eq!(config.base_url, "http://gpu-server:8000/v1");
    assert_eq!(config.model, "llama3:8b");
    assert_eq!(config.api_key, "secret-key");
    assert_eq!(config.temperature, Some(0.7));
    assert_eq!(config.max_tokens, Some(4096));
    assert_eq!(config.timeout_secs, 120);
    assert_eq!(config.max_history_turns, 20);

    // Cleanup
    std::env::remove_var("LLM_BASE_URL");
    std::env::remove_var("LLM_MODEL");
    std::env::remove_var("LLM_API_KEY");
    std::env::remove_var("LLM_TEMPERATURE");
    std::env::remove_var("LLM_MAX_TOKENS");
    std::env::remove_var("LLM_TIMEOUT");
    std::env::remove_var("LLM_MAX_HISTORY_TURNS");
}

#[test]
fn config_ollama_fallback_vars() {
    std::env::remove_var("LLM_BASE_URL");
    std::env::remove_var("LLM_MODEL");
    std::env::remove_var("LLM_API_KEY");
    std::env::set_var("OLLAMA_BASE_URL", "http://ollama:11434/v1");
    std::env::set_var("OLLAMA_MODEL", "qwen2:7b");
    std::env::set_var("OLLAMA_API_KEY", "ollama-key");

    let config = LlmConfig::from_env();

    assert_eq!(config.base_url, "http://ollama:11434/v1");
    assert_eq!(config.model, "qwen2:7b");
    assert_eq!(config.api_key, "ollama-key");

    // Cleanup
    std::env::remove_var("OLLAMA_BASE_URL");
    std::env::remove_var("OLLAMA_MODEL");
    std::env::remove_var("OLLAMA_API_KEY");
}

#[test]
fn config_invalid_temperature_ignored() {
    std::env::set_var("LLM_TEMPERATURE", "not-a-number");
    let config = LlmConfig::from_env();
    assert!(config.temperature.is_none());
    std::env::remove_var("LLM_TEMPERATURE");
}
