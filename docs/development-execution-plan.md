# ACP Agent 開發進程與階段邊界檢核

日期：2026-11-23。狀態：規劃完成，工程 gate 全部尚未執行。目標是以小批次交付達成 Local Partner Beta：可靠完成多步任務、取消、接續、理解中途修正與有來源的外部搜尋。

2026-11-24 修訂：補入檔案越界、持久化時序／單一寫入者、取消競爭與累積資源四組 P1 必測案例。以下案例均為 pending，文件修訂不代表實作已修復。

本文件是開發順序、承諾範圍與 gate 的唯一入口；與舊計畫不同時以本文件為準。[harness 設計](kiro-inspired-harness-plan.md)保留架構細節，[外部搜尋設計](external-search-design.md)保留工具細節；兩份 review 是審查紀錄，不當作已修復證明。參考 [整體 review](acp-agent-plan-review.md)與 [Kiro review](kiro-cli-plan-review.md)。

本輪承諾 P0–P3 加原生搜尋；P4–P6 保留候選，Beta 通過且有具體需求才排入。品牌與網站是獨立發布線，不能阻擋核心工程；沿用 acp-bridge 命令、套件與既有網址，名稱衝突未確認前不改套件名。

## 執行規則

每階段採 ready → implementing → verifying → passed；無法驗證標 blocked，不以 skipped 當 passed。Gate 綁定 commit、設定 hash、平台與測試版本；修改相關介面或不變式後，受影響 gate 回到 verifying。文件完成不代表 gate 通過。

每個工作項目指定實作者與檢核角色；可由同一人分兩次處理，檢核需從驗收條件反查證據。小 PR 應只改一個主要契約，目標 0.5–2 個開發日內可 review，超過則拆成可獨立驗證的切片；這是工作批次上限目標，不是全案工期估算。同一介面同時只允許一個變更項，獨立 fixture／文件工作可先行準備。

共通邊界必查：輸入與型別、權限與身分、資源上限、取消與超時、失敗與重試、持久化與重播、相容性與回退。不是每個 PR 都跑所有測試：依變更影響選套件，但未受影響的省略要有理由。

硬性阻擋：未授權副作用、跨使用者資料外洩、歷史毀損、重複執行不明副作用、不可取消／無界資源、虛報完成、協定終態錯誤。這些不得以平均成功率或速度改善抵銷。其他缺陷需登錄影響與處理版本，只有不影響當階段承諾者才可延後。

## 進程與依賴

| 階段 | 進入條件 | 交付切片 | 出口 gate | 通過後可宣稱 |
| --- | --- | --- | --- | --- |
| P0 基線與契約 | 現有程式及 review 可讀 | 部署／協定 trace、效能量測、已知錯誤重現、設定及文件修正 | G0 | 具備可重現開發基線 |
| P1 有界執行與最小恢復 | G0 通過 | 全工具取消與限額、policy、busy 等待、串流、goal、完成回合 snapshot | G1a 執行；G1b 恢復 | 單代理可控執行、恢復已完成回合 |
| P2 上下文與證據 | G1a 通過；G1b 可獨立收尾 | 穩定前綴、規則／skills、成組壓縮、SourceRecord | G2 | 長對話受控且證據可追溯 |
| P3 完整恢復與搜尋 | G1b、G2 通過 | journal、去重、steer、跨程序排程、原生搜尋 | G3a 恢復；G3b client；G3c 搜尋 | 可進入 Local Partner Beta 候選 |
| Beta 發布候選 | G0–G3 全 passed | 真實模型 A/B、試用、回退演練、套件審查 | GB | 僅對測過的部署組合標 Beta |

關鍵路徑：G0 → G1a → G2 → G3a/G3b/G3c → GB；G1b 是 G3a 的必要前置。P3 三條切片在共用事件與來源 schema 鎖定後可獨立開發，但整合 gate 不可省略。

## P0 基線與契約

先鎖程式 SHA、binary hash、OpenAB 版本、Strata build／模型 hash／context／端點能力，使用去機密 trace 確認 initialize、prompt、cancel、load、permission 實際行為。不讀取或複製整份含憑證部署文件。Review 的 5 tok/s、32K、permission 自動同意、ACP v2 新語意都視為待在目標部署確認的輸入，不直接寫死。

工作切片：

