# acp-bridge 發展路線圖

```
  目標: 被 Ollama 納入 integrations 頁面
  策略: 做深 Ollama 整合 → 加 MCP → 社群推廣 → 正式提 PR
```

---

## 背景

```
  Ollama 在 2026 年開始往雲端發展 (Ollama Cloud + GLM-5.1 on Blackwell)
  同時開放更多整合生態。

  我們從 openab 社群開發中發現地端 AI 的缺口，做了 acp-bridge
  填補「local LLM → agent protocol」這段最後一哩路。

  現在 Ollama 自己也要做這塊了 —
  我們已經準備好了，acp-bridge + openab 的串接已驗證可用。
  是時候讓 Ollama 知道我們存在。
```

---

## 全局時間軸

```
  v0.1.0  初始版本
  v0.2.0  結構化 logging / retry / graceful shutdown
  v0.2.1  ✅ Sprint 1 — 安全性修復 (CWD injection, error response, buffer cap)
  v0.3.0  ✅ Sprint 2 — 穩定性改善 (SSE parsing, connection pool, session TTL)
  v0.4.0  ✅ Phase 1 — Ollama 原生整合 (/api/chat, /api/show, /api/ps)
    │
    │                                        你在這裡 ★
    │
    ├─ Wave 1 宣傳                           現在開始
    │   └─ 目標: "讓人知道你存在"
    │
    ├─ Phase 2: MCP Server 模式 (v0.5.0)     ~2 週
    │   └─ 目標: "Ollama model 變成 MCP tool，能讀檔案"
    │
    ├─ Wave 2 宣傳                           Phase 2 完成後
    │   └─ 目標: "直接跟 Ollama 社群對話，提 PR 到 integrations"
    │
    ├─ Phase 3: A2A 遷移 (v0.6.0)           ~2 週
    │   └─ 目標: "跟上標準，ACP → A2A"
    │
    └─ Wave 3 宣傳                           Phase 3 完成後
        └─ 目標: "更大範圍曝光"
```

---

## Phase 1: Ollama 原生整合 (v0.4.0) ✅ 已完成

### 為什麼要做

```
  現在 acp-bridge 怎麼跟 Ollama 溝通:

  acp-bridge ──→ /v1/chat/completions ──→ Ollama
                 (OpenAI 相容 API)

  這行得通，但 Ollama 有自己的原生 API，功能更多:

  /api/chat     ← 原生對話 (支援更多 Ollama 特有功能)
  /api/tags     ← 列出模型 (你已經用了)
  /api/show     ← 查模型詳情 (context length, 參數量)
  /api/ps       ← 哪些模型正在跑 (載入 VRAM 了沒)

  用原生 API = 告訴 Ollama「我不是隨便套的，我是認真整合的」
```

### 要做的事

```
  ┌────┬──────────────────────────────────────────────────────┐
  │ #  │ 任務                                                 │
  ├────┼──────────────────────────────────────────────────────┤
  │ 1  │ 支援 /api/chat 作為 backend                          │
  │    │                                                      │
  │    │ 自動偵測:                                             │
  │    │   base_url 有 /v1 → 用 OpenAI compat (現有邏輯)      │
  │    │   base_url 沒有 /v1 → 用 Ollama native /api/chat    │
  │    │                                                      │
  │    │ /api/chat 的 SSE 格式跟 OpenAI 不同:                  │
  │    │   OpenAI:  data: {"choices":[{"delta":{"content":..  │
  │    │   Ollama:  {"message":{"content":"..."}}             │
  │    │   → 需要寫新的 stream parser                         │
  │    │                                                      │
  ├────┼──────────────────────────────────────────────────────┤
  │ 2  │ 用 /api/show 自動取得 context length                 │
  │    │                                                      │
  │    │ 現在: max_history_turns 是手動設的 (預設 50)          │
  │    │ 之後: 啟動時問 Ollama「這模型 context 多長」           │
  │    │       → 自動算出合理的 max_history_turns              │
  │    │                                                      │
  │    │   例: gemma4:26b context=8192 tokens                 │
  │    │       → 大約 40 turns 就快滿了                        │
  │    │       → auto-set max_history_turns=35 (留 buffer)    │
  │    │                                                      │
  ├────┼──────────────────────────────────────────────────────┤
  │ 3  │ 用 /api/ps 做更好的 health check                     │
  │    │                                                      │
  │    │ 現在: probe_backend 只看「server 有沒有回應」          │
  │    │ 之後: 還看「模型有沒有載入 VRAM」                      │
  │    │                                                      │
  │    │   ollama ps 回傳:                                     │
  │    │   {"models":[{"name":"gemma4:26b",                   │
  │    │     "size": 15000000000,                              │
  │    │     "vram_size": 15000000000}]}                       │
  │    │                                                      │
  │    │   → 知道模型是否 ready                                │
  │    │   → 如果沒載入，log 提示 "ollama run gemma4:26b"     │
  │    │                                                      │
  ├────┼──────────────────────────────────────────────────────┤
  │ 4  │ 發佈到 crates.io                                     │
  │    │                                                      │
  │    │ → cargo install acp-bridge 一行安裝                   │
  │    │ → Ollama 整合文件可以直接引用                          │
  │    │ → 增加專案可信度                                      │
  │    │                                                      │
  ├────┼──────────────────────────────────────────────────────┤
  │ 5  │ Integration tests                                    │
  │    │                                                      │
  │    │ 新增 mock Ollama native API server:                   │
  │    │ - /api/chat streaming (Ollama 格式)                   │
  │    │ - /api/show 回傳 context length                       │
  │    │ - /api/ps 回傳模型狀態                                │
  │    │ - 自動偵測 backend 類型                                │
  │    │ - context length → auto max_history_turns             │
  │    │                                                      │
  └────┴──────────────────────────────────────────────────────┘
```

