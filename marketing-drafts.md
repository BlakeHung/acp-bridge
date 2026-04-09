# acp-bridge 行銷文草稿

---

## 1. Twitter/X Thread

### Tweet 1 — 主推

acp-bridge — ACP 生態系裡唯一免費的本地 AI Agent

Claude Code / Codex / Gemini CLI 都要付費 API
acp-bridge 讓你用 Ollama + 自己的 GPU 驅動 Zed、JetBrains、Neovim

- ~5MB Rust binary，零 dependency
- 完全離線，code 不離機
- 支援所有 OpenAI-compatible backend

https://github.com/BlakeHung/acp-bridge

#Rust #LocalAI #ACP #Ollama #OpenSource

### Tweet 2 — 生態系

更狠的玩法：

Linear 建 issue → webhook 觸發 → Discord bot 接單 → AI 自動拆解任務、寫 code、跑測試、開 PR

整條 pipeline 跑在你自己的 GPU 上，透過 acp-bridge 橋接

Linear + openab + acp-bridge + Ollama = 免費的 AI DevOps 團隊

一張 GPU 養一個自動化開發 pipeline，零 API 費用

### Tweet 3 — Mac 用戶

Mac Apple Silicon 用戶特別適合：

Unified Memory 讓 RAM 直接當 VRAM 用
M4 Pro 48GB → 跑 32B 模型
MacBook Air 16GB → 跑 7B 模型

Ollama + acp-bridge + Zed = 最佳本地 AI 開發體驗
完全離線，不需要網路，不需要 API key

---

## 2. Reddit / Hacker News / Dev.to

### acp-bridge: Bridge Any Local AI to the ACP Ecosystem — Free, Offline, Private

I built a Rust CLI tool that connects any OpenAI-compatible local AI backend (Ollama, vLLM, LocalAI, LM Studio, llama.cpp) to the Agent Client Protocol (ACP). If you use Zed, JetBrains IDEs, Neovim, or any ACP-compliant harness, you no longer need a paid API — just point acp-bridge at your local model and go.

**Why this exists:** Every first-class ACP agent right now — Claude Code, OpenAI Codex CLI, Gemini CLI, GitHub Copilot CLI — requires a paid cloud API. For students, open-source developers, or privacy-conscious organizations (enterprise, government, medical), that's a real barrier. acp-bridge removes it completely. It's a single ~5MB Rust binary with zero runtime dependencies.

**The bigger picture:** Combined with Linear and openab (a Discord-to-ACP bridge), you can build a fully automated dev pipeline running entirely on your own hardware:

1. Create a Linear issue with a "bot-task" label
2. Webhook fires → Discord bot picks it up
3. AI agent auto-decomposes the task, writes code, runs tests, creates a PR
4. Each stage syncs back to Linear with status updates and comments
5. All inference runs locally via Ollama — zero API cost, code never leaves your network

This is the same architecture powering our "Langya AI Team" project — an AI team of character-themed agents (CTO, full-stack dev, QA, CI/CD engineer) that autonomously handles the plan → code → test → PR lifecycle. The entire pipeline runs on a single GPU through acp-bridge.

On Apple Silicon Macs, unified memory means your full RAM is available as VRAM — even a 16GB MacBook Air can run 7B models smoothly.

**New in latest release:**

- Temperature and max\_tokens control via environment variables
- Configurable HTTP request timeout (default 5 minutes)
- Session cleanup (session/end) to prevent memory leaks
- Stderr logging for easier debugging
- RwLock-based concurrency for better multi-session performance

Repo: https://github.com/BlakeHung/acp-bridge
Feedback and contributions welcome.

---

## 3. 技術社群 / FB 開發者社團

### 【開源分享】acp-bridge — 讓本地 AI 驅動整條自動化開發 pipeline

ACP（Agent Client Protocol）生態系越來越成熟 — Zed editor、JetBrains 全系列、Neovim、Discord bot 都陸續支援。但有個問題：目前所有主流 ACP agent（Claude Code、Codex CLI、Gemini CLI、Copilot CLI）全部都要付費 API。

acp-bridge 是我用 Rust 寫的一個 CLI 工具，把任何 OpenAI-compatible 的本地 AI 服務（Ollama、vLLM、LM Studio、LocalAI、llama.cpp）橋接成標準 ACP server。一顆 ~5MB binary，零 runtime dependency，裝完就能用。

