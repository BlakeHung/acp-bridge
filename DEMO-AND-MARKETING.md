# acp-bridge v0.6.0 — Demo, Marketing & 情境展示

```
  新定位: "When OpenCode can't reach the internet, acp-bridge can."
  核心訊息: 內網 AI agent + A2A 跨 agent 協作
  與 v0.5.0 差異: 不再硬剛 OpenCode，改打互補
```

---

## Demo 展示方案

### Demo 1: A2A Agent Card（30 秒）

```
  場景: 一行指令啟動 A2A server，curl 看到 Agent Card
  重點: 展示 acp-bridge 是 A2A-compatible，可被發現

  步驟:
  1. terminal: acp-bridge --a2a
  2. terminal: curl http://localhost:8080/.well-known/agent.json | jq
  3. 看到 Agent Card JSON（name, version, skills, capabilities）

  錄製: asciinema rec → asciinema.org embed
  時長: ~15 秒
```

```bash
# Demo script
export LLM_BASE_URL=http://localhost:11434
export LLM_MODEL=gemma4:26b

# Terminal 1: 啟動
acp-bridge --a2a

# Terminal 2: 查看 Agent Card
curl -s http://localhost:8080/.well-known/agent.json | jq .

# 預期輸出:
# {
#   "name": "acp-bridge",
#   "description": "Self-hosted AI agent bridge...",
#   "url": "http://0.0.0.0:8080",
#   "version": "0.6.0",
#   "capabilities": { "streaming": false, "pushNotifications": false },
#   "skills": [{ "id": "coding-assistant", "name": "Coding Assistant", ... }]
# }
```

---

### Demo 2: A2A message/send（45 秒）

```
  場景: 用 curl 直接跟 acp-bridge 對話（模擬另一個 agent 呼叫）
  重點: 展示 A2A protocol 的 request-response

  步驟:
  1. acp-bridge --a2a 跑著
  2. curl POST message/send
  3. 看到 task 回應（id, status: completed, artifacts）
```

```bash
# 發送 A2A message/send
curl -s -X POST http://localhost:8080/ \
  -H "Content-Type: application/json" \
  -d '{
    "jsonrpc": "2.0",
    "id": 1,
    "method": "message/send",
    "params": {
      "message": {
        "role": "user",
        "parts": [{"type": "text", "text": "What is the Fibonacci sequence?"}]
      }
    }
  }' | jq .

# 預期輸出:
# {
#   "jsonrpc": "2.0",
#   "id": 1,
#   "result": {
#     "id": "task-uuid",
#     "status": { "state": "completed" },
#     "artifacts": [{ "parts": [{ "type": "text", "text": "..." }] }]
#   }
# }
```

---

### Demo 3: ACP + A2A 雙模式（60 秒）

```
  場景: 展示同一個 binary 支援兩種協議
  重點: 向後相容 + 新能力

  步驟:
  1. 先跑 ACP mode: echo JSON-RPC | acp-bridge
  2. 再跑 A2A mode: acp-bridge --a2a
  3. curl Agent Card + message/send

  錄製: split terminal，左 ACP 右 A2A
```

---

### Demo 4: OpenCode + acp-bridge Multi-Agent（90 秒，旗艦 demo）

```
  場景: Discord 裡兩個 bot，一個用 OpenCode (cloud)，一個用 acp-bridge (內網)
  重點: 「前線 + 後勤」互補架構

  步驟:
  1. Discord channel A: @cloud-bot（OpenCode + Ollama Cloud）
     → 問一般開發問題
  2. Discord channel B: @secure-bot（acp-bridge + 內網 Ollama）
     → 問需要讀內部原始碼的問題
  3. 展示 @secure-bot 用 read_file / search_code 讀內網 code
  4. 同一個 openab instance 管兩個 bot

  config-cloud.toml:
  [agent]
  command = "opencode"
  args = ["acp"]

  config-secure.toml:
  [agent]
  command = "acp-bridge"
  env = { LLM_BASE_URL = "http://internal-gpu:11434", LLM_MODEL = "qwen2.5:32b" }

  錄製: 螢幕錄影 Discord 操作 → GIF
  時長: ~60-90 秒
```

