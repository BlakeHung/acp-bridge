//! A2A (Agent-to-Agent) protocol — HTTP transport, Agent Card, and task lifecycle.
//!
//! Implements Google's A2A protocol for inter-agent communication.
//! Runs alongside or instead of the stdin/stdout ACP transport.

use crate::engine::{self, AppState};
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Json};
use axum::routing::{get, post};
use axum::Router;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::sync::Arc;
use tracing::{debug, info, warn};

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
    /// Optional bearer token required on JSON-RPC requests. When `None`, the
    /// server accepts unauthenticated requests (documented default).
    pub auth_token: Option<String>,
}

impl Default for A2aConfig {
    fn default() -> Self {
        Self {
            host: "0.0.0.0".into(),
            port: 8080,
            agent_name: "acp-bridge".into(),
            agent_description:
                "Self-hosted AI agent bridge for air-gapped and enterprise environments".into(),
            auth_token: None,
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
            auth_token: normalize_token(std::env::var("A2A_AUTH_TOKEN").ok()),
        }
    }
}

/// Treat an empty/whitespace-only token as "no token configured".
pub fn normalize_token(token: Option<String>) -> Option<String> {
    token
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
}

/// Constant-time byte comparison to avoid leaking the token via timing.
fn tokens_match(expected: &str, provided: &str) -> bool {
    let a = expected.as_bytes();
    let b = provided.as_bytes();
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// Validate the `Authorization: Bearer <token>` header against the configured
/// token. Returns `true` when no token is configured (auth disabled).
fn is_authorized(state: &A2aState, headers: &HeaderMap) -> bool {
    let Some(expected) = state.config.auth_token.as_deref() else {
        return true;
    };
    let provided = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::trim)
        .unwrap_or("");
    tokens_match(expected, provided)
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
    headers: HeaderMap,
    Json(req): Json<A2aRequest>,
) -> impl IntoResponse {
    debug!(method = %req.method, "A2A request received");

    if !is_authorized(&state, &headers) {
        warn!(method = %req.method, "Rejected unauthorized A2A request");
        let resp = jsonrpc_error(
            req.id.as_ref(),
            -32001,
            "Unauthorized: invalid or missing bearer token",
        );
        return (StatusCode::UNAUTHORIZED, Json(resp));
    }

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

    if config.auth_token.is_some() {
        info!("A2A bearer-token authentication enabled");
    } else {
        let loopback =
            config.host == "127.0.0.1" || config.host == "localhost" || config.host == "::1";
        if !loopback {
            warn!(
                host = %config.host,
                "A2A server is running WITHOUT authentication on a non-loopback address. \
                 Anyone who can reach this port can send prompts. Set A2A_AUTH_TOKEN to require \
                 a bearer token, or bind A2A_HOST=127.0.0.1."
            );
        }
    }

    let listener = tokio::net::TcpListener::bind(&addr).await?;
    let router = a2a_router(state, config);
    axum::serve(listener, router).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::AppState;
    use crate::llm::LlmConfig;
    use axum::http::header::AUTHORIZATION;

    fn state_with_token(token: Option<&str>) -> A2aState {
        let config = A2aConfig {
            auth_token: token.map(String::from),
            ..A2aConfig::default()
        };
        A2aState {
            app: AppState::new(LlmConfig::from_env()),
            config,
        }
    }

    fn headers_with_auth(value: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(AUTHORIZATION, value.parse().unwrap());
        h
    }

    #[test]
    fn is_authorized_allows_all_when_no_token_configured() {
        let state = state_with_token(None);
        assert!(is_authorized(&state, &HeaderMap::new()));
        assert!(is_authorized(&state, &headers_with_auth("Bearer whatever")));
    }

    #[test]
    fn is_authorized_requires_matching_bearer_when_token_set() {
        let state = state_with_token(Some("s3cret"));
        assert!(is_authorized(&state, &headers_with_auth("Bearer s3cret")));
        assert!(!is_authorized(&state, &headers_with_auth("Bearer wrong")));
        assert!(!is_authorized(&state, &headers_with_auth("s3cret")));
        assert!(!is_authorized(&state, &HeaderMap::new()));
    }

    #[test]
    fn normalize_token_treats_blank_as_none() {
        assert_eq!(normalize_token(None), None);
        assert_eq!(normalize_token(Some("".into())), None);
        assert_eq!(normalize_token(Some("   ".into())), None);
        assert_eq!(
            normalize_token(Some("  secret  ".into())),
            Some("secret".into())
        );
    }

    #[test]
    fn tokens_match_compares_exactly() {
        assert!(tokens_match("s3cret", "s3cret"));
        assert!(!tokens_match("s3cret", "s3cre"));
        assert!(!tokens_match("s3cret", "s3crett"));
        assert!(!tokens_match("s3cret", "wrong!"));
        assert!(!tokens_match("s3cret", ""));
    }
}
