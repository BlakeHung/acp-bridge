//! MCP client — relays the Client's `mcpServers` into the agent loop.
//!
//! ACP Clients pass MCP server configs in `session/new`. Xcode uses this to
//! hand the agent its own tool server (`xcrun mcpbridge`, "xcode-tools"),
//! which builds, tests, runs, renders previews, reads compiler diagnostics,
//! searches Apple docs and edits the project structure. acp-bridge spawns
//! each stdio server, lists its tools, exposes them to the model next to the
//! built-in tools, and routes the model's calls back to the server.
//!
//! Only stdio transport is implemented (the ACP baseline every agent must
//! support); `http` / `sse` entries are skipped and not advertised.
//!
//! No network is involved: a stdio MCP server is a local child process the
//! Client asked for. The relay can be turned off with `LLM_MCP=0`.

use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, oneshot, OnceCell};
use tracing::{debug, info, warn};

/// MCP protocol revision requested in `initialize`.
const MCP_PROTOCOL_VERSION: &str = "2025-06-18";
/// Default max time for a server to answer `initialize` / `tools/list`.
/// Xcode's bridge, bound to a running Xcode via `MCP_XCODE_PID`, has been
/// seen to take 15-40 s for its first `tools/list`.
pub const DEFAULT_CONNECT_TIMEOUT_SECS: u64 = 120;
/// Default timeout for one `tools/call` (builds and test runs are slow).
pub const DEFAULT_TOOL_TIMEOUT_SECS: u64 = 600;
/// Max characters of one tool result handed to the model.
const MAX_RESULT_CHARS: usize = 50_000;
/// Share of the model context the tool schemas of one Xcode server may use
/// before acp-bridge falls back to [`XCODE_CORE_TOOLS`].
const XCODE_SCHEMA_CONTEXT_SHARE: u64 = 4; // 1/4

/// Tool names acp-bridge itself defines; MCP tools that collide get a
/// `<server>__` prefix.
const BUILTIN_TOOL_NAMES: &[&str] = &[
    "read_file",
    "list_dir",
    "search_code",
    "bash",
    "write_file",
    "edit",
    "web_fetch",
    "git_status",
    "git_diff",
    "git_log",
    "git_commit",
];

/// Whether `name` is one of acp-bridge's own tools.
pub fn is_builtin_tool(name: &str) -> bool {
    BUILTIN_TOOL_NAMES.contains(&name)
}

/// The Xcode tools that matter for an edit / build / test loop. Used when
/// the full Xcode tool set would take too much of a small context window
/// (and no explicit `LLM_MCP_TOOLS` list is configured). File reading and
/// search are covered by the built-in tools.
pub const XCODE_CORE_TOOLS: &[&str] = &[
    "BuildProject",
    "GetBuildLog",
    "XcodeRefreshCodeIssuesInFile",
    "GetTestList",
    "RunSomeTests",
    "RunAllTests",
    "RunProject",
    "StopProject",
    "GetConsoleOutput",
    "DocumentationSearch",
    "XcodeListWorkspaces",
    "XcodeOpenWorkspace",
    "XcodeLS",
    "XcodeWrite",
    "XcodeMV",
    "XcodeRM",
    "XcodeMakeDir",
    "AddInfoPlist",
    "AddEntitlement",
    "GetTargetBuildSettings",
    "XcodeListSchemes",
    "XcodeSwitchScheme",
];

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// When to spawn Xcode's tool server if the Client did not pass one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum XcodeMode {
    /// Only when acp-bridge runs under Xcode (default).
    Auto,
    /// Whenever `xcrun mcpbridge` is available.
    On,
    /// Never add it (Client-provided servers are still honoured).
    Off,
}

impl XcodeMode {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "auto" | "" => Some(Self::Auto),
            "1" | "true" | "yes" | "on" | "always" => Some(Self::On),
            "0" | "false" | "no" | "off" | "never" => Some(Self::Off),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct McpConfig {
    /// Honour `mcpServers` from `session/new` (`LLM_MCP`, default on).
    pub enabled: bool,
    /// Spawn `xcrun mcpbridge` when missing (`LLM_XCODE_TOOLS`).
    pub xcode: XcodeMode,
    /// Only expose these MCP tools (`LLM_MCP_TOOLS`, `*` wildcard; empty = all).
    pub include: Vec<String>,
    /// Never expose these MCP tools (`LLM_MCP_EXCLUDE_TOOLS`).
    pub exclude: Vec<String>,
    /// Timeout for one tool call, seconds (`LLM_MCP_TOOL_TIMEOUT`, 0 = none).
    pub tool_timeout_secs: u64,
    /// Max time for `initialize` / `tools/list`, seconds
    /// (`LLM_MCP_CONNECT_TIMEOUT`, 0 = none).
    pub connect_timeout_secs: u64,
}

impl Default for McpConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            xcode: XcodeMode::Auto,
            include: Vec::new(),
            exclude: Vec::new(),
            tool_timeout_secs: DEFAULT_TOOL_TIMEOUT_SECS,
            connect_timeout_secs: DEFAULT_CONNECT_TIMEOUT_SECS,
        }
    }
}

