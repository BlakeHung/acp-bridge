# Changelog

All notable changes to this project will be documented in this file.

Format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).
This project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.3.0] - 2026-04-13

### Added
- **Session limits** — `LLM_MAX_SESSIONS` env var to cap concurrent sessions (default 0 = unlimited). Returns JSON-RPC error `-32004` when limit is reached.
- **Session idle timeout** — `LLM_SESSION_IDLE_TIMEOUT` env var to auto-evict idle sessions after N seconds (default 0 = disabled). Background task periodically cleans up.
- **HTTP connection pooling** — reuses a shared `reqwest::Client` across all requests, reducing TCP/TLS handshake overhead.
- **Security and Limitations sections in README**.

### Fixed
- **SSE `\r\n` parsing** — handles both `\r\n` (HTTP standard) and `\n` line endings, fixing silent message loss with some LLM backends.
- **Temperature validation** — clamped to valid 0.0–2.0 range; NaN/Infinity values are filtered out.

### Changed
- Session state tracks `last_active` timestamp for idle timeout support.
- Session access refactored into `sessions_read()` / `sessions_write()` helpers.
- `Session::new()` constructor replaces direct struct initialization.

## [0.2.1] - 2026-04-12

### Fixed
- **CWD prompt injection** — `cwd` parameter in `session/new` is now sanitized to only allow typical path characters, preventing prompt injection attacks.
- **Missing JSON-RPC response on LLM failure** — stream errors and connection failures now always send a JSON-RPC response with `status: "failed"`, preventing client hangs.
- **Unbounded stream buffer** — SSE stream buffer capped at 10MB to prevent OOM from malicious or buggy backends.
- **Flaky env var tests** — config tests now use a mutex to prevent parallel test pollution.

### Added
- **Integration test suite** — 14 tests with a mock LLM server covering the full stdin/stdout JSON-RPC pipeline.

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
