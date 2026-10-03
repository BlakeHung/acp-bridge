# Review acp-bridge 改動

你是 reviewer,要檢視 acp-bridge 兩個範圍,然後輸出一份 review 報告(下面 Output 段定義)。

## 你必須看的範圍

### 範圍 A — 已 commit(必看)
- Commit `cec3c36`,訊息: `release: v0.8.0 + v0.8.1 + v0.8.2 — ACP v1 conformance, real-client e2e, spec notifications, tool surface expansion, structured backend errors`
- 在 `/workspace/acp-bridge` 內

### 範圍 B — Working-tree(必看)
- 未 commit 的 0.9.0 改動(ACP v2 雙制 wire format)
- 在 `/workspace/acp-bridge` 內

## 你必須跑的指令(按順序)

```bash
# 1. 切到工作目錄
cd /workspace/acp-bridge

# 2. 看 commit 範圍 A 的摘要與完整 diff
git show --stat cec3c36
git show cec3c36

# 3. 看範圍 B(working-tree)的完整 diff 與檔案清單
git status
git diff
git diff --stat

# 4. 跑全部測試,確認 baseline 是綠的
cargo test -- --test-threads=1 2>&1 | tail -60
```

如果 `cargo test` 有任何 fail,**那是 blocker** — 你必須在報告裡標出來,不要當成「可能已知問題」。

## 你必須讀的檔案(依優先序)

| 序 | 檔案 | 為什麼要讀 |
|---|---|---|
| 1 | `src/protocol.rs` | `RequestId` + `ProtocolVersion` enum + `Session` struct 是所有 wire shape 的 source of truth |
| 2 | `src/acp.rs` | 所有 wire emit helpers(v1 + v2 兩套 dispatcher + `notify_*_for` 系列 + `notify_state_idle_for`) |
| 3 | `src/engine.rs` | `initialize` 的 v1/v2 切換邏輯 + `session_prompt` 的 notification / response 順序 |
| 4 | `src/main.rs` | `negotiate_protocol_version` + `Arc::make_mut` 用法 + 所有 emit sites 的 `_for` 切換 |
| 5 | `src/llm.rs` | `LlmError` / `LlmErrorKind` / `as_str` / `is_retryable` |
| 6 | `src/tools.rs` | `resolve_sandboxed_path` + `execute_write_file` 的 `..` reject + `web_fetch_allowed_hosts` empty-list 行為 |
| 7 | `tests/clients/protocol_version.rs` | v1/v2 e2e 鎖定的 wire shape assertion |
| 8 | `tests/clients/harness.rs` | spawn + read_message / read_line 的 split 邏輯 |
| 9 | `docs/scope.md` | 公開 scope doc,確認 graceful reject 表格與 code 對得起來 |
| 10 | `CHANGELOG.md` | 0.8.0–0.9.0 的 entry,確認 commit message 跟實際 diff 一致 |

## 你必須做的檢查(逐項,不可跳過)

### 檢查 1 — Wire shape 對齊 ACP v1 schema(範圍 A)

來源:ACP v1 schema <https://github.com/agentclientprotocol/agent-client-protocol/blob/main/schema/v1/schema.json>

- [ ] **1.1** `RequestId`(`src/protocol.rs`)是否真的原樣回吐 numeric + UUID string?寫一段測試碼證明。
- [ ] **1.2** `session/update`(`src/acp.rs`)是否帶 `sessionId`?`tool_call` 是否帶 `toolCallId` / `kind` / `status: "in_progress"`?
- [ ] **1.3** `agent_thought_chunk` 是否帶 typed `content: {type: "text", text: "..."}`?
- [ ] **1.4** `initialize` response 是否帶 `agentInfo` + `agentCapabilities.promptCapabilities`?`image` 是否 opt-in(`LLM_SUPPORTS_IMAGE`)?
- [ ] **1.5** `session/prompt` response 是否帶 `stopReason`(必須是 `end_turn` 或 `max_turn_requests`)?
- [ ] **1.6** 5 個 graceful rejects 是否都有 `data.reason`?列舉 source code 對照表。

### 檢查 2 — Wire shape 對齊 ACP v2 schema(範圍 B)

