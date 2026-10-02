//! Zed-style e2e test.
//!
//! Reproduces the protocol sequence that `zed-industries/zed` actually
//! emits when launching an ACP agent. Source:
//! `crates/agent_servers/src/acp.rs::client_capabilities_for_agent` and
//! the call site at line ~836 in `acp.rs`.
//!
//! Verifies:
//! - acp-bridge accepts the full Zed clientCapabilities shape without
//!   panicking
//! - `initialize` response carries the expected protocolVersion /
//!   agentCapabilities
//! - `session/new` succeeds even when `mcpServers` is supplied (it is
//!   silently ignored per `docs/scope.md`)
//! - `session/prompt` with a text content block streams back
//!   `agent_message_chunk` notifications and returns a `stopReason` in
//!   the response
//! - `session/load` / `session/set_mode` / `auth/login` are gracefully
//!   rejected with `data.reason`
//!
//! Note: We never call a real LLM; the harness sets `LLM_BASE_URL` to an
//! unreachable port so the chat call fails fast with an error message
//! (which is still streamed as `agent_message_chunk`). That is good
//! enough for protocol-shape assertions.

#[path = "harness.rs"]
mod harness;

use harness::{prompt, Agent};
use serde_json::{json, Value};
use std::time::Duration;

fn zed_initialize_params() -> Value {
    json!({
        "protocolVersion": 1,
        "clientCapabilities": {
            "fs": {"readTextFile": true, "writeTextFile": true},
            "terminal": true,
            "auth": {"terminal": true},
            "session": {
                "config_options": {"boolean": {}},
                "compaction": {},
                "notices": {}
            },
            "elicitation": {
                "form": {},
                "url": {}
            },
            "_meta": {
                "terminal_output": true,
                "terminal-auth": true
            }
        },
        "clientInfo": {
            "name": "zed",
            "version": "1.0.0",
            "title": "Zed"
        }
    })
}

#[test]
fn zed_style_initialize_accepts_full_capability_shape() {
    let mut a = Agent::spawn(&[]);
    a.request(1, "initialize", zed_initialize_params());
    let (_notif, resp) = a.recv_response(&Value::from(1), Duration::from_secs(5));

    assert_eq!(resp["id"], 1);
    let result = &resp["result"];
    assert_eq!(result["protocolVersion"], 1);
    assert!(result.get("agentCapabilities").is_some());
    // image is off by default per docs/scope.md
    assert_eq!(
        result["agentCapabilities"]["promptCapabilities"]["image"],
        false
    );
    // loadSession is not advertised — Zed should not call session/load.
    let load_session = result["agentCapabilities"]
        .get("loadSession")
        .cloned()
        .unwrap_or(json!(false));
    assert!(
        load_session == false,
        "acp-bridge must not advertise loadSession, got: {load_session}"
    );
    a.shutdown();
}

#[test]
fn zed_style_session_new_ignores_mcp_servers_param() {
    let mut a = Agent::spawn(&[]);
    a.request(1, "initialize", zed_initialize_params());
    let _ = a.recv_response(&Value::from(1), Duration::from_secs(5));

    a.request(
        2,
        "session/new",
        json!({
            "cwd": "/tmp",
            "mcpServers": [
                {"name": "filesystem", "command": "fs-mcp", "args": ["--stdio"]}
            ]
        }),
    );
    let (_notif, resp) = a.recv_response(&Value::from(2), Duration::from_secs(5));
    assert!(resp["result"]["sessionId"].is_string());
    a.shutdown();
}

#[test]
fn zed_style_session_prompt_streams_text_chunks_and_returns_stop_reason() {
    let mut a = Agent::spawn(&[]);
    a.request(1, "initialize", zed_initialize_params());
    let _ = a.recv_response(&Value::from(1), Duration::from_secs(5));

    a.request(2, "session/new", json!({"cwd": "/tmp"}));
    let (_, resp) = a.recv_response(&Value::from(2), Duration::from_secs(5));
    let sid = resp["result"]["sessionId"].as_str().unwrap().to_string();

    let (notifications, response) =
        prompt(&mut a, 3, &sid, json!([{"type": "text", "text": "say hi"}]));

    // We expect at least one agent_message_chunk with the (error or reply) text.
    let chunks: Vec<&Value> = notifications
        .iter()
        .filter(|m| m["params"]["update"]["sessionUpdate"] == "agent_message_chunk")
        .collect();
    assert!(
        !chunks.is_empty(),
        "expected at least one agent_message_chunk notification, got: {notifications:?}"
    );
    for chunk in &chunks {
        let content = &chunk["params"]["update"]["content"];
        assert_eq!(content["type"], "text");
        assert!(content["text"].is_string());
    }

    // The final response must carry a valid ACP v1 stopReason.
    let stop_reason = response["result"]["stopReason"].as_str();
    assert!(
        matches!(stop_reason, Some("end_turn") | Some("max_turn_requests")),
        "stopReason must be end_turn or max_turn_requests, got: {stop_reason:?}"
    );
    a.shutdown();
}

#[test]
fn zed_style_session_load_returns_no_persistence_data_reason() {
    let mut a = Agent::spawn(&[]);
    a.request(1, "initialize", zed_initialize_params());
    let _ = a.recv_response(&Value::from(1), Duration::from_secs(5));

    a.request(
        2,
        "session/load",
        json!({"sessionId": "sess_does_not_exist", "cwd": "/tmp"}),
    );
    let (_, resp) = a.recv_response(&Value::from(2), Duration::from_secs(5));
    assert_eq!(resp["error"]["code"], -32001);
    assert_eq!(resp["error"]["data"]["reason"], "no_persistence");
    a.shutdown();
}

#[test]
fn zed_style_session_set_mode_returns_no_modes_data_reason() {
    let mut a = Agent::spawn(&[]);
    a.request(1, "initialize", zed_initialize_params());
    let _ = a.recv_response(&Value::from(1), Duration::from_secs(5));

    a.request(
        2,
        "session/set_mode",
        json!({"sessionId": "sess_x", "modeId": "fast"}),
    );
    let (_, resp) = a.recv_response(&Value::from(2), Duration::from_secs(5));
    assert_eq!(resp["error"]["code"], -32602);
    assert_eq!(resp["error"]["data"]["reason"], "no_modes");
    a.shutdown();
}

#[test]
fn zed_style_auth_login_returns_no_auth_methods_data_reason() {
    let mut a = Agent::spawn(&[]);
    a.request(1, "initialize", zed_initialize_params());
    let _ = a.recv_response(&Value::from(1), Duration::from_secs(5));

    a.request(2, "auth/login", json!({"methodId": "any"}));
    let (_, resp) = a.recv_response(&Value::from(2), Duration::from_secs(5));
    assert_eq!(resp["error"]["code"], -32601);
    assert_eq!(resp["error"]["data"]["reason"], "no_auth_methods");
    a.shutdown();
}
