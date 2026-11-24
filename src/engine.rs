//! Transport-agnostic engine — shared business logic for ACP and A2A transports.
//!
//! All session management, LLM interaction, and tool execution lives here.
//! Transport layers (stdin/stdout ACP, HTTP A2A) call these functions and
//! deliver results in their own format.

use crate::llm::{self, ImageBlock, LlmConfig, DEFAULT_IMAGE_MIME};
use crate::protocol::{AcpError, Session};
use crate::tools;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::Duration;
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};
use uuid::Uuid;

/// Extract concatenated text from a slice of ACP/A2A content parts.
///
/// Each part is expected to be an object with `"type": "text"` and `"text": "..."`.
/// `ContentBlock::ResourceLink` parts (type == "resource_link") are converted
/// to a `[Attached resource: <uri>]` pseudo-line so the LLM has at least a
/// textual hint that a resource was attached. This satisfies ACP v1's
/// "MUST support ContentBlock::ResourceLink in session/prompt" requirement
/// without acp-bridge having to dereference the URI itself — that is the
/// responsibility of the Client per spec.
pub fn extract_text_parts(parts: &[Value]) -> String {
    let mut lines: Vec<String> = Vec::new();
    for p in parts {
        let Some(t) = p.get("type").and_then(|v| v.as_str()) else {
            continue;
        };
        match t {
            "text" => {
                if let Some(text) = p.get("text").and_then(|v| v.as_str()) {
                    lines.push(text.to_string());
                }
            }
            "resource_link" => {
                if let Some(uri) = p.get("uri").and_then(|v| v.as_str()) {
                    let name = p.get("name").and_then(|v| v.as_str()).unwrap_or("");
                    if name.is_empty() {
                        lines.push(format!("[Attached resource: {uri}]"));
                    } else {
                        lines.push(format!("[Attached resource: {name} ({uri})]"));
                    }
                }
            }
            _ => {}
        }
    }
    lines.join("\n")
}

/// Extract base64 image content blocks from a slice of ACP/A2A content parts.
/// Preserves the per-block `mimeType` so multimodal LLM payloads round-trip
/// correctly (PNG, WebP, GIF, etc. — not just JPEG).
pub fn extract_image_parts(parts: &[Value]) -> Vec<ImageBlock> {
    parts
        .iter()
        .filter(|p| p.get("type").and_then(|t| t.as_str()) == Some("image"))
        .filter_map(|p| {
            let data = p.get("data").and_then(|d| d.as_str())?.to_string();
            let mime_type = p
                .get("mimeType")
                .and_then(|m| m.as_str())
                .unwrap_or(DEFAULT_IMAGE_MIME)
                .to_string();
            Some(ImageBlock { data, mime_type })
        })
        .collect()
}

/// Pull user text out of a `session/prompt` `prompt` parameter, tolerating
/// the three shapes we have seen in the wild:
///
/// 1. ACP spec: `Array<ContentBlock>` — handled by `extract_text_parts`.
/// 2. A single ContentBlock object (some clients send the block directly,
///    not wrapped in a one-element array). ResourceLink is converted to a
///    pseudo-line in the same way as in the array case.
/// 3. A plain string (legacy or simplified clients that put the whole
///    prompt directly in the `prompt` field).
///
/// Returns an empty string when the shape matches none of the above. The
/// caller is responsible for rejecting an empty result with a clear error
/// rather than handing an empty message to the LLM.
pub fn extract_user_text_from_prompt(prompt: &Value) -> String {
    match prompt {
        Value::String(s) => s.clone(),
        Value::Array(arr) => extract_text_parts(arr),
        Value::Object(_) => {
            // ResourceLink-as-the-whole-prompt: turn into a single pseudo-line.
            if prompt.get("type").and_then(|v| v.as_str()) == Some("resource_link") {
                let uri = prompt.get("uri").and_then(|v| v.as_str()).unwrap_or("");
                let name = prompt.get("name").and_then(|v| v.as_str()).unwrap_or("");
                if uri.is_empty() {
                    String::new()
                } else if name.is_empty() {
                    format!("[Attached resource: {uri}]")
                } else {
                    format!("[Attached resource: {name} ({uri})]")
                }
            } else {
                prompt
                    .get("text")
                    .and_then(|t| t.as_str())
                    .map(str::to_owned)
                    .unwrap_or_default()
            }
        }
        _ => String::new(),
    }
}

/// Strip a leading `<sender_context>…</sender_context>` block from user
/// text, returning the cleaned text plus the captured inner string for
/// observability.
///
/// OpenAB and similar harnesses prepend a metadata block of this shape:
///
/// ```text
/// <sender_context>
/// {"schema":"openab.sender.v1","user_id":"…",…}
/// </sender_context>
/// <actual user message>
/// ```
///
/// Some LLMs interpret the XML tag as a directive and stall trying to
/// reconcile the wrapper with their tool-calling instructions — the
/// observed symptom is the model returning empty content with no tool
/// calls. Strip the block before forwarding so the model sees only the
/// real user text; the captured inner string is returned alongside so
/// callers can log it at debug level for traceability.
pub fn strip_sender_context(text: &str) -> (String, Option<String>) {
    const OPEN: &str = "<sender_context>";
    const CLOSE: &str = "</sender_context>";

    let Some(start) = text.find(OPEN) else {
        return (text.to_string(), None);
    };
    let after_open = start + OPEN.len();
    let Some(rel_end) = text[after_open..].find(CLOSE) else {
        return (text.to_string(), None);
    };
    let close_start = after_open + rel_end;
    let close_end = close_start + CLOSE.len();

    let inner = text[after_open..close_start].trim().to_string();
    let cleaned = format!("{}{}", &text[..start], &text[close_end..])
        .trim()
        .to_string();
    (cleaned, Some(inner))
}

/// Pull image content blocks out of a `session/prompt` `prompt` parameter
/// across the same three shapes recognized by
/// [`extract_user_text_from_prompt`]. MIME type is preserved per block.
pub fn extract_user_images_from_prompt(prompt: &Value) -> Vec<ImageBlock> {
    match prompt {
        Value::Array(arr) => extract_image_parts(arr),
        Value::Object(_) => {
            if prompt.get("type").and_then(|t| t.as_str()) == Some("image") {
                let Some(data) = prompt.get("data").and_then(|d| d.as_str()) else {
                    return Vec::new();
                };
                let mime_type = prompt
                    .get("mimeType")
                    .and_then(|m| m.as_str())
                    .unwrap_or(DEFAULT_IMAGE_MIME)
                    .to_string();
                vec![ImageBlock {
                    data: data.to_string(),
                    mime_type,
                }]
            } else {
                Vec::new()
            }
        }
        _ => Vec::new(),
    }
}

// ---------------------------------------------------------------------------
// Notification — transport-agnostic events emitted during prompt processing
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub enum Notification {
    Thinking,
    ToolStart {
        /// ACP v1 `toolCallId` — required for clients to pair tool_call /
        /// tool_call_update updates. The LLM assigns this id per call; for
        /// the synthetic outer `llm_chat` event we mint a stable id derived
        /// from the session/round so clients can render it.
        id: String,
        name: String,
    },
    ToolDone {
        id: String,
        name: String,
        status: String,
    },
    TextChunk(String),
}

// ---------------------------------------------------------------------------
// AppState — shared state for all transports
// ---------------------------------------------------------------------------

pub struct AppState {
    /// Sessions are wrapped in `Arc` so that `AppState: Clone` works
    /// cheaply — the clone shares the same inner map. This is what
    /// lets `main::run_acp_loop` call `Arc::make_mut` to swap in the
    /// negotiated `protocol_version` after `initialize` without
    /// disturbing the live-spawned session map.
    pub sessions: Arc<RwLock<HashMap<String, Session>>>,
    /// One cancellation channel per active ACP prompt. Shared across initialize clones.
    pub turn_registry: Arc<std::sync::Mutex<HashMap<String, tokio::sync::watch::Sender<bool>>>>,
    pub config: LlmConfig,
    /// ACP wire-format protocol version negotiated at `initialize`.
    /// All session(s) opened by this Client inherit this version; the
    /// emit helpers (`acp::notify_*`) branch on it so v1 Clients see
    /// v1 notifications and v2 Clients see v2.
    pub protocol_version: crate::protocol::ProtocolVersion,
    /// Tool-permission policy consulted before every tool dispatch
    /// (P1/E12). `AppState::new` uses the default daily-driver profile;
    /// `main` builds the real one from `[policy]` + `LLM_POLICY_*`.
    pub policy: crate::policy::Policy,
    /// Optional on-disk session snapshot store (P1/E14). `None` →
    /// `session/load` answers `-32001 no_persistence`.
    pub session_store: Option<std::sync::Arc<crate::session_store::SessionStore>>,
    /// Single-use, TTL-bound per-operation permits (G1a round-3):
    /// issued via `session/permit`, consumed by a `confirm`-gated tool
    /// call. Deployment-scope `Policy::grants` stay as the coarse knob;
    /// a permit is the per-operation grant the confirm action wants.
    permits: std::sync::Arc<std::sync::Mutex<HashMap<String, Vec<Permit>>>>,
}