來源:ACP v2 schema <https://github.com/agentclientprotocol/agent-client-protocol/blob/main/schema/v2/schema.json>

- [ ] **2.1** `initialize` v2 branch(`src/engine.rs`)是否 emit `info` + `capabilities`,<em>不</em>有 `agentInfo` / `agentCapabilities`?`promptCapabilities.image` 應該是 `{}`(object marker)不是 `true`?
- [ ] **2.2** `notify_tool_start_v2`(`src/acp.rs`)是否 emit `sessionUpdate: "tool_call_update"` 而不是 `"tool_call"`?`status: "in_progress"` 是否在 v2 路徑?
- [ ] **2.3** `notify_plan_v2` 是否 emit `{plan_update, plan: {type: "items", planId, entries[]}}`?`planId` 是否 stable 且 unique?
- [ ] **2.4** `notify_state_idle_for` 對 v1 是不是 no-op(`v2_does_not_emit_state_update_for_v1_clients` 這個 test 是否過)?
- [ ] **2.5** `notify_usage_v2` 的 `cost.currency` 是否有 ISO 4217 validation?壞的 currency 怎麼處理(silently drop 還是 error)?
- [ ] **2.6** `negotiate_protocol_version`(`src/main.rs`)對 undefined / 1 / 2 / >2 的行為。undefined 必須 fallback 到 V1(保守預設),>2 必須 warn。

### 檢查 3 — Sandbox 邊界(範圍 A)

- [ ] **3.1** `resolve_sandboxed_path`(`src/tools.rs`)是否真的擋 `..` escape?寫一段 reflection:能否 bypass 它?
- [ ] **3.2** `execute_write_file` 的 `..` reject 是否用 component 比對?能否 path normalization 之後 bypass?
- [ ] **3.3** `web_fetch_allowed_hosts` 空清單是否真的完全阻擋?透過程式碼追蹤確認。
- [ ] **3.4** `web_fetch` 是否只接受 http/https scheme?其他 scheme(`file://`、`javascript:`)怎麼處理?
- [ ] **3.5** `bash` 工具沒有 OS-level 隔離 — 這是已知 trade-off。在 review 報告裡標出來,不要當 issue。

### 檢查 4 — Stable string contract(範圍 A)

- [ ] **4.1** `LlmErrorKind::as_str()`(`src/llm.rs`)9 個值是否穩定?任何改名都會 break Clients — 標出來。
- [ ] **4.2** `data.reason` 5 個值(`no_persistence` / `no_modes` / `no_auth_methods` / `agent_does_not_call_client_fs` / `agent_does_not_call_client_terminal`)是否穩定?
- [ ] **4.3** `kind_for_tool`(8 個值)是否穩定?
- [ ] **4.4** Slash command 名稱(`/read` / `/ls` / `/search` / `/edit` / `/shell`)是否穩定?

### 檢查 5 — Concurrency 與 concurrency 安全(範圍 A + B)

- [ ] **5.1** `AppState.sessions` 是 `Arc<RwLock<HashMap<...>>>`,clone 共享 inner。確認 lock guard 的 lifetime 沒問題。
- [ ] **5.2** `Session::protocol_version` 在 spawn 出來的 task 裡被讀,沒有跨 thread mutation。
- [ ] **5.3** `Notification` enum 透過 `mpsc::unbounded_channel` 從 engine task 流到 main loop。沒有 unbounded risk?
- [ ] **5.4** `Arc::make_mut(&mut state)` 在 `run_acp_loop` 內 mutation `protocol_version`。refcount == 1 時不 clone,>1 才 clone。確認邏輯正確。

### 檢查 6 — Test fidelity(範圍 A + B)

- [ ] **6.1** `tests/clients/zed_style.rs` 的 init payload 是從 `zed-industries/zed/crates/agent_servers/src/acp.rs` 拉的嗎?確認對應的 field 都覆蓋。
- [ ] **6.2** `tests/clients/inspector_style.rs` 是從 `newioapp/acp-inspector/src/main/acp-connection-manager.ts` 拉的嗎?
- [ ] **6.3** `tests/clients/protocol_version.rs` 的 6 個 test 是否對應 schema/v2/schema.json 的 SessionUpdate discriminator 列表?

