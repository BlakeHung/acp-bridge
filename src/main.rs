//! acp-bridge — ACP/A2A adapter for self-hosted AI services.
//!
//! Supports two transport modes:
//! - **ACP mode** (default): stdin/stdout JSON-RPC, spawned by openab/Zed/JetBrains
//! - **A2A mode** (`--a2a`): HTTP server with Agent Card and A2A protocol

use acp_bridge::a2a::{self, A2aConfig};
use acp_bridge::acp;
use acp_bridge::bench;
use acp_bridge::client;
use acp_bridge::config::{AgentConfig, ConfigFile};
use acp_bridge::engine::{self, AppState, Notification};
use acp_bridge::hardware;
use acp_bridge::llm;
use acp_bridge::protocol::{AcpError, JsonRpcRequest};
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

// ---------------------------------------------------------------------------
// Run mode
// ---------------------------------------------------------------------------

enum RunMode {
    /// stdin/stdout ACP (backward compatible, default)
    Acp,
    /// HTTP A2A server
    A2a,
    /// Client mode — spawn and interact with an external ACP agent
    Client,
    /// Benchmark mode — run fixture prompts against the configured LLM, print stats, exit.
    Bench,
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() {
    // Parse CLI flags before anything else
    let args: Vec<String> = std::env::args().collect();

    if args.iter().any(|a| a == "--version" || a == "-V") {
        println!("acp-bridge {}", env!("CARGO_PKG_VERSION"));
        return;
    }

    if args.iter().any(|a| a == "--help" || a == "-h") {
        println!(
            "acp-bridge {} — ACP/A2A adapter for self-hosted AI",
            env!("CARGO_PKG_VERSION")
        );
        println!();
        println!("USAGE:");
        println!("  acp-bridge [OPTIONS] [config.toml]");
        println!();
        println!("MODES:");
        println!("  (default)    ACP mode — stdin/stdout JSON-RPC (act as agent)");
        println!("  --a2a        A2A mode — HTTP server with Agent Card");
        println!("  --client     Client mode — spawn and interact with an external ACP agent");
        println!(
            "  --bench      Benchmark mode — run fixture prompts against LLM, print stats, exit"
        );
        println!();
        println!("OPTIONS:");
        println!("  --version    Print version");
        println!("  --help       Print this help");
        println!();
        println!("ENVIRONMENT:");
        println!("  LLM_BASE_URL, LLM_MODEL, LLM_API_KEY, LLM_TIMEOUT, ...");
        println!("  A2A_HOST (default: 0.0.0.0), A2A_PORT (default: 8080)");
        println!("  A2A_AGENT_NAME, A2A_AGENT_DESCRIPTION");
        println!("  AGENT_COMMAND, AGENT_ARGS, AGENT_WORKING_DIR (for --client mode)");
        return;
    }

    let mode = if args.iter().any(|a| a == "--bench") {
        RunMode::Bench
    } else if args.iter().any(|a| a == "--client") {
        RunMode::Client
    } else if args.iter().any(|a| a == "--a2a") {
        RunMode::A2a
    } else {
        RunMode::Acp
    };

    // Initialize tracing — writes to stderr, respects RUST_LOG env.
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "acp_bridge=info".parse().unwrap()),
        )
        .with_target(true)
        .with_writer(std::io::stderr)
        .init();

    // Load config: CLI arg (optional TOML path) → env vars → defaults
    let config_path = args.iter().skip(1).find(|a| !a.starts_with('-')).cloned();

    let config_file = config_path
        .as_ref()
        .map(|path| ConfigFile::load(std::path::Path::new(path)));

    // In client mode, we only need the agent config
    if let RunMode::Client = mode {
        let agent_config = config_file
            .as_ref()
            .and_then(|f| f.agent_config())
            .or_else(AgentConfig::from_env);

        match agent_config {
            Some(ac) => {
                for line in hardware::detect().report_lines() {
                    info!("{line}");
                }
                client::run_client_mode(&ac).await;
                return;
            }
            None => {
                eprintln!("Error: --client mode requires agent config.");
                eprintln!("Set AGENT_COMMAND env var or add [agent] section to config.toml");
                return;
            }
        }
    }

    let (config, a2a_config) = match config_file {
        Some(file) => {
            let a2a_cfg = file.a2a_config();
            (file.into_llm_config(), a2a_cfg)
        }
        None => (llm::LlmConfig::from_env(), A2aConfig::from_env()),
    };

    if let RunMode::Bench = mode {
        for line in hardware::detect().report_lines() {
            info!("{line}");
        }
        info!(
            base_url = %config.base_url,
            model = %config.model,
            "Running benchmark"
        );
        let results = bench::run(&config, &bench::default_fixtures()).await;
        bench::print_report(&config, &results);
        return;
    }

    let mode_str = match mode {
        RunMode::Acp => "acp",
        RunMode::A2a => "a2a",
        RunMode::Client => unreachable!(),
        RunMode::Bench => unreachable!(),
    };
    info!(
        version = env!("CARGO_PKG_VERSION"),
        mode = mode_str,
        model = %config.model,
        base_url = %config.base_url,
        ollama_native = config.is_ollama_native(),
        max_history_turns = config.max_history_turns,
        max_sessions = config.max_sessions,
        session_idle_timeout_secs = config.session_idle_timeout_secs,
        "Starting acp-bridge"
    );

    for line in hardware::detect().report_lines() {
        info!("{line}");
    }

    // Probe backend
    probe_backend(&config).await;

    // Build shared state
    let state = AppState::new(config);

    // Spawn idle session cleanup task
    let idle_timeout = state.config.session_idle_timeout_secs;
    if idle_timeout > 0 {
        let state_clone = Arc::clone(&state);
        tokio::spawn(async move {
            let interval = Duration::from_secs(idle_timeout.min(60));
            loop {
                tokio::time::sleep(interval).await;
                state_clone.evict_idle_sessions(idle_timeout);
            }
        });
    }

    // Run in selected mode
    match mode {
        RunMode::Acp => run_acp_loop(state).await,
        RunMode::A2a => {
            if let Err(e) = a2a::serve(state, a2a_config).await {
                error!(error = %e, "A2A server error");
            }
        }
        RunMode::Client => unreachable!("Client mode handled above"),
        RunMode::Bench => unreachable!("Bench mode handled above"),
    }
}

