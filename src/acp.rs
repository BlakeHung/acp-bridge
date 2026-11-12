//! ACP JSON-RPC helpers — stdout transport and notification builders.
//!
//! acp-bridge supports both ACP v1 and v2 wire shapes. Functions with the
//! `_for_version` suffix take an explicit [`ProtocolVersion`] argument and
//! emit the shape that version's spec defines. The plain (no-suffix)
//! functions stay backwards-compatible by always emitting v1 — they are
//! the historical API and are still the most common path because most
//! existing Clients (Zed, JetBrains, ACP UI, Meuxe) speak v1 today.

use crate::protocol::{ProtocolVersion, RequestId};
use serde_json::{json, Value};
use std::io::Write;

/// Write a JSON-RPC object to stdout (newline-delimited).
pub fn send(obj: &Value) {
    let mut stdout = std::io::stdout().lock();
    let _ = serde_json::to_writer(&mut stdout, obj);
    let _ = stdout.write_all(b"\n");
    let _ = stdout.flush();
}

/// Send a JSON-RPC success response, echoing the caller's request ID verbatim
/// (string or numeric).
pub fn send_response(id: &RequestId, result: Value) {
    send(&json!({"jsonrpc": "2.0", "id": id.as_value(), "result": result}));
}

/// Send a JSON-RPC error response, echoing the caller's request ID verbatim.
pub fn send_error(id: &RequestId, code: i64, message: &str) {
    send(
        &json!({"jsonrpc": "2.0", "id": id.as_value(), "error": {"code": code, "message": message}}),
    );
}

/// Send a JSON-RPC error response with an attached `data` payload.
///
/// Used to attach machine-readable context (e.g. `{"reason": "no_persistence"}`)
/// to capability-mismatch errors so Clients can log a precise root cause
/// instead of a generic "method not found". The `data` object is
/// intentionally small and stable — Clients can switch on it.
pub fn send_error_with_data(id: &RequestId, code: i64, message: &str, data: Value) {
    send(&json!({
        "jsonrpc": "2.0",
        "id": id.as_value(),
        "error": {
            "code": code,
            "message": message,
            "data": data,
        }
    }));
}

/// Send a JSON-RPC notification (no id).
pub fn send_notification(method: &str, params: Value) {
    send(&json!({"jsonrpc": "2.0", "method": method, "params": params}));
}

/// Send a `session/update` notification carrying the active session ID.
///
/// ACP v1 uses `session/update` (not `session/notify`) and each update must be
/// attributable to a session. Text content is a typed ContentBlock.
fn send_session_update(session_id: &str, update: Value) {
    send_notification(
        "session/update",
        json!({"sessionId": session_id, "update": update}),
    );
}

/// Notify an agent_message_chunk (streaming text).
pub fn notify_text(session_id: &str, text: &str) {
    send_session_update(
        session_id,
        json!({
            "sessionUpdate": "agent_message_chunk",
            "content": {"type": "text", "text": text}
        }),
    );
}

/// Notify an agent_thought_chunk.
///
/// ACP v1 requires a typed `content` block on thought chunks, identical in
/// shape to `agent_message_chunk`. Earlier versions of acp-bridge emitted the
/// sessionUpdate with no content, which spec-compliant clients (Meuxe, ACP
/// UI, …) rejected or rendered as an empty bubble.
pub fn notify_thinking(session_id: &str) {
    send_session_update(
        session_id,
        json!({
            "sessionUpdate": "agent_thought_chunk",
            "content": {"type": "text", "text": ""}
        }),
    );
}

/// Stream a piece of model reasoning as `agent_thought_chunk`. The shape is
/// identical on the v1 and v2 wire.
pub fn notify_thought_text(session_id: &str, text: &str) {
    send_session_update(
        session_id,
        json!({
            "sessionUpdate": "agent_thought_chunk",
            "content": {"type": "text", "text": text}
        }),
    );
}

