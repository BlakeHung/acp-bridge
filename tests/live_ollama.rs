//! Live-backend E2E — drives a real acp-bridge process over stdio against
//! a real Ollama backend. Skipped unless `ACP_LIVE_BASE_URL` is set:
//!
//! ```text
//! ACP_LIVE_BASE_URL=http://192.168.88.19:8080 \
//! ACP_LIVE_MODEL=qwen36-35b:latest \
//! cargo test --test live_ollama -- --ignored --nocapture
//! ```
//!
//! This is the operator-facing validation for what the mock tests can only
//! approximate: native /api/chat streaming, real `agent_thought_chunk`
//! text, backend timing evidence, the persistent tool journal, image
//! forwarding, and the mid-turn steer queue — on the wire, end to end.

#[path = "clients/harness.rs"]
mod harness;

use harness::Agent;
use serde_json::{json, Value};
use std::time::Duration;

const LONG: Duration = Duration::from_secs(240);
const SHORT: Duration = Duration::from_secs(30);

fn live_env() -> Option<(String, String)> {
    let base = std::env::var("ACP_LIVE_BASE_URL").ok()?;
    let model = std::env::var("ACP_LIVE_MODEL").unwrap_or_else(|_| "qwen36-35b:latest".into());
    Some((base, model))
}