// ---------------------------------------------------------------------------
// Backend probing (shared by both modes)
// ---------------------------------------------------------------------------

async fn probe_backend(config: &llm::LlmConfig) {
    match llm::probe_backend(config).await {
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

    // Query model info (Ollama native only)
    if let Some(info) = llm::query_model_info(config).await {
        info!(
            context_length = info.context_length,
            "Model info from /api/show"
        );
    }

    // Check running models (Ollama)
    if let Some(running) = llm::query_running_models(config).await {
        if running.is_empty() {
            warn!(
                model = %config.model,
                "No models loaded in VRAM — first request may be slow. Run: ollama run {}",
                config.model
            );
        } else {
            info!(count = running.len(), "Running models (loaded in VRAM):");
            for m in &running {
                info!("  - {m}");
            }
        }
    }
}

// ---------------------------------------------------------------------------
// ACP mode — stdin/stdout JSON-RPC loop
// ---------------------------------------------------------------------------

async fn run_acp_loop(state: Arc<AppState>) {
    let stdin = tokio::io::stdin();
    let reader = BufReader::new(stdin);
    let mut lines = reader.lines();

    loop {
        tokio::select! {
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

                        let id_opt = msg.id;
                        let method = msg.method.as_str();
                        let params = msg.params.clone().unwrap_or(json!({}));

                        debug!(?id_opt, method, "Received message");

                        // Notifications (no id, no response expected per JSON-RPC 2.0).
                        let id = match id_opt {
                            Some(id) => id,
                            None => {
                                match method {
                                    "session/cancel" => {
                                        let sid = params
                                            .get("sessionId")
                                            .and_then(|v| v.as_str())
                                            .unwrap_or("");
                                        info!(
                                            session_id = %sid,
                                            "Received session/cancel notification (acknowledged; in-flight cancel not yet implemented)"
                                        );
                                    }
                                    _ => {
                                        debug!(method, "Ignoring unknown notification");
                                    }
                                }
                                continue;
                            }
                        };

                        match method {
                            "initialize" => {
                                let result = engine::initialize(&state.config);
                                acp::send_response(id, result);
                            }
                            "session/new" => {
                                let raw_cwd = params.get("cwd").and_then(|v| v.as_str()).unwrap_or("/tmp");
                                if let Some(servers) = params.get("mcpServers").and_then(|v| v.as_array()) {
                                    if !servers.is_empty() {
                                        debug!(count = servers.len(), "Ignoring mcpServers param (not supported in v0.7)");
                                    }
                                }
                                match engine::session_new(&state, raw_cwd) {
                                    Ok(session_id) => {
                                        acp::send_response(id, json!({"sessionId": session_id}));
                                    }
                                    Err(e) => {
                                        acp::send_error(id, e.code(), &e.to_string());
                                    }
                                }
                            }
                            "session/prompt" => {
                                handle_acp_prompt(id, &params, &state).await;
                            }
                            "session/end" => {
                                let session_id = params.get("sessionId").and_then(|v| v.as_str()).unwrap_or("");
                                if session_id.is_empty() {
                                    let err = AcpError::MissingParam { field: "sessionId".into() };
                                    acp::send_error(id, err.code(), &err.to_string());
                                } else {
                                    match engine::session_end(&state, session_id) {
                                        Ok(()) => acp::send_response(id, json!({"status": "ended"})),
                                        Err(e) => acp::send_error(id, e.code(), &e.to_string()),
                                    }
                                }
                            }
                            "session/load" | "session/resume" => {
                                // ACP uses capability-based negotiation: a client should
                                // only call `session/load` when the agent advertises
                                // `loadSession` in `agentCapabilities`. acp-bridge does
                                // not advertise it, so the correct response is -32601
                                // (method not found) — the keep-the-message-helpful
                                // string is preserved for operators debugging stray
                                // calls.
                                acp::send_error(
                                    id,
                                    -32601,
                                    "session/load and session/resume are not supported by acp-bridge (no persistence layer; loadSession capability is not advertised)",
                                );
                            }
                            "session/set_mode" => {
                                // session/new responses do not include a `modes` array,
                                // so per ACP spec this method is not applicable to
                                // sessions created by acp-bridge. Capability-based
                                // negotiation: respond with -32601 method-not-found.
                                acp::send_error(
                                    id,
                                    -32601,
                                    "session/set_mode is not supported by acp-bridge (sessions are created without a `modes` array)",
                                );
                            }
                            _ => {
                                let err = AcpError::MethodNotFound { method: method.to_string() };
                                acp::send_error(id, err.code(), &err.to_string());
                            }
                        }
                    }
                    Ok(None) => {
                        info!("stdin closed, shutting down gracefully");
                        break;
                    }
                    Err(e) => {
                        error!(error = %e, "Error reading stdin");
                        break;
                    }
                }
            }
            _ = tokio::signal::ctrl_c() => {
                info!("Received shutdown signal, exiting");
                break;
            }
        }
    }

    // Cleanup
    let session_count = state.cleanup();
    if session_count > 0 {
        info!(sessions = session_count, "Cleaned up sessions on exit");
    }
}

