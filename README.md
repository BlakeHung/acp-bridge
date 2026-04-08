# local-ai-acp

ACP ([Agent Client Protocol](https://agentclientprotocol.com)) adapter for **local AI** — bridges any OpenAI-compatible API to ACP-compliant harnesses like [openab](https://github.com/openabdev/openab), Zed, and others.

Written in Rust. No runtime dependencies. Single binary.

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

```bash
# Build
cargo build --release

# Run with Ollama (default)
./target/release/local-ai-acp

# Run with LocalAI
LLM_BASE_URL=http://localhost:8080/v1 LLM_MODEL=gpt-3.5-turbo ./target/release/local-ai-acp

# Run with vLLM
LLM_BASE_URL=http://localhost:8000/v1 LLM_MODEL=meta-llama/Llama-3-8b ./target/release/local-ai-acp

# Run with LM Studio
LLM_BASE_URL=http://localhost:1234/v1 LLM_MODEL=loaded-model ./target/release/local-ai-acp
```

## Mac quick start (Apple Silicon)

Mac with Apple Silicon is ideal for local AI — unified memory means your entire RAM is available as VRAM.

```bash
# 1. Install Ollama
brew install ollama
ollama serve  # start in background

# 2. Pull a model (pick one based on your RAM)
ollama pull gemma4:26b      # 16GB+ RAM (MacBook Pro M3/M4)
ollama pull qwen2.5:32b     # 48GB+ RAM (MacBook Pro M4 Pro)
ollama pull llama3.2:7b     # 8GB+ RAM  (MacBook Air M2/M3)

# 3. Install local-ai-acp
cargo install --git https://github.com/BlakeHung/local-ai-acp

# 4. Use with Zed editor (native ACP support on Mac)
#    Zed Settings > Agent > command = "local-ai-acp"

# Or use with openab (Discord bot)
#    config.toml: command = "local-ai-acp"
```

**Model recommendations by Mac:**

| Mac | RAM | Recommended model | Command |
|-----|-----|-------------------|---------|
| MacBook Air M2/M3 | 8-16GB | `llama3.2:7b` | `ollama pull llama3.2:7b` |
| MacBook Pro M3/M4 | 18-24GB | `gemma4:26b` | `ollama pull gemma4:26b` |
| MacBook Pro M4 Pro | 48GB | `qwen2.5:32b` | `ollama pull qwen2.5:32b` |
| Mac Studio M2/M4 Ultra | 64-192GB | `llama3.1:70b` | `ollama pull llama3.1:70b` |

## Use with openab

[openab](https://github.com/openabdev/openab) is a Discord-to-ACP bridge. Combined with local-ai-acp, anyone in your Discord server can use your local AI — zero API keys, zero cost.

```
Discord user                    Your machine
     │                               │
     │  @bot help me review           │
     │  this PR                       │
     v                                v
  Discord  ──WebSocket──▶  openab (Rust)
                              │
                              │ ACP (stdin/stdout)
                              v
                         local-ai-acp (Rust)
                              │
                              │ HTTP
                              v
                         Ollama + GPU
                         gemma4:26b
```

### Setup

```bash
# 1. Make sure Ollama is running
ollama serve
ollama pull gemma4:26b

# 2. Build local-ai-acp
cd local-ai-acp && cargo build --release
# copy binary to PATH
cp target/release/local-ai-acp /usr/local/bin/

# 3. Configure openab
cat > config.toml <<'EOF'
[discord]
bot_token = "${DISCORD_BOT_TOKEN}"
allowed_channels = ["your-channel-id"]

[agent]
command = "local-ai-acp"
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

Now anyone in your Discord server can `@bot` and get AI responses powered by your local GPU.

### Multi-bot setup

Run multiple Discord bots with different models — each bot is a separate openab instance:

```toml
# config-coder.toml — fast coding model
[discord]
bot_token = "${BOT_TOKEN_CODER}"
allowed_channels = ["your-channel-id"]

[agent]
command = "local-ai-acp"
env = { LLM_BASE_URL = "http://localhost:11434/v1", LLM_MODEL = "qwen2.5:32b" }

# config-reviewer.toml — analytical model
[discord]
bot_token = "${BOT_TOKEN_REVIEWER}"
allowed_channels = ["your-channel-id"]

[agent]
command = "local-ai-acp"
env = { LLM_BASE_URL = "http://localhost:11434/v1", LLM_MODEL = "gemma4:26b" }
```

```bash
# Run both
openab config-coder.toml &
openab config-reviewer.toml &
```

### Team GPU sharing via Discord

One GPU server can serve your entire team:

```
Team member A (no GPU) ──┐
Team member B (no GPU) ──┤── Discord ──▶ openab ──▶ local-ai-acp ──▶ Ollama + GPU
Team member C (no GPU) ──┘                         (your machine)
```

No one needs to install anything — just join the Discord server and `@bot`.

## Environment variables

| Variable | Default | Description |
|----------|---------|-------------|
| `LLM_BASE_URL` | `http://localhost:11434/v1` | OpenAI-compatible endpoint |
| `LLM_MODEL` | `gemma4:26b` | Model name |
| `LLM_API_KEY` | `local-ai` | API key (most local services ignore this) |
| `LLM_SYSTEM_PROMPT` | (auto-generated) | Custom system prompt |

Also supports `OLLAMA_BASE_URL`, `OLLAMA_MODEL`, `OLLAMA_API_KEY` as aliases.

## Install

```bash
# From source
cargo install --git https://github.com/BlakeHung/local-ai-acp

# Or build locally
git clone https://github.com/BlakeHung/local-ai-acp
cd local-ai-acp
cargo build --release
```

## ACP protocol support

| Method | Status |
|--------|--------|
| `initialize` | ✅ Supported |
| `session/new` | ✅ Multi-session with conversation history |
| `session/prompt` | ✅ Streaming via SSE |

| Notification | Status |
|--------------|--------|
| `agent_message_chunk` | ✅ Streaming text chunks |
| `agent_thought_chunk` | ✅ Emitted on prompt start |
| `tool_call` | ✅ LLM call tracking |
| `tool_call_update` | ✅ Completion status |

## Architecture

```
ACP Harness ──stdin/stdout──▶ local-ai-acp ──HTTP──▶ Local AI Server
(openab, Zed)  (JSON-RPC 2.0)   (Rust)       (SSE)   (OpenAI-compatible)
```

## License

MIT