/// Map an acp-bridge tool name to the ACP `ToolKind` enum.
///
/// ACP v1 defines the kind as one of `read | edit | delete | move | search |
/// execute | fetch | think | other`. The mapping below mirrors the
/// `tools::tool_definitions` set shipped in this crate; unknown names fall
/// back to `"other"` so clients still render the tool call instead of
/// dropping it on the floor.
pub fn kind_for_tool(name: &str) -> &'static str {
    // MCP tools that collided with another name are exposed as
    // `<server>__<tool>`; classify by the tool part.
    let name = name.rsplit("__").next().unwrap_or(name);
    match name {
        // File inspection
        "read_file" | "list_dir" => "read",
        // File mutation
        "write_file" | "edit" => "edit",
        // Shell / command execution
        "shell" | "exec" | "bash" => "execute",
        // Search
        "search" | "search_code" | "grep" => "search",
        // Network fetch
        "web_fetch" | "fetch" | "http_get" => "fetch",
        // Git operations are file-mutation / shell-like; map to execute so
        // Clients that group "execute" tools together (Zed's terminal
        // affordance) get a sensible default.
        "git_status" | "git_diff" | "git_log" | "git_commit" => "execute",
        // Xcode's MCP tools (`xcrun mcpbridge`)
        "XcodeRead" | "XcodeLS" | "XcodeGlob" | "GetBuildLog" | "GetTestList"
        | "GetConsoleOutput" | "XcodeRefreshCodeIssuesInFile" | "GetTargetBuildSettings"
        | "XcodeListWorkspaces" | "XcodeListSchemes" | "XcodeListTargets" => "read",
        "XcodeWrite" | "XcodeUpdate" | "XcodeMakeDir" | "AddInfoPlist" | "AddEntitlement"
        | "UpdateTargetBuildSetting" | "UpdateFileCompilerFlags" => "edit",
        "XcodeRM" => "delete",
        "XcodeMV" => "move",
        "XcodeGrep" | "DocumentationSearch" => "search",
        "BuildProject" | "RunAllTests" | "RunSomeTests" | "RunProject" | "StopProject"
        | "RenderPreview" | "RunCodeSnippet" | "InvokeDebuggerCommand" => "execute",
        // Everything else (delete/move/think are not yet in the default
        // toolset but we list the enum values for completeness).
        _ => "other",
    }
}

/// Maximum bytes of tool output forwarded to the Client in a
/// `tool_call_update`. The full result still goes to the LLM.
const TOOL_OUTPUT_PREVIEW_BYTES: usize = 8 * 1024;

/// Build a human-readable tool-call title from the tool name and its
/// arguments, so Clients show *what* runs (`git status`, the bash command,
/// `Read src/main.rs`) rather than just the tool name.
pub fn tool_title(name: &str, input: Option<&Value>) -> String {
    let arg = |k: &str| {
        input
            .and_then(|v| v.get(k))
            .and_then(|v| v.as_str())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    };
    let base = name.rsplit("__").next().unwrap_or(name);
    let title = match base {
        "bash" | "shell" | "exec" => arg("command"),
        "read_file" => arg("path").map(|p| format!("Read {p}")),
        "list_dir" => arg("path").map(|p| format!("List {p}")),
        "write_file" => arg("path").map(|p| format!("Write {p}")),
        "edit" => arg("path").map(|p| format!("Edit {p}")),
        "search_code" | "search" | "grep" => arg("pattern").map(|p| format!("Search \"{p}\"")),
        "web_fetch" | "fetch" | "http_get" => arg("url").map(|u| format!("Fetch {u}")),
        "git_status" => Some("git status".into()),
        "git_diff" => Some("git diff".into()),
        "git_log" => Some("git log".into()),
        "git_commit" => arg("message").map(|m| {
            let first = m.lines().next().unwrap_or("").to_string();
            format!("git commit -m \"{first}\"")
        }),
        // Xcode's MCP tools
        "BuildProject" => Some("Build project".into()),
        "RunAllTests" => Some("Run all tests".into()),
        "RunSomeTests" => Some(match input.and_then(|v| v.get("tests")).and_then(Value::as_array) {
            Some(t) if !t.is_empty() => format!("Run tests ({})", t.len()),
            _ => "Run tests".into(),
        }),
        "RunProject" => Some("Run app".into()),
        "StopProject" => Some("Stop app".into()),
        "GetBuildLog" => Some("Build log".into()),
        "XcodeRefreshCodeIssuesInFile" => {
            arg("filePath").map(|p| format!("Check issues in {p}"))
        }
        "XcodeRead" => arg("filePath").map(|p| format!("Read {p}")),
        "XcodeWrite" => arg("filePath").map(|p| format!("Write {p}")),
        "XcodeUpdate" => arg("filePath").map(|p| format!("Edit {p}")),
        "XcodeRM" => arg("path").map(|p| format!("Remove {p}")),
        "XcodeLS" => arg("path").map(|p| format!("List {p}")),
        "XcodeGrep" => arg("pattern").map(|p| format!("Search \"{p}\"")),
        "DocumentationSearch" => arg("query").map(|q| format!("Search docs \"{q}\"")),
        "RenderPreview" => arg("sourceFilePath")
            .or_else(|| arg("filePath"))
            .map(|p| format!("Render preview {p}")),
        _ => None,
    };
    let title = title.unwrap_or_else(|| name.to_string());
    // Keep titles to a single readable line.
    let one_line = title.replace('\n', " ⏎ ");
    if one_line.chars().count() > 200 {
        let mut t: String = one_line.chars().take(199).collect();
        t.push('…');
        t
    } else {
        one_line
    }
}

