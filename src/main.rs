//! acp-bridge — Minimal ACP adapter for local AI.
//!
//! Single transport: stdin/stdout JSON-RPC 2.0 (ACP).
//! Spawns by openab, Zed, JetBrains, or any ACP harness.

use acp_bridge::acp;
use acp_bridge::bench;
use acp_bridge::config::ConfigFile;
use acp_bridge::engine::{self, AppState, Notification};
use acp_bridge::hardware;
use acp_bridge::llm;
use acp_bridge::protocol::{AcpError, JsonRpcRequest, ProtocolVersion, RequestId};
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
    /// stdin/stdout ACP (default)
    Acp,
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
            "acp-bridge {} — Minimal ACP adapter for local AI",
            env!("CARGO_PKG_VERSION")
        );
        println!();
        println!("USAGE:");
        println!("  acp-bridge [OPTIONS] [config.toml]");
        println!();
        println!("MODES:");
        println!("  (default)    ACP mode — stdin/stdout JSON-RPC (act as agent)");
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
        return;
    }

    let mode = if args.iter().any(|a| a == "--bench") {
        RunMode::Bench
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

    let config = match config_file {
        Some(file) => file.into_llm_config(),
        None => llm::LlmConfig::from_env(),
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

    info!(
        version = env!("CARGO_PKG_VERSION"),
        model = %config.model,
        base_url = %config.base_url,
        backend = ?config.backend(),
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

    // Run ACP stdin/stdout loop
    run_acp_loop(state).await;
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

/// Negotiate the ACP wire-format version from a Client's
/// `initialize` request parameters.
///
/// The Client's `protocolVersion` is an integer (1 or 2 today).
/// Negotiation rule: pick the highest version that both sides
/// support. We support v1 and v2; anything newer than v2 falls back to
/// v2 with a warning. Missing field → v1 (conservative default that
/// works with every existing Client).
///
/// This is the *exact* protocol rule described in
/// <https://agentclientprotocol.com/protocol/initialization> and
/// mirrored by the official Rust / TypeScript / Elixir SDKs.
fn negotiate_protocol_version(params: Option<Value>) -> ProtocolVersion {
    let requested = params
        .as_ref()
        .and_then(|p| p.get("protocolVersion"))
        .and_then(|v| v.as_u64())
        .map(|n| ProtocolVersion(n as u16));
    match requested {
        None => {
            debug!("Client omitted protocolVersion, defaulting to v1");
            ProtocolVersion::V1
        }
        Some(v) if v.as_u16() <= ProtocolVersion::V1.as_u16() => {
            debug!(requested = %v, "Negotiating v1");
            ProtocolVersion::V1
        }
        Some(v) if v.as_u16() == ProtocolVersion::V2.as_u16() => {
            debug!("Negotiating v2");
            ProtocolVersion::V2
        }
        Some(v) => {
            warn!(
                requested = %v,
                "Client requested a version newer than we support; falling back to v2"
            );
            ProtocolVersion::V2
        }
    }
}

async fn run_acp_loop(mut state: Arc<AppState>) {
    let stdin = tokio::io::stdin();
    let reader = BufReader::new(stdin);
    let mut lines = reader.lines();
    let mut prompts = tokio::task::JoinSet::new();

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
                                        if let Some(cancel) = state.turn_registry.lock()
                                            .expect("turn registry lock").get(sid) {
                                            let _ = cancel.send(true);
                                        }
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
                                // Negotiate the wire-format version. The Client
                                // sends its preferred version; we choose the
                                // highest version we both support. If the
                                // Client omits the field we fall back to v1
                                // (the conservative default).
                                let negotiated = negotiate_protocol_version(Some(params.clone()));
                                // Arc::make_mut gives us &mut AppState when
                                // there's only one strong reference; if the
                                // Arc is shared, it clones. Inside
                                // run_acp_loop the Arc has refcount 1, so
                                // make_mut never actually clones.
                                Arc::make_mut(&mut state).protocol_version = negotiated;
                                let result = engine::initialize(&state.config, negotiated);
                                acp::send_response(&id, result);
                            }
                            "session/new" => {
                                let raw_cwd = params.get("cwd").and_then(|v| v.as_str()).unwrap_or("/tmp");
                                if let Some(servers) = params.get("mcpServers").and_then(|v| v.as_array()) {
                                    if !servers.is_empty() {
                                        debug!(count = servers.len(), "Ignoring mcpServers param (not supported in v0.7)");
                                    }
                                }
                                match engine::session_new(&state, raw_cwd, state.protocol_version) {
                                    Ok(session_id) => {
                                        // Post-creation notifications first so
                                        // Clients that want to render the
                                        // slash-command menu / session title
                                        // as soon as the sessionId arrives
                                        // see them attached to the same
                                        // session. Notifications carry the
                                        // sessionId they apply to, so sending
                                        // them before the response is safe
                                        // (Client binds them on receipt of
                                        // the response anyway). Routed via
                                        // the v1/v2 dispatcher so v2 Clients
                                        // get the v2 wire shape (e.g.
                                        // plan_update with planId).
                                        engine::session_new_post_create_notifications(
                                            &session_id,
                                            raw_cwd,
                                            state.protocol_version,
                                        );
                                        acp::send_response(&id, json!({"sessionId": session_id}));
                                    }
                                    Err(e) => {
                                        acp::send_error(&id, e.code(), &e.to_string());
                                    }
                                }
                            }
                            "session/prompt" => {
                                let sid = params.get("sessionId").and_then(Value::as_str)
                                    .unwrap_or("").to_string();
                                let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
                                {
                                    let mut registry = state.turn_registry.lock().expect("turn registry lock");
                                    if registry.contains_key(&sid) {
                                        acp::send_error_with_data(&id, -32001,
                                            "A turn is already in progress for this session; send session/cancel first",
                                            json!({"reason": "turn_in_progress"}));
                                        continue;
                                    }
                                    // Register before spawning: an immediately following cancel must see this turn.
                                    registry.insert(sid, cancel_tx);
                                }
                                let state_task = Arc::clone(&state);
                                while prompts.try_join_next().is_some() {}
                                prompts.spawn(async move {
                                    handle_acp_prompt(id, &params, &state_task, cancel_rx).await;
                                });
                            }
                            "session/end" | "session/close" => {
                                let session_id = params.get("sessionId").and_then(|v| v.as_str()).unwrap_or("");
                                if session_id.is_empty() {
                                    let err = AcpError::MissingParam { field: "sessionId".into() };
                                    acp::send_error(&id, err.code(), &err.to_string());
                                } else {
                                    // `session/end` is the v1 method;
                                    // `session/close` is the v2 baseline
                                    // alias (see schema/v2 SessionCapabilities).
                                    // Both share the same implementation:
                                    // remove the in-memory session. No
                                    // persistence, so re-opening is a no-op.
                                    match engine::session_end(&state, session_id) {
                                        Ok(()) => acp::send_response(&id, json!({"status": "ended"})),
                                        Err(e) => acp::send_error(&id, e.code(), &e.to_string()),
                                    }
                                }
                            }
                            "session/list" => {
                                // v2 baseline session method. Lists currently
                                // active sessions. Returns an empty `sessions`
                                // array (and cursor: null) when there are none.
                                let sessions = engine::session_list(&state);
                                acp::send_response(
                                    &id,
                                    json!({
                                        "sessions": sessions,
                                        "nextCursor": null,
                                    }),
                                );
                            }
                            "session/load" | "session/resume" => {
                                // acp-bridge has no persistence layer; it does not
                                // advertise `loadSession` in `agentCapabilities`, so a
                                // spec-compliant Client should not call this. We
                                // return `-32001` (application error) with a stable
                                // `data.reason` so Clients can branch on it.
                                acp::send_error_with_data(
                                    &id,
                                    -32001,
                                    "acp-bridge has no persistence layer; session/load and session/resume are unavailable",
                                    json!({ "reason": "no_persistence" }),
                                );
                            }
                            "session/delete" => {
                                // v2 optional session method (advertised via
                                // `capabilities.session.delete: {}`). acp-bridge
                                // does not implement deletion from `session/list`
                                // — `session/close` is the equivalent.
                                acp::send_error_with_data(
                                    &id,
                                    -32601,
                                    "session/delete is not supported by acp-bridge; use session/close instead",
                                    json!({ "reason": "not_implemented" }),
                                );
                            }
                            "session/set_mode" => {
                                // session/new does not include a `modes` array, so
                                // per ACP spec this method is not applicable to
                                // sessions created by acp-bridge. Spec-compliant
                                // Clients detect this via the missing `modes` array
                                // and skip the call entirely; this branch is the
                                // safety net for non-compliant Clients.
                                acp::send_error_with_data(
                                    &id,
                                    -32602,
                                    "session/set_mode is not applicable; sessions created by acp-bridge have no modes",
                                    json!({ "reason": "no_modes" }),
                                );
                            }
                            "auth/login" | "auth/logout" => {
                                // acp-bridge advertises `authMethods: []` at
                                // initialize, so a spec-compliant Client should not
                                // call auth methods. Return a clear reason so the
                                // Client can log it.
                                acp::send_error_with_data(
                                    &id,
                                    -32601,
                                    "acp-bridge does not implement authentication; authMethods is empty at initialize",
                                    json!({ "reason": "no_auth_methods" }),
                                );
                            }
                            method if method.starts_with("fs/") => {
                                // The Client-side fs surface is for agents that
                                // want to read/write files via the editor.
                                // acp-bridge uses its own built-in tool set
                                // (`read_file` / `write_file` / `list_dir` / …)
                                // and never calls these. Spec-compliant Clients
                                // detect the agent's lack of need via
                                // `agentCapabilities`; this is the safety net.
                                acp::send_error_with_data(
                                    &id,
                                    -32601,
                                    "acp-bridge does not call client-side filesystem methods",
                                    json!({ "reason": "agent_does_not_call_client_fs" }),
                                );
                            }
                            method if method.starts_with("terminal/") => {
                                acp::send_error_with_data(
                                    &id,
                                    -32601,
                                    "acp-bridge does not call client-side terminal methods",
                                    json!({ "reason": "agent_does_not_call_client_terminal" }),
                                );
                            }
                            _ => {
                                let err = AcpError::MethodNotFound { method: method.to_string() };
                                acp::send_error(&id, err.code(), &err.to_string());
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
                prompts.abort_all();
                break;
            }
        }
    }

    // In-flight prompts still answer: a Client may close stdin right after sending one.
    while prompts.join_next().await.is_some() {}

    // Cleanup
    let session_count = state.cleanup();
    if session_count > 0 {
        info!(sessions = session_count, "Cleaned up sessions on exit");
    }
}

