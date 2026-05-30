//! Tests for the ACP client module.
//!
//! Unit tests for permission handling, event classification, and config parsing.

use acp_bridge::client::{classify_notification, AcpEvent, JsonRpcMessage};
use acp_bridge::config::{AgentConfig, ConfigFile};
use serde_json::json;

// ---------------------------------------------------------------------------
// Event classification tests
// ---------------------------------------------------------------------------

#[test]
fn classify_text_chunk() {
    let msg = JsonRpcMessage {
        id: None,
        method: Some("session/notify".into()),
        result: None,
        error: None,
        params: Some(json!({
            "update": {
                "sessionUpdate": "agent_message_chunk",
                "content": {"text": "Hello world"}
            }
        })),
    };
    match classify_notification(&msg) {
        Some(AcpEvent::Text(t)) => assert_eq!(t, "Hello world"),
        other => panic!("Expected Text, got {other:?}"),
    }
}

#[test]
fn classify_thinking() {
    let msg = JsonRpcMessage {
        id: None,
        method: Some("session/notify".into()),
        result: None,
        error: None,
        params: Some(json!({
            "update": {"sessionUpdate": "agent_thought_chunk"}
        })),
    };
    assert!(matches!(
        classify_notification(&msg),
        Some(AcpEvent::Thinking)
    ));
}

#[test]
fn classify_tool_start_and_done() {
    let start = JsonRpcMessage {
        id: None,
        method: Some("session/notify".into()),
        result: None,
        error: None,
        params: Some(json!({
            "update": {
                "sessionUpdate": "tool_call",
                "toolCallId": "tc-42",
                "title": "Read"
            }
        })),
    };
    match classify_notification(&start) {
        Some(AcpEvent::ToolStart { id, title }) => {
            assert_eq!(id, "tc-42");
            assert_eq!(title, "Read");
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
                "toolCallId": "tc-42",
                "title": "Read",
                "status": "completed"
            }
        })),
    };
    match classify_notification(&done) {
        Some(AcpEvent::ToolDone { id, title, status }) => {
            assert_eq!(id, "tc-42");
            assert_eq!(title, "Read");
            assert_eq!(status, "completed");
        }
        other => panic!("Expected ToolDone, got {other:?}"),
    }
}

#[test]
fn classify_unknown_update_returns_none() {
    let msg = JsonRpcMessage {
        id: None,
        method: Some("session/notify".into()),
        result: None,
        error: None,
        params: Some(json!({
            "update": {"sessionUpdate": "unknown_type"}
        })),
    };
    assert!(classify_notification(&msg).is_none());
}

#[test]
fn classify_non_notification_returns_none() {
    // A response message (has id, no method) should not classify
    let msg = JsonRpcMessage {
        id: Some(1),
        method: None,
        result: Some(json!({"status": "completed"})),
        error: None,
        params: None,
    };
    assert!(classify_notification(&msg).is_none());
}

// ---------------------------------------------------------------------------
// AgentConfig tests
// ---------------------------------------------------------------------------

#[test]
fn agent_config_from_toml() {
    let toml_str = r#"
[agent]
command = "opencode"
args = ["acp"]
working_dir = "/home/node"

[agent.env]
MY_KEY = "my_value"
"#;
    let config: ConfigFile = toml::from_str(toml_str).unwrap();
    let agent = config.agent_config().expect("should have agent config");
    assert_eq!(agent.command, "opencode");
    assert_eq!(agent.args, vec!["acp"]);
    assert_eq!(agent.working_dir, "/home/node");
    assert_eq!(agent.env.get("MY_KEY").unwrap(), "my_value");
}

#[test]
fn agent_config_none_when_missing() {
    let toml_str = r#"
[llm]
model = "gemma4:26b"
"#;
    let config: ConfigFile = toml::from_str(toml_str).unwrap();
    assert!(config.agent_config().is_none());
}

#[test]
fn agent_config_env_override() {
    // Set env vars
    std::env::set_var("AGENT_COMMAND", "test-agent");
    std::env::set_var("AGENT_ARGS", "arg1 arg2");
    std::env::set_var("AGENT_WORKING_DIR", "/custom/dir");

    let config = AgentConfig::from_env().expect("should build from env");
    assert_eq!(config.command, "test-agent");
    assert_eq!(config.args, vec!["arg1", "arg2"]);
    assert_eq!(config.working_dir, "/custom/dir");

    // Cleanup
    std::env::remove_var("AGENT_COMMAND");
    std::env::remove_var("AGENT_ARGS");
    std::env::remove_var("AGENT_WORKING_DIR");
}