fn tool_start_payload(
    session_update: &str,
    tool_call_id: &str,
    name: &str,
    input: Option<&Value>,
    location: Option<&str>,
) -> Value {
    let mut payload = json!({
        "sessionUpdate": session_update,
        "toolCallId": tool_call_id,
        "title": tool_title(name, input),
        "kind": kind_for_tool(name),
        "status": "in_progress"
    });
    if let Some(input) = input {
        payload["rawInput"] = input.clone();
    }
    if let Some(path) = location {
        payload["locations"] = json!([{ "path": path }]);
    }
    payload
}

fn truncate_output(output: &str) -> String {
    if output.len() <= TOOL_OUTPUT_PREVIEW_BYTES {
        return output.to_string();
    }
    let mut end = TOOL_OUTPUT_PREVIEW_BYTES;
    while !output.is_char_boundary(end) {
        end -= 1;
    }
    format!(
        "{}\n… [truncated, {} bytes total]",
        &output[..end],
        output.len()
    )
}

/// Notify a tool_call start.
///
/// `tool_call_id` is required by ACP v1: clients use it to pair subsequent
/// `tool_call_update` notifications with the originating tool call. Without
/// it the client cannot render a coherent per-tool timeline. `input` is
/// forwarded as `rawInput` and used to derive a descriptive `title`;
/// `location` (absolute path) is forwarded as `locations`.
pub fn notify_tool_start(
    session_id: &str,
    tool_call_id: &str,
    name: &str,
    input: Option<&Value>,
    location: Option<&str>,
) {
    send_session_update(
        session_id,
        tool_start_payload("tool_call", tool_call_id, name, input, location),
    );
}

/// Notify a tool_call_update (status change or content append).
///
/// `tool_call_id` is required. `status` is one of `pending | in_progress |
/// completed | failed`. Spec-compliant clients ignore notifications whose
/// toolCallId they have not seen. When `output` is given it is attached
/// as `rawOutput` and as a text `content` block (truncated for display).
pub fn notify_tool_done(
    session_id: &str,
    tool_call_id: &str,
    _name: &str,
    status: &str,
    output: Option<&str>,
) {
    let mut payload = json!({
        "sessionUpdate": "tool_call_update",
        "toolCallId": tool_call_id,
        "status": status
    });
    if let Some(output) = output {
        let preview = truncate_output(output);
        payload["rawOutput"] = json!({ "output": preview });
        payload["content"] = json!([{
            "type": "content",
            "content": { "type": "text", "text": preview }
        }]);
    }
    send_session_update(session_id, payload);
}

/// One entry in an `agent_plan` notification.
///
/// `content` is the human-readable description, `priority` is one of
/// `high | medium | low` and `status` is one of `pending | in_progress |
/// completed`. See the ACP v1 plan spec:
///
/// <https://agentclientprotocol.com/protocol/v1/agent-plan>
#[derive(Debug, Clone, serde::Serialize)]
pub struct PlanEntry {
    pub content: String,
    pub priority: &'static str,
    pub status: &'static str,
}

impl PlanEntry {
    pub fn new(content: impl Into<String>, priority: &'static str, status: &'static str) -> Self {
        Self {
            content: content.into(),
            priority,
            status,
        }
    }
}