- E00：固定 ACP schema revision 與 capability 矩陣；驗證標準方法優先，client 不支援時明確降級。確認 OpenAB 的 queue／load／訊息身分資料，避免重建既有 dispatcher。
- E01：重現 review B1/B2/B4/B7，修復 recovery 歷史重複與 tool ID、cwd 損壞、圖片能力不符、設定靜默回退。未知權限設定不能當作正常啟動；合法舊設定有明確遷移策略。
- E02：本機無內容 metrics、冷熱 prefill／decode、cache 命中、token usage、503、長 context 與受限工具輸出能力探測。每組至少記錄一次冷啟與三次暖請求；缺計數器標未知，不用 bytes/4 當真值。
- E03：修公開能力宣稱與 OpenAB 使用者限制範例；核對安裝資訊、實際工具清單及 scope。制定套件 include 清單與文件公開方案，本次規劃不自動 push 或 publish。

G0 邊界：有效／無效／缺值／未知設定；Unix 特殊字元、繁中及 Windows 路徑；image:false；recovery 後完整 assistant/tool 序列；不同 schema 初始化與不支援方法；metrics 無憑證／對話內容。Windows 未有實測環境時，不通過 Windows 支援 gate，可單獨發 Linux 範圍。

證據：baseline 報告、去機密 golden traces、失敗案例與修復後結果、capability 表、三份 ADR（store、permission 信任、endpoint 排程責任）。Store 以實際持久化 volume 的 crash／鎖定實驗選 JSONL+snapshot 或 SQLite，不先鎖技術；HOME 位置也必須確認確實持久化。

效能預算由量測算出：預估時間 = 未快取輸入 token / 冷 prefill 速率 + 快取處理時間 + 輸出 token / decode 速率 + 工具／排隊時間。輸出包含可得的思考 token；量不到就報範圍。context 預算扣掉固定前綴、工具 schema、輸出保留量與安全餘量。撤銷舊 24 輪／30 分鐘及 70%→45% 的預設建議，G0 以測量結果鎖候選值與性能門檻。

G0 不通過：只修基線與 fixture；不可先擴大輪數或宣稱版本相容。外部端點不可用時可完成 mock 準備，但效能 gate 保持 blocked。

## P1 有界執行與最小恢復

依序 E10 結構化 ToolResult／全工具輸出限額與 artifact → E11 async executor／程序樹取消／HTTP 串流大小限制 → E12 policy／身分與等待狀態 → E13 provider streaming／busy 退避／GoalState → E14 完成回合 snapshot 與跨程序 load。

E10 同時涵蓋 session／owner／store 累積配額；E11/E13 涵蓋唯一終態與晚到結果隔離；E12 涵蓋檔案操作的路徑邊界；E14 涵蓋副作用前持久化與單一寫入者。四項均為 P1 出口必要條件，不延後到 P3。G0 先加入可重現的負例與平台能力判定，P1 完成修復及驗證。

提高工具輪數必須在輸出、取消與 policy 通過後。檔案、git、shell、web 都列入 executor 範圍，不能只把 blocking 工作移到背景 thread 就宣稱能取消。排隊等待、模型等待與執行時間分別計帳；server_busy 遵守整體 deadline，可取消，不盲重送可能已產生副作用的工具。

G1a 邊界：

- 工具輸出剛好上限／超過一個單位、巨量 UTF-8、chunked 與壓縮 HTTP；超限停止讀取並保留有界 artifact 指標。
- 取消發生在排隊、prefill、串流半個 JSON、子程序執行、重試等待各處；本機目標 1 秒確認、3 秒清理程序樹，失敗必須回報殘留，不宣稱遠端 GPU 一定已停止。
- partial tool JSON 與範例 JSON 不執行；完整 schema 檢查通過才 dispatch；12 輪 mock 能完成，達限回未完成。
- 模擬 client 自動 allow，不得把它當真人確認。已有有效授權不重問；額外授權須綁 owner、session、操作摘要／hash、期限並一次使用。操作改變使確認失效，確認訊息不能被第三人或網頁冒充。
- 無 client permission UI 時可進 waiting_input 或 deny，不能卡住無終態；不因普通讀檔與既有授權操作新增確認。

G1b 邊界：snapshot 原子寫入、截斷檔／磁碟滿、owner 不符、不同程序 load、重播不重貼 Discord。最小恢復只恢復已完成回合；若發現 unfinished marker，呈現中斷且不自動重做工具。owner namespace、version 與 unfinished marker 從首版就具備，不能等 P3 補資料隔離。若部署不能提供可信 owner，限制於明確隔離的單使用者 store，不宣稱多使用者安全。