---

### Demo 5: Docker Compose 一鍵部署（45 秒）

```
  場景: docker compose up 直接跑 Ollama + acp-bridge + openab
  重點: 內網環境零設定

  docker-compose.yml:
  services:
    ollama:
      image: ollama/ollama
      deploy:
        resources:
          reservations:
            devices:
              - driver: nvidia
                count: 1
                capabilities: [gpu]

    acp-bridge:
      build: .
      environment:
        - LLM_BASE_URL=http://ollama:11434
        - LLM_MODEL=gemma4:26b
      network_mode: "host"  # for A2A mode
      command: ["acp-bridge", "--a2a"]

  步驟:
  1. docker compose up -d
  2. curl http://localhost:8080/.well-known/agent.json
  3. curl POST message/send → 得到回應
  4. 「從頭到尾沒有出過 localhost」

  錄製: terminal 操作
```

---

## Marketing 文案

### 核心訊息矩陣

```
  ┌────────────────┬─────────────────────────────────────────┐
  │ 受眾           │ 訊息                                     │
  ├────────────────┼─────────────────────────────────────────┤
  │ r/LocalLLaMA   │ "A2A-compatible local agent, no cloud"  │
  │ r/selfhosted   │ "Air-gapped AI agent in Docker"         │
  │ r/rust         │ "Dual protocol: ACP stdin + A2A HTTP"   │
  │ Ollama Discord │ "Ollama as A2A agent backend"           │
  │ 台灣 AI 社群   │ "主權 AI + A2A 標準 + 內網部署"          │
  │ LinkedIn       │ "Enterprise AI agent, data stays local" │
  │ Hacker News    │ "Show HN: A2A agent bridge, 5MB binary" │
  └────────────────┴─────────────────────────────────────────┘
```

---

### 1. r/LocalLLaMA (EN)

```
Title: acp-bridge v0.6.0: Local Ollama as an A2A agent —
       complements OpenCode for air-gapped setups

Body:

tl;dr: acp-bridge now speaks A2A (Google's Agent-to-Agent protocol).
Your local Ollama can be discovered by other agents via Agent Card
and respond to A2A message/send requests.

What changed:
- NEW: A2A HTTP server mode (acp-bridge --a2a)
- NEW: Agent Card at /.well-known/agent.json
- NEW: A2A message/send with task lifecycle
- KEPT: ACP stdin/stdout mode (backward compatible)

Why this matters:
OpenCode is great — 140K stars, native ACP, Ollama Cloud support.
But it needs internet. acp-bridge fills the gap:

- Air-gapped / classified networks → acp-bridge is the only option
- vLLM / TGI / llama.cpp → OpenCode doesn't support these
- Docker Compose embedded → 5MB binary, no Go/npm runtime

The new A2A support means acp-bridge isn't just a standalone agent —
it can be part of a multi-agent system. Imagine:

  OpenCode (cloud) ── A2A ──▶ acp-bridge (internal GPU)

OpenCode handles general tasks, acp-bridge handles sensitive code
on your internal network. Data classification at the agent level.

Demo: curl http://localhost:8080/.well-known/agent.json

GitHub: https://github.com/BlakeHung/acp-bridge

5MB Rust binary. 43 tests. MIT. Zero API cost.
```

---

### 2. r/selfhosted (EN)

