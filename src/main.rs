//! local-ai-acp — ACP adapter for local AI services.
//!
//! Bridges any OpenAI-compatible API (Ollama, LocalAI, vLLM, llama.cpp,
//! LM Studio, text-generation-webui) to Agent Client Protocol (ACP).
//!
//! Reads JSON-RPC 2.0 from stdin, translates to HTTP chat completions,
//! and writes JSON-RPC notifications/responses to stdout.
//!
//! Compatible with openab and any ACP-compliant harness.

mod llm;
mod protocol;

use protocol::{JsonRpcRequest, Session};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::Write;
use std::sync::Mutex;
use tokio::io::{AsyncBufReadExt, BufReader};
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Global state
// ---------------------------------------------------------------------------

static SESSIONS: std::sync::LazyLock<Mutex<HashMap<String, Session>>> =
    std::sync::LazyLock::new(|| Mutex::new(HashMap::new()));

// ---------------------------------------------------------------------------
// stdout helpers — newline-delimited JSON-RPC
// ---------------------------------------------------------------------------

fn send(obj: &Value) {
    let mut stdout = std::io::stdout().lock();
    let _ = serde_json::to_writer(&mut stdout, obj);
    let _ = stdout.write_all(b"\n");
    let _ = stdout.flush();
}

fn send_response(id: u64, result: Value) {
    send(&json!({"jsonrpc": "2.0", "id": id, "result": result}));
}

fn send_error(id: u64, code: i64, message: &str) {
    send(&json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}}));
}

fn send_notification(method: &str, params: Value) {
    send(&json!({"jsonrpc": "2.0", "method": method, "params": params}));
}

// ---------------------------------------------------------------------------
// ACP notification helpers
// ---------------------------------------------------------------------------

fn notify_text(text: &str) {
    send_notification(
        "session/notify",
        json!({"update": {"sessionUpdate": "agent_message_chunk", "content": {"text": text}}}),
    );
}

fn notify_thinking() {
    send_notification(
        "session/notify",
        json!({"update": {"sessionUpdate": "agent_thought_chunk"}}),
    );
}

fn notify_tool_start(title: &str) {
    send_notification(
        "session/notify",
        json!({"update": {"sessionUpdate": "tool_call", "title": title}}),
    );
}

fn notify_tool_done(title: &str, status: &str) {
    send_notification(
        "session/notify",
        json!({"update": {"sessionUpdate": "tool_call_update", "title": title, "status": status}}),
    );
}

// ---------------------------------------------------------------------------
// ACP method handlers
// ---------------------------------------------------------------------------

fn handle_initialize(id: u64, config: &llm::LlmConfig) {
    send_response(
        id,
        json!({
            "agentInfo": {
                "name": format!("local-ai-acp ({})", config.model),
                "version": env!("CARGO_PKG_VERSION")
            },
            "capabilities": {}
        }),
    );
}

fn handle_session_new(id: u64, params: &Value) {
    let cwd = params
        .get("cwd")
        .and_then(|v| v.as_str())
        .unwrap_or("/tmp")
        .to_string();

    let session_id = Uuid::new_v4().to_string();

    let system_prompt = std::env::var("LLM_SYSTEM_PROMPT").unwrap_or_else(|_| {
        format!("You are a helpful coding assistant. The user's working directory is: {cwd}")
    });

    let session = Session {
        cwd,
        messages: vec![json!({"role": "system", "content": system_prompt})],
    };

    SESSIONS
        .lock()
        .unwrap()
        .insert(session_id.clone(), session);

    send_response(id, json!({"sessionId": session_id}));
}

async fn handle_session_prompt(id: u64, params: &Value, config: &llm::LlmConfig) {
    let session_id = match params.get("sessionId").and_then(|v| v.as_str()) {
        Some(s) => s.to_string(),
        None => {
            send_error(id, -32600, "Missing sessionId");
            return;
        }
    };

    let user_text = params
        .get("prompt")
        .and_then(|p| p.as_array())
        .map(|arr| {
            arr.iter()
                .filter(|p| p.get("type").and_then(|t| t.as_str()) == Some("text"))
                .filter_map(|p| p.get("text").and_then(|t| t.as_str()))
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default();

    {
        let mut sessions = SESSIONS.lock().unwrap();
        let session = match sessions.get_mut(&session_id) {
            Some(s) => s,
            None => {
                send_error(id, -32600, &format!("Unknown session: {session_id}"));
                return;
            }
        };
        session.messages.push(json!({"role": "user", "content": user_text}));
    }

    notify_thinking();
    notify_tool_start("llm_chat");

    let messages = {
        let sessions = SESSIONS.lock().unwrap();
        sessions.get(&session_id).map(|s| s.messages.clone()).unwrap_or_default()
    };

    let mut full_response = String::new();

    match llm::stream_chat(config, &messages).await {
        Ok(mut rx) => {
            while let Some(chunk) = rx.recv().await {
                match chunk {
                    llm::StreamChunk::Content(text) => {
                        notify_text(&text);
                        full_response.push_str(&text);
                    }
                    llm::StreamChunk::Error(err) => {
                        notify_text(&format!("\n\n**Error:** {err}\n"));
                    }
                    llm::StreamChunk::Done => break,
                }
            }
        }
        Err(e) => {
            notify_text(&format!("\n\n**Error communicating with LLM:** {e}\n"));
            notify_tool_done("llm_chat", "failed");
            send_response(id, json!({"status": "completed"}));
            return;
        }
    }

    if !full_response.is_empty() {
        let mut sessions = SESSIONS.lock().unwrap();
        if let Some(session) = sessions.get_mut(&session_id) {
            session.messages.push(json!({"role": "assistant", "content": full_response}));
        }
    }

    notify_tool_done("llm_chat", "completed");
    send_response(id, json!({"status": "completed"}));
}

// ---------------------------------------------------------------------------
// Main loop
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() {
    let config = llm::LlmConfig::from_env();

    let stdin = tokio::io::stdin();
    let reader = BufReader::new(stdin);
    let mut lines = reader.lines();

    while let Ok(Some(line)) = lines.next_line().await {
        let trimmed = line.trim().to_string();
        if trimmed.is_empty() {
            continue;
        }

        let msg: JsonRpcRequest = match serde_json::from_str(&trimmed) {
            Ok(m) => m,
            Err(_) => continue,
        };

        let id = msg.id;
        let method = msg.method.as_str();
        let params = msg.params.clone().unwrap_or(json!({}));

        match method {
            "initialize" => handle_initialize(id, &config),
            "session/new" => handle_session_new(id, &params),
            "session/prompt" => handle_session_prompt(id, &params, &config).await,
            _ => send_error(id, -32601, &format!("Method not found: {method}")),
        }
    }
}