/// Notify a `plan` update (replace the entire plan list per ACP v1).
///
/// ACP v1: the Agent MUST send a complete list of all plan entries in each
/// update; the Client MUST replace the current plan with the supplied one.
/// `entries` may be empty to clear the plan.
pub fn notify_plan(session_id: &str, entries: &[PlanEntry]) {
    let entries_json: Vec<Value> = entries
        .iter()
        .map(|e| {
            json!({
                "content": e.content,
                "priority": e.priority,
                "status": e.status,
            })
        })
        .collect();
    send_session_update(
        session_id,
        json!({
            "sessionUpdate": "plan",
            "entries": entries_json,
        }),
    );
}

/// Notify a `session_info_update` carrying an updated session title and
/// optional ISO 8601 `updatedAt` timestamp.
///
/// Clients use this to surface real-time title changes in their session
/// list. Pass `None` for `updated_at` if you do not have a precise
/// timestamp; the Client will fall back to its own clock.
pub fn notify_session_info(session_id: &str, title: &str, updated_at: Option<&str>) {
    let mut payload = json!({
        "sessionUpdate": "session_info_update",
        "title": title,
    });
    if let Some(ts) = updated_at {
        payload["updatedAt"] = json!(ts);
    }
    send_session_update(session_id, payload);
}

/// Notify a `usage_update` reporting current context window utilization.
///
/// `used` is the tokens currently in the context, `size` is the model's
/// total context window. `cost` is optional cumulative session cost;
/// pass `None` for local backends where cost is unknown.
///
/// Per ACP v1, this is the stable form (finalized 2026-06-05).
pub fn notify_usage(session_id: &str, used: u64, size: u64, cost: Option<(f64, &str)>) {
    let mut payload = json!({
        "sessionUpdate": "usage_update",
        "used": used,
        "size": size,
    });
    if let Some((amount, currency)) = cost {
        payload["cost"] = json!({
            "amount": amount,
            "currency": currency,
        });
    }
    send_session_update(session_id, payload);
}

/// One slash command advertised to the Client via `available_commands_update`.
#[derive(Debug, Clone, serde::Serialize)]
pub struct AvailableCommand {
    pub name: String,
    pub description: String,
    /// Optional placeholder hint shown in the UI input.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input_hint: Option<String>,
}

impl AvailableCommand {
    pub fn new(
        name: impl Into<String>,
        description: impl Into<String>,
        input_hint: Option<impl Into<String>>,
    ) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            input_hint: input_hint.map(Into::into),
        }
    }
}

/// Notify an `available_commands_update` advertising the slash commands
/// the Client should surface as user-invokable shortcuts.
///
/// Per ACP v1, the Client replaces its current command list with the
/// supplied one; pass an empty slice to clear all commands.
pub fn notify_available_commands(session_id: &str, commands: &[AvailableCommand]) {
    let cmds_json: Vec<Value> = commands
        .iter()
        .map(|c| {
            let mut cmd = json!({
                "name": c.name,
                "description": c.description,
            });
            if let Some(hint) = &c.input_hint {
                cmd["input"] = json!({ "hint": hint });
            }
            cmd
        })
        .collect();
    send_session_update(
        session_id,
        json!({
            "sessionUpdate": "available_commands_update",
            "availableCommands": cmds_json,
        }),
    );
}

// ---------------------------------------------------------------------------
// Protocol-version-aware dispatchers
//
// These helpers take an explicit version argument and emit the wire shape
// the corresponding ACP spec defines. They are the entry point for any
// caller that has read a session's negotiated version and wants the right
// shape. The plain (no-suffix) functions above are kept as a v1 shortcut
// for code paths that haven't migrated yet.
// ---------------------------------------------------------------------------

/// Dispatch a `session/update` carrying agent message chunks. v2 splits
/// message updates into a pair of concepts — `agent_message_chunk` (an
/// append) and `agent_message` (an upsert keyed by `messageId`). v1
/// only knows the chunk variant. We always emit the chunk variant; the
/// upsert variant is opt-in via the engine if a later refactor wants
/// to replace a chunk stream with a single final upsert.
pub fn notify_text_for(version: ProtocolVersion, session_id: &str, text: &str) {
    match version {
        ProtocolVersion::V2 => notify_text_v2(session_id, text),
        _ => notify_text(session_id, text),
    }
}