```
Title: Self-hosted AI coding agent with A2A protocol —
       Docker + Ollama + acp-bridge, no cloud required

Body:

Just shipped v0.6.0 of acp-bridge. It's a 5MB Rust binary that
turns your local Ollama into an AI agent that:

- Serves an Agent Card (/.well-known/agent.json)
- Responds to A2A message/send requests
- Has built-in sandboxed tools (read files, search code)
- Works in stdin/stdout mode for IDE integration (Zed, JetBrains)

Docker one-liner:
  docker run --network=host -e LLM_BASE_URL=http://localhost:11434 \
    acp-bridge --a2a

Then:
  curl http://localhost:8080/.well-known/agent.json

For Discord access, pair with openab — your team talks to your
local GPU through Discord. Zero per-seat cost.

The A2A support means other agents can delegate tasks to your
self-hosted instance. Sensitive data processing stays on your
hardware while cloud agents handle the rest.

GitHub: https://github.com/BlakeHung/acp-bridge
```

---

### 3. Ollama Discord (EN)

```
Channel: #showcase or #community-projects
```

```
acp-bridge v0.6.0 — Your Ollama is now an A2A agent

New: A2A (Agent-to-Agent) protocol support
Your Ollama can be discovered and called by other AI agents.

  acp-bridge --a2a
  curl http://localhost:8080/.well-known/agent.json

Works alongside OpenCode — not competing, complementing:
- OpenCode: cloud tasks, Ollama Cloud
- acp-bridge: air-gapped, internal GPU, vLLM/llama.cpp

Still supports ACP stdin mode for Zed/JetBrains.
5MB binary. 43 tests. MIT. Ollama native API.

GitHub: https://github.com/BlakeHung/acp-bridge
```

---

### 4. Ollama GitHub Discussions (EN)

```
Category: Show and tell
Title: acp-bridge v0.6.0: Ollama as A2A agent — complements
       OpenCode for enterprise/air-gapped environments
```

```
Hi Ollama community,

acp-bridge v0.6.0 adds A2A (Agent-to-Agent) protocol support
to your local Ollama.

## What's new

1. A2A HTTP server mode: `acp-bridge --a2a`
2. Agent Card: `GET /.well-known/agent.json`
3. A2A message/send: other agents can delegate tasks to your Ollama
4. Dual protocol: ACP (stdin) + A2A (HTTP) in one binary

## Why A2A matters

With OpenCode dominating the "cloud agent" space, acp-bridge focuses
on where OpenCode can't go:

  OpenCode (cloud)  ←── A2A ───→  acp-bridge (your GPU)
                                    ↕
                              read_file / search_code

Multi-agent architecture: OpenCode handles general tasks via Ollama
Cloud, while acp-bridge handles sensitive code on internal hardware.

## Enterprise scenario

  ┌─ channel A: @cloud-bot → OpenCode + Ollama Cloud (general dev)
  │
  └─ channel B: @secure-bot → acp-bridge + internal GPU (classified)

Same Discord, same openab, different trust levels.

## Quick start

  acp-bridge --a2a
  curl http://localhost:8080/.well-known/agent.json

Still supports ACP mode (backward compatible), native Ollama API,
built-in tools, 43 tests, 5MB binary, MIT license.

GitHub: https://github.com/BlakeHung/acp-bridge
Discord bridge: https://github.com/openabdev/openab
```

---

### 5. X / Twitter

```
EN:

acp-bridge v0.6.0 — Your local Ollama is now an A2A agent.

  acp-bridge --a2a
  curl localhost:8080/.well-known/agent.json

Complements OpenCode: cloud tasks → OpenCode, air-gapped → acp-bridge.
Multi-agent via A2A protocol. 5MB Rust binary. MIT.

https://github.com/BlakeHung/acp-bridge

#A2A #Ollama #LocalAI #Rust #OpenSource
```

```
zh-TW:

acp-bridge v0.6.0 — 你的 Ollama 現在是 A2A agent 了

acp-bridge --a2a 一行啟動
其他 agent 可以透過 A2A 協議呼叫你的內網 AI

跟 OpenCode 互補不競爭：
  雲端任務 → OpenCode
  內網機密 → acp-bridge

5MB Rust binary / MIT / 零 API 費用

https://github.com/BlakeHung/acp-bridge

#A2A #Ollama #主權AI #Rust #開源
```

