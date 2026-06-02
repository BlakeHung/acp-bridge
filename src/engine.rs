//! Transport-agnostic engine — shared business logic for ACP and A2A transports.
//!
//! All session management, LLM interaction, and tool execution lives here.
//! Transport layers (stdin/stdout ACP, HTTP A2A) call these functions and
//! deliver results in their own format.

use crate::llm::{self, LlmConfig};
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

/// Maximum number of tool call rounds to prevent infinite loops.
const MAX_TOOL_ROUNDS: usize = 5;

/// Extract concatenated text from a slice of ACP/A2A content parts.
///
/// Each part is expected to be an object with `"type": "text"` and `"text": "..."`.
/// Non-text parts and malformed entries are skipped.
pub fn extract_text_parts(parts: &[Value]) -> String {
    parts
        .iter()
        .filter(|p| p.get("type").and_then(|t| t.as_str()) == Some("text"))
        .filter_map(|p| p.get("text").and_then(|t| t.as_str()))
        .collect::<Vec<_>>()
        .join("\n")
}

/// A multi-modal image block — base64 data plus the MIME type the client
/// declared. Default fallback is `image/jpeg` for clients that omit the
/// MIME (older or out-of-spec).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageBlock {
    pub data: String,
    pub mime_type: String,
}

const DEFAULT_IMAGE_MIME: &str = "image/jpeg";

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
///    not wrapped in a one-element array).
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
        Value::Object(_) => prompt
            .get("text")
            .and_then(|t| t.as_str())
            .map(str::to_owned)
            .unwrap_or_default(),
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
    ToolStart(String),
    ToolDone(String, String),
    TextChunk(String),
}

// ---------------------------------------------------------------------------
// AppState — shared state for all transports
// ---------------------------------------------------------------------------

pub struct AppState {
    pub sessions: RwLock<HashMap<String, Session>>,
    pub config: LlmConfig,
}

impl AppState {
    pub fn new(config: LlmConfig) -> Arc<Self> {
        Arc::new(Self {
            sessions: RwLock::new(HashMap::new()),
            config,
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
pub fn initialize(config: &LlmConfig) -> Value {
    info!(model = %config.model, base_url = %config.base_url, "Initialize");
    json!({
        "protocolVersion": 1,
        "agentInfo": {
            "name": format!("acp-bridge ({})", config.model),
            "version": env!("CARGO_PKG_VERSION")
        },
        "agentCapabilities": {
            "promptCapabilities": {
                "image": true
            }
        },
        "authMethods": []
    })
}

/// Handle `session/new` — creates a new session, returns session ID.
pub fn session_new(state: &AppState, cwd: &str) -> Result<String, AcpError> {
    // Enforce max_sessions limit
    if state.config.max_sessions > 0 {
        let count = state.sessions_read().len();
        if count >= state.config.max_sessions {
            return Err(AcpError::SessionLimitReached {
                max: state.config.max_sessions,
            });
        }
    }

    // Sanitize cwd
    let cwd: String = cwd
        .chars()
        .filter(|c| c.is_alphanumeric() || matches!(c, '/' | '-' | '_' | '.' | ' ' | '~'))
        .collect();

    let session_id = Uuid::new_v4().to_string();

    let system_prompt = state.config.system_prompt.clone().unwrap_or_else(|| {
        std::env::var("LLM_SYSTEM_PROMPT").unwrap_or_else(|_| {
            format!("You are a helpful coding assistant. The user's working directory is: {cwd}")
        })
    });

    let session = Session::new(
        json!({"role": "system", "content": system_prompt}),
        PathBuf::from(&cwd),
    );
    state.sessions_write().insert(session_id.clone(), session);

    info!(session_id = %session_id, max_history = state.config.max_history_turns, "New session");
    Ok(session_id)
}

/// Handle `session/prompt` — runs the LLM with tool loop.
///
/// Sends `Notification` events through `notify_tx` as they happen (for ACP streaming).
/// Returns the final status ("completed" or "failed") and accumulated text.
pub async fn session_prompt(
    state: &Arc<AppState>,
    session_id: &str,
    user_text: &str,
    user_images: &[ImageBlock],
    notify_tx: Option<mpsc::UnboundedSender<Notification>>,
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
                };
            }
        };
        session.touch();
        if user_images.is_empty() {
            session
                .messages
                .push(json!({"role": "user", "content": user_text}));
        } else {
            // Ollama native API: images are base64 strings in "images" array
            // OpenAI compat API: images are in content array with type "image_url"
            if state.config.is_ollama_native() {
                // Ollama native takes base64 strings in `images`; no MIME field.
                let images: Vec<&str> = user_images.iter().map(|i| i.data.as_str()).collect();
                session.messages.push(json!({
                    "role": "user",
                    "content": user_text,
                    "images": images
                }));
            } else {
                // OpenAI-compat multimodal: text + image_url parts in a
                // content array. user_images is guaranteed non-empty here
                // by the outer branch. Use each block's declared MIME type
                // so PNG/WebP/GIF aren't misencoded as JPEG.
                let mut content_parts: Vec<Value> =
                    vec![json!({"type": "text", "text": user_text})];
                for img in user_images {
                    content_parts.push(json!({
                        "type": "image_url",
                        "image_url": {
                            "url": format!("data:{};base64,{}", img.mime_type, img.data)
                        }
                    }));
                }
                session
                    .messages
                    .push(json!({"role": "user", "content": content_parts}));
            }
        }

        if state.config.max_history_turns > 0 {
            let before = session.messages.len();
            session.trim_history(state.config.max_history_turns);
            let after = session.messages.len();
            if before != after {
                debug!(before, after, "Trimmed conversation history");
            }
        }
    }

