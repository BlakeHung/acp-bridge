# Changelog

All notable changes to this project will be documented in this file.

Format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).
This project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.2.0] - 2026-04-09

### Added
- **Structured logging** — replaced `eprintln` with `tracing`. Control verbosity via `RUST_LOG` env var (default: `acp_bridge=info`).
- **Structured error types** — `AcpError` enum with proper JSON-RPC error codes (`-32602` invalid params, `-32001` unknown session, `-32601` method not found, `-32003` LLM error).
- **Conversation history auto-trim** — `LLM_MAX_HISTORY_TURNS` (default 50) prevents memory growth in long sessions. System prompt is always preserved.
- **LLM HTTP retry with exponential backoff** — transient errors (408, 429, 500-504) and connection timeouts retried up to 3 times (500ms, 1s, 2s).
- **Graceful shutdown** — handles SIGINT/SIGTERM and stdin EOF, drains sessions cleanly.
- **TOML config file support** — `./acp-bridge config.toml`. Priority: env var > config file > defaults.
- **Dockerfile** — multi-stage build, non-root user, ~15MB image.
- **GitHub Actions CI** — `cargo check` + `cargo test` + `cargo clippy` + `cargo fmt`.
- **Unit tests** — 14 test cases covering JSON-RPC parsing, history trimming, error codes, config loading.
- **`--version` flag** — prints version and exits.

### Changed
- RwLock poisoning now recovers gracefully instead of panicking.
- Error responses use correct JSON-RPC error codes instead of generic `-32600`.

### Fixed
- Potential memory leak from unbounded conversation history accumulation.

## [0.1.0] - 2026-04-01

### Added
- Initial release.
- ACP JSON-RPC 2.0 transport over stdin/stdout.
- OpenAI-compatible streaming HTTP client (SSE).
- Multi-session support with conversation history.
- Support for Ollama, LocalAI, vLLM, llama.cpp, LM Studio, text-generation-webui, Jan.ai, Tabby.
- ACP methods: `initialize`, `session/new`, `session/prompt`, `session/end`.
- ACP notifications: `agent_message_chunk`, `agent_thought_chunk`, `tool_call`, `tool_call_update`.
