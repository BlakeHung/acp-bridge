use serde::Deserialize;
use serde_json::Value;
use std::time::Instant;

#[derive(Debug, Deserialize)]
pub struct JsonRpcRequest {
    pub id: u64,
    pub method: String,
    pub params: Option<Value>,
}

pub struct Session {
    pub messages: Vec<Value>,
    /// Last activity timestamp for idle timeout.
    pub last_active: Instant,
}

impl Session {
    pub fn new(system_message: Value) -> Self {
        Self {
            messages: vec![system_message],
            last_active: Instant::now(),
        }
    }

    pub fn touch(&mut self) {
        self.last_active = Instant::now();
    }
}

impl Session {
    /// Trim conversation history to keep the system prompt + last `max_turns` pairs.
    /// Each "turn" = one user message + one assistant message.
    /// The system prompt (first message) is always preserved.
    pub fn trim_history(&mut self, max_turns: usize) {
        // messages[0] = system prompt, then alternating user/assistant
        let keep = max_turns * 2; // user + assistant per turn
        if self.messages.len() > keep + 1 {
            let system = self.messages[0].clone();
            let tail = self.messages.split_off(self.messages.len() - keep);
            self.messages = vec![system];
            self.messages.extend(tail);
        }
    }
}

/// ACP-layer error codes following JSON-RPC 2.0 conventions.
#[derive(Debug, thiserror::Error)]
pub enum AcpError {
    #[error("Missing required parameter: {field}")]
    MissingParam { field: String },

    #[error("Unknown session: {session_id}")]
    UnknownSession { session_id: String },

    #[error("Method not found: {method}")]
    MethodNotFound { method: String },

    #[error("LLM communication error: {reason}")]
    LlmError { reason: String },

    #[error("Session limit reached (max: {max})")]
    SessionLimitReached { max: usize },
}

impl AcpError {
    /// JSON-RPC error code for this variant.
    pub fn code(&self) -> i64 {
        match self {
            AcpError::MissingParam { .. } => -32602,   // Invalid params
            AcpError::UnknownSession { .. } => -32001, // Application error
            AcpError::MethodNotFound { .. } => -32601, // Method not found
            AcpError::LlmError { .. } => -32003,       // Application error
            AcpError::SessionLimitReached { .. } => -32004, // Application error
        }
    }
}
