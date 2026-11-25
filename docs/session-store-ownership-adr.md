# ADR: Session store 所有權與耐久協定

日期：2026-11-25。對應 `review-133e2da-followup.md` R1–R5(G1a-Q、G1b-W/D)、G1a-FS 不在本 ADR 範圍。狀態：已採納,實作於同分支。

## 背景

舊協定:`.lock` 檔存在即鎖、檔齡逾 300 秒視為 stale 可回收、`.done` 只記 format/pid。問題:檔齡無法區分「死亡」與「暫停」,舊 guard 會刪掉接管者的鎖;清理依本程序記憶體判定活躍;`.done` 不綁定 snapshot 內容;工具副作用前無耐久紀錄;配額 check-then-write 無跨程序互斥。

## 決策

### 1. OS 持有鎖(flock),取代檔案存在+檔齡語義 — R1/R2

- 每 session 一支 `{id}.lock` 永久錨點檔,對其 fd 取 `flock(LOCK_EX|LOCK_NB)`(fs2 `try_lock_exclusive`)。**任何角色都不得刪除 `.lock` 檔**——刪檔換 inode 會造成鎖分裂。
- 持有者死亡 → 核心自動釋放,無 stale 概念;持有者暫停 → 鎖仍在,接管方 try-lock 失敗。**flock 每 open-file-description 計**,同程序不同 fd 仍互斥。
- 採 flock 而非 fencing lease 的理由:單機檔案系統場景下核心代管生死判定,免除 heartbeat/lease 更新與 fencing token 比較;缺點是 advisory(協作式),但所有寫路徑都在本 store 內,可強制。

### 2. 三種權利分別定義(回應 review 要求)

- **Session 執行權**:`session/new`、`session/load` 成功後,guard 存於 `Session.store_lock`,session 存活期間持有。load 時 try-lock 失敗 → `session_busy` 明確拒絕,不靜默雙寫。由此「暫停的 A 恢復後仍是唯一寫者;B 自始無法進入」。
- **Snapshot 提交權**:`save()` 每次仍 try-lock 同檔(保留舊契約:被佔用則跳過不回報),因 flock 互斥,兩程序不能同時提交。
- **外部工具副作用**:由執行權涵蓋;檔案鎖不能撤銷已發生的副作用,因此加上第 3 項的 intent marker。

### 3. 內容綁定的原子 manifest + `.prev` 輪轉 — R4

- `{id}.done` 改為 `format_version=2\nsha256=<body hash>`:標記綁定特定世代內容,不再只證明「某次 save 完成」。
- 寫入順序:輪轉 `S.json → S.json.prev` → `tmp+fsync → rename` 新 `.json` → `tmp+fsync → rename` 新 `.done`(單檔原子交換=提交點)→ 目錄 `sync_all`(**錯誤向上傳播**,不再吞掉)。
- `load()` 驗 manifest 的 sha256:`.json` 不符則試 `.json.prev`(上一個已提交世代)。kill 於 rename 與 manifest 交換之間 → manifest 仍指向 `.prev`,載入最後可驗證版本,**不失去最後一份有效 snapshot**。
- 舊 format_version=1 標記無法證明所綁內容 → 視為 uncommitted,下一次 save 自然修復。

### 4. Turn intent marker — R3

- 副作用前的耐久紀錄:`{id}.turn` 於 prompt turn 開始、工具調度**之前**以 tmp+rename+fsync 寫入;寫失敗回傳錯誤,工具執行零次。
- Turn 完成路徑:persist 成功後才刪 `.turn`。persist 失敗 → marker 殘留 → 下一次 load 標記 `interrupted` 並在恢復的歷史中附加中斷註記(不自動重做)。
- 粒度:P1 採整回合一個 marker;逐工具 journal 留給 P3。

### 5. Store 級配額臨界區 — R5

- `store.lock` 單一全域錨點,usage 掃描+判定+寫入全程持有 `flock`(try-lock,失敗即跳過不阻塞排隊)。把 check-then-write 變成單一臨界區,兩程序不能同時通過配額檢查;死亡即釋放,下一位重掃即可。
- `cleanup_stale`:候選排除 `active` 之外,再對候選的 `.lock` try-lock——拿不到代表它在別處活著,跳過;拿到才刪其 `json/done/prev/tmp/turn/artifacts`(不刪 `.lock`)。

## 新增依賴

`fs2 0.4`(flock 跨平台:Unix flock / Windows LockFileEx)、`sha2 0.10`(manifest 綁定)。皆為小型穩定套件;審計面增量可接受。

## 不涵蓋 / 殘留限制

- flock 是協作式鎖:繞過 store API 直接寫檔的程式不受約束(記錄為 operator 責任)。
- 程序 kill 測試不等於斷電測試;tmp+fsync+rename 與 dir sync 已按斷電情境設計,但未在斷電注入環境驗證。
- `S.lock` 永久存在使已刪 session 留一支空檔,計入 500-file 上限——有界,接受。
- 多機/網路檔案系統(NFS)不保證 flock 語義,記錄為不支援部署。