/// Split a comma/whitespace separated list.
pub fn parse_list(s: &str) -> Vec<String> {
    s.split(|c: char| c == ',' || c.is_whitespace())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

impl McpConfig {
    pub fn tool_timeout(&self) -> Option<Duration> {
        (self.tool_timeout_secs > 0).then(|| Duration::from_secs(self.tool_timeout_secs))
    }

    fn connect_timeout(&self) -> Option<Duration> {
        (self.connect_timeout_secs > 0).then(|| Duration::from_secs(self.connect_timeout_secs))
    }

    fn allows(&self, tool: &str) -> bool {
        (self.include.is_empty() || self.include.iter().any(|p| glob_match(p, tool)))
            && !self.exclude.iter().any(|p| glob_match(p, tool))
    }
}

/// Minimal `*` wildcard match (case-sensitive).
fn glob_match(pattern: &str, text: &str) -> bool {
    let parts: Vec<&str> = pattern.split('*').collect();
    if parts.len() == 1 {
        return pattern == text;
    }
    let mut rest = text;
    for (i, part) in parts.iter().enumerate() {
        if i == 0 {
            match rest.strip_prefix(part) {
                Some(r) => rest = r,
                None => return false,
            }
        } else if i == parts.len() - 1 {
            return rest.ends_with(part);
        } else if let Some(pos) = rest.find(part) {
            rest = &rest[pos + part.len()..];
        } else {
            return false;
        }
    }
    true
}

// ---------------------------------------------------------------------------
// Server specs (ACP `McpServer`)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub struct ServerSpec {
    pub name: String,
    pub command: String,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
}

impl ServerSpec {
    /// Xcode's tool server, launched through `xcrun`.
    pub fn xcode() -> Self {
        Self {
            name: "xcode-tools".into(),
            command: "xcrun".into(),
            args: vec!["mcpbridge".into()],
            env: Vec::new(),
        }
    }

    /// Whether this spec launches Xcode's tool server.
    pub fn is_xcode(&self) -> bool {
        self.command.ends_with("mcpbridge")
            || self.args.iter().any(|a| a == "mcpbridge")
            || self.name.to_ascii_lowercase().contains("xcode")
    }
}

/// Parse the ACP `mcpServers` array. Returns the stdio servers plus a
/// description of every entry that was skipped.
pub fn parse_servers(value: Option<&Value>) -> (Vec<ServerSpec>, Vec<String>) {
    let mut specs = Vec::new();
    let mut skipped = Vec::new();
    let Some(arr) = value.and_then(Value::as_array) else {
        return (specs, skipped);
    };
    for (i, s) in arr.iter().enumerate() {
        let name = s
            .get("name")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| format!("mcp{i}"));
        let kind = s.get("type").and_then(Value::as_str).unwrap_or("stdio");
        if kind != "stdio" {
            skipped.push(format!("{name} ({kind} transport not supported)"));
            continue;
        }
        let Some(command) = s
            .get("command")
            .and_then(Value::as_str)
            .filter(|c| !c.is_empty())
        else {
            skipped.push(format!("{name} (no command)"));
            continue;
        };
        let args = s
            .get("args")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        // ACP sends `env` as `[{name, value}]`; accept a plain object too.
        let env = match s.get("env") {
            Some(Value::Array(vars)) => vars
                .iter()
                .filter_map(|v| {
                    Some((
                        v.get("name")?.as_str()?.to_string(),
                        v.get("value")?.as_str()?.to_string(),
                    ))
                })
                .collect(),
            Some(Value::Object(map)) => map
                .iter()
                .filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_string())))
                .collect(),
            _ => Vec::new(),
        };
        specs.push(ServerSpec {
            name,
            command: command.to_string(),
            args,
            env,
        });
    }
    (specs, skipped)
}

/// Whether acp-bridge was launched by Xcode: the Client identified itself
/// as Xcode at `initialize`, or the process inherited Xcode's bundle id.
pub fn running_under_xcode(client_name: Option<&str>) -> bool {
    client_name.is_some_and(|n| n.to_ascii_lowercase().contains("xcode"))
        || std::env::var("__CFBundleIdentifier").is_ok_and(|b| b == "com.apple.dt.Xcode")
}

fn xcrun_available() -> bool {
    cfg!(target_os = "macos") && Path::new("/usr/bin/xcrun").exists()
}

/// Final list of servers to start for a session: the Client's stdio
/// servers (if the relay is enabled) plus Xcode's tool server when
/// [`XcodeMode`] asks for it and none of the Client's servers is Xcode's.
pub fn resolve_servers(
    cfg: &McpConfig,
    client_servers: Option<&Value>,
    client_name: Option<&str>,
) -> Vec<ServerSpec> {
    let mut specs = Vec::new();
    if cfg.enabled {
        let (parsed, skipped) = parse_servers(client_servers);
        for s in &parsed {
            info!(server = %s.name, command = %s.command, args = ?s.args, "Client MCP server");
        }
        for s in &skipped {
            warn!(server = %s, "Skipping client MCP server");
        }
        specs = parsed;
    } else if client_servers
        .and_then(Value::as_array)
        .is_some_and(|a| !a.is_empty())
    {
        info!("Ignoring client mcpServers (LLM_MCP=0)");
    }
    let want_xcode = match cfg.xcode {
        XcodeMode::Off => false,
        XcodeMode::On => true,
        XcodeMode::Auto => running_under_xcode(client_name),
    };
    if want_xcode && !specs.iter().any(ServerSpec::is_xcode) {
        if xcrun_available() {
            info!("Adding Xcode tool server (xcrun mcpbridge)");
            specs.push(ServerSpec::xcode());
        } else {
            debug!("Xcode tools wanted but xcrun is not available");
        }
    }
    specs
}

