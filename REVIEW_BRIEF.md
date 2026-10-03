# acp-bridge Code Review Brief

You are reviewing acp-bridge across two scopes:

1. **Commit `cec3c36`** — already landed in `main` (local), covers 0.8.0 → 0.8.1 → 0.8.2 (ACP v1 conformance, real-client e2e, spec notifications, tool surface expansion, structured backend errors).
2. **Working-tree** — uncommitted changes for 0.9.0 (ACP v1 / v2 dual wire format).

Read the scope appropriate to what you're reviewing. Specs cited below are the public contract; treat wire-shape divergence from those specs as a critical issue.

---

## Repository layout

```
/workspace/acp-bridge
├── Cargo.toml                            # workspace package; version pinned here
├── src/
│   ├── acp.rs              # wire helpers + notify_* dispatchers (v1 + v2)
│   ├── bench.rs            # --bench flag
│   ├── config.rs           # ConfigFile (TOML) + LlmConfig merge
│   ├── engine.rs           # sessions + tool loop + initialize / session_new / session_prompt
│   ├── hardware.rs         # GPU detection (best-effort, never blocks)
│   ├── lib.rs              # re-exports
│   ├── llm.rs              # reqwest client + chat/stream_chat + LlmError / LlmErrorKind
│   ├── main.rs             # run_acp_loop stdin/stdout JSON-RPC loop
│   ├── protocol.rs         # RequestId + ProtocolVersion + Session + AcpError
│   └── tools.rs            # built-in tool definitions and execute_tool dispatch
└── tests/
    ├── acp_test.rs                     # JSON-RPC helper tests
    ├── integration_test.rs              # 31 end-to-end tests over stdio
    ├── llm_test.rs                     # LlmConfig / backend detection
    ├── protocol_test.rs                # RequestId / Session / error codes
    └── clients/                        # real-client e2e + dual-version suites
        ├── harness.rs                   # spawn acp-bridge subprocess + JSON-RPC helpers
        ├── zed_style.rs                 # 6 tests
        ├── inspector_style.rs           # 6 tests
        ├── minimal_style.rs             # 5 tests
        ├── notifications.rs             # 4 tests
        └── protocol_version.rs          # 6 tests (v1 / v2 dual)
```

---

## Scope 1 — Commit `cec3c36` (0.8.0 + 0.8.1 + 0.8.2)

### How to read it

```bash
cd /workspace/acp-bridge
git show --stat cec3c36     # 22 files, +3583 / -160
git show cec3c36            # full diff
git log --oneline -5
```

The commit body itself is structured (Breaking / Added / Migration) and is the authoritative summary. Your job is to verify the code matches what the commit message claims, not to re-derive intent.

### What this commit covers

#### 0.8.0 — ACP v1 wire-format conformance

Trigger: <https://github.com/BlakeHung/acp-bridge/issues/13> (Meuxe used UUID string request IDs; acp-bridge silently dropped them, so `initialize` never returned).

- **`RequestId` enum** (`src/protocol.rs`) — `Number(u64) | String(String)`. Preserves numeric + UUID ids verbatim in responses.
- **`session/update` replaces `session/notify`** (`src/acp.rs::send_session_update`) — every outgoing notification carries `sessionId`.
- **`tool_call` / `tool_call_update` carry `toolCallId` / `kind` / `status`** — `kind` from `acp::kind_for_tool(name)`. Engine's `Notification::ToolStart` / `ToolDone` became struct variants with explicit `id` field.
- **`agent_thought_chunk` carries typed `content`** — `{"type": "text", "text": ""}` (empty string is intentional; clients render the thought bubble).
- **`initialize` defaults `promptCapabilities.image` to `false`** — opt-in via `LLM_SUPPORTS_IMAGE=true` or `[llm].supports_image = true`.
- **`session/prompt` response carries `stopReason`** — one of `end_turn` / `max_turn_requests` per ACP v1. Legacy `status` / `text` fields kept alongside for the OpenAB pipeline.

Spec reference: <https://agentclientprotocol.com/protocol/initialization>

#### 0.8.1 — Real-client e2e + graceful handling

