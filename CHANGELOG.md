# Changelog

All notable changes to this project will be documented in this file.

Format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).
This project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Fixed
- `session/cancel` aborts an active ACP prompt and reports cancellation on both wire versions; concurrent prompts for one session are rejected; a cancelled turn is closed in history with a `[cancelled by user]` reply.

## [0.9.2] - 2026-10-06

### Added
- **Thinking-mode tool-call recovery** — reasoning models (DeepSeek-R1,
  Qwen 2.5/3, GLM) frequently emit tool-call JSON inside the content
  channel instead of the structured `tool_calls` field. acp-bridge
  previously treated such responses as a final text answer, meaning the
  agent never dispatched the requested tool and the session stalled.
  The engine now sanitizes the reasoning scaffolding
  (`strip_thinking_blocks`) and recovers embedded tool-call JSON —
  fenced \`\`\`json / \`\`\`tool_call blocks, bare balanced objects with
  `name` + `arguments`/`args`/`parameters`/`input`, or the whole-call
  `{"function": {…}}` shape — reproducing the OpenAI-style tool_call
  objects the structured path would have produced. Recovered rounds
  continue the normal agentic loop and are logged for debugging.
  Eleven new unit tests (`thinking_recovery_tests`). Project page:
  [wchung.tw/acp-bridge/](https://wchung.tw/acp-bridge/).

## [0.9.1] - 2026-10-03

### Fixed — ACP v2 wire-shape blockers
The 0.9.0 release shipped with three wire-shape violations against the
v2 protocol that a spec-compliant v2 Client (e.g. an early OpenCode v2
preview) would have rejected. All three are addressed in this release:

- **`state_update` discriminator** — the v2 schema requires every
  `state_update` payload to have a `state` field set to one of
  `"running" | "idle" | "requires_action"`. The previous code emitted
  `"available": false` (a non-spec field) and was missing `state`. Now
  emits `{sessionUpdate: "state_update", state: "idle", stopReason?}`
  per the `IdleStateUpdate` schema.
- **`session/prompt` v2 response carries `messageId`** — the v2
  `PromptResponse` is `{required: ["messageId"]}`. The previous v1-style
  response (`{stopReason, status, text}`) would have failed schema
  validation. acp-bridge now mints a UUID-derived messageId per prompt
  and emits the v2 wire shape for v2 Clients. The v1 wire is unchanged.
  Legacy `status` / `text` / `stopReason` now live on `state_update`
  for v2 Clients, exactly as the v2 spec requires.
- **v2 session lifecycle methods** — the v2 baseline includes
  `session/new | session/list | session/resume | session/close |
  session/prompt | session/cancel | session/update`. acp-bridge
  previously only implemented the v1 surface (`session/new`,
  `session/end`, `session/prompt`, `session/cancel`). New:
  - `session/close` (v2 baseline; shares the implementation with
    `session/end`)
  - `session/list` (v2 baseline; returns active sessions as
    `{sessions: [{sessionId, cwd}], nextCursor: null}`)
  `session/delete` (v2 optional) and `session/resume` /
  `session/load` return graceful `-32601 not_implemented` /
  `-32001 no_persistence` rejections with the stable `data.reason`
  field.

### Fixed — Ollama native protocol bugs
Three pre-existing bugs in the Ollama native code path that had been
documented as fix-plan priorities P0-C. None were wired through the
test suite (tests pointed the harness at `127.0.0.1:1`, so Ollama
native was never exercised in CI):

- **`tool.arguments` object vs string** — Ollama native `/api/chat`
  returns `function.arguments` as a JSON object; OpenAI-compatible
  backends return it as a JSON-encoded string. The previous code called
  `as_str()` on the value and silently fell back to `"{}"` when it
  wasn't a string, which meant **every** Ollama native tool call ran
  with empty arguments. Now handles object / array / string uniformly.
- **`options.*` sampling fields** — Ollama native wants
  `temperature` and `max_tokens` (renamed `num_predict`) inside an
  `options` object; OpenAI-compatible expects them at the top level.
  The previous code only sent top-level fields, so every Ollama
  native request silently used the model's defaults for sampling.
- **`format_tool_result` Ollama field** — Ollama native tool messages
  use `{"role": "tool", "content": …}` and ignore (some versions
  reject) the `tool_call_id` field that acp-bridge included. The new
  helper emits `tool_call_id` only when the backend is not Ollama
  native.

### Fixed — sandbox escapes
- **`search_code` walks symlinks** — previous code used `path.is_dir()`
  / `path.is_file()`, which follow symlinks. A symlink inside the
  sandbox pointing at `/etc/passwd` (or anywhere outside `working_dir`)
  would be read and its contents returned. Now:
  - Skip entries whose `symlink_metadata` reports `file_type().is_symlink()`
  - Canonicalize the entry and skip it if it does not start with the
    canonicalized working dir
  - Bound recursion depth to `MAX_LIST_DEPTH * 4` to prevent
    adversarial directory structures
- **`web_fetch` redirects bypass `LLM_WEB_ALLOWLIST`** — reqwest's
  default redirect policy follows up to 10 hops to *any* host. A
  server on an allowlisted domain could 302 to an internal host and
  acp-bridge would happily return the body. Now uses
  `redirect::Policy::custom` that re-validates the next hop's host
  against `LLM_WEB_ALLOWLIST` on every redirect, with a 5-hop cap.

### Changed
- **`docs/scope.md` synced with current state** — the previous version
  still said "Not a v2 protocol agent yet" (incorrect as of 0.9.0),
  listed the wrong tool set (5 tools instead of the 11 actually
  shipped in 0.8.2), and referenced a `codex_style.rs` test file
  that was renamed to `minimal_style.rs` long ago. Now reflects the
  real v1 / v2 dual implementation, the full tool surface, and the
  current `tests/clients/` layout.
- **`CHANGELOG.md` corrections** — 0.9.0 entry over-claimed
  `Session::protocol_version` is read at emit sites; the field is
  stored on each Session but the emit helpers currently use
  `AppState.protocol_version` (one Client per process in practice).
  0.8.2 entry under-reported the test count.

### Tests
171 tests total (from 168 in 0.9.0). Added in `tests/clients/protocol_version.rs`:
- `v2_session_prompt_response_carries_message_id` — asserts the v2
  `PromptResponse` carries the required `messageId` and does **not**
  carry the legacy v1 `stopReason` / `status` fields.
- `v2_session_close_succeeds_and_v2_session_delete_gracefully_rejects`
  — confirms the new `session/close` routing and the stable
  `data.reason: "not_implemented"` error on `session/delete`.
- `v2_session_list_returns_session_info_with_cwd` — asserts
  `session/list` returns the active sessions in the v2 wire shape.

Hardened existing tests:
- `v2_emits_state_update_at_end_of_turn` now asserts the
  `state: "idle"` discriminator (the previous version only checked
  the discriminator string, which let the bug through).
- `tests/clients/inspector_style.rs::inspector_style_session_*_returns_method_not_found_gracefully`
  were updated to positive tests — the methods are now implemented
  and the previous negative assertions no longer held.

## [0.9.0] - 2026-10-02
This project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.9.0] - 2026-10-02

### Added
- **ACP v2 protocol support** — acp-bridge now negotiates the wire-format
  version at `initialize` time and emits either v1 or v2
  `session/update` payloads depending on what the Client requested.
  The intent is to keep acp-bridge working as the ACP spec evolves,
  not to special-case any particular AI — Zed / JetBrains / ACP UI /
  Meuxe / ACP Inspector that ship v1 today still get the v1 wire they
  were written against; future v2-only Clients (or Clients that
  advertise v2 in their `initialize`) get the v2 wire.

  Concrete v2 differences acp-bridge now implements:
  - `InitializeResponse` uses unified `info` + `capabilities` fields
    (v1's `agentInfo` / `agentCapabilities` aliases are not emitted
    on the v2 wire)
  - `promptCapabilities.image` is advertised as `{}` (capability
    marker object) on v2, not `true`
  - `tool_call` sessionUpdate is **not emitted** for v2 Clients;
    the loop envelope and per-tool start both use
    `tool_call_update` with `status: "in_progress"` (keyed by
    `toolCallId`, same schema as v1)
  - `plan` becomes `plan_update` with `plan: { type: "items", planId,
    entries[] }` (the `planId` lets Clients track multiple plans
    independently)
  - `available_commands_update` and `session_info_update` use the
    v2 shape (schema-compatible with v1 but routed through the
    unified v2 emit path)
  - `usage_update` enforces the v2 ISO 4217 currency pattern
    (`^[A-Z]{3}$`) on the optional `cost` field when supplied
  - A new `state_update` (Idle) notification with `stopReason` is
    emitted at the end of every prompt turn for v2 Clients. v1
    Clients do not receive this notification (they have no equivalent)

  Helpers added in `src/acp.rs`:
  - `ProtocolVersion` enum with `V1`, `V2`, `LATEST` constants
  - `notify_*_for(version, session_id, ...)` dispatchers that route
    to the v1 or v2 emit functions
  - `notify_state_idle_for()` for the v2-only `state_update`
  - The plain (no-suffix) `notify_*` functions stay as the v1
    shortcut for code paths that have not migrated

- **`AppState` is now `Clone`** — the negotiation step in
  `run_acp_loop` writes the agreed version into the state via
  `Arc::make_mut`. Sessions are stored in an `Arc<RwLock<...>>` so
  the clone shares the same map; cloning happens only when the
  `Arc` is shared (in `run_acp_loop` it has refcount 1).
- **`Session` carries its negotiated `ProtocolVersion`** for
  defense-in-depth and future per-session overrides. In the current
  single-Client-per-process model the global `AppState.protocol_version`
  is what emit helpers actually use (the same value is written into
  every Session opened by that Client); `Session::protocol_version`
  exists so a future multi-Client / per-session routing layer can
  switch on it without changing the call sites.
- **`negotiate_protocol_version(params)`** — new helper in
  `main.rs` that implements the ACP spec's "pick the highest version
  we both support" rule. Clients that omit `protocolVersion` fall
  back to v1 (the conservative default).
- **6 new e2e tests in `tests/clients/protocol_version.rs`** that
  construct minimal v1 and v2 Clients and assert the wire shapes
  match the published schema.

### Migration notes
- **v1 Clients are unchanged.** The plain `acp::notify_*` helpers
  still emit v1, the `initialize` response is identical to 0.8.2 when
  the Client is v1. Existing Clients that omit `protocolVersion`
  continue to work without changes.
- **v2 Clients get a new `state_update` notification** that v1
  Clients never see. Spec-compliant Clients ignore unknown
  discriminators so this is safe.
- **Tool calls on v2 use `tool_call_update` only** — Clients
  written against the v1 `tool_call` discriminator must use the
  `status: "in_progress"` `tool_call_update` instead.

## [0.8.2] - 2026-10-02

### Added

- **New tools** — four more built-in tools round out the surface so AI
  agents can stay inside acp-bridge instead of falling back to
  their own knowledge:
  - **`edit`** — surgical string replacement. Replace exactly one
    occurrence of `old_text` with `new_text` in an existing file.
    Refuses to act if `old_text` is missing or appears more than once,
    so the model has to re-read the file rather than guess.
  - **`write_file`** — create or overwrite a file with new content.
    Sibling to `edit` for whole-file rewrites; rejects `..` escapes.
  - **`web_fetch`** — fetch a URL over HTTP/HTTPS and return the body
    as text. HTML is reduced to readable text (scripts/styles
    stripped, tags removed, whitespace collapsed). 5 MB body cap,
    30 s timeout.
    **Off by default**: requires `LLM_WEB_ALLOWLIST` to be set to a
    comma-separated list of host suffixes (e.g.
    `LLM_WEB_ALLOWLIST=docs.rs,crates.io`). Empty allowlist blocks
    every request. This is an opt-in safety boundary so a
    misconfigured sandbox cannot exfiltrate to internal
    infrastructure.
  - **`git_status`**, **`git_diff`**, **`git_log`**, **`git_commit`** —
    read-only and write git operations, all run inside the session
    working directory. `git_diff` accepts an optional `path` and a
    `staged: true` flag (`--cached`). `git_log` accepts `max_count`
    (clamped to 1–200, default 20) and an optional `path` filter.
    `git_commit` stages the listed `paths` (or `git add -u` when
    `paths` is omitted) and commits with the supplied message.
- **`acp-bridge` now publishes four `plan` notification helpers** —
  `acp::PlanEntry`, `acp::notify_plan`, `acp::notify_session_info`,
  `acp::notify_usage`, `acp::AvailableCommand`,
  `acp::notify_available_commands`. Engine hooks fire
  `available_commands_update` and `session_info_update` immediately
  after `session/new`, and `usage_update` + `session_info_update`
  after `session/prompt` returns.
- **`LlmConfig.context_size`** — model context window in tokens,
  surfaced as `usage_update.size`. Override via `LLM_MODEL_CONTEXT`
  env var or `[llm].model_context` config field (default 32768).
- **`PromptResult::usage`** carries an estimated `used` token count
  (chars / 4 across the session history) so the `usage_update` has
  a number to ship. Local backends rarely stream stable per-turn
  token counts; this is intentionally approximate.
- **`acp::kind_for_tool`** now classifies `edit`, `write_file`,
  `web_fetch`, and the four `git_*` tools so Clients render the
  right icon and affordance.
- **Classified backend errors** — `LlmErrorKind` (`Unreachable`,
  `RateLimited`, `ServerBusy`, `Auth`, `BadRequest`, `NotFound`,
  `Timeout`, `ParseError`, `Unknown`) and `LlmError::is_retryable()`.
  `chat` and `stream_chat` now return `Result<_, LlmError>` instead
  of plain strings. Failed turns emit a structured
  `error.data.category` + `error.data.retryable` in the JSON-RPC
  response so Clients can branch on it (e.g. "auto-retry on
  `backend_unreachable`, show 'check your model name' on
  `not_found`").

### Changed

- **Wire order on `session/prompt` and `session/new`** — the
  post-event notifications (`available_commands_update`,
  `session_info_update`, `usage_update`) are emitted **before** the
  JSON-RPC response, not after. Clients that buffer the entire
  notification stream per turn (most ACP Clients) see the
  notifications bound to the right sessionId; Clients that read
  strictly one-line-at-a-time still get the response on the line
  after the notifications.
- **`PromptResult` gains `error_class: Option<LlmErrorKind>` and
  `error_retryable: bool`** so the engine's failure classification
  reaches the JSON-RPC response without string-matching.

### Tests

- 14 new unit tests (`src/tools.rs`): write / edit (unique match,
  missing, ambiguous, empty), web_fetch allowlist enforcement, HTML
  reduction, git status.
- 3 new unit tests (`src/llm.rs`): `LlmErrorKind::as_str` stability,
  retryable classification, status-code → kind mapping.
- All existing test suites still pass; 168 tests total (after 0.9.0 added 6 v1 / v2 e2e cases in `tests/clients/protocol_version.rs`).

## [0.8.1] - 2026-10-01

### Added
- **`docs/scope.md`** — single source of truth for what acp-bridge supports,
  supports with caveats, and deliberately does not implement. Clients pick
  acp-bridge expecting certain capabilities; this doc sets expectations
  before `initialize` is even called.
- **Real-Client e2e tests** (`tests/clients/`) — three test suites that
  spawn acp-bridge as a subprocess and drive it through the protocol
  sequences actually emitted by named Clients:
  - `tests/clients/zed_style.rs` — Zed Industries' editor (full client
    capabilities, image + text prompt, capability probes)
  - `tests/clients/inspector_style.rs` — ACP Inspector (minimal
    capabilities, request-id fuzz, ResourceLink prompt handling)
  - `tests/clients/minimal_style.rs` — Codex CLI adapter and stripped-down
    Clients (bare capabilities, notification handling)
- **`ContentBlock::ResourceLink` support** — ACP v1 requires Agents to
  accept ResourceLink blocks in `session/prompt`. acp-bridge now converts
  ResourceLinks to `[Attached resource: <name> (<uri>)]` pseudo-lines so
  the LLM has something to act on. Clients such as ACP Inspector that
  attach files via the "Attach" button previously saw
  `MissingParam prompt (expected non-empty text or image content)` errors.
- **Graceful error responses with `data.reason`** — capability-mismatch
  errors now carry a stable `error.data.reason` field Clients can switch
  on for log filtering:
  - `session/load`, `session/resume` → `-32001` with `reason: "no_persistence"`
  - `session/set_mode` → `-32602` with `reason: "no_modes"`
  - `auth/login`, `auth/logout` → `-32601` with `reason: "no_auth_methods"`
  - `fs/read_text_file`, `fs/write_text_file` → `-32601` with
    `reason: "agent_does_not_call_client_fs"`
  - `terminal/*` → `-32601` with `reason: "agent_does_not_call_client_terminal"`

### Changed
- **`session/load` / `session/resume`** — error code changed from `-32601`
  to `-32001` to better reflect the "this is a server-side capability
  gap, not a malformed request" nature of the failure. The message and
  the new `data.reason` field give Clients everything they need.
- **`session/set_mode`** — error code changed from `-32601` to `-32602`
  (invalid params) for the same reason.

### Internal
- New `acp::send_error_with_data()` helper for attaching machine-readable
  context to JSON-RPC error responses.

## [0.8.0] - 2026-09-30

### Breaking Changes
- **`session/update` replaces `session/notify`** — outgoing notifications now use the
  method name mandated by ACP v1. Clients written against the older `session/notify`
  shape (legacy openab pipelines, some custom harnesses) will stop receiving
  updates and must be updated. Reviewer-flagged via [issue #13](https://github.com/BlakeHung/acp-bridge/issues/13).
- **`initialize` no longer advertises `promptCapabilities.image: true` by
  default** — image support is now opt-in via `LLM_SUPPORTS_IMAGE=true` or
  `[llm].supports_image = true`. Previously the agent unconditionally claimed
  image support, which caused spec-compliant Clients (Meuxe, ACP UI, Codeg,
  Gold Band, Casper, DeepChat, …) to forward image attachments to local
  backends without a vision model and surface upstream as empty replies.
- **`tool_call` / `tool_call_update` now require `toolCallId`** — the id is
  required by ACP v1; clients use it to pair start/update notifications into a
  single tool-call timeline. Emitting notifications without the id caused
  Clients to render the call without a coherent start/end pair or drop the
  update entirely. The id is sourced from the LLM's `tool_call.id` field
  (the synthetic outer `llm_chat` envelope uses `llm_chat:<session_id>`).
- **`tool_call` now carries `kind` and `status: "in_progress"`** — required
  by ACP v1. `kind` is derived from the tool name (`read_file`/`list_dir` →
  `read`, `write_file` → `edit`, `shell`/`bash` → `execute`, `search`/`grep`
  → `search`, `fetch` → `fetch`, unknown → `other`).
- **`agent_thought_chunk` now carries a typed `content` block** — empty
  `content: { type: "text", text: "" }` is emitted so spec-compliant Clients
  render the thought bubble correctly. Earlier versions emitted the
  sessionUpdate with no content, which Clients rejected.

### Added
- **`LLM_SUPPORTS_IMAGE` env var / `[llm].supports_image` config option** —
  opt back in to advertising the `image` prompt capability for backends that
  can actually accept image content blocks.
- **`acp::kind_for_tool(name) -> &'static str`** — maps tool names to the
  ACP `ToolKind` enum; reusable from external integrations that build their
  own session-update payloads.
- **Regression tests for ACP v1 wire-format conformance** — three new
  integration tests (`test_initialize_does_not_advertise_image_by_default`,
  `test_tool_call_notification_carries_tool_call_id_and_kind`,
  `test_thought_chunk_carries_content_block`) lock the new shape down so
  future refactors cannot silently regress Meuxe / ACP UI compatibility.
- **Unit tests for `acp::kind_for_tool`** and the new `Notification::ToolStart`
  / `Notification::ToolDone` struct variants in `engine.rs`.

### Changed
- **`Notification::ToolStart` / `Notification::ToolDone`** are now struct
  variants with an explicit `id` field so the engine propagates the LLM-
  supplied `tool_call.id` end-to-end into the ACP notification.
- **`agent_thought_chunk` payload** — see Breaking Changes above; the
  sessionUpdate kind stays the same, the payload gains `content`.
- **`Cargo.toml`** `version` bumped to `0.8.0` to reflect the wire-format
  break.

### Migration Notes
- **Clients that consumed `session/notify`** must switch to reading
  `session/update`. If you maintain a custom harness, see
  <https://agentclientprotocol.com/protocol/session-setup>.
- **Operators who actually run a vision-capable model** (e.g. LLaVA,
  Qwen-VL, Pixtral) should set `LLM_SUPPORTS_IMAGE=true` so Clients know
  to forward image attachments. Leave it unset otherwise — the new default
  matches the ACP v1 spec and avoids surprise image traffic to local
  backends.

### Cross-references
- Issue #13 (Meuxe string/UUID request id) was the trigger for the
  wire-format audit that produced this release. 0.7.8 already shipped the
  request-id fix; 0.8.0 closes out the remaining conformance gaps surfaced
  by surveying Meuxe, ACP UI, Codeg, Gold Band, Casper, and DeepChat.

## [0.7.8] - 2026-07-27

### Breaking Changes
- **A2A mode removed** — `--a2a` HTTP server with Agent Card support is no longer available
- **Client mode removed** — `--client` external ACP agent spawning is no longer available
- **Configuration changes** — `[a2a]` and `[agent]` sections in config.toml are no longer supported

### Added
- **Backend abstraction layer** — New `Backend` enum (Ollama/OpenAi) encapsulates protocol-specific logic for message formatting, response extraction, and tool-call handling
- **Simplified architecture** — Focused ACP-only adapter with cleaner codebase
- **Benchmark mode** — Added `--bench` flag for performance testing

### Removed
- **A2A implementation** — src/a2a.rs (299 lines) deleted
- **Client implementation** — src/client.rs (869 lines) deleted  
- **Client tests** — tests/client_test.rs (173 lines) deleted
- **Marketing materials** — DEMO-AND-MARKETING.md (718 lines) and marketing-drafts.md (176 lines) deleted
- **Dependencies** — axum and libc crates removed from runtime dependencies (axum kept as dev-dependency for tests)

### Changed
- **README updates** — Removed --a2a HTTP server mention, added --bench example, updated Project status to reflect ACP-only scope
- **Configuration system** — Simplified to only support LLM configuration
- **Help text** — Updated to reflect ACP-only positioning (removed --a2a and --client options)
- **Project status** — Updated to reflect v0.7.8 ACP-only scope

### Internal
- **Backend-specific logic moved** — Protocol quirks moved from engine.rs to llm.rs Backend enum
- **Code reduction** — 1,649 lines of code removed overall
- **Simplified RunMode enum** — Now only Acp and Bench modes

### Migration Notes
Users relying on A2A mode should migrate to ACP mode with their ACP harness. Users using client mode should configure their harness to spawn acp-bridge directly via stdin/stdout JSON-RPC.

## [0.7.7] - 2026-06-02

### Fixed
- **`session/prompt` final response was missing the accumulated text** — `handle_acp_prompt` previously sent the assistant's final text exclusively through `Notification::TextChunk` and replied with `{"status": "completed"}`. Upstream pipelines that consume the final response (or that treat `ToolDone("llm_chat","completed")` as the turn boundary and stop reading further notifications) saw an empty reply even though the chunks had been streamed. The final response now also carries `text: result.text` so non-streaming consumers and edge-case race conditions still get the body. Reviewer-flagged by Eren.

## [0.7.6] - 2026-06-02

### Fixed
- **`<sender_context>` metadata in user prompts made models emit empty replies with no tool calls** — OpenAB-style harnesses prepend a `<sender_context>{…json…}</sender_context>` block to the user message. Several local LLMs (observed on Qwen3-Coder via Ollama) interpret the XML wrapper as a directive and stall — the model returns empty content with no tool calls, which surfaces upstream as "the agent doesn't reply" and "the agent doesn't know about brain/KB". `engine::strip_sender_context` now detects the block, removes it from the forwarded user text, and logs the captured inner string at debug level for traceability. Both the ACP (`handle_acp_prompt`) and A2A (`handle_message_send`) entry points strip before the empty-prompt guard and before passing to `session_prompt`. Four unit tests cover the leading-block case, the no-block passthrough, an unterminated open tag, and the all-metadata edge case. Reviewer-flagged by openab-rukawa.

## [0.7.5] - 2026-06-01

### Fixed
- **Inbound image MIME type was discarded and rewritten as JPEG** — both `engine::extract_image_parts` and the per-block path inside `session_prompt` had only kept the base64 data and hard-coded `data:image/jpeg;base64,…` when forwarding to OpenAI-compatible backends. Any ACP/A2A client sending PNG/WebP/GIF content was therefore mislabeled, which can break vision-model decoding or yield undefined multi-modal behaviour. Image extraction now returns a new `ImageBlock { data, mime_type }`, the per-block `mimeType` is threaded through, and `session_prompt` emits `data:<mime>;base64,<data>` using the client's declared MIME (with `image/jpeg` only as a fallback for clients that omit the field). Reviewer-flagged by Eren.
- **A2A transport silently dropped image inputs** — `handle_message_send` only extracted text from `message.parts` and always called `engine::session_prompt(..., &[], None)`, so text+image A2A requests lost their images and image-only A2A requests were rejected as empty. `initialize()` already advertises `agentCapabilities.promptCapabilities.image: true`, so the A2A path now honors it: both text and image parts are extracted, an empty prompt is rejected only when *both* are absent, and images are forwarded to `session_prompt`. Reviewer-flagged by Eren.

## [0.7.4] - 2026-06-01

### Fixed
- **`bench.rs` TOTAL aggregate was dragged down by error fixtures** — when a fixture timed out or failed, its `wall_ms` was still summed into the aggregate even though its completion-token count was absent. The aggregate tok/s now skips error rows. Reviewer-flagged by Mikasa.
- **OpenAI-mode `tok/s` was not labelled as wall-clock-derived** — Ollama-native mode computes tok/s from `eval_duration` (decode only), while OpenAI-compat mode divides by wall_ms (which includes TTFT and transit). The column header now shows `tok/s*` in OpenAI mode and a footnote explains the difference, so readers don't conclude OpenAI backends are slower than they actually are. Reviewer-flagged by Mikasa.

### Changed
- **`bench::Fixture` gains an `Option<&'static str> system_prompt` field** — decode-heavy fixtures (`explain_concept`, `summarize`) now run without a "concise" system prompt so they produce enough tokens to make the timing meaningful. Other fixtures keep a tight prompt because they're intentionally short. Reviewer-flagged by Mikasa.

### Internal
- **`engine.rs` user-message path** — removed a dead inner `if user_images.is_empty()` inside the OpenAI-compat else arm. The outer branch already guarantees the slice is non-empty there; the inner check could never fire. Reviewer-flagged by Eren.

## [0.7.3] - 2026-06-01

### Important
- **Skip 0.7.2 on crates.io — it is the buggy pre-fix code from a cancelled release run.** The 0.7.2 git tag and Docker image at `ghcr.io/blakehung/acp-bridge:0.7.2` point at the fixed code (commit `fc0b9c7`), but crates.io permanently locked the version at the earlier `b253172` snapshot before the cancel landed. Use 0.7.3+ from crates.io. The 0.7.2 entry below still describes the intended contents; 0.7.3 ships those plus the second-round reviewer findings.

### Fixed
- **NVIDIA product names containing commas were truncated** — `parse_nvidia_smi` now splits with `rsplitn(2, ',')` (from the right) instead of `splitn(2, ',')`, so names like "NVIDIA GeForce RTX 4090, Ada" parse correctly and the VRAM column lines up. Reviewer-flagged by Mikasa.
- **`parse_rocm_smi` rejected MB-unit VRAM** — older `rocm-smi` versions emit VRAM in bytes, newer ones emit MB. The old `> 100_000_000` filter discarded MB values entirely. Now takes the max parseable number on the row and converts only when it looks like bytes. Reviewer-flagged by Mikasa.
- **AMD Vulkan-only fallback missed cards with unprefixed / upper-case vendor IDs** — `scan_sysfs_amd` now normalizes the sysfs vendor string and accepts both `0x1002` and `1002`. Reviewer-flagged by Mikasa.
- **First fixture in `--bench` ate the cold-start cost** — `bench::run` now does a discarded warm-up `chat()` before the first measured fixture to prime model load + cache. Reviewer-flagged by Mikasa and Armin.

### Changed
- **`session/load`, `session/resume`, `session/set_mode` now return `-32601`** — these are not supported (we don't advertise `loadSession`, sessions are created without `modes`), so ACP capability-based negotiation calls for method-not-found rather than the `-32001`/`-32602` codes the previous patch used. Message strings still explain the underlying reason. Reviewer-flagged by Armin.

## [0.7.2] - 2026-06-01

### Fixed
- **`session/prompt` with non-array `prompt` parameter dropped the user message** — acp-bridge previously parsed `prompt` strictly as `Array<ContentBlock>` and used `unwrap_or_default()` on the cast, so a `prompt: "查大腦"` (string) or `prompt: {"type":"text","text":"…"}` (single block, not wrapped in an array) collapsed to an empty `Vec`, the user content became `""`, and the LLM had no message to act on. Symptom on the OpenAB side: "the agent doesn't reply". `engine::extract_user_text_from_prompt` and `extract_user_images_from_prompt` now tolerate all three shapes (array / single object / plain string). `handle_acp_prompt` rejects an entirely empty prompt (no text, no images) with `-32602` instead of forwarding empty content to the LLM. `RUST_LOG=acp_bridge=debug` now prints the JSON shape at dispatch entry. Eight unit tests cover the parser; the integration test for empty prompts was updated to assert the rejection.
- **`session/cancel` notification was silently dropped** — `JsonRpcRequest.id` was typed `u64`, so any message without an `id` field (i.e. any ACP notification) failed to deserialize and was logged at `debug!` then skipped. `id` is now `Option<u64>`, and the stdin loop splits dispatch into a request branch (id present, response expected) and a notification branch with a `session/cancel` arm that logs the cancellation. Unknown notifications are debug-logged and ignored per JSON-RPC 2.0. Reviewer-flagged by Armin.
- **`parse_rocm_smi` returned the card identifier instead of the GPU product name** — the filter `find(|s| !s.is_empty() && !s.eq_ignore_ascii_case("card"))` matched `card0` / `card1` (not exact "card"), so the parser always picked the card slot as the name. `fields.get(1)` is the correct column. Reviewer-flagged by Armin in a follow-up pass on `hardware.rs`.

### Changed
- **`main.rs`** — `RunMode::Bench` arm in the final mode dispatch changed from `todo!()` to `unreachable!("Bench mode handled above")`. Bench is handled by an earlier return, so `todo!()` was misleading. Reviewer-flagged by Armin.
- **`bench.rs`** — the TOTAL row's tok/s is now followed by an explanatory line noting that the aggregate is wall-clock based and not directly comparable to per-fixture decode tok/s. Reviewer-flagged by Armin.

### Internal
- `Cargo.lock` synced to track the version bump so `cargo publish` does not see a dirty working tree in CI (this blocked the v0.7.1 release workflow).
- Removed two ad-hoc `eprintln!` debug lines from `handle_acp_prompt` that printed the raw prompt and a 200-char prefix of user text to stderr. Use `tracing::debug!` with `RUST_LOG=acp_bridge=debug` instead.

### Notes
- v0.7.1 was tagged but its release workflow failed at the `cargo publish` step (Cargo.lock dirty). No v0.7.1 GitHub Release exists; v0.7.2 is the next published release after v0.7.0.
- Reviewer coverage: Armin/Kiro reviewed `main.rs`, `protocol.rs`, `bench.rs`, `client.rs`, `engine.rs`, `a2a.rs`, `hardware.rs` (the latter in a follow-up). Eren and Mikasa did not respond.

## [0.7.1] - 2026-06-01

### Added
- **Windows binary** — release workflow now also builds `acp-bridge-windows-amd64.exe` (x86_64-pc-windows-msvc). AMD Ryzen AI / Strix Halo laptops are largely Windows, so the binary tier needed it.
- **`--bench` mode** — `acp-bridge --bench` runs a fixed set of fixture prompts (hello / short code / explain concept / refactor / summarize) against the configured LLM endpoint and prints wall time, prompt/completion tokens, and decode tokens/sec per prompt. Reads OpenAI-style `usage` or Ollama-native `eval_count` / `eval_duration` stats.
- **Best-effort hardware detection at startup** — `src/hardware.rs` probes platform and GPU(s) using `nvidia-smi`, `rocm-smi`, and `/sys/class/drm` sysfs scan; logs Metal / CUDA / ROCm / Vulkan and operator-facing tuning hints. All offline, no network.
- **`docs/apple-silicon.md`** and **`docs/nvidia.md`** — practical setup guides covering memory tiers, model size recommendations, Ollama vs MLX vs vLLM vs llama.cpp trade-offs, and reference rigs (Mac mini M4 Pro, 2× RTX 3090 Ti).
- **Offline-first guarantee** — README now explicitly documents that `acp-bridge` makes no outbound network calls beyond the user-configured LLM endpoint (no telemetry, no update checks, no model registry lookups).

### Changed
- **Release profile** — `Cargo.toml` adds thin-LTO, `codegen-units = 1`, and `strip = true` for smaller / faster release binaries on edge deployments. Unwinding stays default.
- **a2a / engine / main code dedupe** — extracted `jsonrpc_error()` in `a2a.rs` (replaced 3 inline error envelopes) and `engine::extract_text_parts()` / `engine::extract_image_parts()` shared by both ACP and A2A prompt handlers; saved one redundant `String::clone()` in the prompt success path.
- **Spec-gap error responses** — `session/load`, `session/resume`, and `session/set_mode` now return descriptive `-32001` / `-32602` errors explaining the actual constraint (no persistence, no modes advertised) instead of a bare `-32601` method-not-found.
- **OpenAI-compat text-only prompts** — when there are no images, text content is sent as a plain string rather than a single-element array, which improves compatibility with some OpenAI-compatible backends.

### Fixed
- **`client.rs` pending HashMap leak** — three error paths in `send_request` / `send_prompt` now clean up the pending entry before returning (previously timeout / `send_raw` failure / channel closed could leave stale oneshot Senders).

## [0.7.0] - 2026-06-01

### Added
- **ACP Client mode** (`--client`) — acp-bridge can now act as an **ACP client/orchestrator**, spawning external ACP agents (OpenCode, Claude Code, Kiro, Codex, Gemini, etc.) as child processes and communicating via stdin/stdout JSON-RPC 2.0.
- **`AcpConnection`** — full-featured ACP client with process spawning, JSON-RPC request/response matching, notification streaming, and automatic `session/request_permission` auto-reply (picks most permissive option).
- **`AgentConfig`** — new `[agent]` config section and `AGENT_COMMAND`/`AGENT_ARGS`/`AGENT_WORKING_DIR` env vars for specifying which agent to spawn.
- **ACP event classification** — `classify_notification()` parses ACP notifications into typed events (`Text`, `Thinking`, `ToolStart`, `ToolDone`, `Status`) with stable `toolCallId` tracking.
- **Content blocks** — `ContentBlock` type supports text and image content for multi-modal prompts.
- **Session resume** — `session/load` support for resuming previous sessions (when agent supports `loadSession` capability).
- **Process group isolation** — spawned agents run in their own process group (`setpgid`) with clean SIGTERM→SIGKILL cleanup on drop.
- **Environment variable expansion** — `${VAR}` syntax in agent env config values.
- **16 new tests** — 8 unit tests (permission handling, event classification, env expansion) + 8 integration tests (config parsing, event classification, agent config).
- **Interactive CLI wrapper** — `--client` mode provides an interactive REPL for any ACP agent.

### Changed
- **`initialize` response is now spec-compliant** — returns `protocolVersion: 1`, `agentCapabilities.promptCapabilities.image: true`, and `authMethods: []` at the top level. Previously returned only `agentInfo` and an empty `capabilities: {}` field, which prevented ACP clients (Zed, Neovim) from feature-detecting image-prompt support.
- **`session/new` accepts `mcpServers` parameter** — previously the param was silently dropped. v0.7.0 logs it at debug level and otherwise no-ops (MCP server support is not yet implemented). This unblocks clients that send the spec-required field.
- Version bump to 0.7.0.
- `lib.rs` exports new `client` module.
- `config.toml.example` includes `[agent]` section documentation.
- Help text updated with `--client` mode and agent environment variables.

### Notes
- Spec-compliance changes are server-side only; backward-compatible with all existing clients. The new top-level fields are additive — clients that ignored `capabilities: {}` will ignore `agentCapabilities` the same way, and clients that read it gain useful information.
- `session/load`, `session/resume`, and `session/set_mode` remain unimplemented (roadmap 2026-Q3).

## [0.5.0] - 2026-04-14

### Added
- **Built-in tools** — LLM can now call tools to interact with the local filesystem:
  - `read_file`: read file contents (max 1MB, sandboxed to working directory)
  - `list_dir`: list directory tree (max depth 3, max 200 entries)
  - `search_code`: grep for patterns in source files (max 50 matches)
- **Tool call loop** — when LLM requests tool calls, acp-bridge executes them locally and feeds results back, up to 5 rounds.
- **Security sandbox** — all tool paths are canonicalized and validated against the working directory. Symlink traversal, path escape (`../`), and oversized files are blocked.
- **5 new integration tests** — tool call round-trip, sandbox escape prevention, read_file, search_code, unknown tool handling.

### Changed
- `session/prompt` now sends tool definitions to the LLM and handles tool call responses via non-streaming `chat()`.
- Session stores `working_dir` for tool sandboxing.
- Streaming is used for final text response; tool call detection uses non-streaming for reliability.

## [0.4.0] - 2026-04-14

### Added
- **Ollama native API support** — auto-detects backend type: URL without `/v1` uses Ollama native `/api/chat` (NDJSON streaming), URL with `/v1` uses OpenAI-compatible SSE streaming. Both work seamlessly.
- **Model info query** — queries Ollama `/api/show` at startup to retrieve model context length and metadata.
- **Running model check** — queries Ollama `/api/ps` at startup to check if the configured model is loaded in VRAM. Warns if not loaded with a helpful `ollama run` suggestion.
- **NDJSON stream parser** — dedicated parser for Ollama native streaming format (JSON-per-line), separate from OpenAI SSE parser.
- **3 new integration tests** — `test_ollama_native_streaming`, `test_ollama_auto_detect_native`, `test_ollama_openai_compat_still_works` with mock Ollama native server.
- **ROADMAP.md** — development roadmap with Phase 1-3 plan and promotion strategy.

### Changed
- Stream parsing refactored into two dedicated functions: `parse_ollama_native_stream` and `parse_openai_sse_stream`.
- Startup log now includes `ollama_native` flag.
- `LlmConfig` gains `is_ollama_native()` and `chat_url()` methods for backend auto-detection.

## [0.3.0] - 2026-04-13

### Added
- **Session limits** — `LLM_MAX_SESSIONS` env var to cap concurrent sessions (default 0 = unlimited). Returns JSON-RPC error `-32004` when limit is reached.
- **Session idle timeout** — `LLM_SESSION_IDLE_TIMEOUT` env var to auto-evict idle sessions after N seconds (default 0 = disabled). Background task periodically cleans up.
- **HTTP connection pooling** — reuses a shared `reqwest::Client` across all requests, reducing TCP/TLS handshake overhead.
- **Security and Limitations sections in README**.

### Fixed
- **SSE `\r\n` parsing** — handles both `\r\n` (HTTP standard) and `\n` line endings, fixing silent message loss with some LLM backends.
- **Temperature validation** — clamped to valid 0.0–2.0 range; NaN/Infinity values are filtered out.

### Changed
- Session state tracks `last_active` timestamp for idle timeout support.
- Session access refactored into `sessions_read()` / `sessions_write()` helpers.
- `Session::new()` constructor replaces direct struct initialization.

## [0.2.1] - 2026-04-12

### Fixed
- **CWD prompt injection** — `cwd` parameter in `session/new` is now sanitized to only allow typical path characters, preventing prompt injection attacks.
- **Missing JSON-RPC response on LLM failure** — stream errors and connection failures now always send a JSON-RPC response with `status: "failed"`, preventing client hangs.
- **Unbounded stream buffer** — SSE stream buffer capped at 10MB to prevent OOM from malicious or buggy backends.
- **Flaky env var tests** — config tests now use a mutex to prevent parallel test pollution.

### Added
- **Integration test suite** — 14 tests with a mock LLM server covering the full stdin/stdout JSON-RPC pipeline.

## [0.2.0] - 2026-04-09

### Added
- **Structured logging** — replaced `eprintln` with `tracing`. Control verbosity via `RUST_LOG` env var (default: `acp_bridge=info`).
- **Structured error types** — `AcpError` enum with proper JSON-RPC error codes (`-32602` invalid params, `-32001` unknown session, `-32601` method not found, `-32003` LLM error).
- **Conversation history auto-trim** — `LLM_MAX_HISTORY_TURNS` (default 50) prevents memory growth in long sessions. System prompt is always preserved.
- **LLM HTTP retry with exponential backoff** — transient errors (408, 429, 500-504) and connection timeouts retried up to 3 times (500ms, 1s, 2s).
- **Graceful shutdown** — handles SIGINT/SIGTERM and stdin EOF, drains sessions cleanly.
- **TOML config file support** — `./acp-bridge config.toml`. Priority: env var > config file > defaults.
- **Dockerfile** — multi-stage build, non-root user, ~15MB image.
- **GitHub Actions CI** — `cargo check` + `cargo test` + `cargo clippy` + `cargo fmt`.
- **Unit tests** — 14 test cases covering JSON-RPC parsing, history trimming, error codes, config loading.
- **`--version` flag** — prints version and exits.

### Changed
- RwLock poisoning now recovers gracefully instead of panicking.
- Error responses use correct JSON-RPC error codes instead of generic `-32600`.

### Fixed
- Potential memory leak from unbounded conversation history accumulation.

## [0.1.0] - 2026-04-01

### Added
- Initial release.
- ACP JSON-RPC 2.0 transport over stdin/stdout.
- OpenAI-compatible streaming HTTP client (SSE).
- Multi-session support with conversation history.
- Support for Ollama, LocalAI, vLLM, llama.cpp, LM Studio, text-generation-webui, Jan.ai, Tabby.
- ACP methods: `initialize`, `session/new`, `session/prompt`, `session/end`.
- ACP notifications: `agent_message_chunk`, `agent_thought_chunk`, `tool_call`, `tool_call_update`.
