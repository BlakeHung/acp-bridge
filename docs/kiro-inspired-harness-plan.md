# ACP Agent 參考 Kiro CLI 的 harness 開發計畫

> 2026-11-23 進程修訂：階段範圍、依賴、預算定案方式與驗收，以[開發進程與階段邊界檢核](development-execution-plan.md)為準。P0–P3 加原生搜尋為本輪範圍；P4–P6 是候選。本文舊階段安排與固定數值不再作交付承諾。

品牌決議：ACP Agent，展開為 Autonomous Coding Partner，前身為 acp-bridge。網站沿用 wchung.tw，第一階段保留 `/acp-bridge/` 入口；binary、套件及設定保持相容。詳見 [品牌與轉換計畫](acp-agent-brand-and-transition.md)。

研究日期：2026-11-23。對象：Blake 的 Discord → OpenAB → acp-bridge → Strata 工作流程。

本版已依 Kiro CLI 再檢核修訂；[檢核報告](kiro-cli-plan-review.md)記錄缺口、來源、取捨及未驗證事項。這是開發規劃，不是已通過產品驗收的宣告。

建議將 acp-bridge 從輕量工具迴圈演進為可恢復、可中途修正、懂得管理上下文的本機 agent harness。保留 Strata 與既有 ACP 入口，先改善單代理任務完成率，再加入 MCP、hooks 與子代理。參考 Kiro 公開的行為契約，自行設計實作，不以複製其私有 prompt 或內部演算法為目標。

本計畫的原始碼基準是 acp-bridge 0.9.2，commit `d31a256f6e914e1bb62c68d9f1232bed090b27fa`。線上 binary、OpenAB 實際部署版本與 Strata 啟動參數尚未對照；P0 必須完成這項核對。本次只產出計畫，未變更執行程式或部署。

## 研究結論與版本邊界