- **`acp::send_error_with_data()`** helper — attaches a `data` payload to JSON-RPC errors.
- **5 graceful rejects** with `data.reason`:
  - `session/load`, `session/resume` → `-32001` `data.reason: "no_persistence"`
  - `session/set_mode` → `-32602` `data.reason: "no_modes"`
  - `auth/login`, `auth/logout` → `-32601` `data.reason: "no_auth_methods"`
  - `fs/*` → `-32601` `data.reason: "agent_does_not_call_client_fs"`
  - `terminal/*` → `-32601` `data.reason: "agent_does_not_call_client_terminal"`
- **`docs/scope.md`** (new file) — public-facing capability matrix.
- **`tests/clients/`** — spawns acp-bridge as a subprocess and drives it through the protocol sequences actually emitted by named Clients. Init payloads are pulled from each Client's source code, not guessed:
  - `zed_style.rs` mirrors `zed-industries/zed/crates/agent_servers/src/acp.rs::client_capabilities_for_agent`
  - `inspector_style.rs` mirrors `newioapp/acp-inspector/src/main/acp-connection-manager.ts`
  - `minimal_style.rs` mirrors Codex CLI adapter minimal init shape
- **Edge case fix:** `engine.rs::extract_text_parts` handles `ContentBlock::ResourceLink` (converts to `[Attached resource: <name> (<uri>)]` pseudo-line). ACP v1 MUST accept ResourceLink; acp-bridge was rejecting pure-ResourceLink prompts with `MissingParam`.

#### 0.8.2 — Spec notifications + tool surface + structured errors

Spec references: <https://agentclientprotocol.com/protocol/agent-plan>, <https://agentclientprotocol.com/protocol/v1/slash-commands>, <https://agentclientprotocol.com/protocol/v1/session-usage>

- **Helpers in `src/acp.rs`**: `PlanEntry`, `notify_plan`, `notify_session_info`, `notify_usage`, `AvailableCommand`, `notify_available_commands`.
- **Engine hooks** in `engine.rs::session_new` / `session_prompt`:
  - After `session/new` → `available_commands_update` (slash-command menu: `/read`, `/ls`, `/search`, `/edit`, `/shell`) + `session_info_update` (title defaults to cwd basename).
  - After `session/prompt` → `usage_update` (estimated `used` via chars/4, `size` from `LLM_MODEL_CONTEXT` or default 32768) + `session_info_update` with derived title from first non-empty line of the user prompt.
- **`LlmConfig.context_size`** — env `LLM_MODEL_CONTEXT`, config `[llm].model_context`, default 32768.
- **`engine.rs::estimate_tokens`** — chars / 4 heuristic. Documented as approximate.
- **8 new built-in tools** in `src/tools.rs`:
  - `write_file` — sandboxed, 5 MB cap, rejects `..` escape.
  - `edit` — surgical string replacement; refuses ambiguous (count ≠ 1) / missing matches.
  - `web_fetch` — HTTP/HTTPS, 5 MB body cap, 30 s timeout. **Off by default**: requires `LLM_WEB_ALLOWLIST` (comma-separated host suffixes). Empty allowlist blocks every host.
  - `bash`, `git_status`, `git_diff`, `git_log`, `git_commit`.
- **`reduce_html`** (in `src/tools.rs`) — strip `<script>`/`<style>`, remove tags, collapse whitespace, decode 5 common entities.
- **`LlmErrorKind`** (`src/llm.rs`) — 9 categories with stable `as_str()` and `is_retryable()`:

  | Kind | Stable string | Retryable? | Trigger |
  |---|---|---|---|
  | `Unreachable` | `backend_unreachable` | ✓ | Connection refused / DNS / socket |
  | `Timeout` | `timeout` | ✓ | `LLM_TIMEOUT` exceeded |
  | `RateLimited` | `rate_limited` | ✓ | HTTP 408 / 429 |
  | `ServerBusy` | `server_busy` | ✓ | HTTP 5xx |
  | `Auth` | `auth_error` | ✗ | HTTP 401 / 403 |
  | `BadRequest` | `bad_request` | ✗ | HTTP 400 / 422 |
  | `NotFound` | `not_found` | ✗ | HTTP 404 |
  | `ParseError` | `parse_error` | ✗ | Response not JSON |
  | `Unknown` | `unknown` | ✗ | catch-all |

  Wire contract: failed turns emit `error.data.category` + `error.data.retryable` in the JSON-RPC response.

