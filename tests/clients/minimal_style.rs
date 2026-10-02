//! Minimal-Client e2e test.
//!
//! Mirrors what stripped-down Clients (Codex CLI adapter, OpenCode's
//! minimal mode, and most CLI tools) actually send: bare minimum
//! capabilities, single session, prompt-and-exit. No fs/terminal/auth
//! advertised.

#[path = "harness.rs"]
mod harness;

use harness::Agent;
use serde_json::{json, Value};
use std::time::Duration;

fn minimal_init() -> serde_json::Value {
    json!({
        "protocolVersion": 1,
        "clientCapabilities": {},
        "clientInfo": {
            "name": "minimal-test-client",
            "version": "0.0.1"
        }
    })
}

#[test]
fn minimal_style_initialize_with_empty_capabilities() {
    let mut a = Agent::spawn(&[]);
    a.request(1, "initialize", minimal_init());
    let (_, resp) = a.recv_response(&Value::from(1), Duration::from_secs(5));
    assert_eq!(resp["result"]["protocolVersion"], 1);
    // loadSession not advertised
    let load = resp["result"]["agentCapabilities"]
        .get("loadSession")
        .cloned()
        .unwrap_or(json!(false));
    assert!(load == false);
    a.shutdown();
}

#[test]
fn minimal_style_session_end_unknown_session_returns_error() {
    let mut a = Agent::spawn(&[]);
    a.request(1, "initialize", minimal_init());
    let _ = a.recv_response(&Value::from(1), Duration::from_secs(5));

    a.request(2, "session/end", json!({"sessionId": "nope"}));
    let (_, resp) = a.recv_response(&Value::from(2), Duration::from_secs(5));
    // Unknown session id → application error (not method-not-found).
    assert_eq!(resp["error"]["code"], -32001);
    a.shutdown();
}

#[test]
fn minimal_style_session_prompt_missing_session_id_returns_invalid_params() {
    let mut a = Agent::spawn(&[]);
    a.request(1, "initialize", minimal_init());
    let _ = a.recv_response(&Value::from(1), Duration::from_secs(5));

    a.request(
        2,
        "session/prompt",
        json!({"prompt": [{"type": "text", "text": "hi"}]}),
    );
    let (_, resp) = a.recv_response(&Value::from(2), Duration::from_secs(5));
    assert_eq!(resp["error"]["code"], -32602); // invalid params
    a.shutdown();
}

#[test]
fn minimal_style_session_cancel_is_acknowledged_as_notification() {
    // Per ACP spec, session/cancel is a notification (no id). acp-bridge
    // should accept it without error and not crash.
    let mut a = Agent::spawn(&[]);
    a.request(1, "initialize", minimal_init());
    let _ = a.recv_response(&Value::from(1), Duration::from_secs(5));

    a.request(2, "session/new", json!({"cwd": "/tmp"}));
    let (_, resp) = a.recv_response(&Value::from(2), Duration::from_secs(5));
    let sid = resp["result"]["sessionId"].as_str().unwrap().to_string();

    // Notifications have no id — the agent must not respond at all.
    a.notify("session/cancel", json!({"sessionId": sid}));

    // Send a regular request to confirm the agent is still alive.
    a.request(3, "session/end", json!({"sessionId": sid}));
    let (_, resp) = a.recv_response(&Value::from(3), Duration::from_secs(5));
    assert_eq!(resp["result"]["status"], "ended");
    a.shutdown();
}

#[test]
fn minimal_style_unknown_method_returns_method_not_found() {
    let mut a = Agent::spawn(&[]);
    a.request(1, "initialize", minimal_init());
    let _ = a.recv_response(&Value::from(1), Duration::from_secs(5));

    a.request(2, "totally/made_up", json!({}));
    let (_, resp) = a.recv_response(&Value::from(2), Duration::from_secs(5));
    assert_eq!(resp["error"]["code"], -32601);
    a.shutdown();
}