---

### 6. LinkedIn (EN)

```
Announcing acp-bridge v0.6.0 — now with A2A protocol support.

The AI agent ecosystem is consolidating around two protocols:
- ACP (Agent Client Protocol) — IDE ↔ Agent communication
- A2A (Agent-to-Agent) — Agent ↔ Agent collaboration

acp-bridge now supports both. One 5MB binary, zero cloud dependency.

For enterprises, this enables a practical multi-agent architecture:

  Cloud agents (OpenCode) handle general development tasks.
  Local agents (acp-bridge) handle sensitive code on internal GPUs.
  Both communicate via A2A — data classification at the agent level.

Use cases we're seeing:
→ Financial institutions: code review without data leaving the network
→ Government agencies: sovereign AI with local inference
→ Healthcare: HIPAA-compliant code assistance

The new A2A mode serves an Agent Card for service discovery
and handles message/send for synchronous task delegation.

Demo: acp-bridge --a2a → curl localhost:8080/.well-known/agent.json

Open source. MIT license. Built with Rust.
GitHub: https://github.com/BlakeHung/acp-bridge

#AI #A2A #Enterprise #DataSovereignty #LocalAI #OpenSource
```

---

### 7. 台灣 AI 社群 / Facebook (zh-TW)

```
[開源] acp-bridge v0.6.0 — 內網 AI agent + A2A 協作

大家可能都看到 OpenCode 最近很紅（140K+ stars）
它原生支援 Ollama Cloud + ACP，很方便

但有些場景 OpenCode 做不到：
- 公司網路不能連外（air-gapped）
- 需要跑 vLLM / TGI 等特殊推論引擎
- 合規要求資料不出內網

acp-bridge 就是補這個缺口

v0.6.0 新增 A2A 協議支援：
- acp-bridge --a2a 一行啟動 HTTP server
- /.well-known/agent.json Agent Card（服務發現）
- message/send 讓其他 agent 委派任務

跟 OpenCode 互補的架構：

  Discord → openab ─┬─▶ OpenCode (雲端/一般開發)
                     └─▶ acp-bridge (內網/機密專案)

兩個 agent 可以透過 A2A 協作
一般任務交給雲端，敏感任務留在內網
資料分級處理，一套架構搞定

5MB Rust binary / 43 tests / MIT / 零 API 費用

GitHub: https://github.com/BlakeHung/acp-bridge
openab: https://github.com/openabdev/openab

#主權AI #A2A #Ollama #Rust #開源 #台灣
```

---

### 8. Hacker News (EN)

```
Title: Show HN: A2A-compatible local AI agent in a 5MB Rust binary

URL: https://github.com/BlakeHung/acp-bridge

Comment:

acp-bridge turns any OpenAI-compatible LLM backend (Ollama, vLLM,
llama.cpp) into an A2A agent. One binary, dual protocol:

  ACP mode: stdin/stdout JSON-RPC (for IDEs like Zed, JetBrains)
  A2A mode: HTTP with Agent Card (for agent-to-agent collaboration)

The A2A support opens up multi-agent architectures where cloud
agents (OpenCode) handle general work and local agents (acp-bridge)
handle sensitive code. Communication via A2A protocol.

Built-in sandboxed tools: read_file, list_dir, search_code.
Native Ollama API. Retry with backoff. Graceful shutdown.

43 tests, MIT license, ~2000 lines of Rust.
```

---

## 情境展示文件

### 情境 1: 企業內網開發團隊

