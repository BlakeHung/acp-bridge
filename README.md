# ACP-Bridge

ACP ([Agent Client Protocol](https://agentclientprotocol.com)) adapter for **self-hosted AI** — the zero-cloud, zero-dependency bridge for air-gapped and enterprise environments.

When OpenCode can't reach the internet, acp-bridge can still run.

Written in Rust. Single ~5MB binary. Zero runtime dependencies. Fully offline.

## Project status — active development

acp-bridge is actively maintained, focused on a niche neither OpenCode nor Cline currently fills: **fully air-gapped local AI coding agents that speak ACP**. v0.7.x delivers a working subset of the ACP server surface (`initialize`, `session/new`, `session/prompt`, `session/end`) with streaming notifications and tool calling, and advertises `agentCapabilities` so editors like Zed and Neovim can feature-detect correctly.

Roadmap (2026-Q3):

- Full ACP spec compliance — `session/load`, `session/resume`, `session/set_mode`
- Integration tests against thinking-on chat templates (Qwen3, DeepSeek-R1, GLM, Kimi-K2) that currently break in mainstream alternatives
- Optional audit log mode for regulated deployments

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
| Tool surface | Full agent (edit, shell, web) | 3 sandboxed read-only tools |
| Air-gap audit | Per-release verification | Binary small enough to audit once |
| ACP server stability | Active issues on `newSession`, `--port` | Spec-compliant `initialize` + session lifecycle |

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
3. LLM response streams back as SSE chunks
4. Chunks are emitted as ACP `agent_message_chunk` notifications via **stdout**
5. Conversation history is kept per session, auto-trimmed to prevent memory growth

### Key design decisions

- **stdin/stdout transport** — spawned as a child process by the harness, no ports to manage
- **Stateless binary** — no database, no disk writes, all state in memory
- **Retry with exponential backoff** — survives LLM server restarts (Ollama, vLLM rolling updates)
- **Structured logging** — `tracing` with `RUST_LOG` support, writes to stderr (not mixed with JSON-RPC on stdout)

### Offline-first guarantee

acp-bridge makes **zero outbound network calls** beyond the user-configured LLM endpoint:

- No telemetry, no update checks, no anonymous usage stats
- No remote model registry lookups (no calls to `models.dev` or similar)
- No automatic MCP server fetching — `mcpServers` in `session/new` is logged but not executed
- The only HTTP traffic is to `LLM_BASE_URL` (default `http://localhost:11434/v1`)
- The only socket bind is the optional `--a2a` HTTP server (off by default; you opt in)

This makes acp-bridge safe to run on truly air-gapped networks: the binary will work identically with a local Ollama instance on a disconnected machine, and there is no failure path that depends on internet reachability.

## Supported backends

Ollama is supported natively via `/api/chat` (NDJSON streaming). All other backends use the OpenAI-compatible `/v1/chat/completions` (SSE streaming). The backend type is auto-detected from the URL:

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

### Install from Git

```bash
cargo install --git https://github.com/BlakeHung/acp-bridge
```

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
| `LLM_MAX_HISTORY_TURNS` | `50` | Max conversation turns to keep (0 = unlimited) |
| `LLM_MAX_SESSIONS` | `0` | Max concurrent sessions (0 = unlimited) |
| `LLM_SESSION_IDLE_TIMEOUT` | `0` | Evict idle sessions after N seconds (0 = disabled) |
| `RUST_LOG` | `acp_bridge=info` | Log level (`debug`, `info`, `warn`, `error`) |

Also supports `OLLAMA_BASE_URL`, `OLLAMA_MODEL`, `OLLAMA_API_KEY` as aliases.

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

[openab](https://github.com/openabdev/openab) is a Discord-to-ACP bridge. Combined with acp-bridge, anyone in your Discord server can use your local AI — zero API keys, zero cost.

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

All tools are **sandboxed** to the session's working directory — the LLM cannot access files outside it.

## ACP protocol support

| Method | Status |
|--------|--------|
| `initialize` | Supported — advertises `agentCapabilities.promptCapabilities.image: true`, `protocolVersion: 1`, `authMethods: []` |
| `session/new` | Multi-session with conversation history; `mcpServers` param accepted but ignored in v0.7 |
| `session/prompt` | Streaming via SSE; supports image content blocks |
| `session/end` | Session cleanup |
| `session/load` | Not yet — roadmap |
| `session/resume` | Not yet — roadmap |
| `session/set_mode` | Not yet — roadmap |

| Notification | Status |
|--------------|--------|
| `agent_message_chunk` | Streaming text chunks |
| `agent_thought_chunk` | Emitted on prompt start |
| `tool_call` | LLM call tracking |
| `tool_call_update` | Completion status |

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
- **Memory-bounded sessions** — conversation history auto-trims to `LLM_MAX_HISTORY_TURNS` (default 50 turns), preventing OOM in long sessions
- **Session limits** — configurable `LLM_MAX_SESSIONS` to cap concurrent sessions, and `LLM_SESSION_IDLE_TIMEOUT` to auto-evict idle sessions
- **Stream buffer cap** — SSE stream buffer capped at 10MB to prevent unbounded memory growth from malicious or buggy backends
- **HTTP connection pooling** — reuses a shared HTTP client across all requests, reducing TCP/TLS handshake overhead
- **Robust SSE parsing** — handles both `\r\n` (HTTP standard) and `\n` line endings
- **Poison recovery** — RwLock poisoning is handled gracefully instead of panicking

## Security

- **CWD sanitization** — the `cwd` parameter in `session/new` is sanitized to prevent prompt injection attacks
- **Temperature validation** — clamped to valid 0.0–2.0 range; NaN/Infinity values are filtered
- **Error response guarantee** — JSON-RPC response is always sent even when the LLM backend fails, preventing client hangs

## Limitations

- No authentication or authorization — intended to run behind a trusted harness (openab, Zed)
- No persistent storage — all state is in-memory, lost on restart
- Single-process — not designed for horizontal scaling

## License

MIT