/// A single-use, TTL-bound authorization for one `confirm`-gated
/// capability, optionally scoped to a specific tool within it. Issued
/// by the Client via `session/permit` — that call IS the interactive
/// grant channel the `confirm` action verifies against.
pub struct Permit {
    pub id: String,
    pub capability: crate::policy::Capability,
    /// When set, the permit only unlocks this tool name.
    pub tool: Option<String>,
    pub expires_at: std::time::Instant,
    pub consumed: bool,
}

impl Clone for AppState {
    fn clone(&self) -> Self {
        Self {
            sessions: Arc::clone(&self.sessions),
            turn_registry: Arc::clone(&self.turn_registry),
            config: self.config.clone(),
            protocol_version: self.protocol_version,
            policy: self.policy.clone(),
            session_store: self.session_store.clone(),
            permits: Arc::clone(&self.permits),
        }
    }
}

impl AppState {
    pub fn new(config: LlmConfig) -> Arc<Self> {
        Arc::new(Self {
            sessions: Arc::new(RwLock::new(HashMap::new())),
            turn_registry: Arc::new(std::sync::Mutex::new(HashMap::new())),
            config,
            // Default to V1 for safety. `main::run_acp_loop` overwrites
            // this with whatever the Client negotiated during
            // `initialize`. We pick V1 here so unit tests that build an
            // AppState directly get v1 wire format without ceremony.
            protocol_version: crate::protocol::ProtocolVersion::V1,
            policy: crate::policy::Policy::default_policy(),
            session_store: None,
            permits: std::sync::Arc::new(std::sync::Mutex::new(HashMap::new())),
        })
    }

    /// Production constructor — `new` plus the real policy and an
    /// optional session snapshot store resolved at startup.
    pub fn with_policy(
        config: LlmConfig,
        policy: crate::policy::Policy,
        session_store: Option<crate::session_store::SessionStore>,
    ) -> Arc<Self> {
        let state = Self::new(config);
        Arc::new(Self {
            policy,
            session_store: session_store.map(std::sync::Arc::new),
            ..(*state).clone()
        })
    }

    /// Write the session's current snapshot if a store is configured.
    /// Snapshot failures are logged inside the store, never fatal.
    pub fn persist_session(&self, session_id: &str) {
        if let Some(store) = &self.session_store {
            if let Some(session) = self.sessions_read().get(session_id) {
                store.save(session_id, &session.working_dir, &session.messages);
            }
        }
    }

    fn sessions_write(&self) -> std::sync::RwLockWriteGuard<'_, HashMap<String, Session>> {
        match self.sessions.write() {
            Ok(s) => s,
            Err(p) => {
                warn!("Session lock poisoned, recovering");
                p.into_inner()
            }
        }
    }

    fn sessions_read(&self) -> std::sync::RwLockReadGuard<'_, HashMap<String, Session>> {
        match self.sessions.read() {
            Ok(s) => s,
            Err(p) => {
                warn!("Session lock poisoned, recovering");
                p.into_inner()
            }
        }
    }

    /// Evict sessions that have been idle longer than the timeout.
    pub fn evict_idle_sessions(&self, timeout_secs: u64) {
        if timeout_secs == 0 {
            return;
        }
        let timeout = Duration::from_secs(timeout_secs);
        let mut sessions = self.sessions_write();
        let before = sessions.len();
        sessions.retain(|_id, session| session.last_active.elapsed() < timeout);
        let evicted = before - sessions.len();
        if evicted > 0 {
            info!(evicted, remaining = sessions.len(), "Evicted idle sessions");
        }
    }

    /// Issue a single-use, TTL-bound permit for a `confirm`-gated
    /// capability. This IS the interactive grant channel — the Client
    /// calls `session/permit` when the user (or its policy) approves one
    /// specific operation, and the next matching confirm-gated call
    /// consumes it. TTL clamps to [1, 3600]s, default 300.
    pub fn session_permit(
        &self,
        session_id: &str,
        capability_name: &str,
        tool: Option<&str>,
        ttl_secs: u64,
    ) -> Result<Value, AcpError> {
        if !self.sessions_read().contains_key(session_id) {
            return Err(AcpError::UnknownSession {
                session_id: session_id.to_string(),
            });
        }
        let capability = match capability_name.trim().to_ascii_lowercase().as_str() {
            "read" => crate::policy::Capability::Read,
            "write" => crate::policy::Capability::Write,
            "shell" => crate::policy::Capability::Shell,
            "web" => crate::policy::Capability::Web,
            other => {
                return Err(AcpError::MissingParam {
                    field: format!("capability: '{other}' is not one of read|write|shell|web"),
                })
            }
        };
        if let Some(t) = tool {
            match crate::policy::Policy::capability_of(t) {
                Some(c) if c == capability => {}
                Some(_) => {
                    return Err(AcpError::MissingParam {
                        field: format!("tool: '{t}' is not a {capability_name} tool"),
                    })
                }
                None => {
                    return Err(AcpError::MissingParam {
                        field: format!("tool: unknown tool '{t}'"),
                    })
                }
            }
        }
        let ttl = ttl_secs.clamp(1, 3600);
        let permit = Permit {
            id: Uuid::new_v4().to_string(),
            capability,
            tool: tool.map(|s| s.to_string()),
            expires_at: std::time::Instant::now() + Duration::from_secs(ttl),
            consumed: false,
        };
        let out = json!({
            "permitId": permit.id,
            "capability": format!("{:?}", permit.capability),
            "tool": permit.tool,
            "expiresInSeconds": ttl,
            "singleUse": true,
        });
        let mut permits = match self.permits.lock() {
            Ok(p) => p,
            Err(p) => p.into_inner(),
        };
        // Housekeeping: drop expired+consumed permits while we're here.
        let v = permits.entry(session_id.to_string()).or_default();
        v.retain(|p| !p.consumed && p.expires_at > std::time::Instant::now());
        v.push(permit);
        Ok(out)
    }

    /// Consume the oldest matching live permit for (capability, tool).
    /// Returns the permit id on success — single-use means it is marked
    /// consumed before the tool runs, so a crashed call still burns it.
    fn consume_permit(
        &self,
        session_id: &str,
        cap: crate::policy::Capability,
        tool_name: &str,
    ) -> Option<String> {
        let mut permits = match self.permits.lock() {
            Ok(p) => p,
            Err(p) => p.into_inner(),
        };
        let v = permits.get_mut(session_id)?;
        let now = std::time::Instant::now();
        v.iter_mut()
            .find(|p| {
                !p.consumed
                    && p.expires_at > now
                    && p.capability == cap
                    && p.tool.as_deref().is_none_or(|t| t == tool_name)
            })
            .map(|p| {
                p.consumed = true;
                p.id.clone()
            })
    }

    /// Clean up all sessions. Returns the number of sessions cleaned.
    pub fn cleanup(&self) -> usize {
        let mut s = self.sessions_write();
        let n = s.len();
        s.clear();
        n
    }
}

// ---------------------------------------------------------------------------
// Engine functions — called by both ACP and A2A transports
// ---------------------------------------------------------------------------

/// Handle `initialize` — returns agent info.
///
/// ACP v1 spec treats capability flags as the only signal a Client has for
/// whether to send images / audio / embedded resources. Per-spec the default
/// for all three is `false`, and acp-bridge historically advertised
/// `image: true` unconditionally — which caused Clients (Meuxe, ACP UI, …)
/// to forward image attachments to local backends that had no vision model
/// loaded, surfacing upstream as confusing "agent didn't respond" errors.
///
/// We now advertise `image` only when the operator opts in via the
/// `LLM_SUPPORTS_IMAGE` env var or the equivalent field in the config file.
/// `audio` and `embeddedContext` remain `false` — acp-bridge does not yet
/// process them.
///
/// The `protocol_version` parameter controls which wire-shape the
/// response uses:
/// - `ProtocolVersion::V1` → `agentCapabilities` + `agentInfo` shape
///   (every existing Client speaks this today)
/// - `ProtocolVersion::V2` → unified `capabilities` + `info` shape, with
///   `promptCapabilities.image` expressed as `{}` (capability marker)
///   rather than `true`
pub fn initialize(
    config: &LlmConfig,
    protocol_version: crate::protocol::ProtocolVersion,
    session_persistence: bool,
) -> Value {
    info!(
        model = %config.model,
        base_url = %config.base_url,
        protocol_version = %protocol_version,
        "Initialize"
    );

    // v2 marks capability support with the present-of-an-object marker
    // (`{}` = "supported") rather than `true`. We still want to
    // distinguish image support from audio / embeddedContext, so:
    //   - image enabled   → `{"image": {}}` (present, supported)
    //   - image disabled  → omit `image` entirely (not advertised)
    // audio / embeddedContext stay omitted regardless.
    let prompt_capabilities_v2 = if config.prompt_supports_image {
        json!({ "image": {} })
    } else {
        json!({})
    };

    // `loadSession` is only honest when a snapshot store exists —
    // with `LLM_SESSION_DIR` unset, `session/load` answers
    // `no_persistence`, so we must not advertise the capability.
    match protocol_version {
        crate::protocol::ProtocolVersion::V2 => {
            let mut caps = json!({
                "protocolVersion": 2,
                "info": {
                    "name": format!("acp-bridge ({})", config.model),
                    "title": "acp-bridge",
                    "version": env!("CARGO_PKG_VERSION")
                },
                "capabilities": {
                    "session": {
                        "prompt": prompt_capabilities_v2
                    }
                },
                "authMethods": []
            });
            if session_persistence {
                caps["capabilities"]["session"]["load"] = json!({});
            }
            caps
        }
        _ => {
            // v1 (default)
            let prompt_capabilities_v1 = if config.prompt_supports_image {
                json!({
                    "image": true,
                    "audio": false,
                    "embeddedContext": false
                })
            } else {
                json!({
                    "image": false,
                    "audio": false,
                    "embeddedContext": false
                })
            };
            json!({
                "protocolVersion": 1,
                "agentInfo": {
                    "name": format!("acp-bridge ({})", config.model),
                    "version": env!("CARGO_PKG_VERSION")
                },
                "agentCapabilities": {
                    "promptCapabilities": prompt_capabilities_v1,
                    "loadSession": session_persistence
                },
                "authMethods": []
            })
        }
    }
}

