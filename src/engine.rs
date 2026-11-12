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
use std::path::PathBuf;
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

/// Derive a short session title from a `session/prompt` payload.
///
/// Clients such as Xcode send several text blocks per prompt: an injected
/// `<system-reminder>...</system-reminder>` block, project context, and
/// the user's actual message last. Using the first line of the joined text
/// would title every Xcode chat "<system-reminder>## Xcode", so this strips
/// system-reminder blocks and takes the first non-empty line of the last
/// non-empty text block (capped at 80 chars).
pub fn derive_prompt_title(prompt: &Value) -> Option<String> {
    let blocks: Vec<String> = match prompt {
        Value::String(s) => vec![s.clone()],
        Value::Array(parts) => parts
            .iter()
            .filter(|p| p.get("type").and_then(|t| t.as_str()) == Some("text"))
            .filter_map(|p| p.get("text").and_then(|t| t.as_str()).map(str::to_string))
            .collect(),
        _ => Vec::new(),
    };
    blocks
        .iter()
        .rev()
        .map(|b| strip_system_reminders(b))
        .find(|b| !b.trim().is_empty())
        .and_then(|b| {
            b.lines()
                .find(|l| !l.trim().is_empty())
                .map(|l| l.trim().chars().take(80).collect::<String>())
        })
        .filter(|s| !s.is_empty())
}

fn strip_system_reminders(text: &str) -> String {
    const OPEN: &str = "<system-reminder>";
    const CLOSE: &str = "</system-reminder>";
    let mut out = String::new();
    let mut rest = text;
    while let Some(start) = rest.find(OPEN) {
        out.push_str(&rest[..start]);
        match rest[start..].find(CLOSE) {
            Some(end) => rest = &rest[start + end + CLOSE.len()..],
            None => {
                rest = "";
                break;
            }
        }
    }
    out.push_str(rest);
    out
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
        /// tool_call_update updates. The LLM assigns this id per call.
        id: String,
        name: String,
        /// Parsed tool arguments, surfaced to the Client as `rawInput` and
        /// used to build a descriptive title (e.g. the bash command).
        input: Option<Value>,
        /// Absolute path the tool operates on, surfaced as `locations`.
        location: Option<String>,
    },
    ToolDone {
        id: String,
        name: String,
        status: String,
        /// Tool result text, surfaced as `content` / `rawOutput`.
        output: Option<String>,
    },
    TextChunk(String),
    /// Model reasoning streamed live (`agent_thought_chunk`).
    ThoughtChunk(String),
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
    /// How the Client's `mcpServers` (and Xcode's tool server) are handled.
    pub mcp: crate::mcp::McpConfig,
    /// `clientInfo.name` / `title` from `initialize`, used to detect Xcode.
    pub client_name: Option<String>,
}

impl Clone for AppState {
    fn clone(&self) -> Self {
        Self {
            sessions: Arc::clone(&self.sessions),
            turn_registry: Arc::clone(&self.turn_registry),
            config: self.config.clone(),
            protocol_version: self.protocol_version,
            mcp: self.mcp.clone(),
            client_name: self.client_name.clone(),
        }
    }
}

impl AppState {
    pub fn new(config: LlmConfig) -> Arc<Self> {
        Self::with_mcp(config, crate::mcp::McpConfig::default())
    }

