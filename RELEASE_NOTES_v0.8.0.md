# acp-bridge v0.8.0 — Simplified ACP-only adapter

## Summary
This release simplifies acp-bridge from a multi-mode ACP/A2A adapter to a focused, minimal ACP adapter. The A2A HTTP server and client modes have been removed, allowing the project to concentrate on its core strength: air-gap friendly ACP bridging to local inference backends.

## Major Changes

### Removed Features
- **A2A mode** (`--a2a`) — HTTP server with Agent Card support removed
- **Client mode** (`--client`) — External ACP agent spawning removed  
- **Marketing materials** — DEMO-AND-MARKETING.md and marketing-drafts.md removed
- **Dependencies** — axum and libc dependencies removed

### Architecture Improvements
- **Backend abstraction layer** — New `Backend` enum (Ollama/OpenAi) encapsulates protocol-specific logic
- **Simplified configuration** — Removed A2A and agent configuration sections
- **Cleaner codebase** — 1,341 lines of code removed (src/a2a.rs, src/client.rs, tests/client_test.rs)

### Documentation Updates
- **README simplification** — Removed OpenCode comparison table and competitive positioning
- **Focused positioning** — Now positioned as a "minimal ACP adapter for local AI"
- **Removed roadmap promises** — Simplified project status from "active development" to "minimal maintenance"

## Breaking Changes
- **A2A mode no longer available** — Users relying on `--a2a` should use ACP mode instead
- **Client mode no longer available** — Users spawning external agents should migrate to direct ACP usage
- **Configuration changes** — `[a2a]` and `[agent]` sections in config.toml are no longer supported

## Migration Guide
If you were using A2A mode:
```bash
# Old (A2A mode)
acp-bridge --a2a --port 8080

# New (ACP mode - use with ACP harness)
acp-bridge  # stdin/stdout JSON-RPC
```

If you were using client mode:
```bash
# Old (client mode)
acp-bridge --client --agent-command /path/to/agent

# New (direct ACP usage)
# Use your ACP harness (openab, Zed, JetBrains) to spawn acp-bridge directly
```

## What's Preserved
- ✅ Full ACP server surface (`initialize`, `session/new`, `session/prompt`, `session/end`)
- ✅ Ollama native and OpenAI-compatible backend support
- ✅ Streaming notifications and tool calling
- ✅ Built-in sandboxed tools (read_file, list_dir, search_code)
- ✅ Session management and history trimming
- ✅ Air-gap guarantee (zero outbound network calls beyond LLM endpoint)

## Installation
```bash
cargo install acp-bridge
# or download from GitHub Releases
```

## Docker
```bash
docker pull ghcr.io/blakehung/acp-bridge:0.8.0
```

## Support
For issues and questions, use GitHub Issues: https://github.com/BlakeHung/acp-bridge/issues

## Acknowledgments
This release was prepared with assistance from [Devin](https://devin.ai).