/// Handle `session/new` — creates a new session, returns session ID.
///
/// `protocol_version` is the version negotiated at `initialize` for the
/// Client calling us. All subsequent notifications in this session use
/// the v1 or v2 wire shape accordingly.
pub fn session_new(
    state: &AppState,
    cwd: &str,
    protocol_version: crate::protocol::ProtocolVersion,
) -> Result<String, AcpError> {
    // Enforce max_sessions limit
    if state.config.max_sessions > 0 {
        let count = state.sessions_read().len();
        if count >= state.config.max_sessions {
            return Err(AcpError::SessionLimitReached {
                max: state.config.max_sessions,
            });
        }
    }

    // Resolve and validate cwd. The session's `working_dir` is the base of
    // the tool sandbox (`resolve_sandboxed_path` canonicalizes both sides
    // before comparing), so it must be canonical and must actually exist —
    // a mangled or nonexistent cwd would either break every file tool or
    // anchor the sandbox at the wrong place. Reject with a visible error
    // instead of silently rewriting characters (previous behavior stripped
    // `:`/`(`/`)` etc., which destroyed Windows paths like `C:\tools\proj`).
    let working_dir = match std::fs::canonicalize(cwd) {
        Ok(p) if p.is_dir() => p,
        Ok(_) => {
            return Err(AcpError::InvalidCwd {
                cwd: cwd.into(),
                reason: "path exists but is not a directory".into(),
            });
        }
        Err(e) => {
            return Err(AcpError::InvalidCwd {
                cwd: cwd.into(),
                reason: e.to_string(),
            });
        }
    };

    let session_id = Uuid::new_v4().to_string();

    let system_prompt = state.config.system_prompt.clone().unwrap_or_else(|| {
        std::env::var("LLM_SYSTEM_PROMPT").unwrap_or_else(|_| {
            format!("You are a helpful coding assistant. The user's working directory is: {cwd}")
        })
    });

    let session = Session::new(
        json!({"role": "system", "content": system_prompt}),
        working_dir,
        protocol_version,
    );
    state.sessions_write().insert(session_id.clone(), session);

    // Initial snapshot so a brand-new session also survives a restart.
    state.persist_session(&session_id);

    info!(session_id = %session_id, max_history = state.config.max_history_turns, "New session");

    // NOTE: We return `Ok(session_id)` from here BEFORE emitting the
    // post-session-creation notifications below. Callers (e.g.
    // `main::run_acp_loop`) send the JSON-RPC response immediately on
    // receiving the session id, then emit the notifications. This ordering
    // matches what other ACP agents do (OpenClaw, OpenCode) — the Client
    // learns the new sessionId first, then receives the post-session
    // notifications (commands, info update) bound to that id.
    Ok(session_id)
}

/// Post-creation notifications to send after `session/new` returns.
/// Exposed so `main::run_acp_loop` can emit them with the right wire
/// framing after the response is on the wire. The `protocol_version`
/// argument controls which wire shape (v1 vs v2) the dispatchers emit.
pub fn session_new_post_create_notifications(
    session_id: &str,
    cwd: &str,
    protocol_version: crate::protocol::ProtocolVersion,
) {
    // Advertise the slash commands this agent recognises. Clients use this
    // to populate the "/…" shortcut menu. The list is intentionally small:
    // commands are shortcuts, not advertised features. See `tools.rs` for
    // the tool surface that backs the longer-tail capabilities.
    crate::acp::notify_available_commands_for(
        protocol_version,
        session_id,
        &[
            crate::acp::AvailableCommand::new(
                "read",
                "Read a file (alias for read_file tool)",
                Some::<&str>("path"),
            ),
            crate::acp::AvailableCommand::new(
                "ls",
                "List directory contents (alias for list_dir tool)",
                Some::<&str>("path"),
            ),
            crate::acp::AvailableCommand::new(
                "search",
                "Search code (alias for search tool)",
                Some::<&str>("pattern"),
            ),
            crate::acp::AvailableCommand::new(
                "edit",
                "Patch a file via write_file tool",
                Some::<&str>("path"),
            ),
            crate::acp::AvailableCommand::new(
                "shell",
                "Run a shell command via the shell tool",
                Some::<&str>("command"),
            ),
        ],
    );

    // Initial session_info_update — title defaults to the cwd basename so
    // the Client has something to show in its session list before any
    // prompt runs.
    let title = std::path::Path::new(cwd)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(cwd);
    crate::acp::notify_session_info_for(protocol_version, session_id, title, None);
}