fn emit_prompt_notification(version: ProtocolVersion, session_id: &str, notif: Notification) {
    match notif {
        Notification::Thinking => acp::notify_thinking_for(version, session_id),
        Notification::ToolStart { id, name } => {
            acp::notify_tool_start_for(version, session_id, &id, &name)
        }
        Notification::ToolDone { id, name, status } => {
            acp::notify_tool_done_for(version, session_id, &id, &name, &status)
        }
        Notification::TextChunk(text) => acp::notify_text_for(version, session_id, &text),
    }
}

/// Handle ACP session/prompt — runs engine and streams notifications to stdout.
async fn handle_acp_prompt(
    id: RequestId,
    params: &Value,
    state: &Arc<AppState>,
    mut cancel_rx: tokio::sync::watch::Receiver<bool>,
) {
    struct TurnGuard<'a> {
        state: &'a AppState,
        session_id: &'a str,
    }
    impl Drop for TurnGuard<'_> {
        fn drop(&mut self) {
            self.state
                .turn_registry
                .lock()
                .expect("turn registry lock")
                .remove(self.session_id);
        }
    }
    let guard = TurnGuard {
        state,
        session_id: params
            .get("sessionId")
            .and_then(Value::as_str)
            .unwrap_or(""),
    };
    let session_id = match params.get("sessionId").and_then(|v| v.as_str()) {
        Some(s) => s.to_string(),
        None => {
            let err = AcpError::MissingParam {
                field: "sessionId".into(),
            };
            drop(guard);
            acp::send_error(&id, err.code(), &err.to_string());
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
        drop(guard);
        acp::send_error(&id, err.code(), &err.to_string());
        return;
    }

    // Mint a messageId for this prompt. v2's PromptResponse schema requires
    // it (`required: ["messageId"]`); v1 Clients ignore the field but we
    // send it on every wire version to keep the response shape
    // identical between versions. The id is opaque to the Client — we use
    // a UUID so two concurrent turns in the same session can never
    // collide.
    let message_id = uuid::Uuid::new_v4().to_string();

    // Compute a session title from the user's first non-empty line so
    // Clients can surface a meaningful name in their session list. Done
    // before spawning the engine task because `user_text` is moved into
    // the task below.
    let derived_title = user_text
        .lines()
        .find(|l| !l.trim().is_empty())
        .map(|l| l.trim().chars().take(80).collect::<String>())
        .filter(|s: &String| !s.is_empty());

    // Set up notification channel for ACP streaming
    let (notify_tx, mut notify_rx) = mpsc::unbounded_channel::<Notification>();

    // Spawn the engine prompt in a task so we can drain notifications
    let state_clone = Arc::clone(state);
    let sid = session_id.clone();
    let mid = message_id.clone();
    let mut handle = tokio::spawn(async move {
        engine::session_prompt(
            &state_clone,
            &sid,
            &user_text,
            &user_images,
            Some(notify_tx),
            &mid,
        )
        .await
    });

    let mut cancelled = false;
    loop {
        tokio::select! {
            // `changed` also fires for a cancel sent before this task first polls.
            _ = cancel_rx.changed() => {
                handle.abort();
                // Wait for the engine to release its session access before accepting another turn.
                let _ = (&mut handle).await;
                engine::session_cancelled(state, &session_id);
                while let Ok(notif) = notify_rx.try_recv() {
                    emit_prompt_notification(state.protocol_version, &session_id, notif);
                }
                cancelled = true;
                break;
            }
            notif = notify_rx.recv() => {
                match notif {
                    Some(notif) => emit_prompt_notification(state.protocol_version, &session_id, notif),
                    None => break,
                }
            }
        }
    }
    if cancelled {
        drop(guard);
        if state.protocol_version == ProtocolVersion::V2 {
            acp::notify_state_idle_for(state.protocol_version, &session_id, Some("cancelled"));
            acp::send_response(&id, json!({"messageId": message_id}));
        } else {
            acp::send_response(
                &id,
                json!({"stopReason": "cancelled", "status": "cancelled", "text": ""}),
            );
        }
        return;
    }

    let result = handle.await.unwrap_or_else(|_| engine::PromptResult {
        status: "failed".into(),
        text: "Internal error".into(),
        error: None,
        usage: engine::UsageReport {
            used: 0,
            size: state.config.context_size,
        },
        error_class: Some(acp_bridge::llm::LlmErrorKind::Unknown),
        error_retryable: false,
        message_id: message_id.clone(),
    });

    drop(guard);

    // If the engine returned a protocol error (e.g. unknown session), send JSON-RPC error
    if let Some(err) = &result.error {
        acp::send_error(&id, err.code(), &err.to_string());
        return;
    }

    // A successful prompt ends a turn, which ACP signals via `stopReason:
    // "end_turn"`. The generated text is delivered through the streamed
    // `agent_message_chunk` notifications.
    //
    // The legacy `status`/`text` fields are kept alongside `stopReason` so
    // existing OpenAB pipelines that rely on the final response body keep
    // working; standard ACP clients read `stopReason` and ignore the extra
    // fields. Once OpenAB migrates off the legacy format this can be trimmed.
    // ACP v1 StopReason values: end_turn / max_tokens / max_turn_requests /
    // refusal / cancelled. There is no generic "error" reason, so a turn that
    // hits the tool-call round limit maps to `max_turn_requests` (the model
    // made the maximum number of requests in a single turn). The actual error
    // text is already delivered via the streamed `agent_message_chunk` and the
    // legacy `status`/`text` fields below.
    // The legacy `status`/`text` fields are kept alongside `stopReason` so
    // existing OpenAB pipelines that rely on the final response body keep
    // working; standard ACP clients read `stopReason` and ignore the extra
    // fields. Once OpenAB migrates off the legacy format this can be trimmed.
    // ACP v1 StopReason values: end_turn / max_tokens / max_turn_requests /
    // refusal / cancelled. There is no generic "error" reason, so a turn that
    // hits the tool-call round limit maps to `max_turn_requests` (the model
    // made the maximum number of requests in a single turn). The actual error
    // text is already delivered via the streamed `agent_message_chunk` and the
    // legacy `status`/`text` fields below. The stop_reason value lives
    // inside the response-body dispatch below; v1 puts it on the
    // response directly, v2 puts it on `state_update`.

    // If the turn failed due to a classified LLM error, attach the
    // classification to the response so Clients can branch on
    // `error.category` and `error.retryable` without parsing the prose
    // `text` field. We only emit this when `error_class` is set; pure
    // `max_turn_requests` (tool-loop exhaustion) does not classify.
    let error_meta = result.error_class.as_ref().map(|k| {
        json!({
            "category": k.as_str(),
            "retryable": k.is_retryable(),
        })
    });

    // Post-turn notifications come BEFORE the response so Clients that
    // collect both (e.g. via `read_until_response`) see them bound to
    // this turn. The notification `sessionId` field already tells the
    // Client which session they apply to. Routed through the v1/v2
    // dispatcher so v2 Clients get the v2 wire shape.
    acp::notify_usage_for(
        state.protocol_version,
        &session_id,
        result.usage.used,
        result.usage.size,
        None,
    );
    if let Some(title) = derived_title.as_deref() {
        acp::notify_session_info_for(state.protocol_version, &session_id, title, None);
    }

    // v2 Clients expect a `state_update` with `stopReason` so they can
    // render the prompt input affordably. v1 Clients ignore unknown
    // sessionUpdate discriminators per the JSON-RPC spec, so it's safe
    // to always call the dispatcher (it no-ops on v1).
    if state.protocol_version == ProtocolVersion::V2 {
        let stop_reason = if result.status == "completed" {
            "end_turn"
        } else {
            "max_turn_requests"
        };
        acp::notify_state_idle_for(state.protocol_version, &session_id, Some(stop_reason));
    }

    // Wire-shape dispatch on the response itself. v1 Clients keep the
    // legacy `{stopReason, status, text, error?}` shape (ACP v1 has no
    // concept of messageId; the OpenAB pipeline reads `text`). v2
    // Clients receive the spec-compliant `{messageId}` shape —
    // `stopReason` lives on `state_update` per the v2 schema, and
    // messageId is the only required field on PromptResponse.
    let response_body = if state.protocol_version == ProtocolVersion::V2 {
        json!({
            "messageId": result.message_id,
        })
    } else {
        let stop_reason = if result.status == "completed" {
            "end_turn"
        } else {
            "max_turn_requests"
        };
        let mut body = json!({
            "stopReason": stop_reason,
            "status": result.status,
            "text": result.text,
        });
        if let Some(meta) = error_meta {
            body["error"] = meta;
        }
        body
    };
    acp::send_response(&id, response_body);
}
