# acp-bridge Review Report

**Date:** 2026-10-03
**Scope A commit:** cec3c36
**Scope B:** working-tree (0.9.0)
**Test baseline:** `cargo test -- --test-threads=1` → 168 tests, **all green** (87 lib + 6 acp + 6 inspector + 31 integration + 4 llm + 5 minimal + 4 notifications + 13 protocol + 6 protocol_version + 6 zed). `cargo fmt --check` clean. `cargo clippy --all-targets -- -D warnings` clean.
**Note on environment:** 本 container 無 cc/toolchain — 用 `/workspace/apt-cache` 的 debs 解出 gcc-14 + binutils 到 `/workspace/.toolchain`,以 `--sysroot` wrapper 完成 build/test。

## Summary

Scope A (0.8.x) 的 v1 wire shape 經逐項對照官方 `schema/v1/schema.json` **符合規格**,測試 baseline 全綠。Scope B (0.9.0) 的 v2 支援**有三處與官方 `schema/v2/schema.json` 不符**:`state_update` 缺 required `state` discriminator(emit 了不存在的 `available` 欄位)、v2 `session/prompt` response 缺 required `messageId`、v2 session 生命週期方法名 (`session/close`/`list`/`resume`/`delete`) 未實作。另發現 scope 外但同檔案的既有問題:Ollama 原生 tool arguments 被靜默丟棄(fix plan P0-C 未修)、`search_code`/`write_file` symlink escape、`web_fetch` redirect 繞過 allowlist、malformed JSON-RPC 行被靜默吞掉。

**Blocker:** 若以「宣稱支援 ACP v2」為門檻,2.4 (state_update shape) 與 v2 PromptResponse `messageId` 是 wire-shape blocker — spec-compliant v2 client 驗證會失敗。信心:v1 = high;v2 = medium-low(有 e2e 測試但 assert 不夠深,沒抓到 shape bug)。

## Checklist 結果

### 檢查 1 — Wire shape v1

- 1.1 RequestId 原樣回吐: **OK** — `RequestId` untagged enum `Number(u64)|String` 原樣 echo (`src/protocol.rs:59-74`,`send_response`/`send_error` 用 `id.as_value()`)。測試覆蓋:`parse_string_uuid_request_id`、`request_id_as_value_round_trips_{number,string}`、`inspector_style_request_id_fuzz_numeric_string_uuid`(含 UUID 實測)。**Issue(邊界):** float/負數 id 反序列化失敗 → 整行被 skip(`src/main.rs:264-270`),client 會等不到回覆;malformed JSON 也是靜默 skip,沒有 `-32700` parse error 回應。
- 1.2 session/update 帶 sessionId 跟 tool_call fields: **OK** — `send_session_update` 統一帶 `sessionId` (`src/acp.rs:62-67`);`tool_call` 帶 `toolCallId`/`title`/`kind`/`status:"in_progress"` (`src/acp.rs:130-141`)。e2e:`test_tool_call_notification_carries_tool_call_id_and_kind`。
- 1.3 agent_thought_chunk typed content: **OK** — `{"type":"text","text":""}` (`src/acp.rs:86-94`),e2e `test_thought_chunk_carries_content_block`。
- 1.4 initialize image opt-in: **OK** — `prompt_supports_image` gate (`src/engine.rs:358-369`),預設 `image:false`,opt-in 才 `true`;`audio`/`embeddedContext` 固定 false。
- 1.5 session/prompt stopReason: **OK** — `end_turn`/`max_turn_requests` (`src/main.rs:588-592`),response 同時保留 legacy `status`/`text`(有註解說明給 OpenAB 用)。v1 StopReason enum 驗證過這兩個值都在規格內。
- 1.6 graceful reject data.reason: **OK** — 五個都實作:`session/load|resume` → `-32001 no_persistence`(main.rs:373-378)、`session/set_mode` → `-32602 no_modes`(387-392)、`auth/login|logout` → `-32601 no_auth_methods`(399-404)、`fs/*` → `-32601 agent_does_not_call_client_fs`(414-419)、`terminal/*` → `-32601 agent_does_not_call_client_terminal`(421-427)。e2e zed_style 覆蓋前三個。

### 檢查 2 — Wire shape v2(對照官方 schema/v2/schema.json)