/// Handle `session/prompt` — runs the LLM with tool loop.
///
/// Sends `Notification` events through `notify_tx` as they happen (for ACP streaming).
/// Returns the final status ("completed", "partial", or "failed") and accumulated text.
///
/// `message_id` is the opaque identifier acp-bridge mints for this
/// prompt. It is required on the v2 `PromptResponse` and used by v2
/// Clients to correlate the response with `user_message` and
/// `agent_message` session updates. v1 Clients ignore it.
pub async fn session_prompt(
    state: &Arc<AppState>,
    session_id: &str,
    user_text: &str,
    user_images: &[ImageBlock],
    notify_tx: Option<mpsc::UnboundedSender<Notification>>,
    message_id: &str,
) -> PromptResult {
    let notify = |n: Notification| {
        if let Some(tx) = &notify_tx {
            let _ = tx.send(n);
        }
    };

    // Image capability gate (review B4): when the backend was configured
    // without image support, drop image blocks before they reach the
    // provider. Forwarding them produced a bare HTTP 400 from vision-less
    // endpoints — the user-visible symptom was "agent didn't respond".
    let images_dropped = !state.config.prompt_supports_image && !user_images.is_empty();
    let user_images: &[ImageBlock] = if images_dropped { &[] } else { user_images };

    // Add user message, touch session, and trim history
    {
        let mut sessions = state.sessions_write();
        let session = match sessions.get_mut(session_id) {
            Some(s) => s,
            None => {
                return PromptResult {
                    status: "failed".into(),
                    text: format!("Unknown session: {session_id}"),
                    error: Some(AcpError::UnknownSession {
                        session_id: session_id.into(),
                    }),
                    usage: UsageReport {
                        used: 0,
                        size: state.config.context_size,
                    },
                    evidence: TurnEvidence::default(),
                    error_class: None,
                    error_retryable: false,
                    // Caller never wired a prompt through this branch —
                    // the session id doesn't exist — so we emit an
                    // empty messageId. The Client will see a JSON-RPC
                    // error from the outer send_error path anyway.
                    message_id: String::new(),
                };
            }
        };
        session.touch();
        session.messages.push(
            state
                .config
                .backend()
                .format_user_message(user_text, user_images),
        );

        if state.config.max_history_turns > 0 {
            let before = session.messages.len();
            session.trim_history(state.config.max_history_turns);
            let after = session.messages.len();
            if before != after {
                debug!(before, after, "Trimmed conversation history");
            }
        }
    }

    let mut dropped_image_note: Option<String> = None;
    if images_dropped {
        let note = "[note: image content was removed before sending — this endpoint was configured with image support disabled (LLM_SUPPORTS_IMAGE=false).]";
        if user_text.trim().is_empty() {
            // The prompt carried images only; with them dropped there is
            // nothing to send. Answer with the removal notice instead of
            // dispatching an empty prompt to the model.
            notify(Notification::TextChunk(note.to_string()));
            return PromptResult {
                status: "completed".into(),
                text: note.into(),
                error: None,
                usage: UsageReport {
                    used: 0,
                    size: state.config.context_size,
                },
                evidence: TurnEvidence::default(),
                error_class: None,
                error_retryable: false,
                message_id: message_id.to_string(),
            };
        }
        notify(Notification::TextChunk(note.to_string()));
        // Carry the notice into the final result text too — streamed
        // chunks are transient, and a Client that only renders the result
        // must still see that the image never reached the model.
        dropped_image_note = Some(note.to_string());
    }

    notify(Notification::Thinking);
    notify(Notification::ToolStart {
        id: format!("llm_chat:{session_id}"),
        name: "llm_chat".into(),
    });

    let mut had_error = false;
    let mut got_final_response = false;
    let mut truncated_by_budget = false;
    let mut final_text = String::new();
    let mut last_error_class: Option<crate::llm::LlmErrorKind> = None;
    let tool_defs = tools::tool_definitions();
    let max_tool_rounds = state.config.max_tool_rounds;
    let tool_timeout_secs = state.config.tool_timeout_secs;
    let tool_failure_streak = state.config.tool_failure_streak;
    let turn_start = std::time::Instant::now();
    let mut evidence = TurnEvidence::default();
    // Consecutive identical failures (same tool + args + error). A model
    // re-issuing a failing call verbatim is looping, not working.
    let mut fail_key: Option<String> = None;
    let mut fail_count = 0usize;
    let mut streak_aborted = false;

    // Tool calls go through `fit_and_call`: fit the request into the
    // model's context window (drop oldest history + clamp max_tokens;
    // local backends reject prompt+max_tokens>context outright instead
    // of truncating), retry once with harder compaction on a still-
    // overflowing 400.

    // Tool call loop
    for round in 0..max_tool_rounds {
        evidence.rounds = round as u32 + 1;
        let (messages, working_dir) = {
            let sessions = state.sessions_read();
            match sessions.get(session_id) {
                Some(s) => (s.messages.clone(), s.working_dir.clone()),
                None => {
                    warn!(session_id, "Session removed during prompt processing");
                    had_error = true;
                    final_text = format!("Session {session_id} was removed during processing");
                    break;
                }
            }
        };

        let backend = state.config.backend();
        let chat_result =
            fit_and_call(state, session_id, messages, Some(&tool_defs), &notify).await;

        match chat_result {
            Ok(response) => {
                let mut tool_calls = backend.extract_tool_calls(&response);
                // When tool calls were recovered from the content channel
                // (thinking-mode workaround), this carries the cleaned text
                // that belongs on the recorded assistant message.
                let mut recovered_text: Option<String> = None;

                // Thinking-mode workarounds (0.9.2): reasoning models
                // sometimes embed the tool call inside the content
                // channel instead of the structured `tool_calls` field.
                // Recover it before giving up on dispatching this round.
                if tool_calls.is_empty() {
                    let raw_text = backend.extract_response_text(&response);
                    let (clean_text, recovered) = llm::recover_tool_calls_from_content(&raw_text);
                    if !recovered.is_empty() {
                        info!(
                            round,
                            count = recovered.len(),
                            "Recovered tool calls embedded in content (thinking-mode workaround)"
                        );
                        tool_calls = recovered;
                        recovered_text = Some(clean_text);
                    } else {
                        got_final_response = true;
                        let text = clean_text;
                        if !text.is_empty() {
                            final_text = text.clone();
                            {
                                let mut sessions = state.sessions_write();
                                if let Some(session) = sessions.get_mut(session_id) {
                                    session
                                        .messages
                                        .push(json!({"role": "assistant", "content": &text}));
                                }
                            }
                            notify(Notification::TextChunk(text));
                        }
                        break;
                    }
                }

                // Execute tool calls
                info!(round, count = tool_calls.len(), "Executing tool calls");

                // Record exactly one assistant message for this round (B1).
                // Recovered calls need an explicit message: cloning the raw
                // response would record `tool_calls: null`/absent and orphan
                // the `embedded_*` tool results that reference it — and the
                // recovery path previously pushed a second copy of the raw
                // message here, duplicating the assistant turn in history.
                {
                    let mut sessions = state.sessions_write();
                    if let Some(session) = sessions.get_mut(session_id) {
                        let assistant_msg = match &recovered_text {
                            Some(text) => json!({
                                "role": "assistant",
                                "content": text,
                                "tool_calls": &tool_calls,
                            }),
                            None => backend.format_assistant_message("", &tool_calls, &response),
                        };
                        session.messages.push(assistant_msg);
                    }
                }

                for tc in &tool_calls {
                    let func = &tc["function"];
                    let name = func["name"].as_str().unwrap_or("unknown");
                    // `function.arguments` can be either a JSON-encoded
                    // string (OpenAI-compatible backends) or a JSON
                    // object (Ollama native /api/chat). Handle both —
                    // the previous `as_str().unwrap_or("{}")` silently
                    // dropped every Ollama native tool call's actual
                    // arguments (see review §"既有 bug 未修").
                    let args: Value = match func.get("arguments") {
                        Some(Value::String(s)) => serde_json::from_str(s)
                            .unwrap_or_else(|_| Value::Object(Default::default())),
                        Some(Value::Object(_)) | Some(Value::Array(_)) => func["arguments"].clone(),
                        Some(_) | None => Value::Object(Default::default()),
                    };
                    let tool_call_id = tc.get("id").and_then(|v| v.as_str()).unwrap_or("unknown");

                    notify(Notification::ToolStart {
                        id: tool_call_id.to_string(),
                        name: name.into(),
                    });
                    // P1/E12 policy gate: deny → structured error, no
                    // execution; audit → visible in-session notice then
                    // execute (a logged proceed — there is no
                    // interactive permission channel); confirm →
                    // execute only with a deployment grant, else deny;
                    // allow → execute.
                    evidence.tool_calls += 1;
                    let cap = crate::policy::Policy::capability_of(name);
                    let decision = state.policy.decide(name, &args);
                    // A Deny produced by an ungranted `confirm` action
                    // (not a hard `deny`) can be rescued by a live
                    // single-use permit for this capability — and tool,
                    // when the permit is tool-scoped.
                    let confirm_blocked = matches!(decision, crate::policy::Decision::Deny(_))
                        && cap
                            .map(|c| {
                                state.policy.action_for(c) == crate::policy::Action::Confirm
                                    && !state.policy.grants.contains(&c)
                            })
                            .unwrap_or(false);
                    let permit_id = if confirm_blocked {
                        state.consume_permit(session_id, cap.unwrap(), name)
                    } else {
                        None
                    };
                    let policy_denied =
                        matches!(decision, crate::policy::Decision::Deny(_)) && permit_id.is_none();
                    let result = if let Some(pid) = &permit_id {
                        notify(Notification::TextChunk(format!(
                            "\n\n*[policy:permit] consumed {pid} — '{name}' authorized for this call only*"
                        )));
                        tools::execute_tool(
                            &working_dir,
                            name,
                            &args,
                            tool_timeout_secs,
                            &state.policy.deny_path_globs,
                        )
                        .await
                    } else {
                        match decision {
                            crate::policy::Decision::Deny(reason) => tools::ToolResult::err(
                                tools::ToolErrorKind::Denied,
                                format!("Error: {reason}"),
                            ),
                            decision => {
                                if let crate::policy::Decision::Audit(reason) = &decision {
                                    notify(Notification::TextChunk(format!(
                                        "\n\n*[policy:audit] {reason} — proceeding (logged; no interactive permission channel configured)*"
                                    )));
                                }
                                tools::execute_tool(
                                    &working_dir,
                                    name,
                                    &args,
                                    tool_timeout_secs,
                                    &state.policy.deny_path_globs,
                                )
                                .await
                            }
                        }
                    };
                    notify(Notification::ToolDone {
                        id: tool_call_id.to_string(),
                        name: name.into(),
                        status: if result.error_kind.is_none() {
                            "completed"
                        } else {
                            "failed"
                        }
                        .into(),
                    });

                    debug!(tool = name, result_len = result.text.len(), error_kind = ?result.error_kind, "Tool executed");

                    match result.error_kind {
                        None => evidence.tools_ok += 1,
                        Some(kind) => {
                            evidence.tools_failed += 1;
                            if policy_denied || matches!(kind, tools::ToolErrorKind::Denied) {
                                evidence.tools_denied += 1;
                            }
                            if matches!(kind, tools::ToolErrorKind::Timeout) {
                                evidence.tools_timed_out += 1;
                            }
                        }
                    }
                    if result.truncated {
                        evidence.outputs_truncated += 1;
                    }

                    // Identical-failure streak: a model re-issuing the
                    // exact same call after an error is in a loop. Stop
                    // the tool loop early and let the wrap-up produce a
                    // bounded partial answer instead of burning rounds.
                    if let Some(kind) = result.error_kind {
                        let key = format!("{name}|{args}|{kind:?}");
                        if fail_key.as_deref() == Some(key.as_str()) {
                            fail_count += 1;
                        } else {
                            fail_key = Some(key);
                            fail_count = 1;
                        }
                    } else {
                        fail_key = None;
                        fail_count = 0;
                    }

                    {
                        let mut sessions = state.sessions_write();
                        if let Some(session) = sessions.get_mut(session_id) {
                            session
                                .messages
                                .push(backend.format_tool_result(tool_call_id, &result.text));
                        }
                    }

                    if fail_count >= tool_failure_streak && tool_failure_streak > 0 {
                        warn!(
                            tool = name,
                            streak = fail_count,
                            "identical tool failure streak, aborting loop"
                        );
                        evidence.budget_hit = Some("failure_streak".into());
                        {
                            let mut sessions = state.sessions_write();
                            if let Some(session) = sessions.get_mut(session_id) {
                                session.messages.push(json!({
                                    "role": "system",
                                    "content": format!(
                                        "[harness] stopped the tool loop: `{name}` failed identically {fail_count} times in a row. Summarize partial progress instead of retrying the same call."
                                    )
                                }));
                            }
                        }
                        streak_aborted = true;
                        break;
                    }
                }
                if streak_aborted {
                    break;
                }
            }
            Err(e) => {
                // Classify the LLM error so the response can carry a
                // structured `data.category` field that Clients can
                // switch on without parsing prose. See
                // `LlmErrorKind::as_str()` for the stable category names.
                let kind = e.kind.clone();
                let retryable = e.kind.is_retryable();
                let err_msg = format!(
                    "\n\n**Error ({}):** {}\n{}",
                    e.kind.as_str(),
                    e.message,
                    if retryable {
                        "_Hint: this is a transient error; the same prompt may succeed on retry._"
                    } else {
                        "_Hint: this error is not retryable; check the model name, base URL, or prompt shape._"
                    }
                );
                notify(Notification::TextChunk(err_msg.clone()));
                final_text = err_msg;
                had_error = true;
                last_error_class = Some(kind);
                error!(
                    kind = e.kind.as_str(),
                    status = ?e.status,
                    retryable,
                    "LLM communication failed"
                );
                break;
            }
        }
    }

    // The loop can also exit by exhausting the round budget or hitting
    // the repeated-failure abort while the model keeps requesting tools.
    // Before reporting failure, give the model one final call with no
    // tool definitions: it must answer with what it has. This converts a
    // hard error into a bounded partial answer — the turn is reported as
    // `partial` (not `completed`) so downstream automation can
    // distinguish "answered within budget" from "answered because the
    // budget forced a stop". The wrap-up is not a tool round — no tools
    // are offered or executed.
    if !had_error && !got_final_response && evidence.budget_hit.is_none() {
        // Reaching here without a final response and without a streak
        // abort means the round budget ran out.
        evidence.budget_hit = Some("rounds".into());
    }
    if !had_error && !got_final_response {
        let limit_note = if streak_aborted {
            format!("the same tool call failed {tool_failure_streak} times in a row")
        } else {
            format!("the {max_tool_rounds}-round tool-call limit was reached")
        };
        let messages = {
            let sessions = state.sessions_read();
            sessions.get(session_id).map(|s| s.messages.clone())
        };
        if let Some(messages) = messages {
            match fit_and_call(state, session_id, messages, None, &notify).await {
                Ok(response) => {
                    let raw_text = state.config.backend().extract_response_text(&response);
                    let (clean_text, _) = llm::recover_tool_calls_from_content(&raw_text);
                    if !clean_text.trim().is_empty() {
                        let text = format!(
                            "{clean_text}\n\n[note: {limit_note} — this answer may be incomplete]"
                        );
                        {
                            let mut sessions = state.sessions_write();
                            if let Some(session) = sessions.get_mut(session_id) {
                                session
                                    .messages
                                    .push(json!({"role": "assistant", "content": &text}));
                            }
                        }
                        notify(Notification::TextChunk(text.clone()));
                        final_text = text;
                        got_final_response = true;
                        truncated_by_budget = true;
                    }
                }
                Err(e) => {
                    debug!(error = %e, "Wrap-up call after round limit failed");
                }
            }
        }
    }

    if !had_error && !got_final_response {
        warn!(
            max_rounds = max_tool_rounds,
            streak_aborted, "Tool-call loop hit its budget without a final response"
        );
        let msg = format!(
            "\n\n**Error:** reached the tool-call limit ({max_tool_rounds} rounds) without a final answer\n"
        );
        notify(Notification::TextChunk(msg.clone()));
        final_text = msg;
        had_error = true;
    }

    // `partial` = the turn produced an answer only because the tool
    // budget forced a wrap-up. It is not a failure (the model did answer)
    // and it is not `completed` (the model was cut off mid-work); the
    // wire-level `stopReason: max_turn_requests` carries the same
    // distinction for Clients that branch on it.
    let status = if had_error {
        "failed"
    } else if truncated_by_budget {
        "partial"
    } else {
        "completed"
    };
    notify(Notification::ToolDone {
        id: format!("llm_chat:{session_id}"),
        name: "llm_chat".into(),
        status: status.into(),
    });

    // Estimate the current context utilization for `usage_update`. Local
    // backends rarely stream per-turn token counts in a stable shape, so
    // we estimate by summing the textual length of every message in the
    // session history and dividing by 4 chars per token (the canonical
    // LLM rule of thumb). This is intentionally approximate; clients use
    // it for progress bars and not for cost attribution.
    let used_tokens = {
        let sessions = state.sessions_read();
        sessions
            .get(session_id)
            .map(|s| estimate_tokens(&s.messages))
            .unwrap_or(0)
    };

    let final_text = match (&dropped_image_note, had_error, final_text.is_empty()) {
        (Some(note), false, false) => format!("{note}\n\n{final_text}"),
        _ => final_text,
    };

    evidence.wall_ms = turn_start.elapsed().as_millis();

    // Turn-boundary snapshot (E14): persist the post-turn history so a
    // later `session/load` can restore this state after a restart.
    state.persist_session(session_id);

    PromptResult {
        status: status.into(),
        text: final_text,
        error: None,
        usage: UsageReport {
            used: used_tokens,
            size: state.config.context_size,
        },
        evidence,
        error_class: last_error_class.clone(),
        error_retryable: last_error_class.as_ref().is_some_and(|k| k.is_retryable()),
        message_id: message_id.to_string(),
    }
}

