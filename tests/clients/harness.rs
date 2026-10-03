//! Shared harness for spawning acp-bridge as a subprocess and driving it
//! through real Client protocol sequences.
//!
//! These tests are the contract for what "compatible with the Client
//! ecosystem" means for acp-bridge. The harness spawns the binary in
//! `target/debug/acp-bridge` (built by `cargo build`), then drives it
//! line-by-line over stdin/stdout just like a real Client (Zed, ACP
//! Inspector, Codex CLI adapter) would.
//!
//! If you find a real Client whose expectations diverge from what these
//! tests assert, the test is wrong, not the Client — open an issue and we
//! will update both.

use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use serde_json::Value;

/// Path to the acp-bridge binary. Tests should run after `cargo build` so
/// this exists. CI runs `cargo build` before `cargo test` so the path is
/// reliable.
fn acp_bridge_bin() -> PathBuf {
    // CARGO_BIN_EXE_<name> is set by Cargo for integration tests in the
    // same crate; falls back to target/debug for `cargo test` invocations.
    if let Ok(p) = std::env::var("CARGO_BIN_EXE_acp-bridge") {
        return PathBuf::from(p);
    }
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest_dir.join("target").join("debug").join("acp-bridge")
}

/// A live acp-bridge subprocess with stdin/stdout handles.
pub struct Agent {
    /// Kept alive for the lifetime of the Agent so the OS reaps the
    /// child process if the test panics before calling `shutdown`.
    /// Some test binaries (notably `protocol_version`) do not exercise
    /// either path, so `child` is `allow(dead_code)`.
    #[allow(dead_code)]
    pub child: Child,
    pub stdin: ChildStdin,
    pub stdout_rx: mpsc::Receiver<Value>,
    /// Reader thread is joined via Drop via close.
    _reader: Option<thread::JoinHandle<()>>,
}

impl Agent {
    /// Spawn acp-bridge with a fresh isolated working directory and clean
    /// environment. Tests should not depend on the host LLM being up; the
    /// harness talks to acp-bridge over JSON-RPC, so any response is
    /// observable regardless of whether the backend chat call actually
    /// succeeds (we mostly do `initialize` + `session/new` + method probes
    /// that don't reach the LLM).
    pub fn spawn(extra_env: &[(&str, &str)]) -> Self {
        let mut cmd = Command::new(acp_bridge_bin());
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            // Inherit stderr so the harness never deadlocks on a full stderr
            // pipe, and so test failures show up in the test output.
            .stderr(Stdio::inherit())
            // Make sure we never hit a real LLM during tests.
            .env("LLM_BASE_URL", "http://127.0.0.1:1/v1")
            .env("LLM_MODEL", "test-model")
            .env("LLM_TIMEOUT", "5")
            // Silence the tracing subscriber so the inherited stderr line
            // discipline is preserved (tracing writes structured records; we
            // don't want to dump them into the test runner's stdout).
            .env("RUST_LOG", "off")
            .env_remove("LLM_SUPPORTS_IMAGE");
        for (k, v) in extra_env {
            cmd.env(k, v);
        }
        let mut child = cmd.spawn().expect("failed to spawn acp-bridge");
        let stdin = child.stdin.take().expect("stdin pipe");
        let stdout = child.stdout.take().expect("stdout pipe");

        let (tx, rx) = mpsc::channel::<Value>();
        let reader = thread::spawn(move || {
            let reader = BufReader::new(stdout);
            for line in reader.lines() {
                let Ok(line) = line else { break };
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }
                let Ok(v) = serde_json::from_str::<Value>(trimmed) else {
                    eprintln!("[harness] non-JSON line from agent: {trimmed}");
                    continue;
                };
                if tx.send(v).is_err() {
                    break;
                }
            }
        });

        Self {
            child,
            stdin,
            stdout_rx: rx,
            _reader: Some(reader),
        }
    }

    /// Send a JSON-RPC request and return the assigned id (caller-provided).
    pub fn request(&mut self, id: impl Into<Value>, method: &str, params: Value) {
        self.send_raw(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": id.into(),
            "method": method,
            "params": params,
        }));
    }

    /// Send a JSON-RPC notification (no id).
    #[allow(dead_code)]
    pub fn notify(&mut self, method: &str, params: Value) {
        self.send_raw(&serde_json::json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        }));
    }

    fn send_raw(&mut self, v: &Value) {
        let s = serde_json::to_string(v).expect("serialize request");
        writeln!(self.stdin, "{s}").expect("write request to agent stdin");
        self.stdin.flush().expect("flush agent stdin");
    }

    /// Receive the next message from the agent, blocking up to `timeout`.
    /// Returns `None` on timeout; returns `None` only if the timeout fires.
    pub fn recv(&self, timeout: Duration) -> Option<Value> {
        self.stdout_rx.recv_timeout(timeout).ok()
    }

    /// Receive messages until we see one whose `id` matches the target id
    /// (numeric OR string) and has no `method` (i.e. it's a response, not
    /// a notification). All notifications received before the response are
    /// returned alongside. Panics on timeout or stdout close.
    pub fn recv_response(&self, target_id: &Value, timeout: Duration) -> (Vec<Value>, Value) {
        let mut notifications = Vec::new();
        let deadline = std::time::Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                panic!("timed out waiting for response id={target_id}");
            }
            let Some(msg) = self.recv(remaining) else {
                panic!("agent stdout closed before response id={target_id}");
            };
            if msg.get("method").is_none() && msg.get("id") == Some(target_id) {
                return (notifications, msg);
            }
            notifications.push(msg);
        }
    }

    /// Gracefully shut down the agent. `allow(dead_code)` because some
    /// test binaries (notably `protocol_version`) do not exercise it.
    #[allow(dead_code)]
    pub fn shutdown(mut self) {
        // Closing stdin is the documented shutdown signal.
        let _ = self.stdin.flush();
        drop(self.stdin);
        // Give the agent a moment to exit cleanly.
        for _ in 0..20 {
            if matches!(self.child.try_wait(), Ok(Some(_))) {
                break;
            }
            thread::sleep(Duration::from_millis(50));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(h) = self._reader.take() {
            let _ = h.join();
        }
    }
}

/// Send a `session/prompt` and return (notifications, response).
#[allow(dead_code)]
pub fn prompt(agent: &mut Agent, id: i64, session_id: &str, prompt: Value) -> (Vec<Value>, Value) {
    let id_v = Value::from(id);
    agent.request(
        id_v.clone(),
        "session/prompt",
        serde_json::json!({
            "sessionId": session_id,
            "prompt": prompt,
        }),
    );
    agent.recv_response(&id_v, Duration::from_secs(30))
}
