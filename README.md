# ACP-Bridge

ACP ([Agent Client Protocol](https://agentclientprotocol.com)) adapter for **local AI** — bridges any OpenAI-compatible API to ACP-compliant harnesses like [openab](https://github.com/openabdev/openab), Zed, and JetBrains IDEs.

Written in Rust. Single ~5MB binary. Zero runtime dependencies. Fully offline.

## Why

- **Zero API cost** — all inference runs on your hardware
- **Data never leaves your network** — code, prompts, responses stay local
- **One binary, any backend** — Ollama, vLLM, LocalAI, llama.cpp, LM Studio, and more
- **Enterprise-ready** — structured logging, retry with backoff, graceful shutdown, configurable history limits

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

## Supported backends

Any service exposing `/v1/chat/completions` with SSE streaming:

| Backend | Default URL | Notes |
|---------|------------|-------|
| [Ollama](https://ollama.com) | `http://localhost:11434/v1` | Default |
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

### Multi-bot setup

Run multiple Discord bots with different models:

```toml
# config-coder.toml — fast coding model
[agent]
command = "acp-bridge"
env = { LLM_MODEL = "qwen2.5:32b" }

# config-reviewer.toml — analytical model
[agent]
command = "acp-bridge"
env = { LLM_MODEL = "gemma4:26b" }
```

## ACP protocol support

| Method | Status |
|--------|--------|
| `initialize` | Supported |
| `session/new` | Multi-session with conversation history |
| `session/prompt` | Streaming via SSE |
| `session/end` | Session cleanup |

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