/// Rough token estimate from a list of chat messages. Sums the textual
/// content of every message and divides by 4 chars/token. We do NOT try
/// to be precise — local backends do not stream stable per-turn token
/// counts, and clients use this only for progress visualisation.
/// Headroom held back from `context_size` — the char/4 estimate
/// undercounts CJK and tokenizer-heavy text, so the real prompt
/// is often bigger than the number suggests.
const CONTEXT_MARGIN_TOKENS: u64 = 512;
/// Floor for clamped `max_tokens` — a response needs room to exist.
const MIN_OUTPUT_TOKENS: u64 = 512;
/// Assumed generation budget when `LLM_MAX_TOKENS` is unset —
/// most local backends default to ~4096 and count it into the
/// context arithmetic, so we must too.
const DEFAULT_OUTPUT_TOKENS: u64 = 4096;

/// How much of an old message's text survives in-place shrinking.
/// A 50 KB tool result becomes ~2 KB: the model keeps the "what ran
/// and what came back" skeleton without paying full price per token.
const ELIDED_KEEP_CHARS: usize = 2048;

/// Compact messages to fit `prompt_budget` tokens (char/4 estimate).
///
/// Kiro-style anchoring for long investigations: `messages[0]` (system)
/// and `messages[1]` (the first user message — THE TASK) are never
/// dropped, or the agent literally forgets what it was investigating
/// mid-way. Three escalating steps:
///   1. Shrink oversized message contents IN PLACE (elide the middle,
///      keep head+tail) — the tool-call structure and a usable excerpt
///      survive.
///   2. Drop oldest non-anchor messages until the estimate fits —
///      a marker records how many went away so the loss is explicit
///      on the wire.
///
/// Returns (fitted_messages, dropped_count, elided_count).
fn compact_for_context(mut messages: Vec<Value>, prompt_budget: u64) -> (Vec<Value>, usize, usize) {
    if estimate_tokens(&messages) <= prompt_budget {
        return (messages, 0, 0);
    }

    // Step 1 — shrink contents in place, oldest first. The task anchor
    // (messages[1]) is eligible for elision — its head+tail survive —
    // only the system prompt and the live (last) message are immune.
    let mut elided = 0usize;
    for i in 1..messages.len().saturating_sub(1) {
        if estimate_tokens(&messages) <= prompt_budget {
            break;
        }
        if elide_message(&mut messages[i]) {
            elided += 1;
        }
    }
    if estimate_tokens(&messages) <= prompt_budget {
        return (messages, 0, elided);
    }

    // Step 2 — drop whole messages oldest-first between the anchors
    // and the tail. Insert a marker so the model sees the seam.
    let system = messages.first().cloned();
    let task = messages.get(1).cloned();
    let mut rest: Vec<Value> = if messages.len() > 2 {
        messages.split_off(2)
    } else {
        Vec::new()
    };
    let anchor_est = system
        .as_ref()
        .map(|s| estimate_tokens(std::slice::from_ref(s)))
        .unwrap_or(0)
        + task
            .as_ref()
            .map(|t| estimate_tokens(std::slice::from_ref(t)))
            .unwrap_or(0);
    let mut dropped = 0usize;
    // Drop until the WHOLE fitted prompt (anchors + tail) fits — the
    // budget applies to what goes on the wire, not just the tail.
    while rest.len() > 1 && anchor_est + estimate_tokens(&rest) > prompt_budget {
        rest.remove(0);
        dropped += 1;
    }
    let mut out: Vec<Value> = system.into_iter().chain(task).collect();
    if dropped > 0 {
        out.push(json!({
            "role": "user",
            "content": format!("[{dropped} earlier message(s) were compacted away to fit the context window — their contents are gone; reason from the task and the remaining history]")
        }));
    }
    out.extend(rest);
    (out, dropped, elided)
}

