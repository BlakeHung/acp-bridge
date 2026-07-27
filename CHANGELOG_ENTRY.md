## [0.8.0] - 2026-07-27

### Breaking Changes
- **A2A mode removed** — `--a2a` HTTP server with Agent Card support is no longer available
- **Client mode removed** — `--client` external ACP agent spawning is no longer available
- **Configuration changes** — `[a2a]` and `[agent]` sections in config.toml are no longer supported

### Added
- **Backend abstraction layer** — New `Backend` enum (Ollama/OpenAi) encapsulates protocol-specific logic for message formatting, response extraction, and tool-call handling
- **Simplified architecture** — Focused ACP-only adapter with cleaner codebase

### Removed
- **A2A implementation** — src/a2a.rs (299 lines) deleted
- **Client implementation** — src/client.rs (869 lines) deleted  
- **Client tests** — tests/client_test.rs (173 lines) deleted
- **Marketing materials** — DEMO-AND-MARKETING.md (718 lines) and marketing-drafts.md (176 lines) deleted
- **Dependencies** — axum and libc crates removed from Cargo.toml

### Changed
- **README simplification** — Removed OpenCode comparison table and competitive positioning language
- **Project status** — Changed from "active development" to "minimal maintenance"
- **Configuration system** — Simplified to only support LLM configuration
- **Help text** — Updated to reflect ACP-only positioning (removed --a2a and --client options)

### Internal
- **Backend-specific logic moved** — Protocol quirks moved from engine.rs to llm.rs Backend enum
- **Code reduction** — 1,341 lines of code removed overall
- **Simplified RunMode enum** — Now only Acp and Bench modes

### Migration Notes
Users relying on A2A mode should migrate to ACP mode with their ACP harness. Users using client mode should configure their harness to spawn acp-bridge directly via stdin/stdout JSON-RPC.