### P1 必測案例與判定方式

以下案例使用隔離的臨時 workspace／store、假工具與可控制的同步屏障，不在真實使用者資料上注入破壞。競爭案例必須由測試安排確切交錯順序，不能只靠 sleep 或碰運氣重跑。每個案例分別保存預期、實際、事件順序與結果；任一失敗均阻擋對應 G1a／G1b。

| 案例 ID | 安排與輸入 | 通過條件 |
| --- | --- | --- |
| G1a-FS1 | write/edit 的目標或中間父目錄為指向 workspace 外的 symlink；含 dangling link、外部不存在的多層目錄、絕對路徑與 `..` | 未授權操作在建立目錄、建立檔案或截斷前拒絕；外部 sentinel 檔案內容及目錄樹完全不變，不能只驗錯誤字串 |
| G1a-FS2 | 在驗證與開啟／建立之間，替換父目錄或目標為 symlink；在開啟後再 rename 父目錄；適用平台加 junction/reparse point | 不因先檢查後使用而越界；實作須定義可靠的 handle／目錄解析或隔離策略，單次 canonicalize 不算通過。無法保障的平台限制該能力並明示不支援 |
| G1a-FS3 | read/list/edit/write 及 artifact 的建立、讀取與清理；包括可修改外部 inode 的 hard link 情境 | 各操作遵守相同授權根目錄與明確的 link 政策；拒絕操作無外部檔案變更或內容洩漏。讀取與清理不能繞過寫入限制 |
| G1a-C1 | 在完成結果提交與取消決策之間安排兩種先後順序及同時競爭 | 由單一原子狀態轉移決定 winner；每個已接受 prompt 只產生一個協定終態。完成先提交則保留完成，取消先提交則不再發完成；取消確認不冒充第二個 prompt 終態 |
| G1a-C2 | 取消回合 A 後立即啟動 B，再送達 A 的工具結果、串流 chunk、錯誤與重試回呼 | 事件綁 session、turn、tool invocation 及執行世代；A 的過期事件不能進入 B 的歷史、輸出或狀態，不能再次執行工具。必要的 A 診斷只寫入 A 的隔離紀錄 |
| G1a-C3 | 工具忽略取消、程序清理失敗或遠端結果晚到 | 未確認停止的副作用記為 unknown／殘留；不能宣稱已撤銷已發生寫入。衝突工作不得開始直到舊執行被隔離或確認停止；唯讀、不衝突工作依政策另判 |
| G1a-Q1 | 多次小輸出累積到 session、owner、全 store 的 bytes／檔案數限額；含暫存、cache、log、snapshot 與 journal | 限額採先保留再寫入，並發不能超額；達限拒絕新配置或背壓，不能繼續無界落盤。清理失敗、磁碟滿與 inode 耗盡有明確錯誤，不循環重試 |
| G1a-Q2 | queue 滿額、超大單項、兩個程序同時爭取最後容量、取消排隊項目 | queue 同時限制項數與 bytes，超額回可辨識狀態；不暗中丟已接受工作，取消釋放額度。P1 限額涵蓋其已支援的所有 store 寫入者，不等待 P3 的 GPU 排程 |
| G1a-Q3 | owner A 額滿觸發清理，B 有活躍與過期資料；清理時替換路徑／symlink，重啟後重算配額 | 不刪其他 owner 或活躍引用的資料；清理經可信索引與安全路徑解析。中斷後配額可重建，未完成 marker／恢復所需紀錄不可為騰空間被刪除 |
| G1b-D1 | 在 unfinished marker 寫入前、寫入後但尚未完成耐久性操作、耐久性完成後但工具啟動前強制中止 | 副作用只能在 marker 耐久提交成功後啟動；提交失敗則工具呼叫次數為零。耐久提交後即使工具尚未啟動，恢復仍保守標示中斷，不自動重做 |
| G1b-D2 | 工具啟動後、完成後、snapshot 暫存寫入／替換／耐久提交與 marker 結案各邊界中止 | 只有完整且耐久的完成 snapshot 能結案 marker；恢復看到可驗證完成資料或 unfinished，不能出現副作用已發生卻沒有任一紀錄的空窗。已完成回合不重做工具 |
| G1b-W1 | 兩個程序同時 load 並要求寫同一 session；舊程序暫停，新程序嘗試接管，再恢復舊程序 | 同 session 只有一個有效 writer；舊 writer 不能寫回歷史、提交結果或派發新工具。採持有鎖方案時新程序不得強搶活鎖；採 lease 時須具備實際強制的 fencing，不能只靠到期時間 |
| G1b-W2 | writer crash、鎖取得失敗、儲存裝置錯誤、接管時舊子程序仍活著 | 先處理殘留執行與 unknown 狀態，再允許衝突操作；儲存鎖不等於能撤銷舊程序的外部副作用。無法建立排他性就拒絕可寫恢復，不靜默降級成多 writer |