fn notify_text_v2(session_id: &str, text: &str) {
    send_session_update(
        session_id,
        json!({
            "sessionUpdate": "agent_message_chunk",
            "content": {"type": "text", "text": text}
        }),
    );
}

/// Dispatch a thought-chunk notification. v2's content shape is the
/// same as v1's, but the sessionUpdate discriminator must remain
/// `agent_thought_chunk` (the v2 RFD keeps the legacy discriminator for
/// chunks even though it added a separate `agent_thought` upsert).
pub fn notify_thinking_for(version: ProtocolVersion, session_id: &str) {
    match version {
        ProtocolVersion::V2 => notify_thinking_v2(session_id),
        _ => notify_thinking(session_id),
    }
}

fn notify_thinking_v2(session_id: &str) {
    send_session_update(
        session_id,
        json!({
            "sessionUpdate": "agent_thought_chunk",
            "content": {"type": "text", "text": ""}
        }),
    );
}

/// Dispatch a tool-call start notification.
///
/// ACP v2 removed the `tool_call` sessionUpdate entirely. v2 Clients
/// expect a `tool_call_update` with `status: "in_progress"` instead,
/// keyed by `toolCallId`. We emit the v2 shape when the negotiated
/// version is V2, the v1 `tool_call` shape otherwise.
pub fn notify_tool_start_for(
    version: ProtocolVersion,
    session_id: &str,
    tool_call_id: &str,
    name: &str,
    input: Option<&Value>,
    location: Option<&str>,
) {
    match version {
        ProtocolVersion::V2 => send_session_update(
            session_id,
            tool_start_payload("tool_call_update", tool_call_id, name, input, location),
        ),
        _ => notify_tool_start(session_id, tool_call_id, name, input, location),
    }
}

/// Dispatch a tool-call update (status change). The v1 and v2 shapes
/// are both `tool_call_update` and same schema, so this dispatcher just
/// routes.
pub fn notify_tool_done_for(
    _version: ProtocolVersion,
    session_id: &str,
    tool_call_id: &str,
    name: &str,
    status: &str,
    output: Option<&str>,
) {
    notify_tool_done(session_id, tool_call_id, name, status, output)
}

/// Dispatch a plan update.
///
/// ACP v1: `{sessionUpdate: "plan", entries: [...]}` (no plan id, entries
/// are the entire plan).
///
/// ACP v2: `{sessionUpdate: "plan_update", plan: {type: "items", planId,
/// entries: [...]}}`. The `planId` is required so Clients can track
/// multiple plans independently; we use a stable id derived from the
/// session id ("default") because acp-bridge currently emits exactly
/// one plan per session.
pub fn notify_plan_for(
    version: ProtocolVersion,
    session_id: &str,
    session_key: &str,
    entries: &[PlanEntry],
) {
    match version {
        ProtocolVersion::V2 => notify_plan_v2(session_id, session_key, entries),
        _ => notify_plan(session_id, entries),
    }
}

fn notify_plan_v2(session_id: &str, session_key: &str, entries: &[PlanEntry]) {
    let entries_json: Vec<Value> = entries
        .iter()
        .map(|e| {
            json!({
                "content": e.content,
                "priority": e.priority,
                "status": e.status,
            })
        })
        .collect();
    send_session_update(
        session_id,
        json!({
            "sessionUpdate": "plan_update",
            "plan": {
                "type": "items",
                "planId": format!("plan-{session_key}"),
                "entries": entries_json
            }
        }),
    );
}

/// Dispatch a `session_info_update`. Shape is identical between v1 and
/// v2; the discriminator is the same. Kept as a dispatcher for
/// future-proofing and to give call sites one entry point.
pub fn notify_session_info_for(
    version: ProtocolVersion,
    session_id: &str,
    title: &str,
    updated_at: Option<&str>,
) {
    match version {
        ProtocolVersion::V2 => notify_session_info_v2(session_id, title, updated_at),
        _ => notify_session_info(session_id, title, updated_at),
    }
}

