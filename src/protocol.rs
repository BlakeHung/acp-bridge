use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::PathBuf;
use std::time::Instant;

/// ACP wire-format protocol version negotiated at `initialize` time.
///
/// The ACP working group uses a single integer that is bumped only for
/// breaking changes. acp-bridge supports v1 (stable, the version most
/// Clients speak today) and v2 (released 2026, see
/// <https://agentclientprotocol.com/protocol/v2/initialization>).
///
/// On `initialize` the Client's requested version is compared against the
/// set we support. We echo the negotiated value back so it knows whether
/// to stay on its preferred version or follow our fallback.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ProtocolVersion(pub u16);

impl ProtocolVersion {
    /// v1: the version Zed, JetBrains, ACP UI, ACP Inspector, Meuxe,
    /// and Codex CLI adapter all speak today. Stable since 2025.
    pub const V1: ProtocolVersion = ProtocolVersion(1);
    /// v2: released 2026. Unifies `agentCapabilities`/`clientCapabilities`
    /// into a single `capabilities`, adds `messageId`-keyed message
    /// upserts, and removes the `tool_call` notification in favour of
    /// `tool_call_update` as an upsert keyed by `toolCallId`.
    pub const V2: ProtocolVersion = ProtocolVersion(2);

    /// Highest version we support. Negotiated as the fallback when the
    /// Client requests something newer.
    pub const LATEST: ProtocolVersion = Self::V2;

    pub fn as_u16(self) -> u16 {
        self.0
    }
}

impl Default for ProtocolVersion {
    fn default() -> Self {
        // Defaulting to V1 is the conservative choice — Clients that
        // don't bother sending a protocolVersion still get a working
        // session on the wire shape every existing Client speaks.
        Self::V1
    }
}

impl std::fmt::Display for ProtocolVersion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "v{}", self.0)
    }
}

/// JSON-RPC 2.0 request/response ID. Per the spec an `id` may be a
/// string or a number; a `null`/absent `id` marks a notification.
///
/// acp-bridge must preserve the original type and value so clients that use
/// UUID/string IDs (e.g. Meuxe) get their exact ID echoed back.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum RequestId {
    Number(u64),
    String(String),
}

impl RequestId {
    /// Render the ID back into its JSON representation.
    pub fn as_value(&self) -> Value {
        match self {
            RequestId::Number(n) => Value::Number((*n).into()),
            RequestId::String(s) => Value::String(s.clone()),
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct JsonRpcRequest {
    /// Present for requests, absent for notifications (e.g. `session/cancel`).
    pub id: Option<RequestId>,
    pub method: String,
    pub params: Option<Value>,
}

#[derive(Debug, Clone)]
pub struct Session {
    pub messages: Vec<Value>,
    /// Last activity timestamp for idle timeout.
    pub last_active: Instant,
    /// Working directory for this session (used for tool sandboxing).
    pub working_dir: PathBuf,
    /// ACP wire-format protocol version this session uses. Inherited
    /// from the AppState at session creation; the emit helpers branch
    /// on it so a v2 Client gets v2-shaped notifications even if a v1
    /// Client later connects to the same process.
    pub protocol_version: ProtocolVersion,
    /// MCP servers the Client passed in `session/new` (e.g. Xcode's tool
    /// server). Their processes live as long as the session.
    pub mcp: Option<std::sync::Arc<crate::mcp::McpSet>>,
    /// Whether the MCP tool guidance has been appended to the system prompt.
    pub mcp_prompt_applied: bool,
}

impl Session {
    pub fn new(
        system_message: Value,
        working_dir: PathBuf,
        protocol_version: ProtocolVersion,
    ) -> Self {
        Self {
            messages: vec![system_message],
            last_active: Instant::now(),
            working_dir,
            protocol_version,
            mcp: None,
            mcp_prompt_applied: false,
        }
    }

    pub fn touch(&mut self) {
        self.last_active = Instant::now();
    }
}

/// Rough token estimate for one chat message (chars / 4). Counts text
/// content and tool-call names/arguments — a `write_file` call carrying a
/// whole file is as expensive as the file itself.
pub fn estimate_message_tokens(m: &Value) -> u64 {
    let mut chars = match m.get("content") {
        Some(Value::String(s)) => s.len(),
        Some(Value::Array(arr)) => arr
            .iter()
            .filter_map(|b| b.get("text").and_then(|v| v.as_str()))
            .map(|s| s.len())
            .sum(),
        _ => 0,
    };
    if let Some(calls) = m.get("tool_calls").and_then(|v| v.as_array()) {
        for c in calls {
            let f = &c["function"];
            chars += f["name"].as_str().map_or(0, str::len);
            chars += match &f["arguments"] {
                Value::String(s) => s.len(),
                Value::Null => 0,
                other => other.to_string().len(),
            };
        }
    }
    (chars / 4) as u64
}

impl Session {
    /// Keep the system prompt + the last `max_turns` turns.
    /// See [`Session::trim_history_to`].
    pub fn trim_history(&mut self, max_turns: usize) {
        self.trim_history_to(max_turns, None);
    }

    /// Trim conversation history by turn count and/or token budget.
    ///
    /// A *turn* starts at a `user` message and includes everything up to the
    /// next one: the assistant's tool calls, the tool results and the final
    /// answer. Cutting only at user messages guarantees a tool result is
    /// never separated from the assistant message that requested it
    /// (OpenAI-compatible servers reject orphaned `tool` messages).
    ///
    /// - `max_turns`: keep at most this many turns (`0` = no turn cap).
    /// - `max_tokens`: then drop the oldest turns while the estimated size
    ///   exceeds this budget. The latest turn is always kept, even if it
    ///   alone exceeds the budget.
    ///
    /// The system prompt (first message) is always preserved. Returns the
    /// number of messages removed.
    pub fn trim_history_to(&mut self, max_turns: usize, max_tokens: Option<u64>) -> usize {
        let starts: Vec<usize> = self
            .messages
            .iter()
            .enumerate()
            .skip(1)
            .filter(|(_, m)| m.get("role").and_then(|r| r.as_str()) == Some("user"))
            .map(|(i, _)| i)
            .collect();
        if starts.is_empty() {
            return 0;
        }

        // Index into `starts` of the first turn to keep.
        let mut k = 0;
        if max_turns > 0 && starts.len() > max_turns {
            k = starts.len() - max_turns;
        }

        if let Some(budget) = max_tokens {
            let sizes: Vec<u64> = self.messages.iter().map(estimate_message_tokens).collect();
            let system = sizes[0];
            // suffix[i] = tokens of messages[i..]
            let mut suffix = vec![0u64; sizes.len() + 1];
            for i in (0..sizes.len()).rev() {
                suffix[i] = suffix[i + 1] + sizes[i];
            }
            while k + 1 < starts.len() && system + suffix[starts[k]] > budget {
                k += 1;
            }
        }

        let first_keep = starts[k];
        if first_keep <= 1 {
            return 0;
        }
        self.messages.drain(1..first_keep);
        first_keep - 1
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