// ---------------------------------------------------------------------------
// Connection — JSON-RPC over a child's stdin/stdout
// ---------------------------------------------------------------------------

type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value, String>>>>>;

struct Connection {
    tx: mpsc::UnboundedSender<String>,
    pending: Pending,
    next_id: AtomicU64,
    tools_changed: Arc<AtomicBool>,
    // Held so the child is killed (kill_on_drop) when the session goes away.
    _child: Mutex<tokio::process::Child>,
}

/// Removes a pending request and tells the server to stop working on it
/// when a call is abandoned (timeout or turn cancelled).
struct PendingGuard<'a> {
    conn: &'a Connection,
    id: u64,
    done: bool,
}

impl Drop for PendingGuard<'_> {
    fn drop(&mut self) {
        if self.done {
            return;
        }
        self.conn
            .pending
            .lock()
            .expect("mcp pending lock")
            .remove(&self.id);
        self.conn.notify(
            "notifications/cancelled",
            json!({"requestId": self.id, "reason": "cancelled by acp-bridge"}),
        );
    }
}

impl Connection {
    fn spawn(spec: &ServerSpec, cwd: &Path) -> Result<Self, String> {
        let mut cmd = tokio::process::Command::new(&spec.command);
        cmd.args(&spec.args)
            .envs(spec.env.iter().map(|(k, v)| (k, v)))
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        if cwd.is_dir() {
            cmd.current_dir(cwd);
        }
        let mut child = cmd
            .spawn()
            .map_err(|e| format!("failed to start `{}`: {e}", spec.command))?;
        let mut stdin = child.stdin.take().ok_or("no stdin")?;
        let stdout = child.stdout.take().ok_or("no stdout")?;
        let stderr = child.stderr.take();

        let (tx, mut rx) = mpsc::unbounded_channel::<String>();
        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let tools_changed = Arc::new(AtomicBool::new(false));

        // Writer: one JSON message per line.
        tokio::spawn(async move {
            while let Some(line) = rx.recv().await {
                if stdin.write_all(line.as_bytes()).await.is_err()
                    || stdin.write_all(b"\n").await.is_err()
                    || stdin.flush().await.is_err()
                {
                    break;
                }
            }
        });

        // Reader: route responses, answer server requests, watch notifications.
        {
            let pending = Arc::clone(&pending);
            let changed = Arc::clone(&tools_changed);
            let reply_tx = tx.clone();
            let roots = json!({"roots": [{
                "uri": format!("file://{}", cwd.display()),
                "name": cwd.file_name().and_then(|n| n.to_str()).unwrap_or("workspace"),
            }]});
            let server = spec.name.clone();
            tokio::spawn(async move {
                let mut lines = BufReader::new(stdout).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    let Ok(msg) = serde_json::from_str::<Value>(&line) else {
                        debug!(server = %server, line = %line, "Non-JSON line from MCP server");
                        continue;
                    };
                    handle_incoming(&msg, &pending, &changed, &reply_tx, &roots);
                }
                debug!(server = %server, "MCP server closed stdout");
                for (_, waiter) in pending.lock().expect("mcp pending lock").drain() {
                    let _ = waiter.send(Err("MCP server exited".into()));
                }
            });
        }

        if let Some(stderr) = stderr {
            let server = spec.name.clone();
            tokio::spawn(async move {
                let mut lines = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    debug!(server = %server, "mcp stderr: {line}");
                }
            });
        }

        Ok(Self {
            tx,
            pending,
            next_id: AtomicU64::new(1),
            tools_changed,
            _child: Mutex::new(child),
        })
    }

    fn notify(&self, method: &str, params: Value) {
        let msg = json!({"jsonrpc": "2.0", "method": method, "params": params});
        let _ = self.tx.send(msg.to_string());
    }

    async fn request(
        &self,
        method: &str,
        params: Value,
        timeout: Option<Duration>,
    ) -> Result<Value, String> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (waiter_tx, waiter_rx) = oneshot::channel();
        self.pending
            .lock()
            .expect("mcp pending lock")
            .insert(id, waiter_tx);
        let mut guard = PendingGuard {
            conn: self,
            id,
            done: false,
        };

        let msg = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        if self.tx.send(msg.to_string()).is_err() {
            return Err("MCP server is not running".into());
        }
        let result = match timeout {
            Some(t) => match tokio::time::timeout(t, waiter_rx).await {
                Ok(r) => r,
                Err(_) => {
                    return Err(format!("{method} timed out after {}s", t.as_secs()));
                }
            },
            None => waiter_rx.await,
        };
        guard.done = true;
        result.unwrap_or_else(|_| Err("MCP server exited".into()))
    }
}