路徑契約首先約束內建檔案工具與 store 操作；任意 shell／git 的檔案效果不能靠字串過濾提供相同保證。若發布宣稱 workspace 寫入隔離，必須另有經驗證的 OS executor 限制；否則明示 shell 可存取的範圍，不把內建工具測試包裝成整個 agent 的 sandbox。FS2 的可變目錄／hard link 威脅模型需在 G0 ADR 明定，不能以規避測試代替防護。

耐久性契約由 store ADR 指定：檔案模式需涵蓋必要的檔案與父目錄同步、原子替換及恢復順序；資料庫模式需記錄 transaction／同步設定與 volume 保證。程序 kill 測試只證明程序崩潰恢復，不等於斷電耐久性；額外宣稱需相應故障測試。P1 可只保存粗粒度 turn marker，P3 才擴充逐工具 journal，但副作用前 marker 與單一 writer 不能延後。

累積配額在 G0 定出候選值，包含寫入前保留量與終態／恢復紀錄的保留空間。保留空間不足時禁止啟動新副作用；過期清理與配額計帳也必須受同一 owner、路徑與 writer 契約約束。不得只刪歷史以讓 quota 測試通過。

證據：各工具取消 fixture、實際 process tree 清理、記憶體峰值、permission traces、殺掉程序再載入的 transcript。失敗時停用對應新能力，保留原 binary/config 和 store 備份；不可把新權限設定交給會靜默忽略它的舊 binary。

## P2 上下文與證據

E20 固定排序及穩定前綴／完整適用規則 → E21 有界讀取與成組歷史 → E22 壓縮與可見狀態 → E23 SourceRecord／來源 artifact。

固定前綴不放時間戳或變動計數；需變更規則時必須失效重建，不能為 cache 保留過期指示。依適用範圍載入規則，不能為省 token 擅自刪去強制 AGENTS 指示；可選技能按需讀。技術搜尋用英文關鍵詞、在地主題補中文是候選策略，需以題庫確認。

G2 邊界：token 限額前後、未知 tokenizer、三次壓縮、規則變更、tool call/result 不拆散、相互矛盾的新舊限制、壓縮失敗與磁碟滿。每條驗收限制及最新修正都有可檢索證據；無可用空間時停在可恢復狀態，不能循環壓縮或丟掉指示。

SourceRecord 的 URL、段落 ID、hash、抓取時間與 owner 在壓縮後可解析；來源不存在不能捏造引用。ClaimRecord 自動語意對應先選用，不把模型能穩定建立映射當 Beta 前提。

證據：context 快照差異、固定前綴 hash、冷熱耗時對照、每項限制的檢索結果。研究證據 token 上限與所有工具共用 context 帳本，不只限制頁數。4–6K 證據 token 是 review 提議的測試點，是否採用由 G0/G2 決定。

G2 失敗：保留有界讀取與明確預算停止，關閉有問題的自動壓縮；不靠增大 context 掩蓋。長任務能力不標交付。

## P3 完整恢復與搜尋

