//! A2A (Agent-to-Agent) protocol — HTTP transport, Agent Card, and task lifecycle.
//!
//! Implements Google's A2A protocol for inter-agent communication.
//! Runs alongside or instead of the stdin/stdout ACP transport.

use crate::engine::{self, AppState};
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Json};
use axum::routing::{get, post};
use axum::Router;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::sync::Arc;
use tracing::{debug, info};

// ---------------------------------------------------------------------------
// A2A types
// ---------------------------------------------------------------------------

/// A2A Agent Card — served at /.well-known/agent.json
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentCard {
    pub name: String,
    pub description: String,
    pub url: String,
    pub version: String,
    pub capabilities: AgentCapabilities,
    pub skills: Vec<AgentSkill>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentCapabilities {
    pub streaming: bool,
    pub push_notifications: bool,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentSkill {
    pub id: String,
    pub name: String,
    pub description: String,
}

/// A2A JSON-RPC request envelope.
#[derive(Debug, Deserialize)]
pub struct A2aRequest {
    pub jsonrpc: Option<String>,
    pub id: Option<Value>,
    pub method: String,
    pub params: Option<Value>,
}

/// A2A message part (text content).
#[derive(Debug, Serialize, Deserialize)]
pub struct A2aPart {
    #[serde(rename = "type")]
    pub kind: String,
    pub text: String,
}

/// A2A message (user or agent).
#[derive(Debug, Serialize, Deserialize)]
pub struct A2aMessage {
    pub role: String,
    pub parts: Vec<A2aPart>,
}

/// A2A task states.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum TaskState {
    Submitted,
    Working,
    Completed,
    Failed,
}

// ---------------------------------------------------------------------------
// A2A config
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct A2aConfig {
    pub host: String,
    pub port: u16,
    pub agent_name: String,
    pub agent_description: String,
}

impl Default for A2aConfig {
    fn default() -> Self {
        Self {
            host: "0.0.0.0".into(),
            port: 8080,
            agent_name: "acp-bridge".into(),
            agent_description:
                "Self-hosted AI agent bridge for air-gapped and enterprise environments".into(),
        }
    }
}

impl A2aConfig {
    pub fn from_env() -> Self {
        let defaults = Self::default();
        Self {
            host: std::env::var("A2A_HOST").unwrap_or(defaults.host),
            port: std::env::var("A2A_PORT")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(defaults.port),
            agent_name: std::env::var("A2A_AGENT_NAME").unwrap_or(defaults.agent_name),
            agent_description: std::env::var("A2A_AGENT_DESCRIPTION")
                .unwrap_or(defaults.agent_description),
        }
    }
}

// ---------------------------------------------------------------------------
// Router
// ---------------------------------------------------------------------------

pub fn a2a_router(state: Arc<AppState>, a2a_config: A2aConfig) -> Router {
    let shared = Arc::new(A2aState {
        app: state,
        config: a2a_config,
    });
    Router::new()
        .route("/.well-known/agent.json", get(handle_agent_card))
        .route("/", post(handle_a2a_dispatch))
        .with_state(shared)
}

struct A2aState {
    app: Arc<AppState>,
    config: A2aConfig,
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

fn jsonrpc_error(id: Option<&Value>, code: i64, message: impl Into<String>) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": code, "message": message.into() }
    })
}

/// GET /.well-known/agent.json — Agent Card for service discovery.
async fn handle_agent_card(State(state): State<Arc<A2aState>>) -> Json<AgentCard> {
    let card = AgentCard {
        name: state.config.agent_name.clone(),
        description: state.config.agent_description.clone(),
        url: format!("http://{}:{}", state.config.host, state.config.port),
        version: env!("CARGO_PKG_VERSION").into(),
        capabilities: AgentCapabilities {
            streaming: false, // v0.6.0: no streaming yet
            push_notifications: false,
        },
        skills: vec![AgentSkill {
            id: "coding-assistant".into(),
            name: "Coding Assistant".into(),
            description:
                "AI coding assistant with file reading, directory listing, and code search tools"
                    .into(),
        }],
    };
    Json(card)
}

/// POST / — A2A JSON-RPC dispatch.
async fn handle_a2a_dispatch(
    State(state): State<Arc<A2aState>>,
    Json(req): Json<A2aRequest>,
) -> impl IntoResponse {
    debug!(method = %req.method, "A2A request received");

    match req.method.as_str() {
        "message/send" => handle_message_send(&state, req).await,
        _ => {
            let resp = jsonrpc_error(
                req.id.as_ref(),
                -32601,
                format!("Method not found: {}", req.method),
            );
            (StatusCode::OK, Json(resp))
        }
    }
}