### 完成標準

```
  [x] cargo test 全過 (38 tests, 含 3 個 Ollama native tests)
  [x] cargo fmt + clippy 乾淨
  [x] CHANGELOG 更新
  [x] README 更新 (Ollama native 說明)
  [x] PR #3 → merge → v0.4.0 tag + GitHub Release
  [x] 手動測試: openab + acp-bridge + Ollama native mode 串接驗證
  [ ] cargo publish 到 crates.io (待執行)
```

---

## Wave 1 宣傳 (Phase 1 完成後)

### 目標

```
  讓人知道 acp-bridge 存在
  建立初始 GitHub stars + 社群認知
```

### 準備物

```
  [ ] 30 秒 demo GIF
      場景: Discord 打字 → openab 轉發 → acp-bridge log
            → Ollama 回應 → Discord 即時顯示
      工具: asciinema 或 screen recording

  [ ] 一段式介紹文 (EN)
      "acp-bridge — Turn your local Ollama into an AI agent
       backend. One 5MB Rust binary. No API keys. No cloud.
       Your data never leaves your machine."

  [ ] 一段式介紹文 (zh-TW)
      "acp-bridge — 讓你的 Ollama 變成 AI agent 後端。
       5MB Rust binary，不需要 API key，資料不出內網。"
```

### 發佈平台

```
  ┌──────────────────┬────────────────────────────────────┬───────┐
  │ 平台             │ 內容                                │ 語言  │
  ├──────────────────┼────────────────────────────────────┼───────┤
  │ openab Discord   │ v0.4.0 發佈 + 感謝社群             │ zh-TW │
  │ r/LocalLLaMA     │ "5MB binary, Ollama native, $0"   │ EN    │
  │ 台灣 AI 社群     │ 切「主權 AI」角度                   │ zh-TW │
  │ Ollama Discord   │ 簡短介紹 + GIF                     │ EN    │
  │ Ollama GitHub    │ Discussion post                    │ EN    │
  │ crates.io        │ cargo publish (自動曝光)            │ -     │
  └──────────────────┴────────────────────────────────────┴───────┘
```

---

## Phase 2: MCP Server 模式 (v0.5.0)

### 為什麼要做

```
  現在 acp-bridge 只能「聊天」:

  user: "分析我的專案"
  LLM:  "好的...（但我看不到你的檔案）" ← 沒用

  加 MCP 後，LLM 可以呼叫工具:

  user: "分析我的專案"
  LLM:  → tool_call: read_file("src/main.rs")
        → tool_call: list_dir("src/")
        → "你的專案結構是..."  ← 有用了

  MCP = Model Context Protocol
  讓 LLM 能「看到」外面的世界
```

### 架構

```
  現在:
  ┌────────┐     ┌────────────┐     ┌────────┐
  │ openab │ ──→ │ acp-bridge │ ──→ │ Ollama │
  └────────┘     └────────────┘     └────────┘
                  只能聊天

  之後:
  ┌────────┐     ┌────────────┐     ┌────────┐
  │ openab │ ──→ │ acp-bridge │ ──→ │ Ollama │
  └────────┘     │            │     └────┬───┘
                 │   MCP Host │          │
                 │     ┌──────┤   tool_call
                 │     │ MCP  │◀─────────┘
                 │     │Server│
                 │     │      │──→ 讀檔案
                 │     │      │──→ 列目錄
                 │     │      │──→ 搜尋 code
                 │     └──────┘
                 └────────────┘
```