| 切片 | 實作順序 | 必測邊界 | gate 證據 |
| --- | --- | --- | --- |
| E30 journal／去重 | intent → running → result → terminal；持久化 idempotency | 在 intent 前、執行後、結果寫入前後 crash；重複 key／不同 payload；跨 owner；截斷 journal | G3a：恢復狀態正確、unknown 不重做、無資料互漏；不宣稱 exactly-once 外部效果 |
| E31 steer／OpenAB | 先測標準 prompt/config/notice 能力；只補必要的協商擴充 | 忙碌中三次修正、取消與 load 競爭、重播、重複投遞、未知通知 | G3b：修正於定義的安全點生效、只有一個終態、排隊與 steer 明確區分 |
| E32 endpoint admission | 多程序共用排程責任依 ADR 落地 | 兩個 bridge、worker crash、permit 遺失、排隊取消、503 | G3b：不超過實測 endpoint 容量，無死鎖；若外部 client 繞過排程，明示不能保證全域配額 |
| E33 原生搜尋 | adapter → Rust 端段落篩選 → 預算／快取 → 引用 → Discord 狀態 | 無 key／429／過期快取、摘要矛盾、DNS／redirect、注入、外送過濾、資料刪除 | G3c：題庫引用均有有效來源 ID；矛盾與無證據回未確認；成本有界 |

G3a/G3b 必須回歸 P1 的 G1b-D/W 與 G1a-C 案例；P3 新增的是跨 client、steer、去重與完整 journal 整合，不是首次驗證唯一終態、寫入排他或取消隔離。搜尋 artifacts 同樣納入 G1a-Q 累積配額。

搜尋預設維持 allowlist（空清單不連網），另有 offline 與 opt-in public-web。strict offline 需測 shell/MCP 等所有 egress；沒有 OS 約束時只能稱網頁工具停用。LLM endpoint 與公網工具使用不同權限，避免私有 IP 防護誤擋合法本地推論。

外送檢查由程式執行，覆蓋 query、URL、headers、redirect，含已知 secret 值、配置的內部主機與禁止 IP；編碼／未知 secret 不能靠字串比對完全防護，因此只送必要公共詞、限制資料來源，測試不得宣稱零洩漏保證。fixture 用假密鑰。

Review 的「讀網頁後一律確認寫入」調整為：保留來源 taint、不授予網頁指令權限；既有使用者授權的修碼照常進行，新增或改變範圍的副作用由 policy 阻擋／請求有效確認。任意 shell 無 OS 隔離時無法保證隔絕網頁注入；高風險研究可使用 read-only profile。此取捨避免每次查文件都打斷開發，不能把 prompt 指示當強制安全機制。

供應商以當時官方 API、可用性、條款、成本與 fixtures 定案；不沿用 review 中的服務狀態作永久事實，不預先承諾供應商。搜尋與 fetch 先用錄製回放，真實服務少量 smoke。快取與 artifacts 有期限、owner 隔離，session 刪除有一致清理策略。

G3 不通過：可分別停用搜尋或 steer，保留通過的核心能力，但不能標完整 Beta；不能因搜尋通過而忽略恢復失敗。

## 測試分層與 Beta 發布

| 層 | 觸發 | 內容 | 資源安排 |
| --- | --- | --- | --- |
| L0 PR | 每個變更 | fmt／lint／受影響單元與契約、fixture replay | 不依賴網路或真實模型；目標快速回饋，P0 測 CI 時間再定門檻 |
| L1 階段 | 每個 gate | process／HTTP／磁碟故障注入、ACP client 整合、跨程序測試 | 可用假 server；必需的真實 client trace 另標 |
| L2 候選 smoke | 影響推論／工具行為的候選版 | 真實 Strata 短修碼、工具往返、長前綴；恢復／搜尋按變更加題 | 共用 GPU 串行排程，可取消；環境忙不算模型失敗 |
| L3 RC | 全 gate 通過後 | 固定 12 場景各 3 次，舊／新同條件；另加版本化搜尋題庫 | 離峰執行；只有受影響項需重跑，release artifact 變更須重判範圍 |

沿用主計畫 12 場景與評分規則：總成功次數不下降、已成功關鍵案例無新失敗；不是統計顯著性保證。搜尋題庫至少包含直接 URL、最新版本、矛盾摘要、繁中問英文來源、查無資料、惡意頁面；每項結論由人工或固定 rubric 查證，不能只驗 URL 存在。新舊網路資料以同一回放固定，另記少量 live smoke。

GB 還需：G0 預先鎖定的延遲／資源門檻通過；至少兩個日常工作時段的隔離 thread 試用；無未處理硬性阻擋；新舊設定與資料回退演練；套件檔案清單與公開內容審查；發布聲明僅列已測平台與功能。排定 GPU 窗口或等待外部環境不計為工程已完成。

每次 gate 留一份私有證據紀錄：gate ID、commit/build/config hash、fixture/rubric revision、平台／client／model、命令與 exit code、預期／實際結果、artifact 相對路徑、未解問題、reviewer、pass/fail/blocked、日期。狀態初始全部 pending；每個 pass 都必須能追到證據，不能只填勾選。

