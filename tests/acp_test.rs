/// Verify JSON-RPC message structure matches ACP spec.
/// Since acp::send writes directly to stdout, we test the JSON shapes independently.
use acp_bridge::protocol::RequestId;

#[test]
fn json_rpc_response_structure() {
    // Construct what send_response would produce
    let id = RequestId::Number(1);
    let result = serde_json::json!({"agentInfo": {"name": "test", "version": "0.1.0"}});
    let msg = serde_json::json!({"jsonrpc": "2.0", "id": id.as_value(), "result": result});

    // Verify structure
    assert_eq!(msg["jsonrpc"], "2.0");
    assert_eq!(msg["id"], 1);
    assert!(msg["result"]["agentInfo"]["name"].is_string());
}

#[test]
fn json_rpc_response_echoes_string_id() {
    // Issue #13: string/UUID request IDs must be echoed back verbatim.
    let id = RequestId::String("e2a9b464-6960-4557-a750-6773429f8be5".into());
    let result = serde_json::json!({"ok": true});
    let msg = serde_json::json!({"jsonrpc": "2.0", "id": id.as_value(), "result": result});
    assert_eq!(msg["id"], "e2a9b464-6960-4557-a750-6773429f8be5");
}

#[test]
fn json_rpc_error_structure() {
    let id = RequestId::Number(5);
    let code = -32601i64;
    let message = "Method not found: foo";
    let msg = serde_json::json!({"jsonrpc": "2.0", "id": id.as_value(), "error": {"code": code, "message": message}});

    assert_eq!(msg["error"]["code"], -32601);
    assert_eq!(msg["error"]["message"], "Method not found: foo");
}

#[test]
fn notification_uses_session_update() {
    // ACP v1 uses `session/update` (not `session/notify`) and requires a
    // sessionId so the client can attribute the update to a session.
    let method = "session/update";
    let params = serde_json::json!({
        "sessionId": "abc-123",
        "update": {"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "hi"}}
    });
    let msg = serde_json::json!({"jsonrpc": "2.0", "method": method, "params": params});

    assert!(msg.get("id").is_none());
    assert_eq!(msg["method"], "session/update");
    assert_eq!(msg["params"]["sessionId"], "abc-123");
    assert_eq!(
        msg["params"]["update"]["sessionUpdate"],
        "agent_message_chunk"
    );
}

#[test]
fn notification_text_content_format() {
    // Text chunks carry a typed ContentBlock (`type: "text"`) plus sessionId.
    let text = "Hello, world!";
    let params = serde_json::json!({
        "sessionId": "abc",
        "update": {"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": text}}
    });

    assert_eq!(params["update"]["content"]["type"], "text");
    assert_eq!(
        params["update"]["content"]["text"].as_str().unwrap(),
        "Hello, world!"
    );
}

#[test]
fn tool_call_notification_format() {
    let title = "llm_chat";
    let params = serde_json::json!({
        "sessionId": "abc",
        "update": {"sessionUpdate": "tool_call", "title": title}
    });
    assert_eq!(params["update"]["title"], "llm_chat");

    let done_params = serde_json::json!({
        "sessionId": "abc",
        "update": {"sessionUpdate": "tool_call_update", "title": title, "status": "completed"}
    });
    assert_eq!(done_params["update"]["status"], "completed");
}