```
+====================================================================+
|  場景: 金融公司，20 人開發團隊，code 不能上雲端                       |
+====================================================================+
|                                                                      |
|  公司 GPU server (A100)                                              |
|  ├── Ollama (gemma4:26b / qwen2.5:32b)                             |
|  ├── acp-bridge --a2a (port 8080)                                   |
|  └── openab (Discord bot)                                           |
|                                                                      |
|  團隊在 Discord:                                                     |
|  #backend   → @ai-agent "分析這個 API 的 error handling"            |
|  #frontend  → @ai-agent "幫我 review 這個 React component"         |
|  #devops    → @ai-agent "這個 Dockerfile 有什麼問題"                |
|                                                                      |
|  所有推論跑在公司 GPU                                                |
|  所有 code 讀取在公司內網                                             |
|  Discord 只傳文字（不傳 code 到 Discord server）                     |
|  合規 ✓ 資安 ✓ 零 API 費用 ✓                                        |
|                                                                      |
+====================================================================+
```

### 情境 2: 雲端 + 內網混合架構

```
+====================================================================+
|  場景: 新創公司，部分專案機密，部分專案可用雲端                       |
+====================================================================+
|                                                                      |
|  Discord → openab ─┬─▶ OpenCode + Ollama Cloud                     |
|                     │   @cloud-bot (一般開發)                        |
|                     │   ✓ 速度快                                     |
|                     │   ✓ 模型選擇多                                 |
|                     │   ✗ code 會到雲端                               |
|                     │                                                |
|                     └─▶ acp-bridge + 內網 GPU                       |
|                         @secure-bot (機密專案)                       |
|                         ✓ 資料不出內網                                |
|                         ✓ 可讀內部原始碼                              |
|                         ✗ 速度看 GPU                                 |
|                                                                      |
|  openab config:                                                      |
|  ┌─────────────────────────────────────────────────┐                |
|  │ config-cloud.toml                                │                |
|  │ [discord]                                        │                |
|  │ allowed_channels = ["general-dev"]               │                |
|  │ [agent]                                          │                |
|  │ command = "opencode"                             │                |
|  │ args = ["acp"]                                   │                |
|  ├─────────────────────────────────────────────────┤                |
|  │ config-secure.toml                               │                |
|  │ [discord]                                        │                |
|  │ allowed_channels = ["classified-dev"]            │                |
|  │ [agent]                                          │                |
|  │ command = "acp-bridge"                           │                |
|  │ env = { LLM_BASE_URL = "http://10.0.0.5:11434"  │                |
|  │         LLM_MODEL = "qwen2.5:32b" }             │                |
|  └─────────────────────────────────────────────────┘                |
|                                                                      |
+====================================================================+
```

### 情境 3: A2A 跨 agent 協作

```
+====================================================================+
|  場景: OpenCode 做主力，遇到內網資料時委派給 acp-bridge              |
+====================================================================+
|                                                                      |
|  User: "分析 internal-api 的效能瓶頸"                                |
|                                                                      |
|  OpenCode (cloud agent):                                             |
|    → 知道 internal-api 的 code 在內網                                |
|    → 透過 A2A 委派給 acp-bridge                                     |
|                                                                      |
|  POST http://internal:8080/                                          |
|  {                                                                   |
|    "method": "message/send",                                         |
|    "params": {                                                       |
|      "message": {                                                    |
|        "role": "user",                                               |
|        "parts": [{                                                   |
|          "type": "text",                                             |
|          "text": "Read src/api/routes.rs and identify              |
|                   performance bottlenecks"                           |
|        }]                                                            |
|      },                                                              |
|      "metadata": { "cwd": "/opt/internal-api" }                    |
|    }                                                                 |
|  }                                                                   |
|                                                                      |
|  acp-bridge (internal agent):                                        |
|    → 用 read_file 讀內網 code                                       |
|    → 用 search_code 找 pattern                                      |
|    → 回傳分析結果給 OpenCode                                         |
|    → code 從未離開內網                                               |
|                                                                      |
|  OpenCode 整合內外結果，回覆 user                                    |
|                                                                      |
+====================================================================+
```

### 情境 4: CI/CD Pipeline 嵌入

