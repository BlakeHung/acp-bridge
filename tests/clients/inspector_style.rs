//! ACP Inspector-style e2e test.
//!
//! Reproduces the protocol sequence that
//! `newioapp/acp-inspector`'s `acp-connection-manager.ts` actually emits.
//! Source: line ~148 in that file.
//!
//! ACP Inspector is a debug-oriented tool: it advertises fewer capabilities
//! than Zed and probes capability flags aggressively after `initialize`
//! (it checks `loadSession`, `sessionCapabilities.list`, etc.). Our
//! harness makes those same probes and asserts acp-bridge's graceful
//! behavior.
//!
//! Also verifies request-id fuzzing: real Clients sometimes send numeric,
//! string, or UUID ids — acp-bridge must echo them verbatim.

#[path = "harness.rs"]
mod harness;

use harness::Agent;
use serde_json::{json, Value};
use std::time::Duration;

fn inspector_initialize_params() -> Value {
    json!({
        "protocolVersion": 1,
        "clientCapabilities": {
            "fs": {"readTextFile": true, "writeTextFile": true},
            "auth": {"terminal": true}
        },
        "clientInfo": {
            "name": "ACP Inspector",
            "version": "1.0.0"
        }
    })
}

#[test]
fn inspector_style_initialize_with_minimal_capabilities() {
    let mut a = Agent::spawn(&[]);
    a.request(1, "initialize", inspector_initialize_params());
    let (_, resp) = a.recv_response(&Value::from(1), Duration::from_secs(5));
    assert_eq!(resp["result"]["protocolVersion"], 1);
    a.shutdown();
}

#[test]
fn inspector_style_request_id_fuzz_numeric_string_uuid() {
    // Mirrors what the inspector's debug UI does: re-sends initialize
    // with different id types to confirm the agent echoes them.
    let mut a = Agent::spawn(&[]);
    let init = inspector_initialize_params();

    let cases: Vec<Value> = vec![
        json!(1),
        json!("session-list-42"),
        json!("e2a9b464-1c9b-4d6f-9c8a-7f5b3e1d2c4a"),
    ];

    for id_val in cases {
        a.request(id_val.clone(), "initialize", init.clone());
        let (_, resp) = a.recv_response(&id_val, Duration::from_secs(5));
        assert_eq!(
            resp["id"], id_val,
            "id must be echoed verbatim (sent {id_val}, got {})",
            resp["id"]
        );
    }
    a.shutdown();
}

#[test]
fn inspector_style_session_list_returns_empty_array_with_no_sessions() {
    // ACP Inspector probes `session/list` capability before calling it.
    // As of v0.9.0, acp-bridge implements `session/list` (v2 baseline).
    // With no sessions open, the call succeeds and returns an empty
    // list. Clients must tolerate this — `sessions: []` is the
    // documented wire shape.
    let mut a = Agent::spawn(&[]);
    a.request(1, "initialize", inspector_initialize_params());
    let _ = a.recv_response(&Value::from(1), Duration::from_secs(5));

    a.request(2, "session/list", json!({}));
    let (_, resp) = a.recv_response(&Value::from(2), Duration::from_secs(5));
    assert!(
        resp.get("result").is_some(),
        "session/list must succeed: {resp}"
    );
    assert!(
        resp["result"]["sessions"].is_array(),
        "sessions must be an array: {resp}"
    );
    assert_eq!(resp["result"]["sessions"].as_array().unwrap().len(), 0);
    assert!(resp["result"]["nextCursor"].is_null());
    a.shutdown();
}

#[test]
fn inspector_style_session_close_succeeds_and_variants() {
    // As of v0.9.0, `session/close` is implemented (and `session/end` is
    // a v1 alias). When called against a non-existent session, the
    // error is `-32001` (application error, UnknownSession), not the
    // generic `-32601` MethodNotFound.
    let mut a = Agent::spawn(&[]);
    a.request(1, "initialize", inspector_initialize_params());
    let _ = a.recv_response(&Value::from(1), Duration::from_secs(5));

    a.request(2, "session/close", json!({"sessionId": "no-such-session"}));
    let (_, resp) = a.recv_response(&Value::from(2), Duration::from_secs(5));
    assert!(
        resp.get("error").is_some(),
        "session/close on unknown session must return an error, got: {resp}"
    );
    assert_ne!(
        resp["error"]["code"], -32601,
        "session/close is implemented; should not return MethodNotFound"
    );
    a.shutdown();
}

#[test]
fn inspector_style_session_prompt_accepts_resource_link_content_block() {
    // ACP Inspector sometimes wraps the prompt with a ContentBlock::ResourceLink
    // (e.g. when attaching a file via the "Attach" button). Per spec, acp-bridge
    // MUST accept ContentBlock::Text + ContentBlock::ResourceLink; we should
    // not crash and should extract the text via the registered extractor.
    let mut a = Agent::spawn(&[]);
    a.request(1, "initialize", inspector_initialize_params());
    let _ = a.recv_response(&Value::from(1), Duration::from_secs(5));

    a.request(2, "session/new", json!({"cwd": "/tmp"}));
    let (_, resp) = a.recv_response(&Value::from(2), Duration::from_secs(5));
    let sid = resp["result"]["sessionId"].as_str().unwrap().to_string();

    // Mix a text block with a ResourceLink — text-only extractor should pick
    // up the text and ignore the resource link (or fail gracefully if no text).
    a.request(
        3,
        "session/prompt",
        json!({
            "sessionId": sid,
            "prompt": [
                {"type": "text", "text": "describe this file"},
                {
                    "type": "resource_link",
                    "uri": "file:///etc/hostname",
                    "name": "hostname",
                    "mimeType": null
                }
            ]
        }),
    );
    let (_, resp) = a.recv_response(&Value::from(3), Duration::from_secs(10));
    // Should not crash; response carries stopReason.
    let stop_reason = resp["result"]["stopReason"].as_str();
    assert!(
        matches!(stop_reason, Some("end_turn") | Some("max_turn_requests")),
        "expected valid stopReason, got: {stop_reason:?}"
    );
    a.shutdown();
}

#[test]
fn inspector_style_session_prompt_pure_resource_link_succeeds() {
    // ACP v1 requires Agents to accept prompts whose only content block is
    // a ResourceLink (no text). acp-bridge turns the link into a
    // `[Attached resource: ...]` pseudo-line so the LLM still has
    // something to act on.
    let mut a = Agent::spawn(&[]);
    a.request(1, "initialize", inspector_initialize_params());
    let _ = a.recv_response(&Value::from(1), Duration::from_secs(5));

    a.request(2, "session/new", json!({"cwd": "/tmp"}));
    let (_, resp) = a.recv_response(&Value::from(2), Duration::from_secs(5));
    let sid = resp["result"]["sessionId"].as_str().unwrap().to_string();

    a.request(
        3,
        "session/prompt",
        json!({
            "sessionId": sid,
            "prompt": [{
                "type": "resource_link",
                "uri": "file:///etc/hostname",
                "name": "hostname"
            }]
        }),
    );
    let (_, resp) = a.recv_response(&Value::from(3), Duration::from_secs(10));
    let stop_reason = resp["result"]["stopReason"].as_str();
    assert!(
        matches!(stop_reason, Some("end_turn") | Some("max_turn_requests")),
        "expected valid stopReason, got: {stop_reason:?}"
    );
    // The text was non-empty (the pseudo-line), so we must NOT have hit
    // the `MissingParam` error path.
    assert!(resp.get("error").is_none(), "unexpected error: {resp}");
    a.shutdown();
}
