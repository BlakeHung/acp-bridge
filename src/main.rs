//! acp-bridge — ACP adapter for local AI services.
//!
//! Bridges any OpenAI-compatible API (Ollama, LocalAI, vLLM, llama.cpp,
//! LM Studio, text-generation-webui) to Agent Client Protocol (ACP).
//!
//! Reads JSON-RPC 2.0 from stdin, translates to HTTP chat completions,
//! and writes JSON-RPC notifications/responses to stdout.
//!
//! Compatible with openab and any ACP-compliant harness.

use acp_bridge::acp;
use acp_bridge::config::ConfigFile;
use acp_bridge::llm;
use acp_bridge::protocol::{AcpError, JsonRpcRequest, Session};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::RwLock;
use tokio::io::{AsyncBufReadExt, BufReader};
use tracing::{debug, error, info, warn};
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Global state
// ---------------------------------------------------------------------------

static SESSIONS: std::sync::LazyLock<RwLock<HashMap<String, Session>>> =
    std::sync::LazyLock::new(|| RwLock::new(HashMap::new()));

// ---------------------------------------------------------------------------
// ACP method handlers
// ---------------------------------------------------------------------------

fn handle_initialize(id: u64, config: &llm::LlmConfig) {
    info!(model = %config.model, base_url = %config.base_url, "Initialize");
    acp::send_response(
        id,
        json!({
            "agentInfo": {
                "name": format!("acp-bridge ({})", config.model),
                "version": env!("CARGO_PKG_VERSION")
            },
            "capabilities": {}
        }),
    );
}

fn handle_session_new(id: u64, params: &Value, config: &llm::LlmConfig) {
    let raw_cwd = params
        .get("cwd")
        .and_then(|v| v.as_str())
        .unwrap_or("/tmp");

    // Sanitize cwd: only allow typical path characters to prevent prompt injection.
    let cwd: String = raw_cwd
        .chars()
        .filter(|c| c.is_alphanumeric() || matches!(c, '/' | '-' | '_' | '.' | ' ' | '~'))
        .collect();

    let session_id = Uuid::new_v4().to_string();

    let system_prompt = std::env::var("LLM_SYSTEM_PROMPT").unwrap_or_else(|_| {
        format!("You are a helpful coding assistant. The user's working directory is: {cwd}")
    });

    let session = Session {
        messages: vec![json!({"role": "system", "content": system_prompt})],
    };

    match SESSIONS.write() {
        Ok(mut sessions) => {
            sessions.insert(session_id.clone(), session);
        }
        Err(poisoned) => {
            warn!("Session lock was poisoned, recovering");
            let mut sessions = poisoned.into_inner();
            sessions.insert(session_id.clone(), session);
        }
    }

    info!(session_id = %session_id, max_history = config.max_history_turns, "New session");
    acp::send_response(id, json!({"sessionId": session_id}));
}