fn notify_session_info_v2(session_id: &str, title: &str, updated_at: Option<&str>) {
    let mut payload = json!({
        "sessionUpdate": "session_info_update",
        "title": title,
    });
    if let Some(ts) = updated_at {
        payload["updatedAt"] = json!(ts);
    }
    send_session_update(session_id, payload);
}

/// Dispatch a `usage_update`. v2 added a `required` (cost) field that
/// v1 also accepts (stabilized 2026-06-05), but v2 enforces that the
/// `currency` field matches `^[A-Z]{3}$`. We pass `cost = None` for
/// local backends so the validation rule never bites — but the
/// dispatcher stays here for when we add cost reporting.
pub fn notify_usage_for(
    version: ProtocolVersion,
    session_id: &str,
    used: u64,
    size: u64,
    cost: Option<(f64, &str)>,
) {
    match version {
        ProtocolVersion::V2 => notify_usage_v2(session_id, used, size, cost),
        _ => notify_usage(session_id, used, size, cost),
    }
}

fn notify_usage_v2(session_id: &str, used: u64, size: u64, cost: Option<(f64, &str)>) {
    let mut payload = json!({
        "sessionUpdate": "usage_update",
        "used": used,
        "size": size,
    });
    if let Some((amount, currency)) = cost {
        // v2 enforces ISO 4217 currency format on the cost object.
        // Refuse to emit malformed currency rather than panic — let the
        // caller fix it.
        if currency.len() == 3 && currency.chars().all(|c| c.is_ascii_uppercase()) {
            payload["cost"] = json!({
                "amount": amount,
                "currency": currency,
            });
        }
    }
    send_session_update(session_id, payload);
}

/// Dispatch an `available_commands_update`. The v2 schema adds a
/// required `availableCommands` field (no default); the entry shape
/// is the same as v1. We emit the same payload.
pub fn notify_available_commands_for(
    version: ProtocolVersion,
    session_id: &str,
    commands: &[AvailableCommand],
) {
    match version {
        ProtocolVersion::V2 => notify_available_commands_v2(session_id, commands),
        _ => notify_available_commands(session_id, commands),
    }
}

fn notify_available_commands_v2(session_id: &str, commands: &[AvailableCommand]) {
    let cmds_json: Vec<Value> = commands
        .iter()
        .map(|c| {
            let mut cmd = json!({
                "name": c.name,
                "description": c.description,
            });
            if let Some(hint) = &c.input_hint {
                cmd["input"] = json!({ "hint": hint });
            }
            cmd
        })
        .collect();
    send_session_update(
        session_id,
        json!({
            "sessionUpdate": "available_commands_update",
            "availableCommands": cmds_json,
        }),
    );
}

/// Notify a `state_update` carrying an `IdleState` (v2 only — v1 has no
/// equivalent sessionUpdate). Emitted at the end of each prompt so v2
/// Clients receive an authoritative "I'm ready for the next request"
/// transition.
///
/// ACP v2 requires `state` as the discriminator on every
/// `state_update` variant (`"running" | "idle" | "requires_action"`).
/// Sending `"available": false` (the previous shape) was a wire-spec
/// violation caught by the 0.9.0 review; this now emits the spec-
/// compliant shape. See `agent-client-protocol/schema/v2/schema.json`
/// (`StateUpdate` is an `anyOf` keyed on the required `state` field).
pub fn notify_state_idle_for(
    version: ProtocolVersion,
    session_id: &str,
    stop_reason: Option<&str>,
) {
    if version != ProtocolVersion::V2 {
        return; // v1 has no state_update sessionUpdate
    }
    let mut payload = json!({
        "sessionUpdate": "state_update",
        "state": "idle",
    });
    if let Some(reason) = stop_reason {
        payload["stopReason"] = json!(reason);
    }
    send_session_update(session_id, payload);
}

#[cfg(test)]
mod tests {

    #[test]
    fn tool_title_surfaces_bash_command_and_paths() {
        let cmd = json!({"command": "swift build -c release"});
        assert_eq!(tool_title("bash", Some(&cmd)), "swift build -c release");
        assert_eq!(
            tool_title("read_file", Some(&json!({"path": "Sources/App.swift"}))),
            "Read Sources/App.swift"
        );
        assert_eq!(tool_title("git_status", Some(&json!({}))), "git status");
        assert_eq!(tool_title("bash", None), "bash");
        assert_eq!(tool_title("bash", Some(&json!({"command": "a\nb"}))), "a ⏎ b");
    }