### 更完整的玩法：Linear + Discord + 本地 AI 全自動 pipeline

我們團隊實際在跑的架構：

1. 在 Linear 建一張 issue，加上 "bot-task" label
2. Webhook 觸發 Discord bot（透過 openab）
3. AI 自動拆解任務（梅長蘇 CTO agent）
4. AI 自動寫 code（飛流 full-stack agent）
5. AI 自動跑測試（夏冬 QA agent）
6. AI 自動開 PR（列戰英 CI/CD agent）
7. 每個階段自動同步回 Linear，更新狀態和留言

整條 pipeline 的推理都跑在本地 GPU 上，透過 acp-bridge 橋接 Ollama。零 API 費用，code 完全不離開內網。

### 適合的使用情境

- **企業 / 政府 / 醫療** — code 不離開內網，符合資安規範
- **學生 / 開源開發者** — 完全免費
- **團隊共用** — 一張 GPU + openab，整個 Discord server 都能用
- **Mac Apple Silicon** — unified memory 讓 RAM 直接當 VRAM，16GB 就能跑 7B 模型

### 最新版本更新

- 支援 temperature / max\_tokens / timeout 環境變數設定
- 新增 session/end 方法，避免記憶體洩漏
- RwLock 優化多 session 並行效能
- stderr logging 方便除錯

GitHub：https://github.com/BlakeHung/acp-bridge

歡迎 star、試用、提 issue 或 PR！

#Rust #OpenSource #LocalAI #ACP #Ollama #DevOps

---

## 4. Discord 社群公告

**acp-bridge** — 用本地 GPU 驅動整個 ACP 生態系

一個 Rust CLI 工具，把 Ollama / vLLM / LM Studio 橋接成 ACP server，讓 Zed、JetBrains、Neovim 直接用你的本地模型，不用花錢買 API

**搭配 openab + Linear 可以做到：**

Linear 建 issue → webhook 觸發 → AI 自動拆任務、寫 code、跑測試、開 PR
整條 pipeline 跑在你的 GPU 上，零費用

我們團隊實際在用這套架構（代號：琅琊 AI 團隊），有興趣的可以看看 repo，歡迎交流！

https://github.com/BlakeHung/acp-bridge

---

## 5. LinkedIn 專業版

### Building AI-powered development workflows shouldn't require expensive cloud APIs.

I'm sharing acp-bridge — an open-source Rust CLI that bridges any local AI service (Ollama, vLLM, LM Studio) to the Agent Client Protocol (ACP) ecosystem.

**What this enables:**

- Zed, JetBrains, Neovim users can use local AI models as their coding agent
- Teams can share one GPU via Discord (using openab) — zero per-seat API costs
- Enterprise/government orgs get AI coding assistance without data leaving the network

We've taken it further by integrating with Linear for fully automated pipelines: an issue with a "bot-task" label triggers AI agents that decompose the task, write code, run tests, and create pull requests — all powered by local inference.

The entire stack runs on a single GPU. On Apple Silicon Macs, unified memory architecture makes even a 16GB machine capable of running solid models.

If your team is evaluating AI coding tools but concerned about cost, privacy, or vendor lock-in — this might be worth a look.

GitHub: https://github.com/BlakeHung/acp-bridge

#OpenSource #Rust #AI #DevTools #ACP #LocalAI

---

## 投稿策略建議

| 平台 | 版本 | 投稿位置 |
|------|------|---------|
| Twitter/X | Thread (3 則) | 個人帳號 + tag @zabordev @olabordev |
| Reddit | 英文長版 | r/rust, r/selfhosted, r/LocalLLaMA, r/programming |
| Hacker News | 英文長版 | Show HN post |
| Dev.to | 英文長版 | Article with tags: rust, ai, opensource, devtools |
| FB | 中文技術社群版 | 台灣 Rust 社群、台灣軟體工程師、AI 工具分享 |
| PTT | 中文技術社群版 | Soft\_Job, Programming |
| Discord | 公告版 | 相關技術 Discord servers |
| LinkedIn | 專業版 | 個人 profile + Rust / AI 相關 groups |