- 2.1 initialize v2 shape: **OK** — emit `info{name,title,version}` + `capabilities.session.prompt` + `authMethods:[]`,無 `agentInfo`/`agentCapabilities` (`src/engine.rs:342-355`);`image:{}` object marker、disabled 時省略 — 符合 `PromptCapabilities.image`(supply `{}` = supported)。schema required `protocolVersion`+`info` 皆有。測試 `v2_initialize_returns_unified_capabilities_shape`、`initialize_v2_omits_image_when_disabled`。
- 2.2 tool_call_update not tool_call: **OK** — v2 路徑只發 `tool_call_update`,`status:"in_progress"`、`toolCallId`、`kind` 都在 (`src/acp.rs:372-383`);v2 schema 已無 `tool_call` sessionUpdate(只有 permission subject 用同名字串),e2e `v2_tool_call_uses_tool_call_update_not_tool_call` assert 零 `tool_call`。
- 2.3 plan_update with planId: **Code OK / Test gap** — `notify_plan_v2` emit `{sessionUpdate:"plan_update", plan:{type:"items", planId:"plan-<sid>", entries[]}}` (`src/acp.rs:423-445`),對照 `PlanItems` required `[planId, entries]` 符合;`planId` 由 session id derive,session 內 stable。**但** e2e `v2_plan_uses_plan_update_with_plan_id` (tests/clients/protocol_version.rs:115-160) **沒有實際走 plan 路徑** — 只 assert `agent_message_chunk >= 1`,測試名稱誤導,plan_update wire shape 目前無任何測試覆蓋。
- 2.4 state_update v1 no-op: **v1 OK / v2 ISSUE** — v1 no-op 雙重 guard(acp.rs:558-560 + main.rs:626),e2e `v2_does_not_emit_state_update_for_v1_clients` 通過。**但 v2 payload shape 錯:** `notify_state_idle_for` emit `{"sessionUpdate":"state_update","available":false,"stopReason":...}` (src/acp.rs:561-568);官方 `StateUpdate` 是 `anyOf` 以 **`state`** 欄位為 discriminator(`"running"|"idle"|"requires_action"`,required),`IdleStateUpdate` 只加 `_meta`/`stopReason`。正確 shape 應為 `{"sessionUpdate":"state_update","state":"idle","stopReason":"end_turn"}`。`"available"` 不在 schema。e2e `v2_emits_state_update_at_end_of_turn` 只檢 `sessionUpdate`/`stopReason`,沒 assert `state` — 測試 gap 讓此 bug 通過。
- 2.5 cost.currency validation: **OK(行為正確,設計可議)** — `notify_usage_v2` 檢查 `^[A-Z]{3}$`,不合法時 **silently drop cost 欄位**(src/acp.rs:497-507),不 panic、不 error。目前所有 call site `cost` 都傳 `None`,此分支無實際觸發路徑 — 屬於「為未來預留」的防禦碼。
- 2.6 negotiate_protocol_version: **OK** — `None`→V1(conservative)、`≤1`→V1、`=2`→V2、`>2`→V2+warn (`src/main.rs:220-247`);`ProtocolVersion(0)`/負數/float 都落到 V1 分支,行為與官方 SDK 規則一致。e2e `v1_client_gets_legacy_shapes_unchanged`(undefined)+ `v2_does_not_emit_state_update_for_v1_clients`(顯式 1)覆蓋。

**Checklist 外、同屬 v2 wire 的發現:**

- **v2 `session/prompt` response 缺 required `messageId`:** 官方 v2 `PromptResponse` `required:["messageId"]`(turn 完成改由 `state_update` 上報)。acp-bridge 對 v2 client 仍回 `{stopReason,status,text}`(v1 shape,`src/main.rs:635-643`)— schema 驗證必失敗,且 v2 client 等的是 messageId。這是第二個 v2 wire blocker。
- **v2 session 生命週期方法名未實作:** v2 的 baseline 是 `session/new|list|resume|close|prompt|cancel|update`(schema SessionCapabilities doc)。acp-bridge 實作的是 v1 `session/end`;`session/close`/`session/list`/`session/resume`/`session/delete`/`session/set_config_option` 全部落 `MethodNotFound` `-32601`(main.rs:429-432)。`capabilities.session` 宣告 `{}` 在 v2 語意 = 支援 baseline 全部 — 宣告與實作不符。
- **v2 batch 不支援:** schema 有 `AgentBatchCall`/`ClientBatchCall`;批次行 parse `JsonRpcRequest` 失敗 → 靜默 skip。

