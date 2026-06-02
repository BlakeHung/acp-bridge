# Changelog

All notable changes to this project will be documented in this file.

Format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).
This project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

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
