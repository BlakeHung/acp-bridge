# acp-bridge Scope

This document is the source of truth for what acp-bridge supports, what it
supports with caveats, and what it deliberately does not implement. Use it
to set expectations when a Client picks acp-bridge from the ACP registry or
launches it as a subprocess.

acp-bridge positions itself as a **minimal ACP adapter for local AI**, not
a full agent runtime. Anything that would require persistence, an external
auth provider, or a hosted control-plane surface is intentionally out of
scope.

## Supported

| ACP surface | Status | Notes |
|---|---|---|
| `initialize` | ✅ | Advertises `protocolVersion: 1`, `agentInfo`, `agentCapabilities` |
| `session/new` | ✅ | Multi-session with per-session conversation history. `mcpServers` param accepted but ignored (acp-bridge does not relay MCP) |
| `session/prompt` | ✅ | Streaming via SSE → `session/update` notifications. Final response carries `stopReason` per ACP v1 (`end_turn` on success, `max_turn_requests` on tool-loop limit). Text and image content blocks supported |
| `session/end` | ✅ | Removes the session and frees its history |
| `session/cancel` notification | ✅ | Acknowledged with a log line; in-flight cancellation is not yet implemented |
| Streaming `agent_message_chunk` | ✅ | Typed `content: {type: "text", text: ...}` per ACP v1 |
| `agent_thought_chunk` | ✅ | Typed `content: {type: "text", text: ""}` — emitted so Clients render the thought bubble, even when the model emits no reasoning tokens |
| `tool_call` / `tool_call_update` | ✅ | Carries the ACP v1 required `toolCallId`, `title`, `kind`, `status`. Built-in tools: `read_file`, `list_dir`, `write_file`, `search` (regex grep), `shell` |
| Request ID echo | ✅ | Numeric AND string/UUID request ids are echoed verbatim — fixes the issue where Meuxe's UUID ids were silently dropped |
| Stdout framing | ✅ | Newline-delimited JSON, one object per line |

## Supported with caveats

| ACP surface | Caveat | How to enable |
|---|---|---|
| Image content blocks (`ContentBlock::Image`) | Off by default. Turning it on without a vision-capable backend causes Clients (Meuxe, ACP UI, …) to forward images that the LLM cannot parse | Set `LLM_SUPPORTS_IMAGE=true` or `[llm].supports_image = true` |
| `session/prompt` with audio / embedded resource content | Not parsed; audio/resource blocks are silently dropped from the prompt | Out of scope; document if you need it |
| `mcpServers` on `session/new` | Accepted for spec compatibility but **not relayed** to the underlying LLM. The agent uses its own built-in tool set | Future work; tracked but not scheduled |

## Deliberately not implemented

These methods are part of the ACP v1 surface but acp-bridge intentionally
does not implement them. When called, the agent responds with `code: -32601`
(JSON-RPC `Method not found`) carrying a `data: {reason: ...}` field that
explains *why*, so Clients can log a clear root cause instead of a generic
error.

| Method | Reason | Error code |
|---|---|---|
| `session/load` | acp-bridge has no persistence layer | `-32001` (`data.reason: "no_persistence"`) |
| `session/resume` | acp-bridge has no persistence layer | `-32001` (`data.reason: "no_persistence"`) |
| `session/set_mode` | `session/new` does not return a `modes` array | `-32602` (`data.reason: "no_modes"`) |
| `auth/login` | `initialize` returns `authMethods: []` | `-32601` (`data.reason: "no_auth_methods"`) |
| `auth/logout` | Same | `-32601` (`data.reason: "no_auth_methods"`) |
| `fs/read_text_file`, `fs/write_text_file` | acp-bridge never requests client-side file operations | `-32601` (`data.reason: "agent_does_not_call_client_fs"`) |
| `terminal/*` | Same | `-32601` (`data.reason: "agent_does_not_call_client_terminal"`) |

The Client should consult `agentCapabilities` to detect these gaps before
calling — spec-compliant Clients (Zed, JetBrains, ACP Inspector) already
do this. The error responses above are a safety net for Clients that don't.

## Things acp-bridge is not

- **Not a session database.** Restart the agent and all sessions are gone.
  If you need persistence, point acp-bridge at an external ACP-compatible
  agent that does (e.g. Claude Code, Codex CLI, OpenCode).
- **Not an auth provider.** No login flow, no token storage, no user
  identity. If you need per-user sessions, route through a Client that
  gates on its own identity.
- **Not an MCP server.** acp-bridge ignores `mcpServers` in `session/new`.
  Built-in tools are the only tool surface.
- **Not a v2 protocol agent yet.** Spec compliance is locked to ACP v1.
  See [Roadmap](#roadmap) below.
- **Not a web transport.** Transport is stdio JSON-RPC only — there is no
  HTTP / WebSocket / SSE surface. Use a proxy (e.g. `acp2api`) if you
  need to expose acp-bridge to a browser.

## Roadmap

- **v2 protocol negotiation.** Accept `protocolVersion: 2` and emit the
  v2-shape `initialize` response (single `capabilities` + single `info`)
  and v2-shape `session/update` payloads (`messageId`-keyed upserts,
  `tool_call_update` as the only tool surface). Will be a separate breaking
  release (0.9.0) gated on Client adoption; the v1 surface will stay
  supported in parallel for at least one minor cycle.
- **`session/cancel` cancellation.** Acknowledge the notification and
  propagate cancellation into the in-flight LLM request.
- **Optional MCP relay.** Opt-in via config to honor `mcpServers` in
  `session/new`.

## How to verify a Client works against this scope

The `tests/clients/` integration tests spawn acp-bridge as a subprocess
and drive it through the protocol sequences emitted by named Clients:

- `tests/clients/zed_style.rs` — Zed / JetBrains style: full client
  capabilities declared, image + text in the prompt, `session/load` and
  `session/set_mode` called and gracefully rejected.
- `tests/clients/codex_style.rs` — Codex CLI adapter style: minimal
  capabilities, bare tool-call round-trip, session-end at finish.
- `tests/clients/inspector_style.rs` — ACP Inspector style: heavy traffic
  probe, request-id type fuzz (numeric + UUID + null), prompt with
  `ContentBlock::ResourceLink`.

Run them with:

```sh
cargo test --test clients -- --test-threads=1
```

These tests are the contract. If you find a real Client whose
expectations diverge from what the tests assert, the test is wrong, not
the Client — open an issue and we will update both.