fn handle_incoming(
    msg: &Value,
    pending: &Pending,
    tools_changed: &AtomicBool,
    reply_tx: &mpsc::UnboundedSender<String>,
    roots: &Value,
) {
    let method = msg.get("method").and_then(Value::as_str);
    let id = msg.get("id");
    match (method, id) {
        // Response to one of our requests.
        (None, Some(id)) => {
            let Some(id) = id.as_u64() else { return };
            let Some(waiter) = pending.lock().expect("mcp pending lock").remove(&id) else {
                return;
            };
            let outcome = if let Some(err) = msg.get("error") {
                Err(err
                    .get("message")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .unwrap_or_else(|| err.to_string()))
            } else {
                Ok(msg.get("result").cloned().unwrap_or(Value::Null))
            };
            let _ = waiter.send(outcome);
        }
        // Request from the server.
        (Some(method), Some(id)) => {
            let reply = match method {
                "ping" => json!({"jsonrpc": "2.0", "id": id, "result": {}}),
                "roots/list" => json!({"jsonrpc": "2.0", "id": id, "result": roots}),
                _ => json!({"jsonrpc": "2.0", "id": id, "error": {
                    "code": -32601, "message": format!("Method not found: {method}")
                }}),
            };
            let _ = reply_tx.send(reply.to_string());
        }
        // Notification from the server.
        (Some("notifications/tools/list_changed"), None) => {
            tools_changed.store(true, Ordering::Relaxed);
        }
        _ => {}
    }
}

// ---------------------------------------------------------------------------
// Server — one connected MCP server and its tools
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct McpTool {
    name: String,
    description: String,
    input_schema: Value,
}

struct McpServer {
    spec: ServerSpec,
    /// `serverInfo.name` from `initialize` (Xcode reports "xcode-tools").
    server_name: String,
    instructions: Option<String>,
    is_xcode: bool,
    conn: Connection,
    tools: RwLock<Vec<McpTool>>,
}

impl McpServer {
    async fn connect(
        spec: &ServerSpec,
        cwd: &Path,
        cfg: &McpConfig,
        context_size: u64,
    ) -> Result<Self, String> {
        let conn = Connection::spawn(spec, cwd)?;
        let init = conn
            .request(
                "initialize",
                json!({
                    "protocolVersion": MCP_PROTOCOL_VERSION,
                    "capabilities": {"roots": {"listChanged": false}},
                    "clientInfo": {"name": "acp-bridge", "version": env!("CARGO_PKG_VERSION")},
                }),
                cfg.connect_timeout(),
            )
            .await?;
        conn.notify("notifications/initialized", json!({}));

        let server_name = init
            .pointer("/serverInfo/name")
            .and_then(Value::as_str)
            .unwrap_or(&spec.name)
            .to_string();
        let instructions = init
            .get("instructions")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        let is_xcode = spec.is_xcode() || server_name == "xcode-tools";

        let server = Self {
            spec: spec.clone(),
            server_name,
            instructions,
            is_xcode,
            conn,
            tools: RwLock::new(Vec::new()),
        };
        server.refresh_tools(cfg, context_size).await?;
        Ok(server)
    }

    async fn refresh_tools(&self, cfg: &McpConfig, context_size: u64) -> Result<(), String> {
        self.conn.tools_changed.store(false, Ordering::Relaxed);
        let mut all = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let params = match &cursor {
                Some(c) => json!({"cursor": c}),
                None => json!({}),
            };
            let page = self
                .conn
                .request("tools/list", params, cfg.connect_timeout())
                .await?;
            for t in page
                .get("tools")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                let Some(name) = t.get("name").and_then(Value::as_str) else {
                    continue;
                };
                let mut schema = t
                    .get("inputSchema")
                    .cloned()
                    .unwrap_or_else(|| json!({"type": "object", "properties": {}}));
                // Some OpenAI-compatible servers reject object schemas
                // without `properties`.
                if schema.get("properties").is_none() {
                    schema["properties"] = json!({});
                }
                all.push(McpTool {
                    name: name.to_string(),
                    description: t
                        .get("description")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .trim()
                        .to_string(),
                    input_schema: schema,
                });
            }
            cursor = page
                .get("nextCursor")
                .and_then(Value::as_str)
                .filter(|c| !c.is_empty())
                .map(str::to_string);
            if cursor.is_none() {
                break;
            }
        }
        let total = all.len();
        let selected = select_tools(all, cfg, self.is_xcode, context_size);
        info!(
            server = %self.spec.name,
            exposed = selected.len(),
            total,
            "MCP tools loaded"
        );
        *self.tools.write().expect("mcp tools lock") = selected;
        Ok(())
    }

    async fn call(&self, tool: &str, args: &Value, timeout: Option<Duration>) -> String {
        let args = if args.is_object() {
            args.clone()
        } else {
            json!({})
        };
        match self
            .conn
            .request(
                "tools/call",
                json!({"name": tool, "arguments": args}),
                timeout,
            )
            .await
        {
            Ok(result) => format_tool_result(&result),
            Err(e) => format!("Error: {e}"),
        }
    }
}

