//! acp-bridge — Library for building ACP adapters on local AI.
//!
//! Provides:
//! - `llm` — OpenAI-compatible streaming HTTP client
//! - `protocol` — JSON-RPC 2.0 types and error codes
//! - `acp` — ACP notification/response helpers

pub mod acp;
pub mod config;
pub mod llm;
pub mod protocol;