```
+====================================================================+
|  場景: GitHub Actions 裡用 acp-bridge 做 AI code review             |
+====================================================================+
|                                                                      |
|  .github/workflows/ai-review.yml:                                   |
|                                                                      |
|  steps:                                                              |
|    - name: Start Ollama                                              |
|      run: ollama serve &                                             |
|                                                                      |
|    - name: Pull model                                                |
|      run: ollama pull gemma4:12b                                     |
|                                                                      |
|    - name: AI Review                                                 |
|      run: |                                                          |
|        acp-bridge --a2a &                                            |
|        sleep 5                                                       |
|        curl -s -X POST http://localhost:8080/ \                      |
|          -H "Content-Type: application/json" \                       |
|          -d '{                                                       |
|            "jsonrpc":"2.0","id":1,                                   |
|            "method":"message/send",                                  |
|            "params":{                                                |
|              "message":{"role":"user","parts":[                      |
|                {"type":"text","text":"Review the diff: ..."}         |
|              ]},                                                     |
|              "metadata":{"cwd":"$GITHUB_WORKSPACE"}                  |
|            }                                                         |
|          }' > review.json                                            |
|        # Parse and post as PR comment                                |
|                                                                      |
|  5MB binary，不需要 Node/Python runtime                              |
|  Self-hosted runner 上完全離線                                        |
|                                                                      |
+====================================================================+
```

---

## Demo GIF 錄製清單

```
  ┌────┬────────────────────────────────┬──────────┬──────────┐
  │ #  │ Demo                           │ 時長     │ 工具     │
  ├────┼────────────────────────────────┼──────────┼──────────┤
  │ 1  │ Agent Card (curl)              │ ~15 秒   │ asciinema│
  │ 2  │ A2A message/send (curl)        │ ~30 秒   │ asciinema│
  │ 3  │ ACP + A2A 雙模式              │ ~45 秒   │ asciinema│
  │ 4  │ Discord multi-agent            │ ~60 秒   │ 螢幕錄影 │
  │ 5  │ Docker Compose 一鍵            │ ~30 秒   │ asciinema│
  └────┴────────────────────────────────┴──────────┴──────────┘

  優先級:
  ★★★ Demo 1 + 2 — 最容易錄，最有說服力
  ★★  Demo 5 — selfhosted 社群吃這個
  ★   Demo 4 — 需要完整環境，但最震撼
```

---

## 發佈順序

```
  第零波 (準備)
  ├── [x] v0.6.0 code 完成
  ├── [ ] 錄 Demo 1 + 2 GIF
  ├── [ ] 更新 GitHub repo description
  ├── [ ] cargo publish 到 crates.io
  ├── [ ] GitHub Release + CHANGELOG
  └── [ ] 更新 config.toml.example（加 [a2a] section）

  第一波 (Day 1) — 自己的社群
  ├── [ ] openab Discord (zh-TW)
  └── [ ] GitHub Release Notes

  第二波 (Day 1-2) — 技術社群
  ├── [ ] Ollama Discord (#showcase)
  ├── [ ] Ollama GitHub Discussions
  ├── [ ] r/LocalLLaMA
  ├── [ ] r/selfhosted (新增)
  └── [ ] r/rust

  第三波 (Day 2-3) — 社群媒體
  ├── [ ] X / Twitter (EN + zh-TW)
  ├── [ ] LinkedIn
  ├── [ ] Facebook 台灣 AI 社團
  └── [ ] Threads

  第四波 (Day 3-5) — 長文
  ├── [ ] Hacker News (Show HN)
  ├── [ ] Dev.to
  └── [ ] Medium

  第五波 (Phase 3 完成後)
  ├── [ ] Ollama integrations PR
  └── [ ] A2A community announcement
```

---

## 追蹤指標

```
  發佈後 7 天:
  [ ] GitHub stars
  [ ] crates.io downloads
  [ ] Reddit upvotes / comments
  [ ] Ollama Discussion 回應
  [ ] Agent Card 被 curl 的次數（如果有 logging）
  [ ] A2A message/send 請求數
  [ ] Discord 新成員
```