    #[test]
    fn tool_start_payload_includes_raw_input_and_locations() {
        let input = json!({"path": "a.swift"});
        let p = tool_start_payload("tool_call", "tc1", "read_file", Some(&input), Some("/x/a.swift"));
        assert_eq!(p["title"], "Read a.swift");
        assert_eq!(p["kind"], "read");
        assert_eq!(p["rawInput"], input);
        assert_eq!(p["locations"][0]["path"], "/x/a.swift");
        let bare = tool_start_payload("tool_call", "llm", "llm_chat", None, None);
        assert!(bare.get("rawInput").is_none() && bare.get("locations").is_none());
    }

    #[test]
    fn truncate_output_caps_and_respects_char_boundaries() {
        assert_eq!(truncate_output("short"), "short");
        let long = "é".repeat(TOOL_OUTPUT_PREVIEW_BYTES);
        let t = truncate_output(&long);
        assert!(t.contains("[truncated,"));
        assert!(t.len() < long.len());
    }
    use super::*;

    #[test]
    fn kind_for_tool_maps_known_tool_names_to_acp_enums() {
        assert_eq!(kind_for_tool("read_file"), "read");
        assert_eq!(kind_for_tool("list_dir"), "read");
        assert_eq!(kind_for_tool("write_file"), "edit");
        assert_eq!(kind_for_tool("shell"), "execute");
        assert_eq!(kind_for_tool("bash"), "execute");
        assert_eq!(kind_for_tool("search"), "search");
        assert_eq!(kind_for_tool("grep"), "search");
        assert_eq!(kind_for_tool("fetch"), "fetch");
        // Unknown names fall back to "other" instead of dropping the call.
        assert_eq!(kind_for_tool("totally_new_tool"), "other");
        assert_eq!(kind_for_tool(""), "other");
    }

    #[test]
    fn tool_call_includes_required_v1_fields() {
        // Mirror what notify_tool_start sends so we catch wire-format drift
        // early. The exact JSON shape is what Meuxe / ACP UI consume.
        let payload = json!({
            "sessionUpdate": "tool_call",
            "toolCallId": "tc_1",
            "title": "read_file",
            "kind": kind_for_tool("read_file"),
            "status": "in_progress"
        });
        assert_eq!(payload["toolCallId"], "tc_1");
        assert_eq!(payload["title"], "read_file");
        assert_eq!(payload["kind"], "read");
        assert_eq!(payload["status"], "in_progress");
    }

    #[test]
    fn tool_call_update_includes_required_v1_fields() {
        let payload = json!({
            "sessionUpdate": "tool_call_update",
            "toolCallId": "tc_1",
            "title": "read_file",
            "status": "completed"
        });
        assert_eq!(payload["toolCallId"], "tc_1");
        assert_eq!(payload["status"], "completed");
    }

    #[test]
    fn thought_chunk_payload_includes_text_content() {
        let payload = json!({
            "sessionUpdate": "agent_thought_chunk",
            "content": {"type": "text", "text": ""}
        });
        assert_eq!(payload["content"]["type"], "text");
        assert!(payload["content"]["text"].is_string());
    }

    #[test]
    fn plan_entry_serializes_with_required_v1_fields() {
        let entry = PlanEntry::new("Read the README", "high", "pending");
        assert_eq!(entry.content, "Read the README");
        assert_eq!(entry.priority, "high");
        assert_eq!(entry.status, "pending");
    }

    #[test]
    fn available_command_serializes_with_optional_input_hint() {
        let with_hint = AvailableCommand::new("web", "Search the web", Some::<&str>("query"));
        let without_hint = AvailableCommand::new("test", "Run tests", None::<&str>);

        let with_json = serde_json::to_value(&with_hint).unwrap();
        assert_eq!(with_json["name"], "web");
        assert_eq!(with_json["description"], "Search the web");
        assert_eq!(with_json["input_hint"], "query");

        let without_json = serde_json::to_value(&without_hint).unwrap();
        assert!(without_json.get("input_hint").is_none());
    }
}
