use acp_bridge::protocol::{AcpError, JsonRpcRequest, Session};
use serde_json::json;

// ---------------------------------------------------------------------------
// JsonRpcRequest parsing
// ---------------------------------------------------------------------------

#[test]
fn parse_initialize_request() {
    let input = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#;
    let req: JsonRpcRequest = serde_json::from_str(input).unwrap();
    assert_eq!(req.id, 1);
    assert_eq!(req.method, "initialize");
    assert!(req.params.is_some());
}

#[test]
fn parse_request_without_params() {
    let input = r#"{"jsonrpc":"2.0","id":42,"method":"session/end"}"#;
    let req: JsonRpcRequest = serde_json::from_str(input).unwrap();
    assert_eq!(req.id, 42);
    assert_eq!(req.method, "session/end");
    assert!(req.params.is_none());
}

#[test]
fn parse_session_prompt_request() {
    let input = r#"{"jsonrpc":"2.0","id":3,"method":"session/prompt","params":{"sessionId":"abc","prompt":[{"type":"text","text":"hello"}]}}"#;
    let req: JsonRpcRequest = serde_json::from_str(input).unwrap();
    assert_eq!(req.method, "session/prompt");
    let params = req.params.unwrap();
    assert_eq!(params["sessionId"], "abc");
}

// ---------------------------------------------------------------------------
// Session history trimming
// ---------------------------------------------------------------------------

fn make_session(messages: Vec<serde_json::Value>) -> Session {
    let mut session = Session::new(messages[0].clone());
    for msg in &messages[1..] {
        session.messages.push(msg.clone());
    }
    session
}

#[test]
fn trim_history_no_op_when_under_limit() {
    let mut session = make_session(vec![
        json!({"role": "system", "content": "sys"}),
        json!({"role": "user", "content": "hi"}),
        json!({"role": "assistant", "content": "hello"}),
    ]);
    session.trim_history(5); // max 5 turns = 10 messages + system
    assert_eq!(session.messages.len(), 3);
}

#[test]
fn trim_history_keeps_system_and_last_n_turns() {
    let mut session = make_session(vec![
        json!({"role": "system", "content": "sys"}),
        // Turn 1
        json!({"role": "user", "content": "q1"}),
        json!({"role": "assistant", "content": "a1"}),
        // Turn 2
        json!({"role": "user", "content": "q2"}),
        json!({"role": "assistant", "content": "a2"}),
        // Turn 3
        json!({"role": "user", "content": "q3"}),
        json!({"role": "assistant", "content": "a3"}),
    ]);
    session.trim_history(2); // keep last 2 turns

    assert_eq!(session.messages.len(), 5); // system + 2 turns
    assert_eq!(session.messages[0]["role"], "system");
    assert_eq!(session.messages[1]["content"], "q2");
    assert_eq!(session.messages[2]["content"], "a2");
    assert_eq!(session.messages[3]["content"], "q3");
    assert_eq!(session.messages[4]["content"], "a3");
}

#[test]
fn trim_history_exactly_at_limit() {
    let mut session = make_session(vec![
        json!({"role": "system", "content": "sys"}),
        json!({"role": "user", "content": "q1"}),
        json!({"role": "assistant", "content": "a1"}),
    ]);
    session.trim_history(1); // max 1 turn = exactly what we have
    assert_eq!(session.messages.len(), 3);
}

#[test]
fn trim_history_large_conversation() {
    let mut messages = vec![json!({"role": "system", "content": "sys"})];
    for i in 0..100 {
        messages.push(json!({"role": "user", "content": format!("q{i}")}));
        messages.push(json!({"role": "assistant", "content": format!("a{i}")}));
    }
    let mut session = make_session(messages);
    session.trim_history(10);

    // system + 10 turns (20 messages) = 21
    assert_eq!(session.messages.len(), 21);
    assert_eq!(session.messages[0]["role"], "system");
    // Last turn should be q99/a99
    assert_eq!(session.messages[19]["content"], "q99");
    assert_eq!(session.messages[20]["content"], "a99");
}

// ---------------------------------------------------------------------------
// Session touch
// ---------------------------------------------------------------------------

#[test]
fn session_touch_updates_last_active() {
    let session = Session::new(json!({"role": "system", "content": "sys"}));
    let first = session.last_active;
    std::thread::sleep(std::time::Duration::from_millis(10));
    let mut session = session;
    session.touch();
    assert!(session.last_active > first);
}

// ---------------------------------------------------------------------------
// AcpError
// ---------------------------------------------------------------------------

#[test]
fn acp_error_codes() {
    let missing = AcpError::MissingParam {
        field: "sessionId".into(),
    };
    assert_eq!(missing.code(), -32602);
    assert!(missing.to_string().contains("sessionId"));

    let unknown = AcpError::UnknownSession {
        session_id: "abc".into(),
    };
    assert_eq!(unknown.code(), -32001);

    let not_found = AcpError::MethodNotFound {
        method: "foo".into(),
    };
    assert_eq!(not_found.code(), -32601);

    let llm = AcpError::LlmError {
        reason: "timeout".into(),
    };
    assert_eq!(llm.code(), -32003);

    let limit = AcpError::SessionLimitReached { max: 10 };
    assert_eq!(limit.code(), -32004);
    assert!(limit.to_string().contains("10"));
}