### 要做的事

```
  ┌────┬──────────────────────────────────────────────────────┐
  │ #  │ 任務                                                 │
  ├────┼──────────────────────────────────────────────────────┤
  │ 1  │ 內建基礎 MCP tools                                   │
  │    │ - read_file: 讀取指定檔案內容                         │
  │    │ - list_dir: 列出目錄結構                              │
  │    │ - search_code: grep/搜尋 code                        │
  │    │ - run_command: 執行 shell 指令 (可選, 有安全疑慮)     │
  │    │                                                      │
  ├────┼──────────────────────────────────────────────────────┤
  │ 2  │ Tool call 解析                                        │
  │    │ LLM 回應如果包含 tool_call → 執行 → 結果回傳 LLM     │
  │    │ → LLM 再根據結果繼續回答                              │
  │    │                                                      │
  ├────┼──────────────────────────────────────────────────────┤
  │ 3  │ 安全性                                                │
  │    │ - working_dir sandbox (只能讀 cwd 底下的檔案)         │
  │    │ - run_command 白名單 or 預設關閉                      │
  │    │ - 檔案大小限制 (防止讀 10GB log)                      │
  │    │                                                      │
  ├────┼──────────────────────────────────────────────────────┤
  │ 4  │ Integration tests                                    │
  │    │ - mock LLM 回傳 tool_call → 驗證執行結果              │
  │    │ - sandbox 邊界測試                                    │
  │    │ - 大檔案被擋                                          │
  │    │                                                      │
  └────┴──────────────────────────────────────────────────────┘
```

---

## Wave 2 宣傳 (Phase 2 完成後)

### 目標

```
  直接跟 Ollama 社群對話
  提 PR 到 Ollama integrations 頁面
```

### 行動

```
  ┌──────────────────┬────────────────────────────────────┬───────┐
  │ 平台             │ 內容                                │ 語言  │
  ├──────────────────┼────────────────────────────────────┼───────┤
  │ Ollama docs PR   │ integrations 頁面加 acp-bridge     │ EN    │
  │ Ollama GitHub    │ Discussion: "native Ollama +       │ EN    │
  │ Discussions      │ MCP agent in one binary"           │       │
  │ r/LocalLLaMA     │ "Ollama + MCP + Discord in 5MB"   │ EN    │
  │ Hacker News      │ "Show HN: Turn Ollama into an     │ EN    │
  │                  │ MCP server with one binary"        │       │
  │ Dev.to           │ 教學文                              │ EN    │
  │ COSCUP CFP      │ 投稿 (如果時間合)                    │ zh-TW │
  └──────────────────┴────────────────────────────────────┴───────┘
```

---

## Phase 3: A2A 遷移 (v0.6.0)

### 為什麼要做

```
  ACP 正在併入 A2A (Google + IBM + Linux Foundation)
  NIST 指定 MCP + A2A 為互操作性基線
  → 不跟上就會被淘汰

  但 Phase 3 不急，因為:
  - A2A spec 還在變
  - ACP 現有用戶 (openab) 還在用
  - 先把 Phase 1 + 2 做好更重要
```

### 要做的事

```
  [ ] 加 HTTP transport (axum)
  [ ] Agent Card endpoint (/.well-known/agent.json)
  [ ] A2A message/send + Task lifecycle
  [ ] 雙協議支援: stdin(ACP) + HTTP(A2A) 同時聽
```

---

## Wave 3 宣傳 (Phase 3 完成後)

```
  [ ] A2A GitHub Discussions: "first local AI A2A implementation"
  [ ] Linux Foundation / AAIF 相關活動
  [ ] 台灣 AI 政策場景: 主權 AI + A2A 標準
```

---

## 版本策略

```
  規則: 每個 Phase 一個 MINOR，hotfix 用 PATCH

  v0.3.0  ✅ 已發佈 (Sprint 1 + 2)
  v0.4.0  Phase 1 — Ollama 原生整合
  v0.5.0  Phase 2 — MCP Server 模式
  v0.6.0  Phase 3 — A2A 遷移
  v1.0.0  全部穩定後
```