    pub fn with_mcp(config: LlmConfig, mcp: crate::mcp::McpConfig) -> Arc<Self> {
        Arc::new(Self {
            sessions: Arc::new(RwLock::new(HashMap::new())),
            turn_registry: Arc::new(std::sync::Mutex::new(HashMap::new())),
            config,
            // Default to V1 for safety. `main::run_acp_loop` overwrites
            // this with whatever the Client negotiated during
            // `initialize`. We pick V1 here so unit tests that build an
            // AppState directly get v1 wire format without ceremony.
            protocol_version: crate::protocol::ProtocolVersion::V1,
            mcp,
            client_name: None,
        })
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
pub fn initialize(config: &LlmConfig, protocol_version: crate::protocol::ProtocolVersion) -> Value {
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

    match protocol_version {
        crate::protocol::ProtocolVersion::V2 => json!({
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
        }),
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
                    // stdio MCP servers (the ACP baseline) are relayed;
                    // http / sse transports are not.
                    "mcpCapabilities": {"http": false, "sse": false}
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
    session_new_with_mcp(state, cwd, protocol_version, None)
}

/// [`session_new`] plus the Client's `mcpServers` param. The servers
/// (and Xcode's tool server, see [`crate::mcp::resolve_servers`]) start
/// connecting in the background; their tools join the built-in ones from
/// the first prompt on.
pub fn session_new_with_mcp(
    state: &AppState,
    cwd: &str,
    protocol_version: crate::protocol::ProtocolVersion,
    mcp_servers: Option<&Value>,
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

    let cwd = normalize_cwd(cwd);
    let p = std::path::Path::new(&cwd);
    if !p.is_absolute() || !p.is_dir() {
        warn!(cwd = %cwd, "session/new cwd is not an existing absolute directory; tools may fail");
    }

    let session_id = Uuid::new_v4().to_string();

    let system_prompt = state.config.system_prompt.clone().unwrap_or_else(|| {
        std::env::var("LLM_SYSTEM_PROMPT").unwrap_or_else(|_| {
            format!(
                "You are a helpful coding assistant running locally through acp-bridge. \
                 The underlying model is `{}`. The user's working directory is: {cwd:?}",
                state.config.model
            )
        })
    });

    let mut session = Session::new(
        json!({"role": "system", "content": system_prompt}),
        PathBuf::from(&cwd),
        protocol_version,
    );
    let specs = crate::mcp::resolve_servers(&state.mcp, mcp_servers, state.client_name.as_deref());
    if !specs.is_empty() {
        let set = crate::mcp::McpSet::start(
            specs,
            PathBuf::from(&cwd),
            state.mcp.clone(),
            state.config.context_size,
        );
        info!(session_id = %session_id, servers = ?set.server_names(), "Starting MCP servers");
        session.mcp = Some(set);
    }
    state.sessions_write().insert(session_id.clone(), session);

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

/// Clean a Client-supplied `cwd` without changing the path it names.
///
/// The path is used verbatim for tool execution, so legal path characters
/// (`(`, `+`, `@`, `,`, non-ASCII, …) must survive; an earlier allow-list
/// filter silently turned `~/My App (2)` into a directory that does not
/// exist. Only control characters are removed (a newline could otherwise
/// break out of the system-prompt line the path appears on, where it is
/// also quoted). Whitespace is kept: a directory name may legally start or
/// end with a space. A value that is empty or only whitespace falls back
/// to `/tmp`.
fn normalize_cwd(raw: &str) -> String {
    let cleaned: String = raw.chars().filter(|c| !c.is_control()).collect();
    if cleaned.trim().is_empty() {
        "/tmp".to_string()
    } else {
        cleaned
    }
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
/// Returns the final status ("completed" or "failed") and accumulated text.
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

        {
            let removed = session.trim_history_to(
                state.config.max_history_turns,
                Some(history_token_budget(
                    &state.config,
                    &tools::tool_definitions(),
                )),
            );
            if removed > 0 {
                debug!(removed, "Trimmed conversation history");
            }
        }
    }

    notify(Notification::Thinking);

    let mcp = state
        .sessions_read()
        .get(session_id)
        .and_then(|s| s.mcp.clone());
    if let Some(mcp) = &mcp {
        prepare_mcp(state, session_id, mcp, &notify).await;
    }

    let mut had_error = false;
    let mut got_final_response = false;
    let mut final_text = String::new();
    let mut last_error_class: Option<crate::llm::LlmErrorKind> = None;
    let builtin_defs = tools::tool_definitions();
    let mut tool_defs = builtin_defs.clone();

    // Tool call loop
    let max_rounds = state.config.max_tool_rounds;
    // Whether any answer text has reached the Client this turn, so text from
    // a later round is separated from earlier commentary.
    let mut emitted_any_text = false;
    for round in 0..state.config.tool_round_cap() {
        // MCP servers may change their tool list between rounds
        // (`notifications/tools/list_changed`).
        if let Some(mcp) = &mcp {
            tool_defs = builtin_defs.clone();
            tool_defs.extend(mcp.tool_definitions().await);
        }
        let budget = history_token_budget(&state.config, &tool_defs);
        let (messages, working_dir) = {
            let mut sessions = state.sessions_write();
            match sessions.get_mut(session_id) {
                Some(s) => {
                    // Tool rounds grow the history within a turn; drop
                    // older turns (never the current one) to stay in budget.
                    if round > 0 || mcp.is_some() {
                        let removed = s.trim_history_to(0, Some(budget));
                        if removed > 0 {
                            info!(round, removed, "Trimmed older turns to fit context budget");
                        }
                    }
                    (s.messages.clone(), s.working_dir.clone())
                }
                None => {
                    warn!(session_id, "Session removed during prompt processing");
                    had_error = true;
                    final_text = format!("Session {session_id} was removed during processing");
                    break;
                }
            }
        };

        let backend = state.config.backend();
        let mut streamed_text = false;
        let chat_result = if state.config.stream {
            let separate = emitted_any_text;
            llm::chat_streaming(
                &state.config,
                &messages,
                Some(&tool_defs),
                |delta| match delta {
                    llm::StreamDelta::Content(t) => {
                        if streamed_text {
                            notify(Notification::TextChunk(t));
                            return;
                        }
                        // Drop leading whitespace of a round's text (models
                        // often emit bare newlines around tool calls) and
                        // separate it from the previous round's commentary.
                        let t = t.trim_start();
                        if t.is_empty() {
                            return;
                        }
                        if separate {
                            notify(Notification::TextChunk("\n\n".into()));
                        }
                        streamed_text = true;
                        notify(Notification::TextChunk(t.to_string()));
                    }
                    llm::StreamDelta::Reasoning(t) => notify(Notification::ThoughtChunk(t)),
                },
            )
            .await
        } else {
            llm::chat(&state.config, &messages, None, Some(&tool_defs)).await
        };
        emitted_any_text |= streamed_text;

        match chat_result {
            Ok(response) => {
                let mut tool_calls = backend.extract_tool_calls(&response);

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
                        {
                            let mut sessions = state.sessions_write();
                            if let Some(session) = sessions.get_mut(session_id) {
                                let assistant_msg = backend.format_assistant_message(
                                    &clean_text,
                                    &tool_calls,
                                    &response,
                                );
                                session.messages.push(assistant_msg);
                            }
                        }
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
                            // Already delivered live when streaming.
                            if !streamed_text && !text.trim().is_empty() {
                                notify(Notification::TextChunk(text));
                            }
                        }
                        break;
                    }
                }

                // Execute tool calls
                info!(round, count = tool_calls.len(), "Executing tool calls");

                {
                    let mut sessions = state.sessions_write();
                    if let Some(session) = sessions.get_mut(session_id) {
                        let assistant_msg =
                            backend.format_assistant_message("", &tool_calls, &response);
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

                    // Xcode's tools take project-navigator paths, not
                    // filesystem paths, so only built-ins get a location.
                    let location = args
                        .get("path")
                        .and_then(|p| p.as_str())
                        .filter(|_| crate::mcp::is_builtin_tool(name))
                        .map(|p| {
                            let p = std::path::Path::new(p);
                            if p.is_absolute() {
                                p.display().to_string()
                            } else {
                                std::path::Path::new(&working_dir)
                                    .join(p)
                                    .display()
                                    .to_string()
                            }
                        });
                    notify(Notification::ToolStart {
                        id: tool_call_id.to_string(),
                        name: name.into(),
                        input: Some(args.clone()),
                        location,
                    });
                    // Async + cancellable: a `session/cancel` aborts this
                    // future and kills the command's process group.
                    let mcp_result = match &mcp {
                        Some(mcp) => mcp.call(name, &args).await,
                        None => None,
                    };
                    let result = match mcp_result {
                        Some(r) => r,
                        None => {
                            tools::execute_tool_async(
                                &working_dir,
                                name,
                                &args,
                                state.config.bash_timeout(),
                            )
                            .await
                        }
                    };
                    notify(Notification::ToolDone {
                        id: tool_call_id.to_string(),
                        name: name.into(),
                        status: "completed".into(),
                        output: Some(result.clone()),
                    });

                    debug!(tool = name, result_len = result.len(), "Tool executed");

                    {
                        let mut sessions = state.sessions_write();
                        if let Some(session) = sessions.get_mut(session_id) {
                            session
                                .messages
                                .push(backend.format_tool_result(tool_call_id, &result));
                        }
                    }
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

    // The loop can also exit by exhausting max_tool_rounds while the model
    // keeps requesting tools. Without this the turn would be reported as a
    // successful "completed" with empty text, silently swallowing the fact
    // that the model never produced a final answer.
    if !had_error && !got_final_response {
        warn!(
            max_rounds,
            "Tool-call loop hit round limit without a final response"
        );
        let msg = format!(
            "\n\n**Error:** reached the tool-call limit ({max_rounds} rounds) without a final answer\n"
        );
        notify(Notification::TextChunk(msg.clone()));
        final_text = msg;
        had_error = true;
    }

    // Estimate the current context utilization for `usage_update`. Local
    // backends rarely stream per-turn token counts in a stable shape, so
    // we estimate by summing the textual length of every message in the
    // session history and dividing by 4 chars per token (the canonical
    // LLM rule of thumb). This is intentionally approximate; clients use
    // it for progress bars and not for cost attribution.
    let status = if had_error { "failed" } else { "completed" };

    let used_tokens = {
        let sessions = state.sessions_read();
        sessions
            .get(session_id)
            .map(|s| estimate_tokens(&s.messages))
            .unwrap_or(0)
    };

    PromptResult {
        status: status.into(),
        text: final_text,
        error: None,
        usage: UsageReport {
            used: used_tokens,
            size: state.config.context_size,
        },
        error_class: last_error_class.clone(),
        error_retryable: last_error_class.as_ref().is_some_and(|k| k.is_retryable()),
        message_id: message_id.to_string(),
    }
}

/// Token budget for conversation history: 75% of the model context minus
/// the tool definitions sent with every request, leaving the rest for the
/// model's reply. Never below 1/4 of the context, so a large tool set
/// cannot starve the conversation entirely.
fn history_token_budget(config: &crate::llm::LlmConfig, tool_defs: &[Value]) -> u64 {
    let tools: u64 = tool_defs
        .iter()
        .map(|d| (d.to_string().len() / 4) as u64)
        .sum();
    (config.context_size / 4 * 3)
        .saturating_sub(tools)
        .max(config.context_size / 4)
}

/// Wait for the session's MCP servers, report any that failed to start
/// (as a thought, once), and append their tool guidance to the system
/// prompt the first time tools are available.
async fn prepare_mcp(
    state: &AppState,
    session_id: &str,
    mcp: &crate::mcp::McpSet,
    notify: &impl Fn(Notification),
) {
    // Servers connect in the background from `session/new`; Xcode's can
    // take a while the first time. Say so instead of stalling silently.
    if !mcp.is_ready() && !mcp.wait_ready(Duration::from_millis(1500)).await {
        let what = if mcp.has_xcode() {
            "Xcode tools"
        } else {
            "MCP tools"
        };
        notify(Notification::ThoughtChunk(format!(
            "Connecting to {what}…\n"
        )));
        // Bounded by the servers' connect timeout.
        mcp.wait_connected().await;
    }
    for failure in mcp.take_failures().await {
        notify(Notification::ThoughtChunk(format!(
            "acp-bridge: MCP server unavailable ({failure})\n"
        )));
    }
    let applied = state
        .sessions_read()
        .get(session_id)
        .is_none_or(|s| s.mcp_prompt_applied);
    if applied {
        return;
    }
    let Some(addendum) = mcp.system_prompt_addendum().await else {
        return;
    };
    let mut sessions = state.sessions_write();
    if let Some(s) = sessions.get_mut(session_id) {
        s.mcp_prompt_applied = true;
        if let Some(Value::String(content)) =
            s.messages.first_mut().and_then(|m| m.get_mut("content"))
        {
            content.push_str(&addendum);
        }
    }
}

/// Rough token estimate from a list of chat messages. Sums the textual
/// content of every message and divides by 4 chars/token. We do NOT try
/// to be precise — local backends do not stream stable per-turn token
/// counts, and clients use this only for progress visualisation.
pub fn estimate_tokens(messages: &[Value]) -> u64 {
    messages
        .iter()
        .map(crate::protocol::estimate_message_tokens)
        .sum::<u64>()
        .max(1)
}

/// Close a turn aborted by `session/cancel`. A trailing user message with
/// no reply would put two user messages in a row on the next prompt,
/// which chat templates that require alternating roles reject.
pub fn session_cancelled(state: &AppState, session_id: &str) {
    if let Some(session) = state.sessions_write().get_mut(session_id) {
        if session.messages.last().is_some_and(|m| m["role"] == "user") {
            session
                .messages
                .push(json!({"role": "assistant", "content": "[cancelled by user]"}));
        }
    }
}

/// Handle `session/end` — removes a session.
pub fn session_end(state: &AppState, session_id: &str) -> Result<(), AcpError> {
    let removed = state.sessions_write().remove(session_id).is_some();
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

pub struct PromptResult {
    pub status: String,
    pub text: String,
    pub error: Option<AcpError>,
    pub usage: UsageReport,
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

    #[test]
    fn normalize_cwd_keeps_legal_path_characters() {
        for p in [
            "/Users/erik/Developer/My App (2)",
            "/Users/erik/Foo+Bar/a,b@c",
            "/Users/erik/Développement/プロジェクト",
            "/tmp/test",
            "/Users/erik/Project ",
            "/Users/erik/ Leading",
        ] {
            assert_eq!(normalize_cwd(p), p);
        }
    }

    #[test]
    fn normalize_cwd_strips_control_characters_only() {
        assert_eq!(
            normalize_cwd("/tmp/a\nIGNORE PREVIOUS\0"),
            "/tmp/aIGNORE PREVIOUS"
        );
        assert_eq!(normalize_cwd("   "), "/tmp");
        assert_eq!(normalize_cwd(""), "/tmp");
        assert_eq!(normalize_cwd("\n\t"), "/tmp");
    }

    #[test]
    fn session_working_dir_is_the_path_the_client_sent() {
        let dir = std::env::temp_dir().join("acp bridge (cwd) +test");
        std::fs::create_dir_all(&dir).unwrap();
        let state = AppState::with_mcp(
            cfg_with_image(false),
            crate::mcp::McpConfig {
                enabled: false,
                xcode: crate::mcp::XcodeMode::Off,
                ..Default::default()
            },
        );
        let cwd = dir.to_str().unwrap();
        let sid = session_new(&state, cwd, crate::protocol::ProtocolVersion::V1).unwrap();
        let sessions = state.sessions_read();
        let s = &sessions[&sid];
        assert_eq!(s.working_dir, dir);
        assert!(s.messages[0]["content"]
            .as_str()
            .unwrap()
            .contains(&format!("{cwd:?}")));
        drop(sessions);
        let _ = std::fs::remove_dir(&dir);
    }

    #[test]
    fn derive_prompt_title_skips_xcode_system_reminder() {
        let prompt = json!([
            {"type": "text", "text": "<system-reminder>## Xcode\nstuff\n</system-reminder>"},
            {"type": "text", "text": "HFCache/Models.swift\nThe user has no file currently open.\n"},
            {"type": "text", "text": "what model are you currently using?"}
        ]);
        assert_eq!(
            derive_prompt_title(&prompt).as_deref(),
            Some("what model are you currently using?")
        );
    }

    #[test]
    fn derive_prompt_title_single_block_and_string() {
        let prompt = json!([{"type": "text", "text": "\n  Fix the bug\nmore"}]);
        assert_eq!(derive_prompt_title(&prompt).as_deref(), Some("Fix the bug"));
        assert_eq!(
            derive_prompt_title(&json!("hello")).as_deref(),
            Some("hello")
        );
        let only_reminder =
            json!([{"type": "text", "text": "<system-reminder>x</system-reminder>"}]);
        assert_eq!(derive_prompt_title(&only_reminder), None);
    }
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
            max_tool_rounds: crate::llm::DEFAULT_MAX_TOOL_ROUNDS,
            bash_timeout_secs: crate::llm::DEFAULT_BASH_TIMEOUT_SECS,
            stream: true,
            client: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(5))
                .build()
                .expect("client"),
        }
    }

    #[test]
    fn initialize_does_not_advertise_image_by_default() {
        let caps = initialize(&cfg_with_image(false), ProtocolVersion::V1);
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
        let caps = initialize(&cfg_with_image(true), ProtocolVersion::V1);
        assert_eq!(
            caps["agentCapabilities"]["promptCapabilities"]["image"], true,
            "image must be true when prompt_supports_image is set"
        );
    }

    #[test]
    fn initialize_v2_uses_unified_capabilities_and_info_shape() {
        let caps = initialize(&cfg_with_image(true), ProtocolVersion::V2);
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
        let caps = initialize(&cfg_with_image(false), ProtocolVersion::V2);
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
        let caps = initialize(&cfg_with_image(false), ProtocolVersion::default());
        assert_eq!(caps["protocolVersion"], 1);
    }

    #[test]
    fn notification_tool_start_carries_id() {
        let n = Notification::ToolStart {
            id: "tc_42".into(),
            name: "read_file".into(),
            input: None,
            location: None,
        };
        match n {
            Notification::ToolStart { id, name, .. } => {
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
            output: None,
        };
        match n {
            Notification::ToolDone {
                id, name, status, ..
            } => {
                assert_eq!(id, "tc_42");
                assert_eq!(name, "read_file");
                assert_eq!(status, "completed");
            }
            _ => panic!("expected ToolDone"),
        }
    }
}
