//! acp-bridge — Library for building ACP/A2A adapters on local AI.
//!
//! Provides:
//! - `engine` — Transport-agnostic business logic (sessions, LLM, tools)
//! - `a2a` — A2A HTTP transport and Agent Card
//! - `llm` — OpenAI-compatible streaming HTTP client
//! - `protocol` — JSON-RPC 2.0 types and error codes
//! - `acp` — ACP notification/response helpers
//! - `tools` — Built-in sandboxed file tools
//! - `hardware` — Best-effort host backend (GPU / accel) detection

pub mod a2a;
pub mod acp;
pub mod bench;
pub mod client;
pub mod config;
pub mod engine;
pub mod hardware;
pub mod llm;
pub mod protocol;
pub mod tools;