    notify(Notification::Thinking);
    notify(Notification::ToolStart("llm_chat".into()));

    let mut had_error = false;
    let mut final_text = String::new();
    let tool_defs = tools::tool_definitions();

    // Tool call loop
    for round in 0..MAX_TOOL_ROUNDS {
        let (messages, working_dir) = {
            let sessions = state.sessions_read();
            match sessions.get(session_id) {
                Some(s) => (s.messages.clone(), s.working_dir.clone()),
                None => break,
            }
        };

        let chat_result = llm::chat(&state.config, &messages, None, Some(&tool_defs)).await;

        match chat_result {
            Ok(response) => {
                let tool_calls = extract_tool_calls(&response);

                if tool_calls.is_empty() {
                    let text = extract_response_text(&response);
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

                // Execute tool calls
                info!(round, count = tool_calls.len(), "Executing tool calls");

                {
                    let mut sessions = state.sessions_write();
                    if let Some(session) = sessions.get_mut(session_id) {
                        let assistant_msg = if state.config.is_ollama_native() {
                            json!({"role": "assistant", "content": "", "tool_calls": tool_calls})
                        } else {
                            response["choices"][0]["message"].clone()
                        };
                        session.messages.push(assistant_msg);
                    }
                }

                for tc in &tool_calls {
                    let func = &tc["function"];
                    let name = func["name"].as_str().unwrap_or("unknown");
                    let args_str = func["arguments"].as_str().unwrap_or("{}");
                    let args: Value = serde_json::from_str(args_str)
                        .unwrap_or_else(|_| func["arguments"].clone());

                    notify(Notification::ToolStart(name.into()));
                    let result = tools::execute_tool(&working_dir, name, &args);
                    notify(Notification::ToolDone(name.into(), "completed".into()));

                    debug!(tool = name, result_len = result.len(), "Tool executed");

                    {
                        let tool_call_id =
                            tc.get("id").and_then(|v| v.as_str()).unwrap_or("unknown");
                        let mut sessions = state.sessions_write();
                        if let Some(session) = sessions.get_mut(session_id) {
                            session.messages.push(json!({
                                "role": "tool",
                                "content": result,
                                "tool_call_id": tool_call_id
                            }));
                        }
                    }
                }
            }
            Err(e) => {
                let err_msg = format!("\n\n**Error:** {e}\n");
                notify(Notification::TextChunk(err_msg.clone()));
                final_text = err_msg;
                had_error = true;
                error!(error = %e, "LLM communication failed");
                break;
            }
        }
    }

    let status = if had_error { "failed" } else { "completed" };
    notify(Notification::ToolDone("llm_chat".into(), status.into()));

    PromptResult {
        status: status.into(),
        text: final_text,
        error: None,
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

// ---------------------------------------------------------------------------
// Result type
// ---------------------------------------------------------------------------

pub struct PromptResult {
    pub status: String,
    pub text: String,
    pub error: Option<AcpError>,
}

// ---------------------------------------------------------------------------
// LLM response helpers (moved from main.rs)
// ---------------------------------------------------------------------------

/// Extract tool calls from an LLM response (supports both Ollama and OpenAI format).
fn extract_tool_calls(response: &Value) -> Vec<Value> {
    // Ollama native: response.message.tool_calls
    if let Some(calls) = response
        .get("message")
        .and_then(|m| m.get("tool_calls"))
        .and_then(|tc| tc.as_array())
    {
        return calls.clone();
    }

    // OpenAI compat: response.choices[0].message.tool_calls
    if let Some(calls) = response
        .get("choices")
        .and_then(|c| c.get(0))
        .and_then(|c| c.get("message"))
        .and_then(|m| m.get("tool_calls"))
        .and_then(|tc| tc.as_array())
    {
        return calls.clone();
    }

    vec![]
}

/// Extract text content from an LLM response (supports both formats).
fn extract_response_text(response: &Value) -> String {
    // Ollama native: response.message.content
    if let Some(text) = response
        .get("message")
        .and_then(|m| m.get("content"))
        .and_then(|c| c.as_str())
    {
        if !text.is_empty() {
            return text.to_string();
        }
    }

    // OpenAI compat: response.choices[0].message.content
    if let Some(text) = response
        .get("choices")
        .and_then(|c| c.get(0))
        .and_then(|c| c.get("message"))
        .and_then(|m| m.get("content"))
        .and_then(|c| c.as_str())
    {
        return text.to_string();
    }

    String::new()
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