/// Handle ACP session/prompt — runs engine and streams notifications to stdout.
async fn handle_acp_prompt(id: u64, params: &Value, state: &Arc<AppState>) {
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

    let prompt_value = params.get("prompt").cloned().unwrap_or(Value::Null);
    let prompt_kind = match &prompt_value {
        Value::Null => "null",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
        _ => "other",
    };
    debug!(prompt_kind, "session/prompt input shape");

    let raw_user_text = engine::extract_user_text_from_prompt(&prompt_value);
    let (user_text, sender_context) = engine::strip_sender_context(&raw_user_text);
    if let Some(ctx) = &sender_context {
        debug!(
            sender_context_len = ctx.len(),
            "Stripped <sender_context> block from user text"
        );
    }
    let user_images = engine::extract_user_images_from_prompt(&prompt_value);

    if user_text.trim().is_empty() && user_images.is_empty() {
        let err = AcpError::MissingParam {
            field: "prompt (expected non-empty text or image content)".into(),
        };
        acp::send_error(id, err.code(), &err.to_string());
        return;
    }

    // Set up notification channel for ACP streaming
    let (notify_tx, mut notify_rx) = mpsc::unbounded_channel::<Notification>();

    // Spawn the engine prompt in a task so we can drain notifications
    let state_clone = Arc::clone(state);
    let sid = session_id.clone();
    let handle = tokio::spawn(async move {
        engine::session_prompt(
            &state_clone,
            &sid,
            &user_text,
            &user_images,
            Some(notify_tx),
        )
        .await
    });

    // Drain notifications to ACP stdout
    while let Some(notif) = notify_rx.recv().await {
        match notif {
            Notification::Thinking => acp::notify_thinking(),
            Notification::ToolStart(name) => acp::notify_tool_start(&name),
            Notification::ToolDone(name, status) => acp::notify_tool_done(&name, &status),
            Notification::TextChunk(text) => acp::notify_text(&text),
        }
    }

    let result = handle.await.unwrap_or_else(|_| engine::PromptResult {
        status: "failed".into(),
        text: "Internal error".into(),
        error: None,
    });

    // If the engine returned a protocol error (e.g. unknown session), send JSON-RPC error
    if let Some(err) = &result.error {
        acp::send_error(id, err.code(), &err.to_string());
    } else {
        // Include the accumulated text in the final response as well as the
        // streamed `TextChunk` notifications. Some upstream clients (e.g.
        // OpenAB pipelines that treat `ToolDone("llm_chat","completed")`
        // as the turn boundary) rely on the final response for the message
        // body — without `text` here those clients show an empty reply
        // even though the chunks were sent.
        acp::send_response(
            id,
            json!({
                "status": result.status,
                "text": result.text,
            }),
        );
    }
}