### 檢查 7 — 文件 vs code 一致性

- [ ] **7.1** `CHANGELOG.md` 0.8.0 / 0.8.1 / 0.8.2 / 0.9.0 條目,對應到 commit 範圍的實際改動。漏掉什麼?
- [ ] **7.2** `docs/scope.md` 表格的 graceful reject 跟 `src/main.rs` 的實際 code 一致?
- [ ] **7.3** `Cargo.toml` 版本 = 0.9.0?

## 你必須輸出的東西(Output)

寫一份 Markdown review 報告,放在 `/workspace/acp-bridge/REVIEW_REPORT.md`,結構:

```markdown
# acp-bridge Review Report

**Date:** <你跑的日期>
**Scope A commit:** cec3c36
**Scope B:** working-tree (0.9.0)
**Test baseline:** <cargo test 跑完的最後一行 test result>

## Summary
一段話(3–5 行)講整體結論,包含:
- scope A 是否符合 v1 spec
- scope B 是否符合 v2 spec
- 是否有 blocker(競賽測試 fail、wire shape 偏差、sandbox bypass)
- 整體信心:high / medium / low

## Checklist 結果

### 檢查 1 — Wire shape v1
- 1.1 RequestId 原樣回吐: <OK / Issue: <說明>>
- 1.2 session/update 帶 sessionId 跟 tool_call fields: ...
- 1.3 agent_thought_chunk typed content: ...
- 1.4 initialize image opt-in: ...
- 1.5 session/prompt stopReason: ...
- 1.6 graceful reject data.reason: ...

### 檢查 2 — Wire shape v2
- 2.1 initialize v2 shape: ...
- 2.2 tool_call_update not tool_call: ...
- 2.3 plan_update with planId: ...
- 2.4 state_update v1 no-op: ...
- 2.5 cost.currency validation: ...
- 2.6 negotiate_protocol_version: ...

### 檢查 3 — Sandbox
- 3.1 resolve_sandboxed_path: ...
- 3.2 write_file .. reject: ...
- 3.3 web_fetch allowlist empty: ...
- 3.4 web_fetch scheme check: ...
- 3.5 bash no sandbox: (known)

### 檢查 4 — Stable string
- 4.1 LlmErrorKind::as_str: ...
- 4.2 data.reason: ...
- 4.3 kind_for_tool: ...
- 4.4 slash command names: ...

### 檢查 5 — Concurrency
- 5.1 sessions lock: ...
- 5.2 Session::protocol_version: ...
- 5.3 mpsc unbounded: ...
- 5.4 Arc::make_mut: ...

### 檢查 6 — Test fidelity
- 6.1 zed_style source: ...
- 6.2 inspector_style source: ...
- 6.3 protocol_version coverage: ...

### 檢查 7 — 文件一致性
- 7.1 CHANGELOG coverage: ...
- 7.2 docs/scope.md: ...
- 7.3 Cargo.toml version: ...

## 額外發現
任何不在 checklist 內但你看到的問題(例如:變數命名、code smell、dead code、unsafe 區塊、競賽條件、未來風險)

## 建議(非阻斷)
任何你覺得「現在不修但未來該考慮」的事 — 包含技術債、潛在 refactor 機會、文件改善、附加 new feature
```

## 你必須遵守的規範

1. **不可跳過任何一個 checklist item**。即使你寫「N/A」也要寫,並解釋為什麼 N/A。
2. **Issue 必須附 source code 引用**(file:line)。
3. **不要用模糊措辭**。「看起來 OK」、「應該沒問題」、「大概正確」都是 bad signal。用「已驗證 / 測試覆蓋 / source line X:Y 證實」。
4. **不要修 code**。你是 reviewer 不是 engineer。發現問題就寫在報告裡,讓 owner 修。
5. **不要 commit / 不要 push / 不要編輯任何檔**(除了產出 REVIEW_REPORT.md)。
6. **如果 `cargo test` 失敗**,不要繞過 — 把它當 blocker 寫進去 Summary。
7. **完成後只回我一句話**:`Review complete. Report at /workspace/acp-bridge/REVIEW_REPORT.md`。

開始執行。