async fn handle_session_prompt(id: u64, params: &Value, config: &llm::LlmConfig) {
    let session_id = match params.get("sessionId").and_then(|v| v.as_str()) {
        Some(s) => s.to_string(),
        None => {
            let err = AcpError::MissingParam {
                field: "sessionId".into(),
            };
            acp::send_error(id, err.code(), &err.to_string());
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

    // Add user message and trim history
    {
        let mut sessions = match SESSIONS.write() {
            Ok(s) => s,
            Err(p) => {
                warn!("Session lock poisoned, recovering");
                p.into_inner()
            }
        };
        let session = match sessions.get_mut(&session_id) {
            Some(s) => s,
            None => {
                let err = AcpError::UnknownSession {
                    session_id: session_id.clone(),
                };
                acp::send_error(id, err.code(), &err.to_string());
                return;
            }
        };
        session
            .messages
            .push(json!({"role": "user", "content": user_text}));

        // Trim to max_history_turns if configured
        if config.max_history_turns > 0 {
            let before = session.messages.len();
            session.trim_history(config.max_history_turns);
            let after = session.messages.len();
            if before != after {
                debug!(before, after, "Trimmed conversation history");
            }
        }
    }

    acp::notify_thinking();
    acp::notify_tool_start("llm_chat");

    let messages = {
        let sessions = match SESSIONS.read() {
            Ok(s) => s,
            Err(p) => {
                warn!("Session lock poisoned, recovering");
                p.into_inner()
            }
        };
        sessions
            .get(&session_id)
            .map(|s| s.messages.clone())
            .unwrap_or_default()
    };

    let mut full_response = String::new();
    let mut had_error = false;

    match llm::stream_chat(config, &messages, None).await {
        Ok(mut rx) => {
            while let Some(chunk) = rx.recv().await {
                match chunk {
                    llm::StreamChunk::Content(text) => {
                        acp::notify_text(&text);
                        full_response.push_str(&text);
                    }
                    llm::StreamChunk::Error(err) => {
                        error!(error = %err, "Stream error from LLM");
                        acp::notify_text(&format!("\n\n**Error:** {err}\n"));
                        had_error = true;
                        break;
                    }
                    llm::StreamChunk::Done => break,
                }
            }
        }
        Err(e) => {
            let err = AcpError::LlmError { reason: e };
            error!(error = %err, "LLM communication failed");
            acp::notify_text(&format!("\n\n**Error:** {err}\n"));
            had_error = true;
        }
    }

    if !full_response.is_empty() {
        let mut sessions = match SESSIONS.write() {
            Ok(s) => s,
            Err(p) => {
                warn!("Session lock poisoned, recovering");
                p.into_inner()
            }
        };
        if let Some(session) = sessions.get_mut(&session_id) {
            session
                .messages
                .push(json!({"role": "assistant", "content": full_response}));
        }
    }

    let status = if had_error { "failed" } else { "completed" };
    acp::notify_tool_done("llm_chat", status);
    acp::send_response(id, json!({"status": status}));
}

fn handle_session_end(id: u64, params: &Value) {
    let session_id = match params.get("sessionId").and_then(|v| v.as_str()) {
        Some(s) => s.to_string(),
        None => {
            let err = AcpError::MissingParam {
                field: "sessionId".into(),
            };
            acp::send_error(id, err.code(), &err.to_string());
            return;
        }
    };

    let removed = match SESSIONS.write() {
        Ok(mut s) => s.remove(&session_id).is_some(),
        Err(p) => {
            warn!("Session lock poisoned, recovering");
            p.into_inner().remove(&session_id).is_some()
        }
    };

    if removed {
        info!(session_id = %session_id, "Session ended");
        acp::send_response(id, json!({"status": "ended"}));
    } else {
        let err = AcpError::UnknownSession { session_id };
        acp::send_error(id, err.code(), &err.to_string());
    }
}

// ---------------------------------------------------------------------------
// Main loop
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() {
    // Handle --version / -V before anything else
    if let Some(arg) = std::env::args().nth(1) {
        if arg == "--version" || arg == "-V" {
            println!("acp-bridge {}", env!("CARGO_PKG_VERSION"));
            return;
        }
    }

    // Initialize tracing — writes to stderr, respects RUST_LOG env.
    // Default level: info. Example: RUST_LOG=acp_bridge=debug
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "acp_bridge=info".parse().unwrap()),
        )
        .with_target(true)
        .with_writer(std::io::stderr)
        .init();

    // Load config: CLI arg (optional TOML path) → env vars → defaults
    let config_path = std::env::args().nth(1);
    let config = match config_path {
        Some(path) => {
            let file = ConfigFile::load(std::path::Path::new(&path));
            file.into_llm_config()
        }
        None => llm::LlmConfig::from_env(),
    };
    info!(
        version = env!("CARGO_PKG_VERSION"),
        model = %config.model,
        base_url = %config.base_url,
        max_history_turns = config.max_history_turns,
        "Starting acp-bridge"
    );

    // Probe backend: check connectivity and list available models
    match llm::probe_backend(&config).await {
        Ok(models) if models.is_empty() => {
            info!("Connected to backend (no models listed)");
        }
        Ok(models) => {
            info!(count = models.len(), "Available models:");
            for m in &models {
                info!("  - {m}");
            }
            if !models.iter().any(|m| {
                m.starts_with(&config.model)
                    || config.model.starts_with(m.split(':').next().unwrap_or(""))
            }) {
                warn!(configured = %config.model, "Configured model not found in available models");
            }
        }
        Err(reason) => {
            warn!(
                base_url = %config.base_url,
                error = %reason,
                "Cannot reach backend — will retry on first request"
            );
        }
    }

    let stdin = tokio::io::stdin();
    let reader = BufReader::new(stdin);
    let mut lines = reader.lines();

    loop {
        tokio::select! {
            // Read next line from stdin
            line_result = lines.next_line() => {
                match line_result {
                    Ok(Some(line)) => {
                        let trimmed = line.trim().to_string();
                        if trimmed.is_empty() {
                            continue;
                        }

                        let msg: JsonRpcRequest = match serde_json::from_str(&trimmed) {
                            Ok(m) => m,
                            Err(e) => {
                                debug!(error = %e, "Skipping invalid JSON-RPC line");
                                continue;
                            }
                        };

                        let id = msg.id;
                        let method = msg.method.as_str();
                        let params = msg.params.clone().unwrap_or(json!({}));

                        debug!(id, method, "Received request");

                        match method {
                            "initialize" => handle_initialize(id, &config),
                            "session/new" => handle_session_new(id, &params, &config),
                            "session/prompt" => handle_session_prompt(id, &params, &config).await,
                            "session/end" => handle_session_end(id, &params),
                            _ => {
                                let err = AcpError::MethodNotFound { method: method.to_string() };
                                acp::send_error(id, err.code(), &err.to_string());
                            }
                        }
                    }
                    Ok(None) => {
                        // stdin closed (EOF) — parent process (openab) terminated
                        info!("stdin closed, shutting down gracefully");
                        break;
                    }
                    Err(e) => {
                        error!(error = %e, "Error reading stdin");
                        break;
                    }
                }
            }
            // Handle SIGTERM / SIGINT for graceful shutdown
            _ = tokio::signal::ctrl_c() => {
                info!("Received shutdown signal, exiting");
                break;
            }
        }
    }

    // Cleanup: drop all sessions
    let session_count = match SESSIONS.write() {
        Ok(mut s) => {
            let n = s.len();
            s.clear();
            n
        }
        Err(p) => {
            let mut s = p.into_inner();
            let n = s.len();
            s.clear();
            n
        }
    };
    if session_count > 0 {
        info!(sessions = session_count, "Cleaned up sessions on exit");
    }
}