/// Elide a message's oversized text in place: keep head + tail, drop
/// the middle, mark the elision. Applies to string content and to the
/// text blocks inside array-shaped content. Returns whether it
/// actually shrank anything.
fn elide_message(msg: &mut Value) -> bool {
    fn elide(s: &str) -> Option<String> {
        if s.len() <= ELIDED_KEEP_CHARS * 2 {
            return None;
        }
        // char-boundary-safe head + tail: byte offsets can split a
        // multibyte char (CJK tool output hits this constantly).
        let mut head_end = ELIDED_KEEP_CHARS;
        while head_end > 0 && !s.is_char_boundary(head_end) {
            head_end -= 1;
        }
        let head = &s[..head_end];
        let mut start = s.len() - ELIDED_KEEP_CHARS;
        while start < s.len() && !s.is_char_boundary(start) {
            start += 1;
        }
        let tail = &s[start..];
        Some(format!(
            "{head}\n[…{} bytes elided…]\n{tail}",
            s.len() - (head.len() + tail.len())
        ))
    }
    let mut changed = false;
    match msg.get_mut("content") {
        Some(Value::String(s)) => {
            if let Some(e) = elide(s) {
                *s = e;
                changed = true;
            }
        }
        Some(Value::Array(arr)) => {
            for b in arr.iter_mut() {
                if let Some(Value::String(s)) = b.get_mut("text") {
                    if let Some(e) = elide(s) {
                        *s = e;
                        changed = true;
                    }
                }
            }
        }
        _ => {}
    }
    changed
}

/// True when a backend error is the deterministic "prompt + max_tokens
/// exceeds the context" shape local backends emit (llama.cpp, LM
/// Studio, vLLM all phrase it around `context`/`length`).
fn is_context_overflow(e: &crate::llm::LlmError) -> bool {
    if e.kind != crate::llm::LlmErrorKind::BadRequest {
        return false;
    }
    let m = e.message.to_ascii_lowercase();
    m.contains("exceeds the context")
        || m.contains("context length")
        || m.contains("context window")
        || (m.contains("context") && (m.contains("exceed") || m.contains("too long")))
}

/// Extract the real prompt-token count from a context-overflow error —
/// llama.cpp / LM Studio phrase it as `prompt (30314 tokens) + ...`.
/// Knowing the REAL count lets the retry scale the char/4 estimate by
/// the measured ratio instead of guessing again.
fn reported_prompt_tokens(message: &str) -> Option<u64> {
    let m = message.to_ascii_lowercase();
    let start = m.find("prompt (")? + "prompt (".len();
    let digits: String = m[start..]
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect();
    if digits.is_empty() || !m[start..].starts_with(&format!("{digits} tokens")) {
        return None;
    }
    digits.parse().ok()
}

/// Fit a request into the model's context window, then call.
///
/// Local backends reject `prompt + max_tokens > context` outright —
/// they never truncate the prompt. So before every call: (1) drop
/// oldest history when the estimate overflows the prompt budget,
/// (2) clamp `max_tokens` to what the window can actually emit.
/// Dropped history is written back into the session — it is really
/// gone — and the drop is announced via a `[context]` chunk so the
/// turn's memory loss is visible instead of silent.
///
/// If the backend still answers a context-overflow 400 (estimate was
/// optimistic for this content), compact harder and retry once. A
/// second overflow surfaces the backend's error unchanged — nothing
/// here hides a real failure.
async fn fit_and_call(
    state: &AppState,
    session_id: &str,
    messages: Vec<Value>,
    tools: Option<&[Value]>,
    notify: &impl Fn(Notification),
) -> Result<Value, crate::llm::LlmError> {
    let budget = state
        .config
        .context_size
        .saturating_sub(CONTEXT_MARGIN_TOKENS);
    // Clamp the OUTPUT budget into the window first — otherwise a big
    // desired max_tokens would leave zero prompt room and we'd drop
    // every history message even though a smaller response fits fine.
    let out_want = config_output_budget(&state.config)
        .min(budget.saturating_sub(MIN_OUTPUT_TOKENS))
        .max(MIN_OUTPUT_TOKENS.min(budget));
    let (mut msgs, dropped, mut elided_total) =
        compact_for_context(messages, budget.saturating_sub(out_want));
    let mut dropped_total = dropped;

    let send = |cfg_max: Option<u64>, msgs: &[Value]| {
        let mut cfg = state.config.clone();
        cfg.max_tokens = cfg_max;
        let msgs = msgs.to_vec();
        let tools = tools.map(|t| t.to_vec());
        async move { crate::llm::chat(&cfg, &msgs, None, tools.as_deref()).await }
    };

    let est = estimate_tokens(&msgs);
    let eff_max = out_want
        .min(budget.saturating_sub(est))
        .max(MIN_OUTPUT_TOKENS);
    match send(Some(eff_max), &msgs).await {
        Err(e) if is_context_overflow(&e) => {
            // Estimate undercounted. If the backend told us the REAL
            // prompt size (llama.cpp family does), scale the estimate by
            // the measured ratio — CJK text can be 4-10x off char/4, so
            // a fixed halving would still overflow. Fall back to a
            // hard halving when the count isn't reported.
            let est_now = estimate_tokens(&msgs);
            let target = match reported_prompt_tokens(&e.message) {
                Some(real) if real > 0 => {
                    // est = real * (est_now/real) → to land the real
                    // prompt at ~3/4 of the window.
                    let scale = est_now as f64 / real as f64;
                    ((budget * 3 / 4) as f64 * scale) as u64
                }
                _ => budget / 2,
            };
            let (m2, d2, e2) = compact_for_context(msgs.clone(), target.max(1));
            msgs = m2;
            dropped_total += d2;
            elided_total += e2;
            if dropped_total + elided_total > 0 {
                if let Some(s) = state.sessions_write().get_mut(session_id) {
                    s.messages = msgs.clone();
                }
                notify(Notification::TextChunk(format!(
                    "\n\n*[context] compacted history ({dropped_total} message(s) dropped, {elided_total} elided) — prompt no longer fits the {}-token window; earliest history compressed*",
                    state.config.context_size
                )));
            }
            let est2 = estimate_tokens(&msgs);
            let eff2 = out_want
                .min(budget.saturating_sub(est2))
                .clamp(MIN_OUTPUT_TOKENS, 2048);
            send(Some(eff2), &msgs).await
        }
        r => {
            if dropped_total + elided_total > 0 {
                if let Some(s) = state.sessions_write().get_mut(session_id) {
                    s.messages = msgs;
                }
                notify(Notification::TextChunk(format!(
                    "\n\n*[context] compacted history ({dropped_total} message(s) dropped, {elided_total} elided) — prompt exceeded the {}-token window; earliest history compressed*",
                    state.config.context_size
                )));
            }
            r
        }
    }
}

/// Response budget for the next call: configured `LLM_MAX_TOKENS` or
/// the local-backend-style default, never under the floor.
fn config_output_budget(config: &LlmConfig) -> u64 {
    config
        .max_tokens
        .unwrap_or(DEFAULT_OUTPUT_TOKENS)
        .max(MIN_OUTPUT_TOKENS)
}

pub fn estimate_tokens(messages: &[Value]) -> u64 {
    let total_chars: usize = messages
        .iter()
        .map(|m| {
            // `content` may be a string, an array of ContentBlocks, or
            // null; extract any text we can find.
            match m.get("content") {
                Some(Value::String(s)) => s.len(),
                Some(Value::Array(arr)) => arr
                    .iter()
                    .filter_map(|b| b.get("text").and_then(|v| v.as_str()))
                    .map(|s| s.len())
                    .sum(),
                _ => 0,
            }
        })
        .sum();
    ((total_chars / 4) as u64).max(1)
}

/// Close a turn aborted by `session/cancel`. A trailing user message with
/// no reply would put two user messages in a row on the next prompt,
/// which chat templates that require alternating roles reject.
pub fn session_cancelled(state: &AppState, session_id: &str) {
    session_aborted(state, session_id, "[cancelled by user]");
}

/// Same history repair for a turn killed by the whole-turn deadline.
pub fn session_timed_out(state: &AppState, session_id: &str) {
    session_aborted(state, session_id, "[turn timed out]");
}

fn session_aborted(state: &AppState, session_id: &str, marker: &str) {
    if let Some(session) = state.sessions_write().get_mut(session_id) {
        if session.messages.last().is_some_and(|m| m["role"] == "user") {
            session
                .messages
                .push(json!({"role": "assistant", "content": marker}));
        }
    }
    state.persist_session(session_id);
}

/// Outcome of a `session/load` attempt — distinct enough that the wire
/// layer can return the right error code and `data.reason`.
pub enum SessionLoadOutcome {
    /// Session is live in memory (already was, or just restored).
    Loaded,
    /// Store disabled → `-32001 no_persistence`.
    NoStore,
    /// Store enabled but no snapshot for this id (or its working dir
    /// no longer exists) → `-32002 session_not_found`.
    NotFound,
    /// Snapshot exists but failed to parse → `-32001` with reason
    /// `load_failed` — corrupt state must not silently become an
    /// empty session.
    Corrupt,
}

/// Handle `session/load` / `session/resume` (P1/E14).
///
/// - Session already live → return it as-is.
/// - Store enabled → read the snapshot; a snapshot whose working
///   directory no longer exists is treated as not-found rather than
///   resurrected anchored at a stale path.
/// - Store disabled → `NoStore` (the pre-E14 `no_persistence` path).
pub fn session_load(state: &AppState, session_id: &str) -> SessionLoadOutcome {
    if state.sessions_read().contains_key(session_id) {
        return SessionLoadOutcome::Loaded;
    }
    let Some(store) = &state.session_store else {
        return SessionLoadOutcome::NoStore;
    };
    match store.load(session_id) {
        Ok(Some(loaded)) => {
            let working_dir = match loaded.working_dir.canonicalize() {
                Ok(p) if p.is_dir() => p,
                _ => return SessionLoadOutcome::NotFound,
            };
            let session = Session {
                messages: loaded.messages,
                last_active: std::time::Instant::now(),
                working_dir,
                protocol_version: state.protocol_version,
            };
            state
                .sessions_write()
                .insert(session_id.to_string(), session);
            info!(session_id = %session_id, "Session restored from snapshot");
            SessionLoadOutcome::Loaded
        }
        Ok(None) => SessionLoadOutcome::NotFound,
        Err(e) => {
            warn!(session_id = %session_id, error = %e, "session snapshot load failed");
            SessionLoadOutcome::Corrupt
        }
    }
}