### Tests for scope 1

```bash
cd /workspace/acp-bridge
cargo test -- --test-threads=1
# 162 tests total:
#   87 lib unit
#   31 integration
#   16 real-client e2e (zed / inspector / minimal)
#    4 spec notifications
#    6 acp_test + 13 protocol_test + 4 llm_test + 1 main bin
```

### Review focus for scope 1

**Critical (wire contract):**
- `src/protocol.rs::RequestId` echoes numeric / UUID ids verbatim. Diff in the JSON output between `id` sent by Client and `id` in our response must be zero.
- `src/acp.rs::kind_for_tool(name)` maps tool names to ACP v1 `ToolKind` enum (`read | edit | delete | move | search | execute | fetch | think | other`).
- `src/llm.rs::LlmErrorKind::as_str()` returns stable strings — these are part of the public contract. Renaming breaks Clients.
- `src/tools.rs::resolve_sandboxed_path` must reject absolute paths and `..` escapes. `execute_write_file` enforces this with a `..` rejection loop.
- `src/tools.rs::web_fetch_allowed_hosts` empty default blocks everything — safety boundary.

**Important (correctness):**
- `src/tools.rs::execute_edit` fails on missing / ambiguous / empty `old_text` rather than silently corrupting the file.
- `src/llm.rs::send_with_retry` distinguishes retryable (408/429/5xx + timeout/connect) from non-retryable (4xx other).
- `src/engine.rs::estimate_tokens` is documented as approximate.
- `tests/clients/` init payloads are pulled from real Client source code, not guessed.

**Style (adherence to nits):**
- `engine.rs::ProtocolVersion` (added in scope 2 below) — make sure you understand the wire shape changes in scope 2 before reviewing the `initialize` function for v1 correctness.

---

## Scope 2 — Working-tree for 0.9.0 (uncommitted)

### How to read it

```bash
cd /workspace/acp-bridge
git status                                # should show ~10 modified files
git diff                                   # full diff vs commit cec3c36
git diff src/protocol.rs src/acp.rs src/engine.rs src/main.rs src/llm.rs tests/clients/protocol_version.rs
```

Files touched in this scope (per `git status`):

| Status | File | Reason |
|---|---|---|
| M | `Cargo.toml` | bump 0.8.2 → 0.9.0 |
| M | `Cargo.lock` | auto |
| M | `CHANGELOG.md` | 0.9.0 entry |
| M | `src/protocol.rs` | `ProtocolVersion` enum, `Session` carries `protocol_version` |
| M | `src/acp.rs` | `notify_*_for(version, ...)` dispatchers + `notify_*_v2` impls + `notify_state_idle_for` |
| M | `src/engine.rs` | `initialize(protocol_version)`, `session_new(protocol_version)`, `session_new_post_create_notifications(protocol_version)`, `AppState: Clone` (sessions use `Arc<RwLock<...>>`) |
| M | `src/llm.rs` | `LlmConfig: Clone` (reqwest `Client` is cheap-clone) |
| M | `src/main.rs` | `negotiate_protocol_version(params)`, `run_acp_loop(mut state: Arc<AppState>)` for `Arc::make_mut`, all emit sites switched to `_for` versions |
| M | `tests/clients/harness.rs` | `#[allow(dead_code)]` on `Agent::child` and `Agent::shutdown` |
| M | `tests/protocol_test.rs` | `Session::new` calls updated to pass `ProtocolVersion::V1` |
| A | `tests/clients/protocol_version.rs` | 6 new e2e tests for v1 / v2 dual |

### What this scope does

Adds ACP **v2** wire-format support. **The intent is to keep acp-bridge working as the ACP spec evolves**, not to special-case any particular AI. The ACP working group's v2 spec (<https://agentclientprotocol.com/protocol/v2/initialization>) is a public document; we follow it.

When a Client sends `initialize` with `protocolVersion: 2`, acp-bridge negotiates v2 and emits v2-shaped `session/update` payloads. When a Client sends v1 (or omits the field), acp-bridge falls back to v1. Same code base; dispatch happens at the `acp::notify_*_for(version, ...)` boundary.