/// Apply include/exclude filters; for Xcode, fall back to the core tool set
/// when the full schemas would take more than 1/4 of the model context.
fn select_tools(
    all: Vec<McpTool>,
    cfg: &McpConfig,
    is_xcode: bool,
    context_size: u64,
) -> Vec<McpTool> {
    let filtered: Vec<McpTool> = all.into_iter().filter(|t| cfg.allows(&t.name)).collect();
    if !is_xcode || !cfg.include.is_empty() {
        return filtered;
    }
    let tokens: u64 = filtered.iter().map(schema_tokens).sum();
    let limit = context_size / XCODE_SCHEMA_CONTEXT_SHARE;
    if tokens <= limit {
        return filtered;
    }
    let core: Vec<McpTool> = filtered
        .into_iter()
        .filter(|t| XCODE_CORE_TOOLS.contains(&t.name.as_str()))
        .collect();
    warn!(
        schema_tokens = tokens,
        context_size,
        exposed = core.len(),
        "Full Xcode tool set is too large for the model context; exposing core tools only \
         (raise LLM_MODEL_CONTEXT or set LLM_MCP_TOOLS to choose)"
    );
    core
}

fn schema_tokens(t: &McpTool) -> u64 {
    ((t.name.len() + t.description.len() + t.input_schema.to_string().len()) / 4) as u64
}

/// Render a `tools/call` result as text for the model.
pub fn format_tool_result(result: &Value) -> String {
    let mut parts: Vec<String> = Vec::new();
    for block in result
        .get("content")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        match block.get("type").and_then(Value::as_str) {
            Some("text") => {
                if let Some(t) = block.get("text").and_then(Value::as_str) {
                    parts.push(t.to_string());
                }
            }
            Some("image") | Some("audio") => {
                let kind = block["type"].as_str().unwrap_or("media");
                let mime = block.get("mimeType").and_then(Value::as_str).unwrap_or("?");
                parts.push(format!("[{kind} content ({mime}) omitted]"));
            }
            Some("resource") => {
                let r = &block["resource"];
                if let Some(t) = r.get("text").and_then(Value::as_str) {
                    parts.push(t.to_string());
                } else if let Some(uri) = r.get("uri").and_then(Value::as_str) {
                    parts.push(format!("[resource: {uri}]"));
                }
            }
            Some("resource_link") => {
                if let Some(uri) = block.get("uri").and_then(Value::as_str) {
                    parts.push(format!("[resource: {uri}]"));
                }
            }
            _ => {}
        }
    }
    if parts.is_empty() {
        if let Some(sc) = result.get("structuredContent").filter(|v| !v.is_null()) {
            parts.push(sc.to_string());
        }
    }
    let mut text = parts.join("\n");
    if text.trim().is_empty() {
        text = "(no output)".into();
    }
    if result.get("isError").and_then(Value::as_bool) == Some(true) {
        text = format!("Error: {text}");
    }
    if text.chars().count() > MAX_RESULT_CHARS {
        let total = text.len();
        text = text.chars().take(MAX_RESULT_CHARS).collect();
        text.push_str(&format!("\n… [truncated, {total} bytes total]"));
    }
    text
}

// ---------------------------------------------------------------------------
// McpSet — all servers of one session
// ---------------------------------------------------------------------------

/// A tool as offered to the model.
struct Exposed {
    name: String,
    server: usize,
    tool: String,
    def: Value,
}

pub struct McpSet {
    specs: Vec<ServerSpec>,
    cwd: PathBuf,
    cfg: McpConfig,
    context_size: u64,
    servers: OnceCell<Vec<Arc<McpServer>>>,
    failures: Mutex<Vec<String>>,
    reported: AtomicBool,
}

impl std::fmt::Debug for McpSet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpSet")
            .field(
                "servers",
                &self.specs.iter().map(|s| &s.name).collect::<Vec<_>>(),
            )
            .finish()
    }
}

impl McpSet {
    /// Create the set and start connecting in the background, so servers
    /// are usually ready by the time the first prompt arrives.
    pub fn start(
        specs: Vec<ServerSpec>,
        cwd: PathBuf,
        cfg: McpConfig,
        context_size: u64,
    ) -> Arc<Self> {
        let set = Arc::new(Self {
            specs,
            cwd,
            cfg,
            context_size,
            servers: OnceCell::new(),
            failures: Mutex::new(Vec::new()),
            reported: AtomicBool::new(false),
        });
        if tokio::runtime::Handle::try_current().is_ok() {
            let bg = Arc::clone(&set);
            tokio::spawn(async move {
                bg.connected().await;
            });
        }
        set
    }

    pub fn server_names(&self) -> Vec<String> {
        self.specs.iter().map(|s| s.name.clone()).collect()
    }

    /// Whether every server has finished connecting (or failed to).
    pub fn is_ready(&self) -> bool {
        self.servers.initialized()
    }

    /// Wait until [`is_ready`](Self::is_ready), at most `limit`.
    pub async fn wait_ready(&self, limit: Duration) -> bool {
        tokio::time::timeout(limit, self.connected()).await.is_ok()
    }

    /// Wait until every server has connected or failed (each bounded by
    /// the connect timeout).
    pub async fn wait_connected(&self) {
        self.connected().await;
    }