/// Handle A2A `message/send` — synchronous request-response.
///
/// Creates a temporary session, runs the prompt, returns the result, cleans up.
async fn handle_message_send(state: &A2aState, req: A2aRequest) -> (StatusCode, Json<Value>) {
    let params = req.params.unwrap_or(json!({}));

    let parts = params
        .get("message")
        .and_then(|m| m.get("parts"))
        .and_then(|p| p.as_array());

    let (raw_user_text, user_images) = match parts {
        Some(arr) => (
            engine::extract_text_parts(arr),
            engine::extract_image_parts(arr),
        ),
        None => (String::new(), Vec::new()),
    };
    let (user_text, sender_context) = engine::strip_sender_context(&raw_user_text);
    if let Some(ctx) = &sender_context {
        debug!(
            sender_context_len = ctx.len(),
            "Stripped <sender_context> block from A2A message text"
        );
    }

    if user_text.is_empty() && user_images.is_empty() {
        let resp = jsonrpc_error(
            req.id.as_ref(),
            -32602,
            "Missing or empty message — expected at least one text or image part",
        );
        return (StatusCode::OK, Json(resp));
    }

    // Create temporary session
    let cwd = params
        .get("metadata")
        .and_then(|m| m.get("cwd"))
        .and_then(|c| c.as_str())
        .unwrap_or("/tmp");

    let session_id = match engine::session_new(&state.app, cwd) {
        Ok(id) => id,
        Err(e) => {
            let resp = jsonrpc_error(req.id.as_ref(), e.code(), e.to_string());
            return (StatusCode::OK, Json(resp));
        }
    };

    let task_id = uuid::Uuid::new_v4().to_string();
    info!(task_id = %task_id, session_id = %session_id, "A2A message/send");

    // Run prompt (no notification channel — A2A is request-response)
    let result =
        engine::session_prompt(&state.app, &session_id, &user_text, &user_images, None).await;

    // Clean up session
    let _ = engine::session_end(&state.app, &session_id);

    // Build A2A response
    let task_state = if result.status == "completed" {
        "completed"
    } else {
        "failed"
    };

    let resp = json!({
        "jsonrpc": "2.0",
        "id": req.id,
        "result": {
            "id": task_id,
            "status": {
                "state": task_state
            },
            "artifacts": [{
                "parts": [{
                    "type": "text",
                    "text": result.text
                }]
            }]
        }
    });

    (StatusCode::OK, Json(resp))
}

// ---------------------------------------------------------------------------
// Server entry point
// ---------------------------------------------------------------------------

