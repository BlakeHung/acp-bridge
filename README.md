# ACP Agent

**Autonomous Coding Partner** — a local-first coding agent, formerly **acp-bridge**.

ACP Agent is evolving from an ACP adapter into a persistent local agent harness: project instructions, long-running tasks, recoverable sessions, and tools powered by your own inference server. These expanded capabilities are under development; see the [implementation plan](docs/kiro-inspired-harness-plan.md) for milestones and the [current capability matrix](docs/scope.md) for what works today.

The current executable, Cargo package, repository, and configuration names remain `acp-bridge`, so existing installations and OpenAB launch commands continue to work. In protocol documentation, **ACP** still means [Agent Client Protocol](https://agentclientprotocol.com); **Autonomous Coding Partner** is our product name expansion.

> Project page & docs: **[wchung.tw/acp-bridge/](https://wchung.tw/acp-bridge/)** ([中文](https://wchung.tw/zh/acp-bridge/))

Brand and website transition: [ACP Agent product direction](docs/acp-agent-brand-and-transition.md).

Written in Rust. Single ~5MB binary. Zero runtime dependencies. Fully offline.

## Project status — active development

ACP Agent is actively maintained, with a focus on local inference and ACP-compatible clients. The implementation currently retains the `acp-bridge` name.

**v0.9.2** (latest) — acp-bridge negotiates `protocolVersion` at `initialize` and emits the matching wire shape for each version:

- **v1** (the version every existing Client speaks today — Zed, JetBrains, ACP UI, Meuxe, ACP Inspector, Codex CLI adapter) — `initialize`, `session/new`, `session/list`, `session/close`, `session/end` (the v1 alias), `session/prompt`, `session/cancel`. 11 built-in tools: `read_file`, `list_dir`, `search_code`, `write_file`, `edit` (surgical string replacement), `web_fetch` (HTTP/HTTPS with HTML reduction, opt-in via `LLM_WEB_ALLOWLIST`), `bash`, `git_status` / `git_diff` / `git_log` / `git_commit`. `session/update` notifications: `plan`, `session_info_update`, `usage_update`, `available_commands_update`. Structured backend errors via `LlmErrorKind` (stable wire strings in `error.data.category`). End-to-end tests replay the real init payloads of Zed / ACP Inspector / Codex CLI.

- **v2** — wire-shape negotiation, unified `info` + `capabilities`, `tool_call_update`, `state_update` (`idle` + `stopReason`), `plan_update` with `planId`, `session/prompt` response carrying `messageId`. v2 is still a moving target in the spec: acp-bridge does **not** yet implement `session/resume` (a v2 baseline method), `idempotencyKey`, `state_update` for running work, or hold-and-insert prompts during active work — see `docs/scope.md` for the pinned revision and gap list.

For Clients on the v1 wire (the common case today) the v0.9.x release is a strict additive change — same wire as v0.8.x plus more tools.

For air-gapped, self-hosted, and audit-sensitive deployments the design constraints are unchanged: single ~5 MB static Rust binary, zero runtime dependencies beyond the configured LLM endpoint, sandboxed tool calls within the session working directory, opt-in network access via `LLM_WEB_ALLOWLIST`. See `docs/scope.md` for the capability matrix.

Roadmap:

- ACP v2 `agent_message` upsert (currently chunk-only; v2 RFD allows either).
- ACP v2 `terminal_update` / `terminal_output_chunk` (agent-owned terminal output).
- ACP Registry `agent.json` so Zed / ACP UI can one-click install acp-bridge.

## Relationship to OpenCode

[OpenCode](https://opencode.ai) is a feature-rich coding agent with an ACP surface (`opencode acp`) and a broad provider matrix via the Vercel AI SDK. For online, cloud-leaning workflows it is the right tool.

For the air-gapped local-AI path, however, OpenCode has several open issues as of mid-2026:

- ACP server `newSession` returns `Method not found` ([opencode#24846])
- `opencode acp --port` exits immediately on start ([opencode#22795])
- Ollama / vLLM / llama.cpp / LM Studio adapters have unresolved tool-calling bugs across thinking-on templates ([opencode#22132], [opencode#27920], [opencode#25351])
- Air-gap mode still leaks network calls to models.dev, LSP manifests, and ripgrep binary fetch ([opencode#18492])

acp-bridge targets the same protocol but a narrower scope: **air-gap clean, local-first, ACP-compliant**.

| Concern | OpenCode | acp-bridge |
|---------|----------|-----------|
| Provider breadth | 75+ via AI SDK (cloud-leaning) | OpenAI-compatible + Ollama native |
| Network footprint | models.dev, LSP, update, ripgrep fetches | Outbound only to configured LLM endpoint |
| Runtime | Node.js + npm | Single 5MB static Rust binary |
| Tool surface | Full agent (edit, shell, web) | 11 tools: `read_file`, `list_dir`, `search_code`, `write_file`, `edit`, `web_fetch`, `bash`, `git_status` / `git_diff` / `git_log` / `git_commit`. `web_fetch` is opt-in (`LLM_WEB_ALLOWLIST`); `bash` runs unconstrained in the session working dir |
| Air-gap audit | Per-release verification | Binary small enough to audit once |
| ACP server stability | Active issues on `newSession`, `--port` | ACP v1: `initialize` + `session/{new, list, close, end, prompt, cancel}`; v2 negotiated wire shapes (baseline gaps listed in `docs/scope.md`); `load`/`resume`/`delete` return graceful rejections |

**Use OpenCode** when you want the full cloud-and-local agent toolkit. **Use acp-bridge** when the deployment requires a fully offline, audit-friendly bridge — air-gapped sites, regulated industries, edge / embedded ACP harnesses, CI runners with strict egress policies.

See [When to use acp-bridge vs OpenCode](#when-to-use-acp-bridge-vs-opencode) below for a per-scenario breakdown.

[opencode#24846]: https://github.com/anomalyco/opencode/issues/24846
[opencode#22795]: https://github.com/anomalyco/opencode/issues/22795
[opencode#22132]: https://github.com/anomalyco/opencode/issues/22132
[opencode#27920]: https://github.com/anomalyco/opencode/issues/27920
[opencode#25351]: https://github.com/anomalyco/opencode/issues/25351
[opencode#18492]: https://github.com/anomalyco/opencode/issues/18492

## Why acp-bridge

```
OpenCode, Claude Code, Codex CLI — all need internet access for API keys or cloud models.
acp-bridge addresses the "can't go online" and "won't go online" cases.
```

- **Air-gapped / internal deployment** — data never leaves the machine; suitable for strict-compliance enterprise environments
- **Zero cloud dependency** — all inference runs on your hardware, no API key required
- **Special backends** — vLLM, llama.cpp, TGI, and other inference engines OpenCode doesn't directly support
- **Ollama native integration** — auto-detects Ollama and uses native `/api/chat` with NDJSON streaming, model info query, and VRAM status check
- **Embeddable** — 5MB binary; drop into Docker Compose, CI/CD pipelines, or any ACP harness
- **Enterprise-ready** — structured logging, retry with backoff, graceful shutdown, configurable history limits

## When to use acp-bridge vs OpenCode

```
┌─────────────────────────────┬──────────────────┬──────────────────────┐
│ Scenario                    │ OpenCode         │ acp-bridge           │
├─────────────────────────────┼──────────────────┼──────────────────────┤
│ Online + Ollama Cloud       │ ✓ preferred      │ works, redundant     │
│ Online + Claude/GPT API     │ ✓ preferred      │ ✗                    │
│ Internal + Ollama local     │ works            │ ✓ preferred          │
│ Air-gapped                  │ ✗                │ ✓ only choice        │
│ vLLM / TGI / llama.cpp      │ ✗                │ ✓ only choice        │
│ Docker Compose embed        │ works, heavy     │ ✓ 5MB binary         │
│ Strict offline compliance   │ verify yourself  │ ✓ guaranteed offline │
└─────────────────────────────┴──────────────────┴──────────────────────┘
```

## Architecture

```
                          acp-bridge
                     ┌─────────────────────┐
                     │  JSON-RPC 2.0       │
ACP Harness          │  ┌───────────────┐  │         Local AI Server
(openab, Zed,   ────stdin──▶ ACP Router │  │         (OpenAI-compatible)
 JetBrains)          │  └──────┬────────┘  │
                     │         │           │
              ◀──stdout───  Notify/       │
              (streaming)   Response       │
                     │         │           │
                     │  ┌──────▼────────┐  │
                     │  │  LLM Client   │──── HTTP/SSE ──▶  /v1/chat/completions
                     │  │  - retry      │  │
                     │  │  - backoff    │  │         ┌─────────────────┐
                     │  │  - streaming  │  │         │ Ollama / vLLM / │
                     │  └───────────────┘  │         │ LocalAI / ...   │
                     │                     │         └─────────────────┘
                     │  ┌───────────────┐  │
                     │  │ Session Store  │  │
                     │  │ - history     │  │
                     │  │ - auto-trim   │  │
                     │  └───────────────┘  │
                     └─────────────────────┘
```

### Data flow

1. Harness sends JSON-RPC request via **stdin**
2. acp-bridge translates to OpenAI chat completion API call
3. The backend response arrives; tool calls execute (up to 5 rounds per prompt), then the final text is emitted as `agent_message_chunk` notifications via **stdout**
4. Per-round progress (`thinking`, tool call start/update) is also reported as `session/update` notifications
5. Conversation history is kept per session, summarized near the prompt budget, and pruned in complete tool groups when necessary

### Key design decisions

- **stdin/stdout transport** — spawned as a child process by the harness, no ports to manage
- **Stateless binary** — no database, no disk writes, all state in memory
- **Bounded retry with exponential backoff** — transient errors are retried up to 3 times (~3.5 s total); this rides out brief blips, not long backend outages — a restarted server surfaces as a retryable error, it doesn't hang the prompt
- **Structured logging** — `tracing` with `RUST_LOG` support, writes to stderr (not mixed with JSON-RPC on stdout)

### Offline-first guarantee

The acp-bridge **binary itself** makes **zero outbound network calls** beyond the user-configured LLM endpoint:

- No telemetry, no update checks, no anonymous usage stats
- No remote model registry lookups (no calls to `models.dev` or similar)
- No automatic MCP server fetching — `mcpServers` in `session/new` is logged but not executed
- The only background HTTP traffic is to `LLM_BASE_URL` (default `http://localhost:11434/v1`)

Two tools change the network surface when enabled — keep them in mind for air-gap audits:

- `web_fetch` is opt-in: it only reaches hosts on `LLM_WEB_ALLOWLIST` (empty = no web access).
- `bash` runs arbitrary commands — the model could invoke `curl` itself. Egress policy for that path is the deployment's job (container, firewall), not a property of the binary.

This makes acp-bridge safe to run on truly air-gapped networks: with `LLM_WEB_ALLOWLIST` empty and `bash` disabled or egress-blocked, the binary will work identically on a disconnected machine, and there is no failure path that depends on internet reachability.

## Supported backends

Ollama is supported natively via `/api/chat`. All other backends use the OpenAI-compatible `/v1/chat/completions`. The backend type is auto-detected from the URL:

- URL **without** `/v1` suffix → Ollama native mode
- URL **with** `/v1` suffix → OpenAI-compatible mode

| Backend | Default URL | Mode |
|---------|------------|------|
| [Ollama](https://ollama.com) | `http://localhost:11434` | Native (recommended) |
| [Ollama](https://ollama.com) | `http://localhost:11434/v1` | OpenAI compat (also works) |
| [LocalAI](https://localai.io) | `http://localhost:8080/v1` | Drop-in OpenAI replacement |
| [vLLM](https://docs.vllm.ai) | `http://localhost:8000/v1` | High-performance inference |
| [llama.cpp server](https://github.com/ggml-org/llama.cpp) | `http://localhost:8080/v1` | Lightweight |
| [LM Studio](https://lmstudio.ai) | `http://localhost:1234/v1` | Desktop app |
| [text-generation-webui](https://github.com/oobabooga/text-generation-webui) | `http://localhost:5000/v1` | Enable OpenAI extension |
| [Jan.ai](https://jan.ai) | `http://localhost:1337/v1` | Desktop app |
| [Tabby](https://tabby.tabbyml.com) | `http://localhost:8080/v1` | Code completion |

## Quick start

### From source

```bash
# Build
cargo build --release

# Run with Ollama (default)
./target/release/acp-bridge

# Run with vLLM
LLM_BASE_URL=http://localhost:8000/v1 LLM_MODEL=meta-llama/Llama-3-8b ./target/release/acp-bridge

# Run with config file
./target/release/acp-bridge config.toml

# Run benchmark mode
./target/release/acp-bridge --bench
```

### With Docker

```bash
# Build image
docker build -t acp-bridge .

# Run (connect to host's Ollama)
docker run --network=host acp-bridge

# Run with custom model
docker run --network=host -e LLM_MODEL=llama3.2:7b acp-bridge
```

### Prebuilt binaries

Per-release binaries for Linux (x86_64, aarch64), macOS (x86_64, arm64), and Windows x86_64 are attached to each [GitHub Release](https://github.com/BlakeHung/acp-bridge/releases) — no build step needed.

### Install from crates.io or Git

```bash
cargo install acp-bridge
# or track the repo
cargo install --git https://github.com/BlakeHung/acp-bridge
```

### As a Zed agent

```json
{
  "agent_servers": {
    "acp-bridge": {
      "type": "custom",
      "command": "/path/to/acp-bridge",
      "args": [],
      "env": { "LLM_BASE_URL": "http://localhost:11434/v1", "LLM_MODEL": "gemma4:26b" }
    }
  }
}
```

> Slow backends (large local models, CPU inference) need a longer `LLM_TIMEOUT` than the 300 s default — a slow first-token is normal, not a hang. For example `LLM_TIMEOUT=1800`.

## Configuration

acp-bridge supports three configuration methods (highest priority wins):

1. **Environment variables** — best when spawned by openab
2. **TOML config file** — best for standalone deployment
3. **Built-in defaults** — works out of the box with Ollama

### Environment variables

| Variable | Default | Description |
|----------|---------|-------------|
| `LLM_BASE_URL` | `http://localhost:11434/v1` | OpenAI-compatible endpoint |
| `LLM_MODEL` | `gemma4:26b` | Model name |
| `LLM_API_KEY` | `local-ai` | API key (most local services ignore this) |
| `LLM_SYSTEM_PROMPT` | (auto-generated) | Custom system prompt |
| `LLM_TEMPERATURE` | (model default) | Sampling temperature (0.0-2.0) |
| `LLM_MAX_TOKENS` | (model default) | Maximum tokens to generate |
| `LLM_TIMEOUT` | `300` | HTTP request timeout in seconds |
| `LLM_MAX_HISTORY_TURNS` | `50` | User-turn count that requests a summary (0 disables this trigger; token budgets still apply) |
| `LLM_MAX_SESSIONS` | `256` | Max concurrent sessions; `0` restores the old unlimited behavior |
| `LLM_SESSION_IDLE_TIMEOUT` | `0` | Evict idle sessions after N seconds (0 = disabled) |
| `LLM_SUPPORTS_IMAGE` | `false` | Opt-in: advertise `promptCapabilities.image: true` at `initialize`. Set to `true` only if the configured backend can actually accept image content blocks (e.g. a vision-capable model). |
| `LLM_MODEL_CONTEXT` | `32768` | Actual server context window. Request estimates include message metadata, tool arguments and schemas; successful provider usage calibrates the estimate. Output and a 512-token safety margin are reserved before dispatch. |
| `LLM_AUTO_SUMMARIZE` | `true` | Summarize older history using the same backend without tools. Failure falls back to deterministic tool-output elision and group pruning; four calls elapse before another summary attempt. |
| `LLM_SUMMARIZE_AT` | `0.6` | Summary trigger as a fraction of the available prompt budget (finite value from 0.1 to 0.95). TOML: `llm.summarize_at`. |
| `LLM_ARTIFACT_CHARS` | `8192` | Inline budget per tool result. Larger outputs are written to `{LLM_SESSION_DIR}/artifacts/<session>/<call>.txt` and history keeps a head excerpt plus a `read_file` pointer. Without `LLM_SESSION_DIR` outputs truncate at this size. |
| `LLM_MAX_INFLIGHT` | `8` | Global bound on concurrent in-flight turns across all sessions. Over the limit the prompt is refused with an identifiable `server_busy` status; a completing or cancelled turn frees the slot. 0 = unlimited. |
| `LLM_MAX_PROMPT_CHARS` | `200000` | Largest accepted prompt payload (serialized chars). Over the limit → `prompt_too_large` error, nothing queued. 0 = unlimited. |
| `LLM_MAX_TOOL_ROUNDS` | `50` | Max model↔tool round-trips per turn before the bounded wrap-up path runs (aligned with openab-agent's default) |
| `LLM_TOOL_TIMEOUT` | `120` | Per-tool wall-clock timeout in seconds; bash/git/web_fetch processes are killed (with their process group) on expiry |
| `LLM_TURN_TIMEOUT` | `0` | Whole-turn deadline in seconds (0 = no limit) |
| `LLM_TOOL_FAILURE_STREAK` | `3` | Abort the tool loop after N consecutive identical tool failures (same tool + args + error) |
| `LLM_POLICY_READ` / `LLM_POLICY_WRITE` | `allow` / `allow` | Tool policy actions: `allow` · `audit` (visible notice, then run) · `confirm` (run only with a grant) · `deny` |
| `LLM_POLICY_SHELL` / `LLM_POLICY_WEB` | `audit` / `audit` | Tool policy actions for shell/network tools |
| `LLM_POLICY_GRANTS` | (empty) | Comma-separated capability grants (`read,write,shell,web`) that let `confirm` actions execute — deployment scope. For per-operation grants the Client can also call `session/permit` (single-use, TTL-bound, optional tool scope; emits `[policy:permit]` on consumption) |
| `LLM_POLICY_DENY_PATHS` | `.env, .env.*, *.pem, *.key, .ssh/**, .aws/**, .gnupg/**` | Workspace-relative globs refused on `path` args AND inside list_dir/search_code results |
| `LLM_SESSION_DIR` | (unset) | Directory for per-session JSON snapshots. Unset = no persistence; `session/load` answers `no_persistence` |
| `LLM_WEB_ALLOWLIST` | (empty) | Comma-separated host suffixes the `web_fetch` tool is allowed to reach. Empty = block all web access. |
| `RUST_LOG` | `acp_bridge=info` | Log level (`debug`, `info`, `warn`, `error`) |

Also supports `OLLAMA_BASE_URL`, `OLLAMA_MODEL`, `OLLAMA_API_KEY` as aliases.

Context counts are estimates, not exact tokenizer counts. A backend overflow triggers at most one context retry with a smaller budget; an unchanged request is not replayed. When the backend names its real window (`context (N tokens)` — e.g. Strata's `fit_max_tokens` budget) the session budgets against that smaller window from the retry onward. HTTP 413 payload rejections take the same fit-and-retry path. Summary requests have their own input/output budget and a timeout bounded by `LLM_TIMEOUT` (at most 300 seconds). Empty, truncated, tool-calling or insufficiently smaller summaries are discarded. The original task, latest user instruction, system rules and two recent message groups remain verbatim after a successful summary. Deterministic fallback preserves all user messages and keeps tool calls with their results.

A single user message estimated above half the usable window is pre-trimmed at the door: with `LLM_SESSION_DIR` set it is written to `artifacts/<session>/user-input.txt` and history keeps a bounded head plus a `read_file` pointer; without a store the protected-input rejection remains the boundary. Inbound stdio frames are capped at 4 MiB — oversized frames are dropped and the stream resyncs.

Summaries are lossy and a small local model may omit evidence. The session snapshot contains the current working context, not an archive of removed messages; this change does not create a transcript archive or a retrieval index. Re-read source files before relying on elided details. Images are conservatively estimated from their serialized representation, which can reject inputs that a vision tokenizer could fit. Diagnostics report numeric sizes, error class, overflow classification and retry attempt without adding raw prompts or backend error bodies to the context log.

### Config file

```bash
cp config.toml.example config.toml
# Edit as needed
./acp-bridge config.toml
```

See [config.toml.example](config.toml.example) for all options.

## Mac quick start (Apple Silicon)

Mac with Apple Silicon is ideal for local AI — unified memory means your entire RAM is available as VRAM.

```bash
# 1. Install Ollama
brew install ollama
ollama serve

# 2. Pull a model
ollama pull gemma4:26b

# 3. Install acp-bridge
cargo install --git https://github.com/BlakeHung/acp-bridge

# 4. Use with Zed editor (native ACP support)
#    Zed Settings > Agent > command = "acp-bridge"
```

**Model recommendations by Mac:**

| Mac | RAM | Recommended model | Command |
|-----|-----|-------------------|---------|
| MacBook Air M2/M3 | 8-16GB | `llama3.2:7b` | `ollama pull llama3.2:7b` |
| MacBook Pro M3/M4 | 18-24GB | `gemma4:26b` | `ollama pull gemma4:26b` |
| MacBook Pro M4 Pro | 48GB | `qwen2.5:32b` | `ollama pull qwen2.5:32b` |
| Mac Studio M2/M4 Ultra | 64-192GB | `llama3.1:70b` | `ollama pull llama3.1:70b` |

## Use with openab

[openab](https://github.com/openabdev/openab) is a Discord-to-ACP bridge. Combined with acp-bridge, members of your Discord server can use your local AI — zero API keys, zero cost.

> **Access control is your job.** Neither acp-bridge nor openab restricts *who* can talk to the agent by default — anyone who can reach the Discord channel can run prompts and tools on your machine. Configure `allowed_users` / channel restrictions in your openab config before exposing it to a shared server.

```
Team member A ──┐
Team member B ──┤── Discord ──▶ openab ──▶ acp-bridge ──▶ Ollama + GPU
Team member C ──┘                          (your machine)
```

### Multi-agent with OpenCode

openab supports spawning different agents per channel. Combine acp-bridge (local/sensitive) with OpenCode (cloud) for the best of both worlds:

```
Discord → openab ─┬─▶ OpenCode     (cloud tasks, Ollama Cloud)
                   │
                   └─▶ acp-bridge   (local/sensitive tasks, internal GPU)
```

```toml
# config-cloud.toml — general dev (OpenCode + Ollama Cloud)
[agent]
command = "opencode"
args = ["acp"]

# config-secure.toml — sensitive projects (acp-bridge + internal GPU)
[agent]
command = "acp-bridge"
env = { LLM_BASE_URL = "http://internal-gpu:11434", LLM_MODEL = "qwen2.5:32b" }
```

### Setup

```bash
# 1. Make sure Ollama is running
ollama serve
ollama pull gemma4:26b

# 2. Build acp-bridge
cd acp-bridge && cargo build --release
cp target/release/acp-bridge /usr/local/bin/

# 3. Configure openab
cat > config.toml <<'EOF'
[discord]
bot_token = "${DISCORD_BOT_TOKEN}"
allowed_channels = ["your-channel-id"]

[agent]
command = "acp-bridge"
args = []
working_dir = "/path/to/your/project"
env = { LLM_BASE_URL = "http://localhost:11434/v1", LLM_MODEL = "gemma4:26b" }

[pool]
max_sessions = 5
session_ttl_hours = 24
EOF

# 4. Run openab
export DISCORD_BOT_TOKEN="your-token"
cargo run -- config.toml
```

## Built-in tools

When the LLM supports function calling (Ollama with compatible models, OpenAI-compatible APIs), acp-bridge provides built-in tools that let the LLM interact with your local filesystem:

| Tool | Description | Limits |
|------|-------------|--------|
| `read_file` | Read file contents | Max 1MB, sandboxed to working dir |
| `list_dir` | List directory tree | Max depth 3, max 200 entries |
| `search_code` | Grep for patterns | Max 50 matches |
| `write_file` | Create or overwrite a file | Max 5MB; rejects `..`, symlinked-parent escapes, and symlink targets outside the working dir |
| `edit` | Replace a unique substring in a file | Refuses ambiguous / missing matches |
| `web_fetch` | Fetch a URL over HTTP/HTTPS | 5MB cap enforced while downloading, 30s timeout. Requires `LLM_WEB_ALLOWLIST` |
| `bash` | Run a bash command in the working dir | `LLM_TOOL_TIMEOUT` wall-clock timeout, process-group kill on timeout AND on exit (grandchildren can't hold pipes), retained output bounded (~1 MB/stream, truncated at 50 KB for the model) |
| `git_status` | Compact `git status` | — |
| `git_diff` | Show unstaged (or staged) changes | Optional `path` filter |
| `git_log` | One-line-per-commit log | `max_count` (default 20, max 200) |
| `git_commit` | Stage listed paths and commit | — |

File tools (`read_file`, `list_dir`, `search_code`, `write_file`, `edit`) are **sandboxed** to the session's working directory. `bash` and the `git_*` tools run unrestricted commands with the working directory as their cwd — treat that surface like shell access. `web_fetch` is gated by `LLM_WEB_ALLOWLIST` (empty = no web access).

## ACP protocol support

`acp-bridge` negotiates the wire-format version with each Client at
`init` time. v1 Clients (the common case today) and v2 Clients get
the matching wire shape from the same code base.

### Methods (v1 + v2 baseline)

| Method | Status |
|--------|--------|
| `initialize` | Supported. v1: emits `agentCapabilities` + `agentInfo`. v2: emits unified `capabilities` + `info`. Image capability opt-in via `LLM_SUPPORTS_IMAGE`. |
| `session/new` | Multi-session with per-session conversation history. `mcpServers` param accepted but ignored (acp-bridge has no MCP relay). |
| `session/prompt` | Progress via `session/update` notifications (`thinking`, tool-call updates, answer chunks). Text and `ContentBlock::ResourceLink` always supported; `ContentBlock::Image` blocks are dropped with a visible notice unless `LLM_SUPPORTS_IMAGE=true`. Final response shape is version-aware (v1: `{stopReason, status, text}`; v2: `{messageId}` with `stopReason` on `state_update`). |
| `session/cancel` notification | Aborts in-flight LLM requests and answers the prompt with cancellation. Subprocess tools (bash/git) are killed with their process group; synchronous file tools finish before cancellation takes effect. |
| `session/end` (v1) / `session/close` (v2 baseline) | Session cleanup. Both methods share the same implementation. |
| `session/list` (v2 baseline) | Returns active sessions as `{sessions: [{sessionId, cwd}], nextCursor: null}`. |
| `session/load` | With `LLM_SESSION_DIR` set: restores a snapshotted session across restarts; unknown id → `-32002 session_not_found`. Without it: `-32001 no_persistence`. |
| `session/resume` | Same. |
| `session/delete` (v2 optional) | Graceful `-32601 not_implemented` rejection. Use `session/close` instead. |
| `session/set_mode` | Graceful `-32602 no_modes` rejection. `session/new` does not return a `modes` array. |

### Session/update notifications (v1 + v2)

| Notification | Discriminator | Status |
|--------------|---------------|--------|
| Text chunk | `agent_message_chunk` (v1 + v2) | Carries the final answer and interim notices (e.g. dropped-image notice) |
| Thought chunk | `agent_thought_chunk` (v1 + v2) | Emitted at prompt start so Clients can render the thought bubble |
| Tool call start (v1) | `tool_call` | Carries `toolCallId`, `title`, `kind`, `status: "in_progress"` |
| Tool call start (v2) | `tool_call_update` (upsert) | Same fields. v2 removed the legacy `tool_call` discriminator. |
| Tool call update | `tool_call_update` (v1 + v2) | Carries the new `status` (`pending` / `in_progress` / `completed` / `failed`) |
| Plan (v1) | `plan` | `entries: PlanEntry[]`. Helper is in place; the LLM does not currently auto-emit plans. |
| Plan (v2) | `plan_update` | Wrapped in `plan: { type: "items", planId, entries[] }` so Clients can track multiple plans independently. |
| Session info | `session_info_update` | v1 + v2. Carries the session `title`. acp-bridge emits it after `session/new` (defaults to cwd basename) and after each `session/prompt` (first line of user prompt). |
| Token usage | `usage_update` | v1 + v2. `used` is chars/4 across the session history; `size` from `LLM_MODEL_CONTEXT` (default 32768). |
| Slash commands | `available_commands_update` | v1 + v2. Advertises `/read`, `/ls`, `/search`, `/edit`, `/shell`. |
| Turn end (v2 only) | `state_update` | Emitted at end of each prompt with `state: "idle"` and `stopReason`. v1 Clients ignore this unknown discriminator per the JSON-RPC spec. |

### Structured backend errors

`acp-bridge` returns an `error.data.category` + `error.data.retryable`
field on failed prompt responses so Clients can branch on the failure
class without parsing prose. See `LlmErrorKind::as_str()` in
`src/llm.rs` for the full list of stable category strings.

### Where to look

- `docs/scope.md` — capability matrix and graceful-reject reference.
- `tests/clients/` — end-to-end e2e tests against real init payloads
  pulled from Zed / ACP Inspector / Codex CLI source code.

## Observability

Logs are written to **stderr** in structured format via `tracing`. Control verbosity with `RUST_LOG`:

```bash
# Default (info)
./acp-bridge

# Debug mode — see all requests, retries, history trimming
RUST_LOG=acp_bridge=debug ./acp-bridge

# Quiet mode — errors only
RUST_LOG=acp_bridge=error ./acp-bridge
```

When spawned by openab, logs go to the child process's stderr. To capture them, configure openab to pipe stderr (see openab docs).

## Reliability

- **Retry with exponential backoff** — transient errors (408, 429, 500, 502, 503, 504) and connection timeouts are retried up to 3 times with exponential backoff (500ms, 1s, 2s)
- **Graceful shutdown** — handles SIGINT/SIGTERM and stdin EOF cleanly, drains in-flight requests
- **Budgeted working context** — history is summarized near the configured prompt budget; oversized tool results are elided and older complete tool groups may be removed. Protected inputs that cannot fit are rejected before dispatch.
- **Session limits** — configurable `LLM_MAX_SESSIONS` to cap concurrent sessions, and `LLM_SESSION_IDLE_TIMEOUT` to auto-evict idle sessions
- **Stream buffer cap** — SSE stream buffer capped at 10MB to prevent unbounded memory growth from malicious or buggy backends
- **HTTP connection pooling** — reuses a shared HTTP client across all requests, reducing TCP/TLS handshake overhead
- **Robust SSE parsing** — handles both `\r\n` (HTTP standard) and `\n` line endings
- **Poison recovery** — RwLock poisoning is handled gracefully instead of panicking

## Security

- **CWD validation** — the `cwd` parameter in `session/new` is canonicalized and must name an existing directory; anything else is rejected with `-32602` instead of being silently rewritten
- **Strict config** — an unparseable TOML file, unknown section/field, or present-but-invalid `LLM_*` env var is a startup error (exit code 2), not a silent fallback
- **Temperature validation** — clamped to valid 0.0–2.0 range; NaN/Infinity values are filtered
- **Error response guarantee** — JSON-RPC response is always sent even when the LLM backend fails, preventing client hangs

## Limitations

- No authentication or authorization — intended to run behind a trusted harness (openab, Zed)
- Session persistence is opt-in flat-file snapshots (`LLM_SESSION_DIR`), not a durable session database — concurrent writers, crash markers, and compaction are out of scope
- Single-process — not designed for horizontal scaling
- Prompts are executed one per session — a second `session/prompt` during an active turn is rejected (`turn_in_progress`) rather than queued

## License

MIT