Kiro 官方把 V3 描述成各種前端共用、獨立執行的 harness，負責對話、工具、上下文、權限及模型呼叫。ACP 是 client 與 harness 的邊界；因此 acp-bridge 可保留相同的分工，不必另外重寫 Discord 前端。[官方架構](https://kiro.dev/docs/how-kiro-works/)

Kiro 的 V1／V2／V3 是 harness 世代，不等於 ACP wire protocol 版本。官方目前仍列出 V2 專屬啟動旗標及 V3 遷移差異；CLI 與 ACP 的 harness 選擇設定也分開。acp-bridge README 所稱的 ACP v1／v2 必須另以鎖定的 schema 和 client 測試驗證，不能依 Kiro V3 名稱推導相容性。[harness 選擇](https://kiro.dev/docs/cli/agent-harness/)、[CLI 命令](https://kiro.dev/docs/cli/reference/cli-commands/)

官方功能文件是可觀察行為的依據，不足以確定 Kiro 內部 prompt、排程策略、摘要演算法或模型訓練方式。本計畫的資料結構、預算、閾值與工期均為 acp-bridge 的設計建議。Kiro IDE、Web、Crew 的專屬能力不直接算作 CLI 功能；例如不把 Crew 的壓縮閾值當成 CLI 預設。

## Kiro 模式與 acp-bridge 的取捨

| 能力 | Kiro 公開模式 | acp-bridge 的開發決策 |
| --- | --- | --- |
| 任務執行 | 組上下文、模型產生動作、檢查工具、執行、回饋後繼續 | 建立明確狀態機與任務預算，移除固定 5 輪作為唯一控制 |
| Steering | 持久規則、AGENTS.md、條件式規則 | 優先實作 workspace 與巢狀 AGENTS.md，記錄來源及適用範圍 |
| Skills | 先載名稱與描述，使用時才載全文 | 沿用 SKILL.md，避免將整個 workspace 技能塞入每個 prompt |
| Compaction | 保存目標、決策、進度與近期訊息的摘要 | token 預算管理＋結構化摘要＋工具訊息完整性 |
| Sessions | 本機自動存檔、列出與恢復 | durable session store，恢復時核對檔案與未決動作 |
| Memory | 跨 session 的可檢索知識，與歷史／規則分開 | 後期 opt-in；先做好 session，避免過早引入向量資料庫 |
| 中途輸入 | Steer 在安全點接收；Queue 在回合後送出 | OpenAB 接收與標記訊息，bridge 在工具邊界消費 |
| 權限 | capability 規則、明確允許／詢問／拒絕 | 可預先授權日常工作，避免每次讀檔測試都打斷使用者 |
| Hooks | prompt、tool、session、compaction 生命週期 | 先採少數可預測事件，限制時間與遞迴 |
| MCP | 工具、資源、prompts 與連線管理 | opt-in 本機 stdio 優先，遠端與 OAuth 後續處理 |
| Subagents | 隔離對話上下文，再回傳結果 | 單 GPU 預設限制模型併發，先做唯讀研究與驗證角色 |
| Rewind | conversation fork 與檔案還原分開 | 先 fork session；檔案還原需獨立設計及明確操作 |
| Specs／Plan | 將需求、設計、執行工作分開 | 做輕量 plan 與驗收條件，避免所有小修改都強制三份文件 |

Steering 的 AGENTS.md、條件載入與 custom agent resources 有各自規則，不應假設自訂 agent 一定自動繼承所有 Steering。[Steering](https://kiro.dev/docs/steering/)

Skills 的核心價值是逐步載入，而不是增加常駐 prompt。[Agent Skills](https://kiro.dev/docs/skills/)

Kiro 的壓縮保留近期訊息及結構化任務資訊；session 自動存檔與跨 session memory 是不同機制。[Compaction](https://kiro.dev/docs/compaction/)、[Sessions](https://kiro.dev/docs/cli/chat/session-management/)、[Memory](https://kiro.dev/docs/memory/)

Steer 與 Queue 的差別是送入當前工作還是等待下一回合，不是每次追問都取消重跑。[Queue steering](https://kiro.dev/docs/cli/chat/queue-steering/)

Kiro permissions 有能力分類及限制優先規則；shell 會解析複合命令。這類規則不等於 OS sandbox。[Permissions](https://kiro.dev/docs/permissions/)

Hooks、MCP、子代理各自擴充執行流程；子代理有獨立上下文，但可共享工作環境，不能將上下文隔離理解為檔案隔離。[Hooks](https://kiro.dev/docs/hooks/types/)、[MCP](https://kiro.dev/docs/mcp/)、[Subagents](https://kiro.dev/docs/custom-agents/subagents/)

Kiro CLI 的 `/rewind` 是對話分支；文件列出的 CLI 檔案 checkpoint 是 V2 experimental，不能宣稱 V3 CLI 已有與 IDE 完全相同的檔案回復能力。[Checkpoints and rewind](https://kiro.dev/docs/checkpoints/)

## 原始碼差距

| 已確認現況 | 證據位置 | 對 Blake 的影響 | 優先級 |
| --- | --- | --- | --- |
| `MAX_TOOL_ROUNDS` 預設（原 5,已調為 50 對齊 openab-agent 的 `OPENAB_AGENT_MAX_TOOL_LOOPS`) | `src/llm.rs`/`src/config.rs` | 5 對探索型模型太緊;50 給足查找/修改/測試/修正循環餘裕 | 已處理 |
| 工具迴圈走非串流 `llm::chat` | `src/engine.rs:593` | 每輪等整段完成，Discord 容易長時間無資訊 | P0 |
| shell 用同步 `Command::output()` | `src/tools.rs:678` | 註解雖寫 timeout，實際這段沒有 timeout；取消不能保證停掉子程序 | P0 |
| 歷史按訊息數切尾段 | `src/protocol.rs:121` | 沒有 token 管理，也可能切斷 tool call/result 組 | P0 |
| load/resume 明確回傳 no_persistence | `src/main.rs:405` | bridge 重啟後無法在本層恢復對話 | P1 |
| 預設 system prompt 只帶 cwd | `src/engine.rs:418` | 未見本層自動載入 AGENTS.md／Skills | P1 |
| session/new 忽略 mcpServers | `src/main.rs:321` | client 傳入 MCP 設定不會成為模型可用工具 | P2 |
| 文字內容可恢復成工具呼叫 | `src/llm.rs:270`、`src/engine.rs:605` | 救回 Strata 格式問題，但需要區分範例與執行要求 | P0 |
| 活躍 session 再 prompt 會拒絕 | `src/main.rs:356` 附近 | 中途追問需 client 排隊，沒有 bridge steer mailbox | P1 |
| 設定格式錯誤會退回 defaults | `src/config.rs:43` 附近 | 新增權限／預算設定後不能仍靜默退回不符預期的配置 | P0 |
| scope 文件明列無 OS shell sandbox | `docs/scope.md` | 不應以 file tool 的路徑檢查宣稱 bash 也被隔離 | P0 文件修正 |

`tests/strata_compat.rs` 用 mock server 驗證兩種工具回覆格式；這是有價值的契約測試，但不等於已通過真實 Strata 長任務驗收。README 的能力描述與 `scope.md`、程式存在落差，應一起整理。

## 目標架構與責任

```text
Discord → OpenAB（身分、thread、投遞、顯示）──┐
Zed / 其他 ACP client ─────────────────────┤ ACP
未來 CLI / headless adapter ───────────────┘
                 | 標準 ACP + 協商過的 optional extensions
acp-bridge
  Session actor ── Event journal / task state
      |
  Context builder ── AGENTS / steering / skills / summaries
      |
  Turn runner ── Provider adapter ── Strata / Ollama / 相容端點
      |
  Tool dispatcher ── Policy / hooks ── Async executor / MCP
      |
  結果與進度事件 → Session actor → ACP client
```

每個 session 只有一個狀態擁有者，模型呼叫與工具執行在 actor 外執行，透過事件回傳；避免持鎖等待 HTTP 或 shell。OpenAB 管理 Discord message ID 到 session ID 的關係；bridge 管理語意對話與工具事實。Strata 管理推論及模型快取，不承擔任務資料庫。

bridge 可新增 `turn/`、`context/`、`store/`、`policy/`、`hooks/`、`mcp/` 模組；保留 `main.rs` 為 transport/router，`llm.rs` 為 provider 層。先抽出界面，再逐項遷移，避免一次重寫。

## 核心設計

### 任務狀態與可中斷執行

區分三種層級：tool round 是一次模型與工具往返；turn 是一次使用者輸入到 terminal response；goal iteration 是一次實作加驗證循環，可跨多個 turn。不能把 Kiro Goal 文件的預設 5 iterations 與目前 bridge 的 5 tool rounds 當成同一限制。[Kiro Goal](https://kiro.dev/docs/cli/chat/goal/)

新增 `GoalState`：objective、acceptance_criteria、iteration、budget、evidence、status。第一版只在明確選擇 goal／交付型任務時使用，不把簡單問答自動升級成無限工作。驗證結果分 pass／fail／inconclusive；完成需每項必要條件有對應證據，沒有可執行檢查的條件可用人工確認或有來源的人工式檢視，不能把無法驗證當通過。先由同代理驗證，獨立 reviewer 是後期可選加強，不是完成驗收的前提。

定義 `idle → preparing → waiting_model → checking_tool → running_tool → verifying → completed`，另有 `waiting_input`、`paused_budget`、`cancelled`、`failed`。狀態改變必須有 session ID、turn ID、遞增事件序號與原因。

以 `TurnBudget` 取代固定輪數：模型輪數、整輪時間、單工具時間、重複失敗上限、可選 token 上限。初始實驗值為 24 輪、30 分鐘整輪、120 秒一般 shell、連續 3 次相同失敗後重新規劃或報告；這些不是 Kiro 預設值，也不適合套到所有影片轉錄／建置工作。長工具可由 task profile 放寬。

達預算時保留完成項目與下一步，以可恢復狀態結束，不把未完成任務標成成功。簡單聊天不必啟動完整 plan；修改程式的工作應記錄驗證證據，不能以「模型說完成」取代測試結果。

恢復承諾分級：P1 僅同一程序內接續與輸出任務摘要；P3 才支援跨程序 durable 恢復。等待使用者或排隊時間與有效工作預算分開記錄，不能因等待核准耗盡模型執行配額。

shell 改為 async subprocess，分離 stdout/stderr 串流、exit code、signal、timeout。Unix 使用 process group，Windows 使用 Job Object 或等效程序樹管理。取消先通知再限時終止，回收子程序；一般取消無法撤銷已寫入的檔案與外部動作，結果須明確記錄。

### Provider 串流與工具格式

定義 `TextDelta`、`ToolCallDelta`、`Usage`、`Finish`、`Error` 等內部事件。累積完整 tool arguments 並通過 schema 檢查後才能執行；不能看到半段 JSON 就動作。串流斷線後保留已顯示文字及未執行工具狀態，不自動重播整輪。

Strata compatibility recovery 變成明確 provider 選項。正常 structured tool_calls 優先；fallback 僅處理受限格式，無法分辨範例時回傳格式修正要求。加入「解釋此 JSON」負例，不能因文章含工具名稱就執行。

推論連線、首字等待、chunk idle、整輪截止分開計時。HTTP 暫時錯誤可退避，工具是否重試則由工具副作用分類決定。capability profile 記錄已驗證的 vision、tool streaming、usage、context 等能力，不能只憑端點名稱假設。

### Context 與規則載入

除了常駐規則與 skills，提供暫時 session attachment 的 add／remove／list，含來源、檔案 hash、token 估算及更新狀態。大 repo 先使用有界搜尋、行範圍讀檔與 artifact 引用，不以「把整個 repo 放進 prompt」代替檢索。全文索引／語意知識庫延後；這與學習式 memory 分開。[Kiro context 分類](https://kiro.dev/docs/cli/chat/context/)

組裝順序：harness 固定規則、有效使用者／workspace 規則、適用專案規則、已啟用 skills、任務摘要、近期完整訊息組、當前輸入。這是模型 context 順序，不讓 repo 檔案覆寫使用者授權或權限政策。

保留 `/workspace/AGENTS.md`；依目標檔案套用祖先目錄的巢狀規則，載入清單包含來源、hash、適用範圍。不得將 CLAUDE.md 的 Claude runtime 限制套到 acp-bridge。runtime 能力來自實際環境探測與 profile，不由角色名稱推測。

skills 先載索引，明確使用者觸發優先；需要時才讀全文及相對資源。提供 `/context`、`/skills` 或等效查詢，讓使用者知道哪些內容真的載入。壓縮後仍保留已啟用技能的識別與重載指標。

token 計量優先使用 provider usage 與適用 tokenizer；不可用時標示估算並留裕量，尤其繁中、程式碼與工具 schema 不能一律 chars/4。預算包含 system、tools、images、history 及預留生成空間。

建議在預算約 70% 啟動壓縮、目標降至約 45%，先用實測調整。摘要固定包含目標、限制、最新修正、完成／未完成工作、檔案、驗證結果及未決工具。所有 tool_call/result 成組保留；大輸出落 artifact，只放摘要及可重新讀取的位置。摘要失敗保留原紀錄並回報，不刪掉唯一證據。

不等待 HTTP context-overflow 才壓縮。若單則輸入已超出容量，先回報超量項目或分段讀取；摘要也需要保留模型輸入／輸出空間。以版本化 ContextSnapshot 切換內容，不更改邏輯 session ID，原始事件仍可追溯。這是本產品決策，不照抄舊 Kiro context 頁面「compaction 建立新 session」的版本相關敘述。

增加 `.acpignore` 與可見的 exclude 設定，讓檔案探索、attachment、索引及 skills 資源載入遵守一致過濾；區分檢索排除與硬性 fs deny，前者不是 shell 隔離。ignored 檔案與未支援附件需明確回覆，不靜默丟棄。只在使用者啟用相容模式時讀 `.kiroignore`，不聲稱兩者完全等價。[Kiroignore](https://kiro.dev/docs/kiroignore/)

### Session store 與恢復語意

建議採 bundled SQLite，保留單一可執行檔、免外部資料庫服務；接受 binary 體積可能增加，重新量測而非維持未驗證的 5MB 宣稱。

最小資料：sessions、turns、messages、tool_runs、events、context_snapshots、artifacts。保存 schema version、workspace canonical path、owner namespace、模型/profile 版本、task state。用交易保存事件與 projection，附序號供重連去重。

工具狀態為 planned／started／succeeded／failed／cancelled／unknown。先記錄 intent 再執行，再記錄結果；崩潰若發生在副作用完成但結果尚未寫入之間，必須標 unknown 並核對外部狀態。不能宣稱一般 shell 或 MCP 能做到 exactly-once，也不能恢復後盲目重送 commit／upload。

恢復時檢查 session owner、工作目錄、檔案 hash／git 狀態及未決程序；同一 session 只能有一個寫入者。close 與 delete 分開；關閉不刪歷史。API 金鑰只以環境參照載入，不保存至 snapshot、匯出檔或 prompt log。工具輸出依 retention 管理；使用者可列出、匯出及刪除歷史。

### Steer 與 OpenAB 投遞

回合 terminal 契約：ACP client 以對應 `session/prompt` 的 response/error 結束 pending request，不能將中途 idle、文字中的「完成」或任何未協商通知當作成功。重連先安裝更新 handler，再 load；replay 帶可去重序號及明確標記，不能重播為新工具執行。斷線不自動重送原 prompt。Kiro V3 遷移文件明確要求區分 prompt response 與 turn-end metadata，故以該文件作參考而非舊總覽範例。[ACP V3 migration](https://kiro.dev/docs/cli/v3/acp-migration/)

同一 session 的輸入分為 follow-up、steer、cancel。steer 在目前工具完成後、下一個工具授權／模型呼叫之前消費；每則帶穩定 message ID，收到與已套用分別回覆。取消是高優先訊號，不排在推論佇列尾端。

先保留標準 ACP prompt/cancel；進階 steer、context、queue 查詢使用協商過的 `_acp_bridge/` 私有命名空間（提案），不冒用 Kiro 的 `_kiro/` 或 `_kiro.dev/`。不支援擴充的 client 退回回合間 queue，不靜默丟訊息。wire shape 必須依 P0 鎖定 schema 定案。

OpenAB 負責使用者與 thread 身分驗證，sender metadata 的文字內容本身不當作可信授權。兩層確認去重責任，避免 Discord 重送與 bridge retry 同時造成重做。

標準 mode/config/permission 方法優先，僅在缺少標準能力時提案私有方法。命令目錄、tool 目錄、MCP inventory 各自分開；沒有已定義 route 的 slash command 不顯示為可用。權限請求期間的補充文字要明確歸入 steer 或 cancel-and-replace；不能當作許可選項。所有新增 extension 都有版本、schema、能力旗標與不支援時的 fallback。

endpoint 排程必須跨 bridge processes 生效：OpenAB 若每個 session 啟動不同程序，程序內 Semaphore 無法限制整台 Strata。P0 確認拓撲；優先由共享 gateway／OpenAB 配額提供全域 admission control，bridge 處理 retry-after、jitter、有界 queue 與取消。若直接多 client 沒有共同 scheduler，明確記錄此限制。邏輯背景工作不等於 GPU 同時推論，parent 不可持有推論 permit 等待 child。

### Policy 與 hooks

採 read／write／shell／web／mcp 等能力，實作 allow／ask／deny；先不複製 Kiro 的模型信任分類器。提供 Blake 的日常開發 profile：已授權 workspace 讀寫與指定測試命令可持續執行，難以回復的操作依既有要求確認。計畫模式應由工具政策限制寫入，不能只靠 prompt。

權限來自可信使用者設定與 session 授權；repo 中的 agent profile 可以縮減工具，不可自行授予更高權限。政策匹配記錄原因，但不記錄 credential 值。沒有 client permission UI 時，ask 轉 waiting_input 或結構化拒絕，不自動當 allow。

shell 複合命令不能只比字首；使用可解析的命令結構或限制到已知 argv。OS sandbox 是獨立可選能力，跨 runtime 不做未驗證保證。

hooks 首批採 SessionStart、PromptSubmit、PreToolUse、PostToolUse、PreCompact、PostCompact、TurnEnd。明確定義 event schema、exit code、stdout 大小、timeout、工作目錄與環境白名單。repo 提供的可執行 hook 需經 workspace trust 配置；不要一讀 repo 就執行。PreToolUse 如改參數，重新驗證與重新評估權限。測試 hook 應於適當批次邊界執行，避免每次寫檔就跑整套測試。

### MCP 與子代理

MCP 不是把 mcpServers 原樣送給 Strata；bridge 要實作 client，連線、發現 schema、轉成模型 tools、呼叫 server、再回傳結果。先做 opt-in stdio，之後加入 HTTP、認證生命週期、resources/prompts。採工具 namespace 與按需探索，避免數百工具常駐 prompt。server 斷線與工具錯誤都保留可判讀分類。

子代理先做 researcher／reviewer 的唯讀 profile。獨立 history，接收精簡任務與必要規則；回傳結論、證據、未確定事項，不把所有 transcript 塞回主代理。限制深度、總預算與同端點模型併發；單張 3090 初始上限建議 1，外部工具可在安全範圍內並行。需要平行寫碼時再加入 worktree／檔案所有權協調。

### Memory 與 checkpoints 的範圍

長期 memory 放在 session 恢復之後。採 repository／user 分域，存來源、日期與適用條件，可檢視與刪除。預設不自動把錯誤假設學成規則；先提供明確保存或候選記憶流程。

conversation fork 可先做。檔案 checkpoint 僅能承諾覆蓋 bridge 追蹤到的寫入；shell、MCP、外部編輯不能假裝可全部撤銷。回復前比對目前 hash，保護使用者後續修改；不使用 git reset --hard 作通用回復。

Tangent 作 P5 的 fork 延伸：保存 parent 與 branch point，旁支獨立對話，回到主線不自動混入旁支全文；可明確合併帶來源的摘要。旁支仍共享 workspace，預設唯讀，不宣稱檔案隔離。[Kiro Tangent](https://kiro.dev/docs/cli/chat/tangent/)

### Specs 與可恢復 workflow

原計畫的「profiles＋hooks」不等於 workflow。Kiro 將 workflow 視為有 steps、handoffs、loops、waits 與恢復點的 runtime 圖。[Kiro Workflows](https://kiro.dev/docs/workflows/)

ACP Agent 的 P6 獨立實作最小 workflow：順序 step、有限 repeat、condition、wait、artifact handoff；parallel／join 在順序版穩定後加入。每個 step 綁定 session、輸入 artifact hash、條件與結果；狀態採 pending／running／waiting／passed／failed／cancelled／unknown。父 turn 結束不等於背景 workflow 完成。停用 recipe 不等於取消已啟動 run。

workflow 在 node boundary 保存，等待外部事件使用 timer／工具輪詢，不為了等待持續消耗模型 token。retries 僅重跑明確可重試節點，副作用未知時先核對。初始提供 investigate、implement-and-verify 兩個 recipe，不自動發訊息或發布。方案審閱可由事前授權涵蓋，避免把 Kiro 每個 UI 提示原樣複製成 Blake 的額外確認負擔。

Specs 在 P4 提供可選的需求／設計／任務 artifact，為需求與任務建立穩定 ID、依賴與驗收映射。新增需求會讓受影響任務回到待驗證，不因舊勾選仍存在就判完成。小修改使用單一輕量 plan，較大功能才展開文件。Kiro 文件區分 Feature、Bugfix 與 Quick Spec，不能將 Specs 只等同「列 TODO」。[Kiro Specs](https://kiro.dev/docs/specs/)

### CLI 入口與地端產品化

P4 增加可腳本化的 headless adapter，P5 才考慮簡單互動 CLI；兩者重用同一 harness，不另寫工具迴圈。提案 `acp-agent run --output-format jsonl` 與 `acp-agent acp`，舊 `acp-bridge` stdio 行為不變。JSONL output 與 ACP JSON-RPC 分開；每個 run 必有唯一 terminal record，exit status 區分成功、失敗、取消、待輸入／預算不足。無互動權限不得無限等 stdin。[Kiro headless](https://kiro.dev/docs/cli/headless/)

P0 加入診斷設計，P4 交付 `doctor`：列出 runtime、build、端點能力、模型名稱、context、shell、workspace 與 MCP 狀態；credential 只顯示是否設定。提供 Strata、Ollama native、通用相容端點三種範例，需測試才列為 supported。模型切換只在安全邊界，重算 token 與 context，保留工具事實，不自動將本機資料送到雲端 fallback。

首次設定區分純本機、LAN、自訂遠端端點；連不上時顯示實際錯誤，不直接歸因 sandbox。可選外部能力都明示來源；offline profile 以測試網路紀錄確認只連授權本機 provider，無隱含 registry 或更新請求。

## 開發階段與交付

下表 P0–P3 人日僅保留初稿估算供追蹤，不代表修訂後範圍的工期；新增需求需依 P0 結果重新估算。估算基準為一位熟悉 Rust 的工程師，含測試與 review；真實 Strata 與 Windows 可用性會影響進度。

| 階段 | 工作與模組 | 依賴 | 估計 | 出場條件 |
| --- | --- | --- | --- | --- |
| P0 基線與契約 | 核對部署 binary；鎖 ACP schema；保存匿名 mock fixtures；修文件能力矩陣；設計事件／tool result 型別 | 無 | 2–3 人日 | 可重現 5 輪、取消、長歷史與 Strata 格式問題；不含機密的 baseline 報告 |
| P1 可持續單代理 | TurnBudget、async executor、串流 tool deltas、取消、重複錯誤控制、基本 policy、結構化輸出 | P0 | 6–9 人日 | 超過 5 輪任務完成；取消程序樹；範例 JSON 不執行；不重播有副作用工具 |
| P2 上下文與規則 | Context builder、AGENTS、skill 索引、token 預算、成組歷史、壓縮與狀態摘要 | P1 | 5–8 人日 | 三次壓縮後保留原始限制與最新修正；不產生孤立 tool result |
| P3 恢復與 Discord | SQLite、session load/list、owner mapping、steer mailbox、OpenAB queue／status、endpoint 排程 | P1、P2 | 6–9 人日 | 重啟恢復、訊息去重、中途修正生效、unknown 工具不盲重試 |
| P4 擴充與使用入口 | profile、spec/plan、hooks、stdio MCP、工具探索、headless、doctor | P3 | 待重新估算 | MCP、hook 與 headless terminal 契約通過 |
| P5 進階功能 | 唯讀子代理、fork、Tangent、可選 memory；另評估有限檔案 checkpoint、遠端 MCP | P4 | 待重新估算 | 單 GPU 無飢餓、子代理不能提權、fork 不改檔、memory 可管理 |
| P6 背景工作編排 | durable workflow、step handoff、wait/repeat、monitor、失敗節點恢復 | P3–P5 | 待技術驗證 | 中斷後不重做已完成節點；parent 與背景 run 狀態可分辨 |

原 P0–P3 的 19–29 人日及全案 30–48 人日是初稿粗估，不再作本版交付承諾。Goal、跨程序排程、headless、workflow 等補入後，應在 P0 以平台、PR 與整合依賴重新估算；不以任意加百分比製造精確感。完整 TUI、跨雲同步、帳號計費、IDE LSP、Powers marketplace 與模型 Auto routing 仍不在首輪範圍。

## 可直接拆成 PR 的 backlog

| PR | 主要內容 | 主要驗收 |
| --- | --- | --- |
| 01 | 基線、schema pin、能力文件與設定嚴格驗證 | 錯誤設定不靜默退回；實際能力與 initialize 一致 |
| 02 | ToolResult／事件模型、exit code、UTF-8 安全截斷 | 中文超長輸出不 panic；錯誤工具不標成功 |
| 03 | async shell、timeout、process tree cancel | Unix／Windows 程序樹清理及 stdout 壓力測試 |
| 04 | TurnBudget、GoalState、重複失敗、驗收證據 | 12 輪 fixture 完成；達限制同程序可接續；fail/inconclusive 不標完成 |
| 05 | provider tool streaming、受限 recovery | 任意 chunk 邊界、斷線、範例 JSON 負例 |
| 06 | 基本 policy 與 ACP permission round trip | 既有授權不重問；拒絕不執行；無 UI 不誤允許 |
| 07 | Context builder、AGENTS、skills | 巢狀規則、相對資源、去重及 runtime 區分 |
| 08 | token 預算、tool grouping、compaction | 超長繁中／程式碼、壓縮失敗與限制保留 |
| 09 | SQLite journal、load/list/close、遷移 | 四種 crash 邊界與 unknown 工具處理 |
| 10 | OpenAB steer／queue／去重／狀態 | 進行中三次修正、不遺失、不重複執行 |
| 11 | profiles、plan、hooks | read-only 模式、hook timeout 與改參數後重驗 |
| 12 | stdio MCP、工具探索、斷線管理 | 發現→呼叫→回饋，disabled server 不啟動 |
| 13 | 子代理、fork、memory | 獨立上下文、單 GPU 排程、權限及資料隔離 |
| 14 | headless／doctor／相容命令 | JSONL 唯一終態、exit status、無互動權限可結束；舊命令不變 |
| 15 | durable workflow 與 artifacts | 節點邊界恢復、wait 不用模型、unknown 不盲重試 |
| 16 | workflow monitor／Tangent 與摘要合併 | parent 保持可互動；旁支不污染主線；取消不當回復 |

02→03、04→05 可逐步整合；09 依事件結構穩定後落地。06 必須在 MCP／可執行 hooks 前完成。14 屬 P4，15 屬 P6；16 的 Tangent 屬 P5、workflow monitor 屬 P6，實作時可再拆 PR。數字是追蹤 ID，不是合併順序。早期 P1 不宣稱跨程序恢復，該承諾要等 09–10 通過。

## 驗收與衡量

固定 Strata build、模型檔 hash、量化、context、sampling、GPU／RAM 與冷暖快取條件，A/B 比較舊 harness 與新 harness。Kiro 可做行為參照，但不同模型的成功率差異不能當作 harness 的因果證據。

先建立 12 個場景，各跑 3 次：多檔 bug 修正、測試失敗再修正、超過 5 輪工作、繁中長對話、多次中途修正、shell 取消、Strata 503、串流斷線、程序崩潰恢復、工具 JSON 範例、跨 thread 隔離、圖片或附件能力不支援時的明確回報。MCP／子代理場景隨階段加入。

硬性契約測試：

- 取消本機測試程序的確認目標 1 秒內、程序樹清理目標 3 秒內；不把遠端推論伺服器停止運算當成 HTTP 斷線必然結果。
- 12 輪 mock 任務能完成；預算耗盡不謊報成功。
- 連續三次 compaction 後，每條原始驗收限制與後續修改都仍可檢索。
- crash 注入在 tool intent 前、啟動後、結果保存前、保存後；不得自動重做狀態不明的副作用。
- 新 session 與不同 owner 的歷史不互漏；同 session 重複投遞只套用一次。
- stream 分段與 UTF-8 邊界不造成工具重複、資料毀損或 panic。
- 不在 stdout JSON-RPC 混入 log；credential 不出現在測試產物與診斷摘要。
- Goal fixture 故意回傳「完成」但測試失敗，必須繼續修正或以未完成終止；必要條件 inconclusive 時不得完成。
- 同 endpoint 的兩個 bridge process 加一個子代理，驗證配額無超額／死鎖；取消排隊項目不等待 GPU。
- replay 必須先於 load 結果完成接收，斷線後不重送 prompt；未知通知不能讓 client 崩潰。
- headless 的 success/error/cancel/waiting_input 各有唯一 terminal record，不能以 EOF 當成功。
- workflow 在三個節點的各邊界中斷；已完成節點不重跑，外部等待不呼叫模型。
- attachment、ignore、跨目錄規則及不支援媒體分別測試；rule reload 不覆蓋使用者後續指示。

真實任務量測：完成率、使用者補救次數、首個可見進度、模型首字延遲、整體完成時間、工具失敗率、prefill token、輸出 token、峰值 RAM／VRAM。三次小樣本只作初篩，不宣稱統計顯著；候選版至少跨數天實際使用後再定預設。成功率不可低於舊版，其他改善幅度在 P0 取得基線後訂定。

修訂後的發布 gate：所有適用硬性契約測試全過；12 個固定真實場景 × 3 次使用同一版本化評分規則，逐項比較舊版與候選版。總成功次數不得下降、已成功關鍵場景不得出現新失敗；任何差異均附 artifact 判讀。P0 先收集 latency baseline 再鎖定性能門檻，候選版測完後不可回頭放寬門檻來宣稱成功。這是小樣本發布門檻，不是統計顯著性的保證。

## 相容性與發布

所有新功能用 feature/config 開關漸進啟用，保留原有 env 名稱並提供遷移警告。建議新增 `[agent]`、`[context]`、`[session]`、`[permissions]`、`[mcp]`；例如 max_tool_rounds、max_turn_seconds、compaction_threshold、persistence 等名稱皆為提案，尚未實作。

先在獨立測試 thread／獨立資料目錄運行新 binary，不覆蓋原部署。每階段保留前一版 binary 與設定；資料庫 migration 採向前新增、升級前備份，舊版不讀不認得的新 schema。回退 binary 不等於回退已執行的 workspace 操作。

上線前需核對 OpenAB 的取消、permission response、message ID、session mapping 與 stdout framing；`openab-pty` 的 PTY buffering 不代表 Discord ACP 路徑已支援 steer，本計畫不以它代替實際整合測試。

## 第一個里程碑

保留目前 Strata，完成 PR 01–08 的核心能力，再以 PR 09–10 達成：Blake 在 Discord 下達多步驟工作，途中補充限制，agent 持續執行且顯示進度；遇到預算、取消或重啟能清楚保存狀態，之後接續，不靠使用者重新交代整個任務。

這是本計畫的首要產品成果。MCP、子代理與跨 session memory 在此基礎上擴充；不以「看起來像 Kiro」作為驗收標準。

## 外部搜尋補充（2026-11-23）

新增 [外部搜尋與查證設計](external-search-design.md)。原生 web_search、來源證據與引用是 P1–P3 的新增範圍，優先於子代理且不依賴 MCP；PR 17–18 的分工、依賴、預算與驗收以該設計為準。現有 web_fetch 的 5 MB 檢查發生在完整讀取後，需改成串流期間硬限制。新增範圍須重估工期，不能沿用先前數字。