fn session_dir() -> String {
    let dir = std::env::temp_dir().join(format!("acp-live-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir.display().to_string()
}

fn spawn_live() -> Option<(Agent, String)> {
    let (base, model) = live_env()?;
    let sdir = session_dir();
    let agent = Agent::spawn(&[
        ("LLM_BASE_URL", base.as_str()),
        ("LLM_MODEL", model.as_str()),
        ("LLM_API_KEY", "ollama"),
        ("LLM_MODEL_CONTEXT", "32768"),
        ("LLM_SUPPORTS_IMAGE", "true"),
        ("LLM_POLICY_SHELL", "allow"),
        ("LLM_ARTIFACT_CHARS", "3072"),
        ("LLM_SESSION_DIR", sdir.as_str()),
        ("LLM_TIMEOUT", "14400"),
        ("LLM_MAX_TOOL_ROUNDS", "50"),
    ]);
    Some((agent, sdir))
}

fn open_session(agent: &mut Agent) -> String {
    agent.request(
        1,
        "initialize",
        json!({"protocolVersion": 1, "clientCapabilities": {}}),
    );
    let (n, r) = agent.recv_response(&json!(1), SHORT);
    assert!(
        r.get("error").is_none(),
        "initialize failed: {r} (notifs: {n:?})"
    );

    agent.request(2, "session/new", json!({"cwd": "/tmp"}));
    let (_n, r) = agent.recv_response(&json!(2), SHORT);
    r["result"]["sessionId"]
        .as_str()
        .expect("sessionId")
        .to_string()
}

fn notif_text(n: &Value) -> String {
    serde_json::to_string(n).unwrap_or_default()
}

#[test]
#[ignore]
fn live_native_stream_timing_journal_steer() {
    let Some((mut agent, sdir)) = spawn_live() else {
        eprintln!("ACP_LIVE_BASE_URL unset — skipping live test");
        return;
    };
    let sid = open_session(&mut agent);

    // ── T1: native streaming + backendTiming + backendModels ──────────
    agent.request(
        10,
        "session/prompt",
        json!({"sessionId": sid, "prompt": [{"type":"text","text":"Reply with exactly: OK"}]}),
    );
    let (notifs, resp) = agent.recv_response(&json!(10), LONG);
    assert!(resp.get("error").is_none(), "prompt failed: {resp}");
    let result = &resp["result"];
    assert_eq!(result["status"].as_str().unwrap_or(""), "completed");

    let chunks: Vec<&Value> = notifs
        .iter()
        .filter(|n| notif_text(n).contains("agent_message_chunk"))
        .collect();
    assert!(
        !chunks.is_empty(),
        "expected streamed agent_message_chunk notifications, got {notifs:?}"
    );
    let ev = &result["evidence"];
    assert!(
        ev["backendTiming"].is_object(),
        "native backend must report timing, evidence: {ev}"
    );
    assert!(
        ev["backendTiming"]["decodeMs"].as_f64().unwrap_or(0.0) > 0.0,
        "decode duration should be positive: {ev}"
    );
    assert!(
        ev["backendModels"]
            .as_array()
            .map(|m| !m.is_empty())
            .unwrap_or(false),
        "expected backendModels from /api/ps: {ev}"
    );
    let text = result["text"].as_str().unwrap_or("");
    eprintln!("T1 response text: {text} | evidence: {ev}");

    // ── T2: real tool call + journal records ─────────────────────────
    agent.request(
        20,
        "session/prompt",
        json!({"sessionId": sid, "prompt": [{"type":"text","text":
            "Use the list_dir tool to list the contents of /workspace. Then reply DONE."}]}),
    );
    let (notifs, resp) = agent.recv_response(&json!(20), LONG);
    assert!(resp.get("error").is_none(), "tool prompt failed: {resp}");
    let tool_wire = notif_text(&json!(notifs));
    assert!(
        tool_wire.contains("list_dir") || tool_wire.contains("tool_call"),
        "expected tool-call notifications on the wire: {tool_wire}"
    );
    // Journal: <sid>.tools.jsonl must exist with start+done pairs.
    let journal_path = std::path::Path::new(&sdir).join(format!("{sid}.tools.jsonl"));
    // The session save is async-ish — give the fs a moment.
    for _ in 0..10 {
        if journal_path.exists() {
            break;
        }
        std::thread::sleep(Duration::from_millis(300));
    }
    let journal = std::fs::read_to_string(&journal_path)
        .unwrap_or_else(|_| panic!("tool journal not written at {journal_path:?}"));
    assert!(
        journal.contains("\"k\":\"start\""),
        "journal lacks start: {journal}"
    );
    assert!(
        journal.contains("\"k\":\"done\""),
        "journal lacks done: {journal}"
    );
    eprintln!("T2 journal:\n{journal}");

    // ── T3: steer queue (received → queued/applied) ──────────────────
    // Kick off a turn that will take a while, then steer a second prompt.
    agent.request(
        30,
        "session/prompt",
        json!({"sessionId": sid, "prompt": [{"type":"text","text":
            "Count from 1 to 30, one number per line. After that, list the contents of /workspace using list_dir."}]}),
    );
    std::thread::sleep(Duration::from_secs(2)); // let the turn start
    agent.request(
        31,
        "session/prompt",
        json!({"sessionId": sid, "prompt": [{"type":"text","text":
            "Also tell me whether you saw the steer marker."}]}),
    );
    let (n30, r30) = agent.recv_response(&json!(30), LONG);
    let (n31, r31) = agent.recv_response(&json!(31), LONG);
    assert!(r30.get("error").is_none(), "first prompt failed: {r30}");
    assert!(r31.get("error").is_none(), "steered prompt failed: {r31}");
    let wire = format!("{}{}", notif_text(&json!(n30)), notif_text(&json!(n31)));
    assert!(
        wire.contains("steer"),
        "expected steer notifications on the wire"
    );
    eprintln!(
        "T3 steer outcome — r31 result: {}",
        serde_json::to_string_pretty(&r31["result"]).unwrap_or_default()
    );

    // ── T4: image prompt (vision path) ───────────────────────────────
    // 1x1 PNG — transport-level validation: the payload must reach the
    // backend (no "image content was removed" note) and the turn
    // completes rather than blowing up on the 4.8 MB+ base64 estimate.
    let png_1x1 = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==";
    agent.request(
        40,
        "session/prompt",
        json!({"sessionId": sid, "prompt": [
            {"type":"text","text":"I attached a tiny image. Just answer YES."},
            {"type":"image","data": png_1x1, "mimeType": "image/png"}
        ]}),
    );
    let (notifs, resp) = agent.recv_response(&json!(40), LONG);
    assert!(resp.get("error").is_none(), "image prompt failed: {resp}");
    let result = &resp["result"];
    assert_eq!(result["status"].as_str().unwrap_or(""), "completed");
    let all = format!(
        "{}{}",
        notif_text(&json!(notifs)),
        result["text"].as_str().unwrap_or("")
    );
    assert!(
        !all.contains("image content was removed"),
        "image was dropped instead of forwarded: {all}"
    );
    eprintln!(
        "T4 image response: {}",
        result["text"].as_str().unwrap_or("")
    );

    agent.shutdown();
}
