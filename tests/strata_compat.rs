//! Strata (Niko1221/Strata) compatibility test.
//!
//! Strata is a consumer one-click local inference engine (Qwen3.8-Flash-Next
//! on Windows/Linux) serving an OpenAI-compatible `/v1` surface on
//! localhost. It is a candidate acp-bridge backend next to Ollama.
//!
//! These tests reproduce the two response shapes a thinking-mode Qwen
//! produces through that surface and verify acp-bridge dispatches both:
//!
//! 1. structured `tool_calls` *plus* a reasoning block in `content`
//!    (0.8.x path — reasoning stripped from displayed text, structured
//!    calls drive dispatch)
//! 2. no structured `tool_calls` at all — the tool call exists only as
//!    fenced JSON inside an *unterminated* think-tag block (0.9.2
//!    recovery path)

use axum::{extract::State, response::IntoResponse, routing::get, Json, Router};
use serde_json::{json, Value};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[path = "clients/harness.rs"]
mod harness;

use harness::{prompt, Agent};

const THINK_TAG: &str = "\u{3c}think\u{3e}";
const THINK_CLOSE: &str = "\u{3c}\u{2f}think\u{3e}";

#[derive(Default)]
struct MockState {
    chat_calls: AtomicUsize,
    /// messages array of the last `/v1/chat/completions` request.
    last_messages: Mutex<Option<Value>>,
    embed_only: AtomicBool,
}

fn round1_script(embed_only: bool) -> String {
    if embed_only {
        // unterminated think block + fenced tool call JSON in content
        format!(
            "{THINK_TAG}Reading the file now.\n```json\n\
             {{\"name\": \"read_file\", \"arguments\": {{\"path\": \"src/lib.rs\"}}}}\n```"
        )
    } else {
        // closed think block + structured tool call arrives separately
        format!(
            "{THINK_TAG}The user asked me to look at src/lib.rs. I'll read it.{THINK_CLOSE}\
             Read the file."
        )
    }
}

async fn strata_chat(
    State(state): State<Arc<MockState>>,
    Json(body): Json<Value>,
) -> impl IntoResponse {
    let n = state.chat_calls.fetch_add(1, Ordering::SeqCst) + 1;
    *state.last_messages.lock().unwrap() = Some(body["messages"].clone());

    let content = if n == 1 {
        round1_script(state.embed_only.load(Ordering::SeqCst))
    } else {
        "done".to_string()
    };
    let tool_calls = if n == 1 && !state.embed_only.load(Ordering::SeqCst) {
        json!([{
            "id": "call_strata_1",
            "type": "function",
            "function": {"name": "read_file", "arguments": "{\"path\": \"src/lib.rs\"}"}
        }])
    } else {
        Value::Null
    };

    Json(json!({
        "id": "chatcmpl-strata",
        "object": "chat.completion",
        "model": "Qwen3.8-Flash-Next",
        "choices": [{
            "index": 0,
            "finish_reason": if n == 1 { "tool_calls" } else { "stop" },
            "message": {"role": "assistant", "content": content, "tool_calls": tool_calls}
        }]
    }))
}

async fn start(port: u16, embed_only: bool) -> (Agent, Arc<MockState>) {
    let state = Arc::new(MockState {
        chat_calls: AtomicUsize::new(0),
        last_messages: Mutex::new(None),
        embed_only: AtomicBool::new(embed_only),
    });
    let app = Router::new()
        .route(
            "/v1/models",
            get(|| async {
                Json(json!({
                    "object": "list",
                    "data": [{"id": "Qwen3.8-Flash-Next", "object": "model"}]
                }))
            }),
        )
        .route("/v1/chat/completions", axum::routing::post(strata_chat))
        .with_state(state.clone());
    let listener = tokio::net::TcpListener::bind(format!("127.0.0.1:{port}"))
        .await
        .unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    tokio::time::sleep(Duration::from_millis(100)).await;

    let base: &'static str = Box::leak(format!("http://127.0.0.1:{port}/v1").into_boxed_str());
    let agent = Agent::spawn(&[
        ("LLM_BASE_URL", base),
        ("LLM_MODEL", "Qwen3.8-Flash-Next"),
        ("LLM_API_KEY", "strata-local"),
    ]);
    (agent, state)
}

fn prompt_block() -> Value {
    json!([{"type": "text", "text": "read src/lib.rs and summarize"}])
}

fn assert_tool_result_rounded(messages: &Value) {
    let msgs = messages.as_array().expect("messages array");
    let has_tool = msgs
        .iter()
        .any(|m| m["role"] == "tool" && m["tool_call_id"] == "call_strata_1");
    assert!(has_tool, "second round must carry the tool result");
}

fn assert_rounded_dispatch(agent: &mut Agent, state: &Arc<MockState>) {
    agent.request(
        1,
        "initialize",
        json!({"protocolVersion": 1, "clientCapabilities": {}}),
    );
    let (_nodes, _init) = agent.recv_response(&json!(1), Duration::from_secs(5));
    agent.request(2, "session/new", json!({}));
    let (_nodes, new) = agent.recv_response(&json!(2), Duration::from_secs(5));
    let session_id = new["result"]["sessionId"].as_str().unwrap().to_string();

    let (notifs, resp) = prompt(agent, 3, &session_id, prompt_block());
    assert_eq!(
        resp["result"]["stopReason"], "end_turn",
        "response = {resp}"
    );

    // a tool ran: both tool_call and tool_call_update surfaced
    assert!(
        notifs
            .iter()
            .any(|m| m["params"]["update"]["sessionUpdate"] == "tool_call"),
        "tool_call update missing; notifs = {notifs:?}"
    );
    assert!(
        notifs
            .iter()
            .any(|m| m["params"]["update"]["sessionUpdate"] == "tool_call_update"),
        "tool_call_update missing"
    );

    // final answer text is clean of reasoning scaffolding
    let final_text: String = notifs
        .iter()
        .filter(|m| m["params"]["update"]["sessionUpdate"] == "agent_message_chunk")
        .filter_map(|m| {
            m["params"]["update"]["content"]["text"]
                .as_str()
                .map(String::from)
        })
        .collect();
    assert!(
        !final_text.contains(THINK_TAG) && !final_text.contains("```json"),
        "reasoning scaffolding must not reach the Client: {final_text:?}"
    );

    // two rounds: tool dispatch + answer
    assert_eq!(
        state.chat_calls.load(Ordering::SeqCst),
        2,
        "expected tool round + answer round"
    );
    let messages = state.last_messages.lock().unwrap().clone().unwrap();
    assert_tool_result_rounded(&messages);
}

/// Shape 1 — Strata/Qwen returns structured tool_calls next to a think
/// block in content. Structured dispatch dominates; scaffolding stripped.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn strata_structured_tool_calls_with_reasoning_block() {
    let (mut agent, state) = start(18070, false).await;
    assert_rounded_dispatch(&mut agent, &state);
}

/// Shape 2 — Strata/Qwen emits the tool call only inside the content
/// channel (unterminated think block + fenced JSON). The 0.9.2 recovery
/// path must produce the same two-round outcome.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn strata_embedded_tool_call_recovery() {
    let (mut agent, state) = start(18071, true).await;
    assert_rounded_dispatch(&mut agent, &state);
}
