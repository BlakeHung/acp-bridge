//! ACP Client — spawn and communicate with external ACP agents.
//!
//! This module lets acp-bridge act as an **ACP client** (orchestrator),
//! spawning external agents (OpenCode, Claude Code, Kiro, etc.) as child
//! processes and communicating via stdin/stdout JSON-RPC 2.0.
//!
//! Ported from openab's AcpConnection pattern.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin};
use tokio::sync::{mpsc, oneshot, Mutex};
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tracing::{debug, error, info, warn};

// ---------------------------------------------------------------------------
// JSON-RPC types (client-side: we send requests, receive responses)
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
struct JsonRpcRequest {
    jsonrpc: &'static str,
    id: u64,
    method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    params: Option<Value>,
}

impl JsonRpcRequest {
    fn new(id: u64, method: impl Into<String>, params: Option<Value>) -> Self {
        Self {
            jsonrpc: "2.0",
            id,
            method: method.into(),
            params,
        }
    }
}

#[derive(Debug, Serialize)]
struct JsonRpcResponse {
    jsonrpc: &'static str,
    id: u64,
    result: Value,
}

impl JsonRpcResponse {
    fn new(id: u64, result: Value) -> Self {
        Self {
            jsonrpc: "2.0",
            id,
            result,
        }
    }
}

/// Incoming JSON-RPC message — could be a response or notification.
#[derive(Debug, Clone, Deserialize)]
pub struct JsonRpcMessage {
    pub id: Option<u64>,
    pub method: Option<String>,
    pub result: Option<Value>,
    pub error: Option<JsonRpcError>,
    pub params: Option<Value>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct JsonRpcError {
    pub code: i64,
    pub message: String,
}

impl std::fmt::Display for JsonRpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "JSON-RPC error {}: {}", self.code, self.message)
    }
}

// ---------------------------------------------------------------------------
// ACP event classification
// ---------------------------------------------------------------------------

/// Classified ACP notification event.
#[derive(Debug)]
pub enum AcpEvent {
    Text(String),
    Thinking,
    ToolStart {
        id: String,
        title: String,
    },
    ToolDone {
        id: String,
        title: String,
        status: String,
    },
    Status,
}