    /// Whether any configured server is Xcode's tool server.
    pub fn has_xcode(&self) -> bool {
        self.specs.iter().any(ServerSpec::is_xcode)
    }

    async fn connected(&self) -> &[Arc<McpServer>] {
        self.servers
            .get_or_init(|| async {
                let mut out = Vec::new();
                for spec in &self.specs {
                    match McpServer::connect(spec, &self.cwd, &self.cfg, self.context_size).await {
                        Ok(s) => out.push(Arc::new(s)),
                        Err(e) => {
                            warn!(server = %spec.name, error = %e, "MCP server unavailable");
                            self.failures
                                .lock()
                                .expect("mcp failures lock")
                                .push(format!("{}: {e}", spec.name));
                        }
                    }
                }
                out
            })
            .await
    }

    /// Connection problems not yet shown to the user (reported once).
    pub async fn take_failures(&self) -> Vec<String> {
        self.connected().await;
        if self.reported.swap(true, Ordering::Relaxed) {
            return Vec::new();
        }
        self.failures.lock().expect("mcp failures lock").clone()
    }

    /// Refresh servers that announced `tools/list_changed`, then map every
    /// exposed tool to a model-facing name unique across built-ins and
    /// servers (`<server>__<tool>` on collision).
    async fn exposed(&self) -> Vec<Exposed> {
        let servers = self.connected().await;
        for s in servers {
            if s.conn.tools_changed.load(Ordering::Relaxed) {
                if let Err(e) = s.refresh_tools(&self.cfg, self.context_size).await {
                    warn!(server = %s.spec.name, error = %e, "Refreshing MCP tools failed");
                }
            }
        }
        let mut taken: HashSet<String> = BUILTIN_TOOL_NAMES.iter().map(|s| s.to_string()).collect();
        let mut out = Vec::new();
        for (idx, s) in servers.iter().enumerate() {
            for t in s.tools.read().expect("mcp tools lock").iter() {
                let mut name = sanitize_name(&t.name);
                if taken.contains(&name) {
                    name = sanitize_name(&format!("{}__{}", s.spec.name, t.name));
                }
                if !taken.insert(name.clone()) {
                    continue;
                }
                out.push(Exposed {
                    def: json!({
                        "type": "function",
                        "function": {
                            "name": name,
                            "description": t.description,
                            "parameters": t.input_schema,
                        }
                    }),
                    name,
                    server: idx,
                    tool: t.name.clone(),
                });
            }
        }
        out
    }

    /// Tool definitions (OpenAI function format) for every exposed MCP tool.
    pub async fn tool_definitions(&self) -> Vec<Value> {
        self.exposed().await.into_iter().map(|e| e.def).collect()
    }

    /// Run `name` if it is an MCP tool; `None` means it is not one.
    pub async fn call(&self, name: &str, args: &Value) -> Option<String> {
        let exposed = self.exposed().await;
        let e = exposed.iter().find(|e| e.name == name)?;
        let server = Arc::clone(&self.connected().await[e.server]);
        debug!(server = %server.spec.name, tool = %e.tool, "MCP tool call");
        Some(server.call(&e.tool, args, self.cfg.tool_timeout()).await)
    }

    /// Text appended to the system prompt once the servers are connected:
    /// each server's own instructions plus, for Xcode, guidance on when to
    /// prefer its tools. `None` when no tools are exposed.
    pub async fn system_prompt_addendum(&self) -> Option<String> {
        let exposed = self.exposed().await;
        if exposed.is_empty() {
            return None;
        }
        let servers = self.connected().await;
        let mut out = String::new();
        for (idx, s) in servers.iter().enumerate() {
            let names: Vec<&str> = exposed
                .iter()
                .filter(|e| e.server == idx)
                .map(|e| e.name.as_str())
                .collect();
            if names.is_empty() {
                continue;
            }
            if s.is_xcode {
                out.push_str(&xcode_guidance(&names));
            } else {
                out.push_str(&format!(
                    "\n\n## Tools from MCP server `{}`\n{}",
                    s.server_name,
                    names.join(", ")
                ));
                if let Some(i) = &s.instructions {
                    out.push_str(&format!("\n{i}"));
                }
            }
        }
        (!out.is_empty()).then_some(out)
    }
}

/// OpenAI function names must match `^[a-zA-Z0-9_-]{1,64}$`.
fn sanitize_name(s: &str) -> String {
    let mut n: String = s
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    n.truncate(64);
    if n.is_empty() {
        n.push_str("tool");
    }
    n
}