/// Handle `session/end` — removes a session.
pub fn session_end(state: &AppState, session_id: &str) -> Result<(), AcpError> {
    let removed = state.sessions_write().remove(session_id).is_some();
    if removed {
        if let Some(store) = &state.session_store {
            store.delete(session_id);
        }
    }
    if removed {
        info!(session_id = %session_id, "Session ended");
        Ok(())
    } else {
        Err(AcpError::UnknownSession {
            session_id: session_id.into(),
        })
    }
}

/// Handle `session/list` (v2 baseline) — returns a list of currently
/// active sessions. The wire shape follows the v2 schema's
/// `SessionListResponse`: `{sessions: SessionInfo[], nextCursor: null}`.
///
/// We do not currently include `cwd` or `title` in `SessionInfo` (we
/// only track `messages`, `working_dir`, `protocol_version`, and
/// `last_active`); `cwd` is reachable via the working_dir field so
/// emitting it costs nothing.
pub fn session_list(state: &AppState) -> Vec<Value> {
    state
        .sessions_read()
        .iter()
        .map(|(session_id, session)| {
            json!({
                "sessionId": session_id,
                "cwd": session.working_dir.display().to_string(),
            })
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Result type
// ---------------------------------------------------------------------------

/// Per-turn execution evidence (P1/E13 GoalState): what the turn
/// actually did, in structured form. Automation reads this instead of
/// parsing prose — e.g. "did the agent hit a budget wall, and where".
/// Serialized under `evidence` on the v1 prompt response and on the
/// v2 `state_update` that ends the turn.
#[derive(Debug, Default, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnEvidence {
    /// Model/tool round-trips used (the wrap-up call is not a round).
    pub rounds: u32,
    /// Tool calls dispatched or gated.
    pub tool_calls: u32,
    /// Tool calls that returned without an error kind.
    pub tools_ok: u32,
    /// Tool calls that returned an error kind (policy denies and
    /// timeouts included — they are also counted separately below).
    pub tools_failed: u32,
    /// Tool calls rejected by policy before executing.
    pub tools_denied: u32,
    /// Tool calls killed by the per-tool timeout.
    pub tools_timed_out: u32,
    /// Tool results that hit the output cap.
    pub outputs_truncated: u32,
    /// Which budget forced the wrap-up, if any: "rounds" |
    /// "failure_streak". `None` = the model finished inside budget.
    pub budget_hit: Option<String>,
    /// Whole-turn wall-clock time.
    pub wall_ms: u128,
}

pub struct PromptResult {
    pub status: String,
    pub text: String,
    pub error: Option<AcpError>,
    pub usage: UsageReport,
    /// Structured execution evidence for the turn — see [`TurnEvidence`].
    pub evidence: TurnEvidence,
    /// Classification of the most recent LLM error (if any). `None` means
    /// the turn completed without a backend failure. Carries a stable
    /// `LlmErrorKind` so the JSON-RPC response can attach a structured
    /// `error.data.category` field Clients can switch on.
    pub error_class: Option<crate::llm::LlmErrorKind>,
    /// True if the last LLM failure was a transient error the Client
    /// could retry by re-issuing the same prompt.
    pub error_retryable: bool,
    /// ACP v2 `PromptResponse.messageId` — opaque, unique within the
    /// session. Required by the v2 schema. acp-bridge mints this when it
    /// accepts the prompt (before invoking the LLM) and echoes it on
    /// the response; v2 Clients use it to correlate the response with
    /// `user_message` / `agent_message` session updates.
    pub message_id: String,
}

/// Estimated token usage for a completed prompt turn. The wire shape
/// consumed by `acp::notify_usage` (`used`, `size`) — no cost is
/// reported because acp-bridge runs against local backends where cost
/// is unknown.
#[derive(Debug, Clone, Copy)]
pub struct UsageReport {
    pub used: u64,
    pub size: u64,
}

// ---------------------------------------------------------------------------
// Backend-specific message formatting, response extraction, and tool-call handling
// now live in `crate::llm::Backend` so `engine` stays transport-agnostic.

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::Backend;
    use crate::protocol::ProtocolVersion;

    #[test]
    fn extract_user_text_handles_acp_array_shape() {
        let prompt = serde_json::json!([
            {"type": "text", "text": "查大腦"},
            {"type": "text", "text": "查 X 的看法"}
        ]);
        let text = extract_user_text_from_prompt(&prompt);
        assert_eq!(text, "查大腦\n查 X 的看法");
    }

    #[test]
    fn extract_user_text_handles_single_block_object() {
        let prompt = serde_json::json!({"type": "text", "text": "hello"});
        assert_eq!(extract_user_text_from_prompt(&prompt), "hello");
    }

    #[test]
    fn extract_user_text_handles_resource_link_in_array() {
        let prompt = serde_json::json!([
            {"type": "text", "text": "describe this"},
            {"type": "resource_link", "uri": "file:///etc/hostname", "name": "hostname"}
        ]);
        let text = extract_user_text_from_prompt(&prompt);
        assert!(text.contains("describe this"));
        assert!(text.contains("[Attached resource: hostname (file:///etc/hostname)]"));
    }

    /// Context compaction keeps system + newest, drops oldest first.
    #[test]
    fn compact_for_context_drops_oldest() {
        let msgs = vec![
            json!({"role":"system","content":"sys"}),
            json!({"role":"user","content":"a".repeat(4000)}),
            json!({"role":"assistant","content":"b".repeat(4000)}),
            json!({"role":"user","content":"c".repeat(100)}),
        ];
        // ~2025 token est (8100 chars/4). Budget 900: elision is too
        // small to elide (4KB max) → the oldest non-anchor message
        // drops; system + task anchors + tail survive.
        let (fitted, dropped, _) = compact_for_context(msgs, 900);
        assert_eq!(dropped, 1);
        assert_eq!(fitted[0]["role"], "system");
        assert_eq!(fitted[1]["content"], "a".repeat(4000)); // task anchor
                                                            // marker + remaining tail
        assert!(fitted.iter().any(|m| m["content"]
            .as_str()
            .map(|s| s.contains("compacted away"))
            .unwrap_or(false)));
        assert_eq!(fitted.last().unwrap()["content"], "c".repeat(100));
        // Under budget → untouched.
        let msgs2 = vec![json!({"role":"system","content":"s"})];
        let (f2, d2, _) = compact_for_context(msgs2, 500);
        assert_eq!(d2, 0);
        assert_eq!(f2.len(), 1);
    }

    /// Permits are single-use, session-scoped, and tool-scoped.
    #[test]
    fn permits_are_single_use_and_scoped() {
        let state = AppState::new(cfg_with_image(false));
        let sid = crate::engine::session_new(&state, "/tmp", crate::protocol::ProtocolVersion::V1)
            .unwrap();

        // Capability-scoped permit consumes once.
        let p1 = state.session_permit(&sid, "shell", None, 60).unwrap();
        assert!(p1["permitId"].is_string());
        assert_eq!(p1["singleUse"], true);
        assert!(state
            .consume_permit(&sid, crate::policy::Capability::Shell, "bash")
            .is_some());
        assert!(
            state
                .consume_permit(&sid, crate::policy::Capability::Shell, "bash")
                .is_none(),
            "consumed permit must not match twice"
        );

        // Tool-scoped permit only unlocks its tool.
        state
            .session_permit(&sid, "shell", Some("git_status"), 60)
            .unwrap();
        assert!(
            state
                .consume_permit(&sid, crate::policy::Capability::Shell, "bash")
                .is_none(),
            "tool-scoped permit must not unlock another tool"
        );
        assert!(state
            .consume_permit(&sid, crate::policy::Capability::Shell, "git_status")
            .is_some());

        // Permits don't leak across sessions, and bad inputs fail clean.
        let sid2 = crate::engine::session_new(&state, "/tmp", crate::protocol::ProtocolVersion::V1)
            .unwrap();
        state.session_permit(&sid2, "web", None, 60).unwrap();
        assert!(
            state
                .consume_permit(&sid, crate::policy::Capability::Web, "web_fetch")
                .is_none(),
            "permit must not leak across sessions"
        );
        assert!(state.session_permit(&sid2, "bogus", None, 60).is_err());
        assert!(
            state
                .session_permit(&sid2, "shell", Some("write_file"), 60)
                .is_err(),
            "tool outside the capability must be rejected"
        );
        assert!(state.session_permit("no-such", "shell", None, 60).is_err());
    }

    #[test]
    fn extract_user_text_handles_pure_resource_link_object() {
        // Some Clients send a ResourceLink as the entire prompt.
        let prompt = serde_json::json!({
            "type": "resource_link",
            "uri": "file:///etc/hostname",
            "name": "hostname"
        });
        assert_eq!(
            extract_user_text_from_prompt(&prompt),
            "[Attached resource: hostname (file:///etc/hostname)]"
        );
    }

    #[test]
    fn extract_user_text_handles_resource_link_without_name() {
        let prompt = serde_json::json!({
            "type": "resource_link",
            "uri": "file:///x"
        });
        assert_eq!(
            extract_user_text_from_prompt(&prompt),
            "[Attached resource: file:///x]"
        );
    }

    #[test]
    fn extract_user_text_handles_plain_string() {
        let prompt = serde_json::json!("hello world");
        assert_eq!(extract_user_text_from_prompt(&prompt), "hello world");
    }

    #[test]
    fn extract_user_text_returns_empty_on_null() {
        let prompt = serde_json::Value::Null;
        assert_eq!(extract_user_text_from_prompt(&prompt), "");
    }

    #[test]
    fn extract_user_text_ignores_non_text_array_entries() {
        let prompt = serde_json::json!([
            {"type": "image", "data": "iVBORw0K..."},
            {"type": "text", "text": "describe this"}
        ]);
        assert_eq!(extract_user_text_from_prompt(&prompt), "describe this");
    }

    #[test]
    fn extract_user_images_handles_acp_array_shape() {
        let prompt = serde_json::json!([
            {"type": "text", "text": "describe"},
            {"type": "image", "data": "AAAA", "mimeType": "image/png"},
            {"type": "image", "data": "BBBB"}
        ]);
        let images = extract_user_images_from_prompt(&prompt);
        assert_eq!(
            images,
            vec![
                ImageBlock {
                    data: "AAAA".into(),
                    mime_type: "image/png".into()
                },
                ImageBlock {
                    data: "BBBB".into(),
                    mime_type: "image/jpeg".into()
                },
            ]
        );
    }

    #[test]
    fn extract_user_images_handles_single_image_object() {
        let prompt = serde_json::json!({
            "type": "image",
            "data": "AAAA",
            "mimeType": "image/webp"
        });
        let images = extract_user_images_from_prompt(&prompt);
        assert_eq!(
            images,
            vec![ImageBlock {
                data: "AAAA".into(),
                mime_type: "image/webp".into()
            }]
        );
    }

    #[test]
    fn extract_user_images_defaults_to_jpeg_when_mime_missing() {
        let prompt = serde_json::json!([{"type": "image", "data": "AAAA"}]);
        let images = extract_user_images_from_prompt(&prompt);
        assert_eq!(images.len(), 1);
        assert_eq!(images[0].mime_type, "image/jpeg");
    }

    #[test]
    fn extract_user_images_empty_on_string_prompt() {
        let prompt = serde_json::json!("just text");
        assert!(extract_user_images_from_prompt(&prompt).is_empty());
    }

    #[test]
    fn strip_sender_context_pulls_leading_block_and_returns_clean_text() {
        let input = "<sender_context>\n{\"schema\":\"openab.sender.v1\",\"user_id\":\"729\"}\n</sender_context>查一下大腦";
        let (cleaned, ctx) = strip_sender_context(input);
        assert_eq!(cleaned, "查一下大腦");
        assert!(ctx.is_some());
        let ctx = ctx.unwrap();
        assert!(ctx.contains("openab.sender.v1"));
        assert!(ctx.contains("729"));
    }

    #[test]
    fn strip_sender_context_passthrough_when_no_block() {
        let (cleaned, ctx) = strip_sender_context("hello world");
        assert_eq!(cleaned, "hello world");
        assert!(ctx.is_none());
    }

    #[test]
    fn strip_sender_context_handles_open_tag_without_close() {
        let input = "<sender_context>unterminated";
        let (cleaned, ctx) = strip_sender_context(input);
        assert_eq!(cleaned, input);
        assert!(ctx.is_none());
    }

    #[test]
    fn strip_sender_context_when_block_is_whole_input_yields_empty_text() {
        let input = "<sender_context>just metadata</sender_context>";
        let (cleaned, ctx) = strip_sender_context(input);
        assert_eq!(cleaned, "");
        assert_eq!(ctx.unwrap(), "just metadata");
    }

    #[test]
    fn response_helpers_support_ollama_and_openai_formats() {
        let tool_call = json!({"id": "call-1", "function": {"name": "read_file"}});
        let ollama = json!({
            "message": {"content": "ollama", "tool_calls": [tool_call.clone()]}
        });
        let openai = json!({
            "choices": [{"message": {"content": "openai", "tool_calls": [tool_call.clone()]}}]
        });

        assert_eq!(Backend::Ollama.extract_response_text(&ollama), "ollama");
        assert_eq!(
            Backend::Ollama.extract_tool_calls(&ollama),
            vec![tool_call.clone()]
        );
        assert_eq!(Backend::OpenAi.extract_response_text(&openai), "openai");
        assert_eq!(Backend::OpenAi.extract_tool_calls(&openai), vec![tool_call]);
    }

    fn cfg_with_image(supports_image: bool) -> LlmConfig {
        LlmConfig {
            base_url: "http://localhost:11434/v1".into(),
            model: "m".into(),
            api_key: "k".into(),
            system_prompt: None,
            temperature: None,
            max_tokens: None,
            timeout_secs: 5,
            max_history_turns: 50,
            max_sessions: 0,
            session_idle_timeout_secs: 0,
            prompt_supports_image: supports_image,
            context_size: 32768,
            max_tool_rounds: 5,
            tool_timeout_secs: 30,
            turn_timeout_secs: 0,
            tool_failure_streak: 3,
            client: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(5))
                .build()
                .expect("client"),
        }
    }

    #[test]
    fn initialize_does_not_advertise_image_by_default() {
        let caps = initialize(&cfg_with_image(false), ProtocolVersion::V1, false);
        assert_eq!(caps["protocolVersion"], 1);
        assert_eq!(
            caps["agentCapabilities"]["promptCapabilities"]["image"], false,
            "image must default to false to avoid clients forwarding image \
             attachments to local backends without vision"
        );
        assert_eq!(
            caps["agentCapabilities"]["promptCapabilities"]["audio"],
            false
        );
        assert_eq!(
            caps["agentCapabilities"]["promptCapabilities"]["embeddedContext"],
            false
        );
    }

    #[test]
    fn initialize_advertises_image_when_opted_in() {
        let caps = initialize(&cfg_with_image(true), ProtocolVersion::V1, false);
        assert_eq!(
            caps["agentCapabilities"]["promptCapabilities"]["image"], true,
            "image must be true when prompt_supports_image is set"
        );
    }

    #[test]
    fn initialize_v2_uses_unified_capabilities_and_info_shape() {
        let caps = initialize(&cfg_with_image(true), ProtocolVersion::V2, false);
        assert_eq!(caps["protocolVersion"], 2);
        // v2 collapses agentCapabilities / clientCapabilities into a single
        // `capabilities`, and `agentInfo` / `clientInfo` into `info`.
        // Spec deliberately forbids the v1-style aliases — only one of
        // each pair is permitted on the wire.
        assert!(caps.get("info").is_some(), "v2 must emit info");
        assert!(
            caps.get("capabilities").is_some(),
            "v2 must emit capabilities"
        );
        assert!(
            caps.get("agentInfo").is_none(),
            "v1 agentInfo must not appear on v2 wire"
        );
        assert!(
            caps.get("agentCapabilities").is_none(),
            "v1 agentCapabilities must not appear on v2 wire"
        );
        // Image is advertised as `{}` (capability marker) on v2, not `true`.
        let image = &caps["capabilities"]["session"]["prompt"]["image"];
        assert!(
            image.is_object(),
            "v2 image capability should be an object marker, got: {image}"
        );
    }

    #[test]
    fn initialize_v2_omits_image_when_disabled() {
        let caps = initialize(&cfg_with_image(false), ProtocolVersion::V2, false);
        let prompt_caps = &caps["capabilities"]["session"]["prompt"];
        assert!(
            prompt_caps.get("image").is_none(),
            "v2 image must be omitted (not `false`) when not supported; \
             clients interpret present-as-object as 'supported' and absent \
             as 'not advertised'"
        );
    }

    #[test]
    fn initialize_falls_back_to_v1_for_unknown_versions() {
        // Default Default for ProtocolVersion is V1, so omitting the arg
        // should produce the v1 wire shape — Clients that omit
        // protocolVersion still get a working session on the most widely
        // deployed wire shape.
        let caps = initialize(&cfg_with_image(false), ProtocolVersion::default(), false);
        assert_eq!(caps["protocolVersion"], 1);
    }

    #[test]
    fn notification_tool_start_carries_id() {
        let n = Notification::ToolStart {
            id: "tc_42".into(),
            name: "read_file".into(),
        };
        match n {
            Notification::ToolStart { id, name } => {
                assert_eq!(id, "tc_42");
                assert_eq!(name, "read_file");
            }
            _ => panic!("expected ToolStart"),
        }
    }

    #[test]
    fn notification_tool_done_carries_id_and_status() {
        let n = Notification::ToolDone {
            id: "tc_42".into(),
            name: "read_file".into(),
            status: "completed".into(),
        };
        match n {
            Notification::ToolDone { id, name, status } => {
                assert_eq!(id, "tc_42");
                assert_eq!(name, "read_file");
                assert_eq!(status, "completed");
            }
            _ => panic!("expected ToolDone"),
        }
    }
}