### 檢查 3 — Sandbox

- 3.1 resolve_sandboxed_path: **核心 OK,但有未使用它的路徑** — `canonicalize` + `starts_with(canonical_wd)` 對已存在路徑能擋 `..` 與 symlink escape (`src/tools.rs:239-265`)。**Issue:`search_code`/`search_dir` 不走此函數** — 用 `path.is_dir()/is_file()` 追蹤 symlink (tools.rs:585-595),`wd/link→/etc` 可讀取 working dir 外檔案內容;且無深度/循環上限,symlink 環 → 無限遞迴 → stack exhaustion。`list_dir_recursive` 用 `file_type()`(不追 symlink)無此問題。
- 3.2 write_file `..` reject: **部分** — `Component::ParentDir` 檢查可擋 `..`(tools.rs:699-706,有測試),但**不做 canonicalize**:`wd/link` 為指到外部的 symlink 時 `write_file("link/x")` 可逃逸;Windows 絕對路徑 `C:\x` 經 `join` 會整個取代 working_dir → 逃逸(本機 Linux 不受影響,屬平台缺口)。
- 3.3 web_fetch allowlist empty: **OK** — 空清單 return error(tools.rs:830-833),test `web_fetch_blocks_when_allowlist_empty`;host suffix 比對用 `.{suffix}` 邊界,`evil-example.com`/`example.com.evil.com` 都不會誤中。
- 3.4 web_fetch scheme check: **OK 但 redirect 是洞** — 只允許 `http|https`(tools.rs:823-828,有測試)。**Issue:reqwest 預設 follow ≤10 redirects 到任意 host**(tools.rs:848-860 未設 `redirect::Policy::none()`),allowlist 只驗第一跳 — 被允許的 host 302 到內網即繞過。另 `resp.bytes()` 先把整個 body 讀進記憶體才檢 5MB cap,OOM 仍可能。
- 3.5 bash no sandbox: **已知 trade-off**(per review prompt 註記)。附帶發現:doc comment 說 "with a timeout" 但 `Command::output()` 沒有 timeout 機制(tools.rs:633,640-672);輸出先全量 buffer 再 truncate 50K,runaway 輸出會吃記憶體。

### 檢查 4 — Stable string

- 4.1 LlmErrorKind::as_str: **OK** — 9 值 `backend_unreachable|rate_limited|server_busy|auth_error|bad_request|not_found|timeout|parse_error|unknown` (`src/llm.rs:429-441`),`llm_error_kind_as_str_is_stable` test 鎖定。
- 4.2 data.reason: **OK** — 5 值 `no_persistence|no_modes|no_auth_methods|agent_does_not_call_client_fs|agent_does_not_call_client_terminal`(main.rs 同上 e2e 覆蓋)。
- 4.3 kind_for_tool: **OK** — 輸出值 `read|edit|execute|search|fetch|other` 全部在 v1 `ToolKind` enum(含 `delete|move|think|switch_mode` 未用屬合法);`kind_for_tool_maps_known_tool_names_to_acp_enums` test 鎖定。
- 4.4 slash command names: **OK** — `/read|/ls|/search|/edit|/shell` (`src/engine.rs:452-482`)。小 nit:`/edit` description 寫 "via write_file tool"(其實有 `edit` tool);`/shell` description 說 "shell tool" 但實際工具名是 `bash` — 文件用語不一致,不影響 wire。

### 檢查 5 — Concurrency

- 5.1 sessions lock: **OK** — `sessions_write`/`sessions_read` 有 poison recovery;`session_prompt` 內所有 lock guard 都在 `.await` 前 drop(engine.rs:512-548,564-575,589-596,605-612,635-642)。
- 5.2 Session::protocol_version: **Issue(設計債)** — 欄位在 `Session::new` 寫入後**從未被讀取**;所有 emit site 用 `state.protocol_version`(main.rs:533-541,611-632)。CHANGELOG 0.9.0 宣稱「emit helpers branch on a stable per-session value」與程式不符 — 目前是 global version,若 re-`initialize` 改 version,既有 session 會跟著換 wire shape(而非保有 session-level 版本)。single-client spawn 模型下無實害。
- 5.3 mpsc unbounded: **OK(低風險)** — `Notification` 每 turn 數量有限(thinking+tool events+chunk);unbounded channel 無 real OOM 路徑。
- 5.4 Arc::make_mut: **邏輯 OK,註解不準** — `run_acp_loop` 內 `state` 若 `session_idle_timeout_secs>0`,idle-eviction task 先 `Arc::clone`(main.rs:135)→ refcount=2 → `make_mut` **會 clone**;clone 共享 sessions map、只影響 loop 自己的 `protocol_version`,行為仍正確。註解「refcount is 1」在有 idle task 時不成立(main.rs:309-313,133-143)。