## Review 處理對照

| Review 項目 | 決策 | 落點 |
| --- | --- | --- |
| B1/B2/B4/B7；RM1–RM5/RM7/RM9/RM10 | 採納為基線修復與文件校正，需重現 | E01/E03、G0 |
| B3/B6、慢 prefill、server_busy | 採納且先於放寬輪數 | E02/E10/E11/E13、G1a |
| ACP 標準／OpenAB queue、permission | 採納標準優先；部署事實需 trace，不直接認定 review 全適用 | E00/E12/E31 |
| P1 最小 load、JSONL 替代 SQLite | load 提前但 owner/unfinished 保護不能延後；store 由 ADR 定案 | E14/G1b、E30 |
| prefix cache／證據 token | 採納；不得刪強制規則或捏造精確 token | E20–E23/G2 |
| 外送過濾／taint | 採納程式檢查；不採一律逐次確認，按有效授權與副作用範圍處理 | E12/E33/G3c |
| ClaimRecord／搜尋 provider | 前者選用，後者實測定案；SourceRecord 必要 | E23/E33 |
| P4–P6 擴張、Kiro 功能清單 | 延後，必須需求證據與效益評估 | 候選 gate |
| 名稱、網站、RM6/RM8、公開政策 | 保留現有技術名稱；公開摘要與內部規劃分開 | E03、品牌 gate |
| 2026-11-24 檔案越界 review | 採納；補 symlink／路徑替換／link 政策與無副作用證據 | E12、G1a-FS1–FS3 |
| 2026-11-24 最小恢復時序 review | 採納；副作用前耐久 marker、單 writer 與接管隔離提前 | E14、G1b-D1–D2／W1–W2 |
| 2026-11-24 取消競爭 review | 採納；唯一終態與晚到結果隔離屬 P1 | E11/E13、G1a-C1–C3 |
| 2026-11-24 累積資源 review | 採納；分層配額、容量保留、queue 上限與清理隔離 | E10、G1a-Q1–Q3 |

Review 原文署名與使用者稱呼不一致時，按檔案路徑和版本辨識，不重寫審查者署名。採納代表納入計畫，不是問題已修復。

## 候選階段與品牌邊界

P4 進入：GB 通過且有具體工作受阻；逐項選 profiles、MCP、hooks、headless，不整包承諾。出口：workspace trust、工具權限、斷線取消、唯一終態全部有 fixture。

P5 進入：有可重現的單代理瓶頸與排程餘裕；先唯讀子代理。出口：獨立 context、共享檔案邊界、不能提權、排程公平、memory 可刪，無效益即不推出。

P6 進入：有需跨 turn／跨重啟持續的真實流程；出口：節點 crash、wait 不耗模型、有限 repeat、unknown 不重做、監控與取消契約通過。P4–P6 各自重新估工期，不繼承舊 30–48 人日。

品牌 gate 獨立：找到真實網站 source；驗證現行 route／canonical／hreflang；確認名稱可辨識及套件可用性；公開內容與 crate 清單審查。wchung.tw 延伸的實際網址在上線前測試。核心 Beta 不等待改名；本次不發布網站、套件或規劃文件。

## 下一個可執行工作包

先做 E00–E03，順序為「重現與去機密 baseline → schema/ADR → 小修復 → G0」。輸出工程工期區間、GPU 時數與外部依賴等待三份估算，不能把 GPU 等待當開發人日。G0 後只細估 P1，P2/P3 保持區間，完成每個 gate 再更新下一階段；新增範圍必須指出取代哪項或增加多少成本。

## 狀態註記（2026-11-24 晚更新）

本表底部原標「全部 pending」已過時。實際：E01 修復、P1 前半切片與 round-2/3 硬化、以及 P2 前段的 context manager（成組裁剪＋工作記憶摘要，`fix/g1a-round2@86f6a2b`，rc.5）已交付並通過本機測試；G0 因 golden trace/Strata 量測/Windows 環境缺失仍 blocked；G1a/G1b 部分交付但 gate 未驗（缺 C/Q 競態與 D/W fencing 案例）；G2 僅 E21/E22 主體落地；G3a/b/c、GB 未開始。細節以 `acp-agent-evidence/p0-gap-inventory.md` 總覽表為準。
