//! acp-bridge — Minimal ACP adapter for local AI.
//!
//! Provides:
//! - `engine` — Transport-agnostic business logic (sessions, LLM, tools)
//! - `llm` — Pluggable local LLM backend abstraction (Ollama native + OpenAI-compatible)
//! - `protocol` — JSON-RPC 2.0 types and error codes
//! - `acp` — ACP notification/response helpers
//! - `tools` — Built-in sandboxed file tools
//! - `hardware` — Best-effort host backend (GPU / accel) detection

pub mod acp;
pub mod bench;
pub mod config;
pub mod engine;
pub mod hardware;
pub mod llm;
pub mod protocol;
pub mod tools;