/// Classify a JSON-RPC notification into an AcpEvent.
pub fn classify_notification(msg: &JsonRpcMessage) -> Option<AcpEvent> {
    let params = msg.params.as_ref()?;
    let update = params.get("update")?;
    let session_update = update.get("sessionUpdate")?.as_str()?;

    let tool_id = update
        .get("toolCallId")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    match session_update {
        "agent_message_chunk" => {
            let text = update.get("content")?.get("text")?.as_str()?;
            Some(AcpEvent::Text(text.to_string()))
        }
        "agent_thought_chunk" => Some(AcpEvent::Thinking),
        "tool_call" => {
            let title = update
                .get("title")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            Some(AcpEvent::ToolStart { id: tool_id, title })
        }
        "tool_call_update" => {
            let title = update
                .get("title")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let status = update
                .get("status")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            if status == "completed" || status == "failed" {
                Some(AcpEvent::ToolDone {
                    id: tool_id,
                    title,
                    status,
                })
            } else {
                Some(AcpEvent::ToolStart { id: tool_id, title })
            }
        }
        "plan" => Some(AcpEvent::Status),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Content blocks
// ---------------------------------------------------------------------------

/// A content block for the ACP prompt — text or image.
#[derive(Debug, Clone)]
pub enum ContentBlock {
    Text { text: String },
    Image { media_type: String, data: String },
}

impl ContentBlock {
    pub fn text(text: impl Into<String>) -> Self {
        Self::Text { text: text.into() }
    }

    fn to_json(&self) -> Value {
        match self {
            ContentBlock::Text { text } => json!({"type": "text", "text": text}),
            ContentBlock::Image { media_type, data } => {
                json!({"type": "image", "data": data, "mimeType": media_type})
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Permission auto-reply
// ---------------------------------------------------------------------------

/// Pick the most permissive selectable option from ACP permission options.
fn pick_best_option(options: &[Value]) -> Option<String> {
    // Prefer allow_always > allow_once
    for kind in ["allow_always", "allow_once"] {
        if let Some(option) = options
            .iter()
            .find(|o| o.get("kind").and_then(|k| k.as_str()) == Some(kind))
        {
            return option
                .get("optionId")
                .and_then(|id| id.as_str())
                .map(str::to_owned);
        }
    }

    // Fallback: first non-reject option
    for option in options {
        let kind = option.get("kind").and_then(|k| k.as_str());
        if kind == Some("reject_once") || kind == Some("reject_always") {
            continue;
        }
        return option
            .get("optionId")
            .and_then(|id| id.as_str())
            .map(str::to_owned);
    }

    None
}

/// Build a permission response for `session/request_permission`.
fn build_permission_response(params: Option<&Value>) -> Value {
    match params
        .and_then(|p| p.get("options"))
        .and_then(|o| o.as_array())
    {
        None => json!({"outcome": {"outcome": "selected", "optionId": "allow_always"}}),
        Some(options) => {
            if let Some(option_id) = pick_best_option(options) {
                json!({"outcome": {"outcome": "selected", "optionId": option_id}})
            } else {
                json!({"outcome": {"outcome": "cancelled"}})
            }
        }
    }
}

/// Expand `${VAR}` environment variable references.
fn expand_env(val: &str) -> String {
    if val.starts_with("${") && val.ends_with('}') {
        let key = &val[2..val.len() - 1];
        std::env::var(key).unwrap_or_default()
    } else {
        val.to_string()
    }
}

// ---------------------------------------------------------------------------
// AcpConnection — the core client
// ---------------------------------------------------------------------------

/// A connection to an external ACP agent running as a child process.
///
/// Communicates via stdin/stdout JSON-RPC 2.0. Automatically handles
/// `session/request_permission` by approving with the most permissive option.
pub struct AcpConnection {
    _proc: Child,
    /// Process group ID for clean kill.
    child_pgid: Option<i32>,
    stdin: Arc<Mutex<ChildStdin>>,
    next_id: AtomicU64,
    pending: Arc<Mutex<HashMap<u64, oneshot::Sender<JsonRpcMessage>>>>,
    notify_tx: Arc<Mutex<Option<mpsc::UnboundedSender<JsonRpcMessage>>>>,
    /// The ACP session ID (set after session/new or session/load).
    pub acp_session_id: Option<String>,
    /// Whether the agent supports session/load for resuming sessions.
    pub supports_load_session: bool,
    /// Last activity timestamp.
    pub last_active: Instant,
    _reader_handle: JoinHandle<()>,
}

impl AcpConnection {
    /// Spawn an external ACP agent as a child process.
    pub async fn spawn(
        command: &str,
        args: &[String],
        working_dir: &str,
        env: &HashMap<String, String>,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        info!(
            cmd = command,
            ?args,
            cwd = working_dir,
            "Spawning ACP agent"
        );

        let mut cmd = tokio::process::Command::new(command);
        cmd.args(args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .current_dir(working_dir);

        // Create a new process group for clean kill on drop.
        #[cfg(unix)]
        unsafe {
            cmd.pre_exec(|| {
                if libc::setpgid(0, 0) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }

        for (k, v) in env {
            cmd.env(k, expand_env(v));
        }

        let mut proc = cmd
            .spawn()
            .map_err(|e| format!("Failed to spawn agent '{command}': {e}"))?;

        let child_pgid = proc.id().and_then(|pid| i32::try_from(pid).ok());
        let stdout = proc.stdout.take().ok_or("Failed to capture agent stdout")?;
        let stdin = proc.stdin.take().ok_or("Failed to capture agent stdin")?;
        let stdin = Arc::new(Mutex::new(stdin));

        let pending: Arc<Mutex<HashMap<u64, oneshot::Sender<JsonRpcMessage>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let notify_tx: Arc<Mutex<Option<mpsc::UnboundedSender<JsonRpcMessage>>>> =
            Arc::new(Mutex::new(None));

        // Background reader task: reads stdout, dispatches responses/notifications
        let reader_handle = {
            let pending = pending.clone();
            let notify_tx = notify_tx.clone();
            let stdin_clone = stdin.clone();
            tokio::spawn(async move {
                let mut reader = BufReader::new(stdout);
                let mut line = String::new();
                loop {
                    line.clear();
                    match reader.read_line(&mut line).await {
                        Ok(0) => break, // EOF
                        Ok(_) => {}
                        Err(e) => {
                            error!("Agent stdout reader error: {e}");
                            break;
                        }
                    }

                    let msg: JsonRpcMessage = match serde_json::from_str(line.trim()) {
                        Ok(m) => m,
                        Err(_) => continue,
                    };
                    debug!(line = line.trim(), "acp_recv");

                    // Auto-reply session/request_permission
                    if msg.method.as_deref() == Some("session/request_permission") {
                        if let Some(id) = msg.id {
                            let title = msg
                                .params
                                .as_ref()
                                .and_then(|p| p.get("toolCall"))
                                .and_then(|t| t.get("title"))
                                .and_then(|t| t.as_str())
                                .unwrap_or("?");

                            let outcome = build_permission_response(msg.params.as_ref());
                            info!(title, %outcome, "Auto-respond permission");
                            let reply = JsonRpcResponse::new(id, outcome);
                            if let Ok(data) = serde_json::to_string(&reply) {
                                let mut w = stdin_clone.lock().await;
                                let _ = w.write_all(format!("{data}\n").as_bytes()).await;
                                let _ = w.flush().await;
                            }
                        }
                        continue;
                    }

                    // Response (has id) → resolve pending AND forward to subscriber
                    if let Some(id) = msg.id {
                        let mut map = pending.lock().await;
                        if let Some(tx) = map.remove(&id) {
                            // Also forward to subscriber so they see prompt completion
                            let sub = notify_tx.lock().await;
                            if let Some(ntx) = sub.as_ref() {
                                let _ = ntx.send(JsonRpcMessage {
                                    id: Some(id),
                                    method: None,
                                    result: msg.result.clone(),
                                    error: msg.error.clone(),
                                    params: None,
                                });
                            }
                            let _ = tx.send(msg);
                            continue;
                        }
                    }

                    // Notification → forward to subscriber
                    let sub = notify_tx.lock().await;
                    if let Some(tx) = sub.as_ref() {
                        let _ = tx.send(msg);
                    }
                }

                // Connection closed — resolve all pending with error
                let mut map = pending.lock().await;
                for (_, tx) in map.drain() {
                    let _ = tx.send(JsonRpcMessage {
                        id: None,
                        method: None,
                        result: None,
                        error: Some(JsonRpcError {
                            code: -1,
                            message: "agent connection closed".into(),
                        }),
                        params: None,
                    });
                }
            })
        };

        Ok(Self {
            _proc: proc,
            child_pgid,
            stdin,
            next_id: AtomicU64::new(1),
            pending,
            notify_tx,
            acp_session_id: None,
            supports_load_session: false,
            last_active: Instant::now(),
            _reader_handle: reader_handle,
        })
    }

    fn next_id(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }

    async fn send_raw(&self, data: &str) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        debug!(data = data.trim(), "acp_send");
        let mut w = self.stdin.lock().await;
        w.write_all(data.as_bytes()).await?;
        w.write_all(b"\n").await?;
        w.flush().await?;
        Ok(())
    }

    async fn send_request(
        &self,
        method: &str,
        params: Option<Value>,
    ) -> Result<JsonRpcMessage, Box<dyn std::error::Error + Send + Sync>> {
        let id = self.next_id();
        let req = JsonRpcRequest::new(id, method, params);
        let data = serde_json::to_string(&req)?;

        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);

        if let Err(e) = self.send_raw(&data).await {
            self.pending.lock().await.remove(&id);
            return Err(e);
        }

        let timeout_secs = if method == "session/new" { 120 } else { 30 };
        let resp =
            match tokio::time::timeout(std::time::Duration::from_secs(timeout_secs), rx).await {
                Ok(Ok(resp)) => resp,
                Ok(Err(_)) => {
                    self.pending.lock().await.remove(&id);
                    return Err(format!("Channel closed waiting for {method}").into());
                }
                Err(_) => {
                    self.pending.lock().await.remove(&id);
                    return Err(format!("Timeout waiting for {method} response").into());
                }
            };

        if let Some(err) = &resp.error {
            return Err(format!("{err}").into());
        }
        Ok(resp)
    }

    /// Send `initialize` to the agent and detect capabilities.
    pub async fn initialize(&mut self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let resp = self
            .send_request(
                "initialize",
                Some(json!({
                    "protocolVersion": 1,
                    "clientCapabilities": {},
                    "clientInfo": {"name": "acp-bridge", "version": env!("CARGO_PKG_VERSION")},
                })),
            )
            .await?;

        let result = resp.result.as_ref();
        let agent_name = result
            .and_then(|r| r.get("agentInfo"))
            .and_then(|a| a.get("name"))
            .and_then(|n| n.as_str())
            .unwrap_or("unknown");
        self.supports_load_session = result
            .and_then(|r| r.get("agentCapabilities"))
            .and_then(|c| c.get("loadSession"))
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        info!(
            agent = agent_name,
            load_session = self.supports_load_session,
            "Agent initialized"
        );
        Ok(())
    }

    /// Send `session/new` to create a new session. Returns the session ID.
    pub async fn session_new(
        &mut self,
        cwd: &str,
    ) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
        let resp = self
            .send_request("session/new", Some(json!({"cwd": cwd, "mcpServers": []})))
            .await?;

        let session_id = resp
            .result
            .as_ref()
            .and_then(|r| r.get("sessionId"))
            .and_then(|s| s.as_str())
            .ok_or("No sessionId in session/new response")?
            .to_string();

        info!(session_id = %session_id, "Agent session created");
        self.acp_session_id = Some(session_id.clone());
        Ok(session_id)
    }

    /// Send `session/prompt` with content blocks and return a receiver for
    /// streaming notifications. The final message will have `id` set.
    pub async fn session_prompt(
        &mut self,
        content_blocks: Vec<ContentBlock>,
    ) -> Result<
        (mpsc::UnboundedReceiver<JsonRpcMessage>, u64),
        Box<dyn std::error::Error + Send + Sync>,
    > {
        self.last_active = Instant::now();

        let session_id = self
            .acp_session_id
            .as_ref()
            .ok_or("No active session")?
            .clone();

        let (tx, rx) = mpsc::unbounded_channel();
        *self.notify_tx.lock().await = Some(tx);

        let id = self.next_id();
        let prompt_json: Vec<Value> = content_blocks.iter().map(|b| b.to_json()).collect();

        let req = JsonRpcRequest::new(
            id,
            "session/prompt",
            Some(json!({
                "sessionId": session_id,
                "prompt": prompt_json,
            })),
        );
        let data = serde_json::to_string(&req)?;

        let (resp_tx, _resp_rx) = oneshot::channel();
        self.pending.lock().await.insert(id, resp_tx);

        if let Err(e) = self.send_raw(&data).await {
            self.pending.lock().await.remove(&id);
            return Err(e);
        }
        Ok((rx, id))
    }

    /// Call after prompt streaming is done to clean up subscriber.
    pub async fn prompt_done(&mut self) {
        *self.notify_tx.lock().await = None;
        self.last_active = Instant::now();
    }

    /// Check if the agent process is still alive.
    pub fn alive(&self) -> bool {
        !self._reader_handle.is_finished()
    }

    /// Resume a previous session by ID via `session/load`.
    pub async fn session_load(
        &mut self,
        session_id: &str,
        cwd: &str,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let resp = self
            .send_request(
                "session/load",
                Some(json!({"sessionId": session_id, "cwd": cwd, "mcpServers": []})),
            )
            .await?;

        if resp.error.is_some() {
            return Err("session/load rejected by agent".into());
        }
        info!(session_id, "Agent session loaded");
        self.acp_session_id = Some(session_id.to_string());
        Ok(())
    }

    /// Kill the entire process group: SIGTERM → SIGKILL.
    #[cfg(unix)]
    fn kill_process_group(&mut self) {
        let pgid = match self.child_pgid {
            Some(pid) if pid > 0 => pid,
            _ => return,
        };
        unsafe {
            libc::kill(-pgid, libc::SIGTERM);
        }
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(1500));
            unsafe {
                libc::kill(-pgid, libc::SIGKILL);
            }
        });
    }

    #[cfg(not(unix))]
    fn kill_process_group(&mut self) {
        // On non-Unix, just drop the process handle.
    }
}

impl Drop for AcpConnection {
    fn drop(&mut self) {
        self.kill_process_group();
    }
}

// ---------------------------------------------------------------------------
// Client mode — run acp-bridge as an ACP client
// ---------------------------------------------------------------------------

use crate::config::AgentConfig;

/// Run acp-bridge in client mode: spawn an external ACP agent, forward
/// stdin prompts to it, and print streamed responses to stdout.
///
/// This turns acp-bridge into an interactive CLI wrapper around any ACP agent.
pub async fn run_client_mode(agent_config: &AgentConfig) {
    info!(
        command = %agent_config.command,
        args = ?agent_config.args,
        working_dir = %agent_config.working_dir,
        "Starting client mode"
    );

    // Spawn the agent
    let mut conn = match AcpConnection::spawn(
        &agent_config.command,
        &agent_config.args,
        &agent_config.working_dir,
        &agent_config.env,
    )
    .await
    {
        Ok(c) => c,
        Err(e) => {
            error!(error = %e, "Failed to spawn agent");
            return;
        }
    };

    // Initialize
    if let Err(e) = conn.initialize().await {
        error!(error = %e, "Failed to initialize agent");
        return;
    }

    // Create session
    let cwd = std::env::current_dir()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|_| "/tmp".to_string());

    if let Err(e) = conn.session_new(&cwd).await {
        error!(error = %e, "Failed to create agent session");
        return;
    }

    // Interactive loop: read lines from stdin, send as prompts
    let stdin = tokio::io::stdin();
    let reader = BufReader::new(stdin);
    let mut lines = reader.lines();

    loop {
        // Read user input
        eprint!("> ");
        let line = match lines.next_line().await {
            Ok(Some(l)) if !l.trim().is_empty() => l,
            Ok(Some(_)) => continue,
            _ => break,
        };

        // Send prompt
        let blocks = vec![ContentBlock::text(&line)];
        let (mut rx, _prompt_id) = match conn.session_prompt(blocks).await {
            Ok(r) => r,
            Err(e) => {
                error!(error = %e, "Failed to send prompt");
                continue;
            }
        };

        // Stream response
        while let Some(msg) = rx.recv().await {
            // If this is the final response (has id), we're done
            if msg.id.is_some() {
                break;
            }

            if let Some(event) = classify_notification(&msg) {
                match event {
                    AcpEvent::Text(text) => print!("{text}"),
                    AcpEvent::Thinking => eprint!("[thinking] "),
                    AcpEvent::ToolStart { title, .. } => eprintln!("[tool: {title}]"),
                    AcpEvent::ToolDone { title, status, .. } => {
                        eprintln!("[tool done: {title} → {status}]")
                    }
                    AcpEvent::Status => {}
                }
            }
        }
        println!(); // newline after response
        conn.prompt_done().await;

        if !conn.alive() {
            warn!("Agent process exited");
            break;
        }
    }

    info!("Client mode ended");
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picks_allow_always_over_other_options() {
        let options = vec![
            json!({"kind": "allow_once", "optionId": "once"}),
            json!({"kind": "allow_always", "optionId": "always"}),
            json!({"kind": "reject_once", "optionId": "reject"}),
        ];
        assert_eq!(pick_best_option(&options), Some("always".to_string()));
    }

    #[test]
    fn falls_back_to_first_non_reject() {
        let options = vec![
            json!({"kind": "reject_once", "optionId": "reject"}),
            json!({"kind": "workspace_write", "optionId": "workspace-write"}),
        ];
        assert_eq!(
            pick_best_option(&options),
            Some("workspace-write".to_string())
        );
    }

    #[test]
    fn returns_none_when_only_reject() {
        let options = vec![
            json!({"kind": "reject_once", "optionId": "r1"}),
            json!({"kind": "reject_always", "optionId": "r2"}),
        ];
        assert_eq!(pick_best_option(&options), None);
    }

    #[test]
    fn builds_cancelled_when_no_selectable() {
        let resp = build_permission_response(Some(&json!({
            "options": [{"kind": "reject_once", "optionId": "r"}]
        })));
        assert_eq!(resp, json!({"outcome": {"outcome": "cancelled"}}));
    }

    #[test]
    fn builds_allow_always_when_no_options() {
        let resp = build_permission_response(None);
        assert_eq!(
            resp,
            json!({"outcome": {"outcome": "selected", "optionId": "allow_always"}})
        );
    }

    #[test]
    fn classify_text_notification() {
        let msg = JsonRpcMessage {
            id: None,
            method: Some("session/notify".into()),
            result: None,
            error: None,
            params: Some(json!({
                "update": {
                    "sessionUpdate": "agent_message_chunk",
                    "content": {"text": "hello"}
                }
            })),
        };
        match classify_notification(&msg) {
            Some(AcpEvent::Text(t)) => assert_eq!(t, "hello"),
            other => panic!("Expected Text, got {other:?}"),
        }
    }

    #[test]
    fn classify_tool_events() {
        let start = JsonRpcMessage {
            id: None,
            method: Some("session/notify".into()),
            result: None,
            error: None,
            params: Some(json!({
                "update": {
                    "sessionUpdate": "tool_call",
                    "toolCallId": "tc1",
                    "title": "read_file"
                }
            })),
        };
        match classify_notification(&start) {
            Some(AcpEvent::ToolStart { id, title }) => {
                assert_eq!(id, "tc1");
                assert_eq!(title, "read_file");
            }
            other => panic!("Expected ToolStart, got {other:?}"),
        }

        let done = JsonRpcMessage {
            id: None,
            method: Some("session/notify".into()),
            result: None,
            error: None,
            params: Some(json!({
                "update": {
                    "sessionUpdate": "tool_call_update",
                    "toolCallId": "tc1",
                    "title": "read_file",
                    "status": "completed"
                }
            })),
        };
        match classify_notification(&done) {
            Some(AcpEvent::ToolDone { id, status, .. }) => {
                assert_eq!(id, "tc1");
                assert_eq!(status, "completed");
            }
            other => panic!("Expected ToolDone, got {other:?}"),
        }
    }

    #[test]
    fn expand_env_var() {
        std::env::set_var("_ACP_TEST_VAR", "hello");
        assert_eq!(expand_env("${_ACP_TEST_VAR}"), "hello");
        assert_eq!(expand_env("literal"), "literal");
        std::env::remove_var("_ACP_TEST_VAR");
    }
}