/// Run the A2A HTTP server.
pub async fn serve(
    state: Arc<AppState>,
    config: A2aConfig,
) -> Result<(), Box<dyn std::error::Error>> {
    let addr = format!("{}:{}", config.host, config.port);
    info!(addr = %addr, "Starting A2A HTTP server");

    let listener = tokio::net::TcpListener::bind(&addr).await?;
    let router = a2a_router(state, config);
    axum::serve(listener, router).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::LlmConfig;
    use reqwest::Client;
    use std::sync::Mutex;
    use std::time::Duration;

    /// Serialize env-var mutating tests so they don't race on `A2A_*`.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn test_app_state() -> Arc<AppState> {
        let config = LlmConfig {
            base_url: "http://127.0.0.1:1/v1".into(),
            model: "test-model".into(),
            api_key: "test-key".into(),
            system_prompt: None,
            temperature: None,
            max_tokens: None,
            timeout_secs: 5,
            max_history_turns: 50,
            max_sessions: 0,
            session_idle_timeout_secs: 0,
            client: Client::builder()
                .timeout(Duration::from_secs(5))
                .build()
                .expect("client"),
        };
        AppState::new(config)
    }

    /// Serve the A2A router on an ephemeral port; returns its base URL.
    async fn serve_a2a() -> String {
        let router = a2a_router(test_app_state(), A2aConfig::default());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        format!("http://{addr}")
    }

    // -- config -------------------------------------------------------------

    #[test]
    fn config_default_values() {
        let c = A2aConfig::default();
        assert_eq!(c.host, "0.0.0.0");
        assert_eq!(c.port, 8080);
        assert_eq!(c.agent_name, "acp-bridge");
        assert!(!c.agent_description.is_empty());
    }

    #[test]
    fn config_from_env_overrides_and_defaults() {
        let _lock = ENV_LOCK.lock().unwrap();
        std::env::set_var("A2A_HOST", "127.0.0.1");
        std::env::set_var("A2A_PORT", "9999");
        std::env::set_var("A2A_AGENT_NAME", "custom-agent");
        std::env::remove_var("A2A_AGENT_DESCRIPTION");

        let c = A2aConfig::from_env();
        assert_eq!(c.host, "127.0.0.1");
        assert_eq!(c.port, 9999);
        assert_eq!(c.agent_name, "custom-agent");
        // Falls back to default when the env var is absent.
        assert_eq!(c.agent_description, A2aConfig::default().agent_description);

        std::env::remove_var("A2A_HOST");
        std::env::remove_var("A2A_PORT");
        std::env::remove_var("A2A_AGENT_NAME");
    }

    #[test]
    fn config_from_env_ignores_invalid_port() {
        let _lock = ENV_LOCK.lock().unwrap();
        std::env::set_var("A2A_PORT", "not-a-number");
        let c = A2aConfig::from_env();
        assert_eq!(c.port, A2aConfig::default().port);
        std::env::remove_var("A2A_PORT");
    }

    // -- serialization ------------------------------------------------------

    #[test]
    fn agent_card_serializes_camel_case() {
        let card = AgentCard {
            name: "n".into(),
            description: "d".into(),
            url: "http://x".into(),
            version: "1.2.3".into(),
            capabilities: AgentCapabilities {
                streaming: false,
                push_notifications: true,
            },
            skills: vec![AgentSkill {
                id: "s".into(),
                name: "S".into(),
                description: "desc".into(),
            }],
        };
        let v = serde_json::to_value(&card).unwrap();
        assert_eq!(v["name"], "n");
        assert_eq!(v["capabilities"]["pushNotifications"], true);
        assert_eq!(v["skills"][0]["id"], "s");
    }

    #[test]
    fn task_state_serializes_lowercase() {
        assert_eq!(
            serde_json::to_value(TaskState::Submitted).unwrap(),
            json!("submitted")
        );
        assert_eq!(
            serde_json::to_value(TaskState::Completed).unwrap(),
            json!("completed")
        );
        assert_eq!(
            serde_json::to_value(TaskState::Failed).unwrap(),
            json!("failed")
        );
    }

    #[test]
    fn a2a_message_round_trips() {
        let raw = json!({
            "role": "user",
            "parts": [{"type": "text", "text": "hello"}]
        });
        let msg: A2aMessage = serde_json::from_value(raw).unwrap();
        assert_eq!(msg.role, "user");
        assert_eq!(msg.parts.len(), 1);
        assert_eq!(msg.parts[0].kind, "text");
        assert_eq!(msg.parts[0].text, "hello");
    }

    #[test]
    fn jsonrpc_error_shape() {
        let id = json!(7);
        let err = jsonrpc_error(Some(&id), -32601, "Method not found: x");
        assert_eq!(err["jsonrpc"], "2.0");
        assert_eq!(err["id"], 7);
        assert_eq!(err["error"]["code"], -32601);
        assert_eq!(err["error"]["message"], "Method not found: x");
    }

    // -- router -------------------------------------------------------------

    #[tokio::test]
    async fn agent_card_endpoint_serves_discovery_document() {
        let base = serve_a2a().await;
        let client = Client::new();
        let resp = client
            .get(format!("{base}/.well-known/agent.json"))
            .send()
            .await
            .unwrap();
        assert!(resp.status().is_success());
        let card: Value = resp.json().await.unwrap();
        assert_eq!(card["name"], "acp-bridge");
        assert_eq!(card["skills"][0]["id"], "coding-assistant");
        assert_eq!(card["capabilities"]["streaming"], false);
    }

    #[tokio::test]
    async fn dispatch_unknown_method_returns_method_not_found() {
        let base = serve_a2a().await;
        let client = Client::new();
        let resp = client
            .post(&base)
            .json(&json!({"jsonrpc": "2.0", "id": 1, "method": "does/not/exist"}))
            .send()
            .await
            .unwrap();
        let body: Value = resp.json().await.unwrap();
        assert_eq!(body["id"], 1);
        assert_eq!(body["error"]["code"], -32601);
    }

    #[tokio::test]
    async fn message_send_rejects_empty_message() {
        let base = serve_a2a().await;
        let client = Client::new();
        let resp = client
            .post(&base)
            .json(&json!({
                "jsonrpc": "2.0",
                "id": 2,
                "method": "message/send",
                "params": {"message": {"parts": []}}
            }))
            .send()
            .await
            .unwrap();
        let body: Value = resp.json().await.unwrap();
        assert_eq!(body["id"], 2);
        assert_eq!(body["error"]["code"], -32602);
    }
}