#### Concrete v2 differences implemented

| Aspect | v1 wire | v2 wire |
|---|---|---|
| `InitializeResponse` | `agentInfo` + `agentCapabilities` | Unified `info` + `capabilities` (role-agnostic) |
| `promptCapabilities.image` | `true` / `false` | `{}` (capability marker) or omit |
| `promptCapabilities.audio` / `embeddedContext` | `false` always | Omitted by default (absent = not supported) |
| Tool start | `tool_call` sessionUpdate with `toolCallId`/`kind`/`status: "in_progress"` | **`tool_call_update`** sessionUpdate with same fields. Legacy `tool_call` is **not emitted** for v2 Clients. |
| Tool end | `tool_call_update` | Same. |
| Plan | `{sessionUpdate: "plan", entries[]}` | `{sessionUpdate: "plan_update", plan: {type: "items", planId, entries[]}}` — `planId` lets Clients track multiple plans independently. |
| Session info | `{title, updatedAt?}` | Same. |
| Token usage | `{used, size, cost?}` | Same, but `cost.currency` is enforced as `^[A-Z]{3}$`. |
| Slash commands | `{availableCommands[]}` | Same. |
| End-of-turn | (none) | **`state_update`** (Idle) with `stopReason` (`end_turn` / `max_turn_requests` / etc.). |
| Message upsert | (none) | New `agent_message` + `messageId` upsert. **Not implemented yet** in acp-bridge (we emit chunk-only, which v2 RFD allows). |
| Agent-owned terminal | (none) | New `terminal_update` / `terminal_output_chunk`. **Not implemented yet**. |

Spec reference: <https://github.com/agentclientprotocol/agent-client-protocol/blob/main/schema/v2/schema.json>

#### Negotiation

```
fn negotiate_protocol_version(params: Option<Value>) -> ProtocolVersion {
    let requested = params.get("protocolVersion").and_then(|v| v.as_u64()).map(u16);
    match requested {
        None                          => V1 (Undetermined → conservative default)
        Some(1)                      => V1 (client not above v1)
        Some(2)                      => V2 (exact alignment)
        Some(n) where n > 2          => V2 (newer than we support, downgrade)
    }
}
```

Matches the ACP spec rule and what the official Rust / TypeScript SDKs do.

#### Why `Arc::make_mut`

`AppState` carries `protocol_version` as a field. The initialize handler wants to write it. `run_acp_loop` holds an `Arc<AppState>`. The cleanest path is `Arc::make_mut(&mut state)` which gives `&mut AppState` when refcount == 1 (no clone) and clones the inner value when refcount > 1. Inside `run_acp_loop` the refcount is 1, so make_mut never actually clones.

`AppState: Clone` is hand-written: `std::sync::RwLock` doesn't implement `Clone`, so `sessions` was wrapped in `Arc<RwLock<HashMap<String, Session>>>` (the clone shares the same inner map). `LlmConfig` derives `Clone` (reqwest `Client` is cheap-clone under the hood).

### Tests for scope 2

```bash
cd /workspace/acp-bridge
cargo test -- --test-threads=1
# 168 tests total (162 from cec3c36 + 6 new v1 / v2 tests in tests/clients/protocol_version.rs)
```

New tests (all in `tests/clients/protocol_version.rs`):