### 檢查 6 — Test fidelity

- 6.1 zed_style source: **已驗證** — 比對實際 `zed-industries/zed/crates/agent_servers/src/acp.rs::client_capabilities_for_agent`(main HEAD):`fs{readTextFile,writeTextFile}`、`terminal:true`、`auth{terminal}`、`session{config_options{boolean}}`、`elicitation{form,url}`、`_meta{terminal_output,terminal-auth}`、`clientInfo{name:"zed",title=release_channel}`、`protocolVersion:1` — 測試 payload 全部吻合。差異:真實 Zed 的 `compaction`/`notices` 是 beta-flag gated,測試無條件帶入(無害,server 忽略未知欄位)。
- 6.2 inspector_style source: **OK(合理)** — 聲稱取自 `newioapp/acp-inspector`,payload 為 minimal caps(fs+auth),與「debug tool probe capability」定位一致;未逐欄位對上游 diff。
- 6.3 protocol_version coverage: **部分** — 6 test 對應 discriminator,但三個 gap:(a) `state_update` 沒 assert `state` 欄位 → 漏掉 2.4 的 shape bug;(b) `v2_plan_uses_plan_update_with_plan_id` 名不副實,plan_update 無覆蓋;(c) v2 prompt response `messageId` 未驗證 → 漏掉第二個 blocker。

### 檢查 7 — 文件一致性

- 7.1 CHANGELOG coverage: **大致 OK,一處不實** — 0.8.0/0.8.1/0.8.2/0.9.0 entry 與 diff 對得起來;但 0.9.0 寫「Session carries its negotiated ProtocolVersion so that emit helpers can branch on a stable, per-session value」— 程式實際用 `state.protocol_version`,session 欄位是 dead state(見 5.2)。另 0.8.2 寫「159 tests total」與實際 168 不符(含後續加的 6 個 v2 test + 既有)。
- 7.2 docs/scope.md: **多處 stale** — (a) 工具表寫 `read_file|list_dir|write_file|search(regex grep)|shell`,實際是 11 個工具含 `bash|edit|web_fetch|git_*`,且 search 是 plain-text contains 非 regex、`shell` 不存在(是 `bash`);(b) 「Not a v2 protocol agent yet」已被 0.9.0 推翻;(c) 引用 `tests/clients/codex_style.rs` 但檔案是 `minimal_style.rs`;(d) 「initialize advertises protocolVersion: 1」現在是 1 或 2。
- 7.3 Cargo.toml version: **OK** — `version = "0.9.0"`。

## 額外發現

非 checklist 但重要的問題,依嚴重度排序:

1. **Ollama 原生 tool arguments 被靜默丟棄(= fix plan P0-C 仍未修)。** `src/engine.rs:617-619`:`func["arguments"].as_str().unwrap_or("{}")` — Ollama `/api/chat` 回傳的 `arguments` 是 **object** 非 string,`as_str()` 回 `None` → fallback `"{}"` 成功 parse 成空物件 → **所有 Ollama 原生 tool call 都以空參數執行**,`unwrap_or_else` 分支永不觸發。現有測試只用 string arguments fixture(`tests/integration_test.rs:1065,1111`),object path 零覆蓋。
2. **Ollama options 映射錯誤(P0-C)。** `build_body` 對兩種 backend 都送頂層 `temperature`/`max_tokens`(`src/llm.rs:643-649`);Ollama 原生需要 `options.temperature`/`options.num_predict`,頂層欄位被忽略。
3. **Ollama tool result 格式(P0-C)。** `format_tool_result` 一律回 `{"role":"tool","content":...,"tool_call_id":id}`(`src/llm.rs:102-105`);Ollama 原生 tool message 以 `name` 關聯。且 `tool_call_id` 上游 `tc.get("id")` fallback `"unknown"`(engine.rs:620)— Ollama 不產生 call id → 同輪多個 tool call 全叫 `unknown`,toolCallId 去重失效。
4. **Malformed JSON-RPC 靜默吞掉。** `serde_json::from_str` 失敗 → `debug!` + `continue`(main.rs:264-270);無 `-32700` parse error,client 端乾等。fix plan 明確要求「不靜默吞掉有回應義務的錯誤」。`id:null` 被當 notification 處理(可議但可辯護)。
5. **`session_prompt` 非真串流(P1-A 未做)。** 主流程 `llm::chat` 非串流拿全文才發單一 `TextChunk`;`stream_chat` 存在但 engine 未用。`session/cancel` 只記 log、stdin loop 在 prompt 期間被 `await` 卡住無法讀取 — 與 fix plan P1-A 描述一致,屬已知未完成項非回歸。
6. **`trim_history` 可切斷 tool_call/tool_result pair(P1-C)。** 以訊息數切(`src/protocol.rs:121-130`),可在 assistant tool_calls 與其 result 之間截斷 → 送出不合法歷史給 backend。
7. **cwd sanitize 破壞 Windows 路徑(P1-C)。** 字元過濾去掉 `:`/`\`(`src/engine.rs:407-410`),`C:\foo` → `Cfoo`;fix plan 已列為待修。
8. **`main.rs:578-587` 註解段落整段重複貼上** — 564-577 與 578-587 內容相同(legacy status/text 說明 ×2)。
9. **`engine.rs:429-435` 註解與行為相反** — 說「response 先送、notification 後送」,實際 `main.rs:340-345` 是 notifications 先、response 後(CHANGELOG 0.8.2 也記載改為 notifications-first)。註解 stale。
10. **`notify_state_idle_for` 帶不必要的 `#[allow(dead_code)]`**(acp.rs:552)— 已被 main.rs:632 呼叫。
11. **LLM backend 失敗時 `stopReason:"max_turn_requests"`** — v1 enum 無 error reason,附 `error.category`/`retryable` 補救;可接受但語意不準(真正的 max_turn_requests 與 backend failure 同碼)。
12. **`tool_call_update` 的 `title` 塞程式名**(如 `read_file`)— v2 schema `title` 定位是 human-readable、`name` 才是程式名;acp-bridge 未 emit `name` 欄位(optional,可補)。
13. **`send()` 分兩次 write(obj + `\n`)** — 若未來有並行 emit site 會 interleave;目前序列化在單一 drain loop,低風險。
14. **`estimate_tokens` `.max(1)` 下限** — 空歷史也回 1, trivial。
15. **e2e 全部走 LLM-unreachable 路徑** — harness 把 backend 指到 `127.0.0.1:1`,所有 prompt turn 測的都是 error path;success path 靠 unit/integration mock 補。屬測試設計取捨,非 bug。

## 建議(非阻斷)

- **v2 驗收前三修:** (a) `notify_state_idle_for` 加 `"state":"idle"` 拿掉 `available`;(b) v2 `session/prompt` 依 schema 回 `messageId`(或明確文件宣告 v2 僅涵蓋 notification shape,prompt 仍走 v1 語意 — 但要承認不是 full v2);(c) 實作 `session/close`/`list`/`delete`/`resume`/`set_config_option` 或在 `capabilities.session` 宣告較窄範圍。
- **e2e 補三個 assert:** `state_update` 的 `state` 欄位、v2 prompt response 的 `messageId`、實際走 `notify_plan_v2` 的測試(現 test 名不符實)。
- **修 fix plan P0-C:** engine.rs arguments 分支改成 object 直接保留 / string parse;Ollama body 用 `options.*`;tool result 依 backend 選 `tool_call_id` vs `name`;為無 id 的後端 mint 穩定 call id。
- **sandbox:** `search_dir` 改用 `file_type()` 不追 symlink + 加深度/循環上限 + prefix check;`write_file` 對 parent canonicalize;`web_fetch` 設 `redirect::Policy::none()` 並對 stream 讀取加 cap;`execute_bash` 補 timeout(或修正註解)。
- **文件:** 更新 `docs/scope.md` 工具表、v2 段落、codex→minimal 檔名;CHANGELOG 0.9.0 修正「per-session value」敘述;main.rs 重複註解刪一段;engine.rs:429 註解改成 notifications-first。
- **健壯性:** malformed JSON 回 `-32700`(id:null);`session/cancel` 與 in-flight prompt 的真正取消(P1-A 排程);`Session::protocol_version` 要麼用上(emit site 讀 session 的 version)、要麼拿掉。