/// System-prompt guidance for Xcode's tools. Only lines whose tools are
/// actually exposed are included, so the model is never told to call a
/// tool it does not have.
pub fn xcode_guidance(available: &[&str]) -> String {
    let has = |t: &str| available.contains(&t);
    let any = |ts: &[&str]| ts.iter().any(|t| has(t));
    let mut lines: Vec<String> = Vec::new();
    let mut add = |cond: bool, line: &str| {
        if cond {
            lines.push(format!("- {line}"));
        }
    };
    add(
        has("BuildProject"),
        "Build with `BuildProject`, never `xcodebuild` or `swift build` in bash. \
         Its result lists the errors; use `GetBuildLog` (severity \"error\") for more detail.",
    );
    add(
        has("XcodeRefreshCodeIssuesInFile"),
        "After editing a Swift/Objective-C file, check it with `XcodeRefreshCodeIssuesInFile` \
         (fast, no full build) and fix what it reports before moving on.",
    );
    add(
        any(&["RunSomeTests", "RunAllTests"]),
        "Run tests with `GetTestList` / `RunSomeTests` / `RunAllTests`, never `xcodebuild test`.",
    );
    add(
        has("RunProject"),
        "Run the app with `RunProject` (stop with `StopProject`) and read its output with \
         `GetConsoleOutput`.",
    );
    add(
        has("RenderPreview"),
        "Check SwiftUI views visually with `RenderPreview`.",
    );
    add(
        has("DocumentationSearch"),
        "Look up Apple APIs with `DocumentationSearch` instead of guessing signatures or \
         availability.",
    );
    add(
        has("XcodeWrite"),
        "Create new source files with `XcodeWrite` so Xcode adds them to the project and target; \
         plain file writes can leave them out of the build.",
    );
    add(
        any(&["XcodeMV", "XcodeRM", "XcodeMakeDir"]),
        "Move, rename, delete files or create groups with `XcodeMV` / `XcodeRM` / `XcodeMakeDir`, \
         not `mv`/`rm`/`mkdir`, so the project file stays consistent.",
    );
    add(
        any(&["AddInfoPlist", "AddEntitlement"]),
        "Change Info.plist keys with `AddInfoPlist` and entitlements with `AddEntitlement`; \
         never edit those files or project.pbxproj by hand.",
    );
    add(
        any(&["GetTargetBuildSettings", "UpdateTargetBuildSetting"]),
        "Read and change build settings with `GetTargetBuildSettings` / \
         `UpdateTargetBuildSetting`.",
    );
    add(
        any(&[
            "XcodeLS",
            "XcodeGlob",
            "XcodeRead",
            "XcodeWrite",
            "XcodeUpdate",
        ]),
        "Xcode file tools take project-navigator paths (e.g. `MyApp/ContentView.swift`), not \
         absolute filesystem paths; use `XcodeLS` / `XcodeGlob` to find them. Reading and \
         editing existing files with the built-in `read_file` / `edit` tools is fine.",
    );
    add(
        has("XcodeListWorkspaces"),
        "If a tool reports several open workspaces, call `XcodeListWorkspaces` and pass \
         `workspaceIdentifier`.",
    );
    add(
        has("XcodeOpenWorkspace"),
        "If a tool says this agent is not approved yet, call `XcodeOpenWorkspace` with the \
         .xcodeproj/.xcworkspace path in the working directory; Xcode then asks the user to \
         approve.",
    );
    add(
        any(&["BuildProject", "RunSomeTests", "RunAllTests"]),
        "Before saying a code change is done, build (and run the relevant tests) and report \
         the result.",
    );
    format!(
        "\n\n## Xcode tools\nYou are working inside Xcode and can use Xcode's own tools. \
         Prefer them over shell commands for anything Xcode does:\n{}",
        lines.join("\n")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_acp_stdio_servers_and_skips_http() {
        let v = json!([
            {"name": "xcode-tools", "command": "/usr/bin/xcrun", "args": ["mcpbridge"],
             "env": [{"name": "MCP_XCODE_PID", "value": "42"}]},
            {"type": "http", "name": "remote", "url": "https://x", "headers": []},
            {"name": "broken"}
        ]);
        let (specs, skipped) = parse_servers(Some(&v));
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].command, "/usr/bin/xcrun");
        assert_eq!(specs[0].env, vec![("MCP_XCODE_PID".into(), "42".into())]);
        assert!(specs[0].is_xcode());
        assert_eq!(skipped.len(), 2);
    }

    #[test]
    fn resolve_does_not_duplicate_client_xcode_server() {
        let cfg = McpConfig {
            xcode: XcodeMode::On,
            ..McpConfig::default()
        };
        let v = json!([{"name": "xcode", "command": "/Applications/Xcode.app/Contents/Developer/usr/bin/mcpbridge", "args": []}]);
        let specs = resolve_servers(&cfg, Some(&v), None);
        assert_eq!(specs.len(), 1);
    }

    #[test]
    fn resolve_respects_disabled_relay_and_xcode_off() {
        let cfg = McpConfig {
            enabled: false,
            xcode: XcodeMode::Off,
            ..McpConfig::default()
        };
        let v = json!([{"name": "a", "command": "a"}]);
        assert!(resolve_servers(&cfg, Some(&v), Some("Xcode")).is_empty());
    }

    #[test]
    fn glob_and_filters() {
        assert!(glob_match("Xcode*", "XcodeRead"));
        assert!(glob_match("*Tests", "RunAllTests"));
        assert!(glob_match("Get*Log", "GetBuildLog"));
        assert!(!glob_match("Xcode*", "BuildProject"));
        let cfg = McpConfig {
            include: vec!["Xcode*".into(), "BuildProject".into()],
            exclude: vec!["XcodeRM".into()],
            ..McpConfig::default()
        };
        assert!(cfg.allows("XcodeRead"));
        assert!(cfg.allows("BuildProject"));
        assert!(!cfg.allows("XcodeRM"));
        assert!(!cfg.allows("RunAllTests"));
    }

    fn tool(name: &str, desc_len: usize) -> McpTool {
        McpTool {
            name: name.into(),
            description: "x".repeat(desc_len),
            input_schema: json!({"type": "object", "properties": {}}),
        }
    }

    #[test]
    fn xcode_tools_fall_back_to_core_set_on_small_context() {
        let all = vec![tool("BuildProject", 4000), tool("StringCatalogEdit", 40000)];
        let cfg = McpConfig::default();
        let small = select_tools(all.clone(), &cfg, true, 32_768);
        assert_eq!(
            small.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(),
            ["BuildProject"]
        );
        let big = select_tools(all.clone(), &cfg, true, 262_144);
        assert_eq!(big.len(), 2);
        // Non-Xcode servers are never trimmed automatically.
        assert_eq!(select_tools(all, &cfg, false, 1_000).len(), 2);
    }

    #[test]
    fn formats_tool_results() {
        let r = json!({"content": [{"type": "text", "text": "Build succeeded"}]});
        assert_eq!(format_tool_result(&r), "Build succeeded");
        let r = json!({"content": [{"type": "text", "text": "not approved"}], "isError": true});
        assert_eq!(format_tool_result(&r), "Error: not approved");
        let r = json!({"content": [], "structuredContent": {"result": true}});
        assert_eq!(format_tool_result(&r), r#"{"result":true}"#);
        let r = json!({"content": [{"type": "image", "mimeType": "image/png", "data": "AA"}]});
        assert_eq!(
            format_tool_result(&r),
            "[image content (image/png) omitted]"
        );
    }

    #[test]
    fn xcode_guidance_only_mentions_available_tools() {
        let g = xcode_guidance(&["BuildProject", "GetBuildLog"]);
        assert!(g.contains("BuildProject"));
        assert!(!g.contains("RunAllTests"));
        assert!(!g.contains("XcodeWrite"));
    }

    #[test]
    fn sanitizes_tool_names() {
        assert_eq!(
            sanitize_name("xcode tools__Build.Project"),
            "xcode_tools__Build_Project"
        );
        assert_eq!(sanitize_name(&"a".repeat(80)).len(), 64);
    }

    /// End-to-end against a scripted MCP server (python3 is on every macOS
    /// and CI image this crate targets; skip otherwise).
    #[tokio::test]
    async fn relays_tools_from_a_stdio_server() {
        if std::process::Command::new("python3")
            .arg("-V")
            .output()
            .is_err()
        {
            return;
        }
        let script = r#"
import sys, json
for line in sys.stdin:
    m = json.loads(line)
    mid, meth = m.get("id"), m.get("method")
    if mid is None:
        continue
    if meth == "initialize":
        r = {"protocolVersion": "2025-06-18", "capabilities": {"tools": {}},
             "serverInfo": {"name": "xcode-tools", "version": "1"}}
    elif meth == "tools/list":
        r = {"tools": [
            {"name": "BuildProject", "description": "Builds", "inputSchema": {"type": "object"}},
            {"name": "bash", "description": "clash", "inputSchema": {"type": "object", "properties": {}}}]}
    elif meth == "tools/call":
        r = {"content": [{"type": "text", "text": "called " + m["params"]["name"]}]}
    else:
        r = {}
    print(json.dumps({"jsonrpc": "2.0", "id": mid, "result": r}), flush=True)
"#;
        let spec = ServerSpec {
            name: "fake".into(),
            command: "python3".into(),
            args: vec!["-I".into(), "-c".into(), script.into()],
            env: vec![],
        };
        let set = McpSet::start(
            vec![spec],
            std::env::temp_dir(),
            McpConfig::default(),
            262_144,
        );
        let defs = set.tool_definitions().await;
        let names: Vec<&str> = defs
            .iter()
            .filter_map(|d| d["function"]["name"].as_str())
            .collect();
        assert_eq!(names, ["BuildProject", "fake__bash"]);
        assert_eq!(defs[0]["function"]["parameters"]["properties"], json!({}));
        assert_eq!(
            set.call("BuildProject", &json!({})).await.as_deref(),
            Some("called BuildProject")
        );
        assert_eq!(
            set.call("fake__bash", &json!({})).await.as_deref(),
            Some("called bash")
        );
        assert_eq!(set.call("read_file", &json!({})).await, None);
        let addendum = set.system_prompt_addendum().await.unwrap();
        assert!(addendum.contains("## Xcode tools"));
        assert!(addendum.contains("BuildProject"));
        assert!(set.take_failures().await.is_empty());
    }

    #[tokio::test]
    async fn reports_servers_that_fail_to_start() {
        let spec = ServerSpec {
            name: "missing".into(),
            command: "/nonexistent/mcp-server".into(),
            args: vec![],
            env: vec![],
        };
        let set = McpSet::start(
            vec![spec],
            std::env::temp_dir(),
            McpConfig::default(),
            32_768,
        );
        assert!(set.tool_definitions().await.is_empty());
        let f = set.take_failures().await;
        assert_eq!(f.len(), 1);
        assert!(f[0].starts_with("missing:"));
        assert!(set.take_failures().await.is_empty());
    }
}