| Test | Verifies |
|---|---|
| `v2_initialize_returns_unified_capabilities_shape` | v2 init emits `info` + `capabilities`, **does not** emit `agentInfo` / `agentCapabilities`. |
| `v2_tool_call_uses_tool_call_update_not_tool_call` | v2 wire has **zero** `tool_call` sessionUpdate; only `tool_call_update`. |
| `v2_emits_state_update_at_end_of_turn` | v2 emits `state_update` (Idle) with `stopReason` at end of every prompt. |
| `v2_does_not_emit_state_update_for_v1_clients` | v1 wire never sees `state_update` (it's a v2-only concept). |
| `v2_plan_uses_plan_update_with_plan_id` | v2 plan wraps entries in `plan: { type: "items", planId, entries[] }`. |
| `v1_client_gets_legacy_shapes_unchanged` | Client omitting `protocolVersion` gets v1 wire (backward compatibility). |

### Review focus for scope 2

**Critical (wire shape against ACP v2 schema):**

The single most important review question for this scope is: **does the v2 wire shape match the official `schema/v2/schema.json` line-by-line?**

- `src/engine.rs::initialize(protocol_version)` v2 branch — verify `info`, `capabilities.session.prompt.image` (object marker `{}`, not `true`), `authMethods`. Confirm `agentInfo` / `agentCapabilities` are **absent**.
- `src/acp.rs::notify_tool_start_v2` — verify `sessionUpdate: "tool_call_update"` (not `tool_call`), with `toolCallId`, `title`, `kind`, `status: "in_progress"`. The v1 `notify_tool_start` emits `sessionUpdate: "tool_call"`; that branch must NOT run for v2 Clients.
- `src/acp.rs::notify_plan_v2` — verify `plan: { type: "items", planId: "plan-<key>", entries[] }`. The v1 `notify_plan` emits entries directly on the sessionUpdate.
- `src/acp.rs::notify_state_idle_for` — must be no-op for v1 (v1 has no `state_update` concept). The v1 Clients test (`v2_does_not_emit_state_update_for_v1_clients`) guards this.
- `src/acp.rs::notify_usage_v2` — when `cost` is given, `currency` must be 3 uppercase letters. Currently we silently drop malformed currency rather than panic; verify this is the desired behavior.

**Important (negotiation):**
- `main::negotiate_protocol_version` — handle undefined, 1, 2, and `n > 2`. Undefined falls back to V1 (conservative). Numbers > 2 should warn and fall back to V2.
- `tests/clients/protocol_version.rs::v1_client_gets_legacy_shapes_unchanged` covers undefined; verify a numeric v1 request also falls through to v1.

**Structural:**
- `AppState: Clone` is hand-written because `std::sync::RwLock` is not `Clone`. Sessions are wrapped in `Arc<RwLock<HashMap<...>>>` so the clone shares the map.
- `Arc::make_mut(&mut state)` is used in `main.rs::run_acp_loop` to mutate `protocol_version`. Inside that function, refcount is 1, so `make_mut` never clones.
- `Session::protocol_version` is set at `Session::new` time and never mutated. The same value is also held on `AppState`. They are always consistent because both are set at `session/new` time from the negotiated `state.protocol_version`.

**Style:**
- `notify_*_for` dispatcher pattern matches the official SDKs: a single entry point per concept, dispatching to v1 / v2 impls.
- `cargo fmt --check` and `cargo clippy --all-targets -- -D warnings` should both be clean. If they aren't, that's a sign something is off — the project policy is zero warnings.

---

## TL;DR for the reviewer

**Commit `cec3c36` (0.8.x)** makes acp-bridge actually work with spec-compliant v1 Clients (Zed, JetBrains, ACP UI, Meuxe, Codex CLI adapter, ACP Inspector), adds 8 built-in tools so AI agents prefer spawning acp-bridge over doing their own inference, and adds structured `LlmErrorKind` so Clients can branch on failure categories.

**Working-tree for 0.9.0** adds ACP v2 wire-format support: same code base, dispatch by `protocolVersion` negotiated at `initialize`. v2 differences are wire-shape only; v1 Clients see no behavior change.

**Most important review checks across both scopes:**

1. **Wire shape against the official ACP schema** (`schema/v1/schema.json` for scope 1, `schema/v2/schema.json` for scope 2). Public contract; spec drift breaks every Client.
2. **Stable string contracts** (`LlmErrorKind::as_str`, `data.reason` values, `kind_for_tool` enum strings). These are wire-stable.
3. **Sandbox boundaries** — `resolve_sandboxed_path` (path traversal), `execute_write_file` (`..` reject), `web_fetch_allowed_hosts` (empty allowlist blocks). Local-model runs often have looser OS-level isolation than hosted agents.
4. **Negotiation correctness** — `negotiate_protocol_version` for v1, v2, undefined, and `n > v2`.
5. **Wire order** — notifications emitted before the JSON-RPC response (so buffer-per-turn Clients receive everything in one read).
6. **Test fidelity** — `tests/clients/` init payloads must come from real Client source code, not guesswork.