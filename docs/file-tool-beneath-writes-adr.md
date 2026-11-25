# ADR: file tool 寫入改 capability-beneath(R6 / G1a-FS2/FS3)

日期：2026-11-25。對應 `review-133e2da-followup.md` R6。狀態：已採納。

## 威脅模型

攻擊者是一個對 workspace 有寫入權的並發行程（另一個 agent、build script、被污染的相依工具——不是 model 本身,tool dispatch 是單執行緒）。它可以在 `execute_write_file`/`execute_edit` 的 canonicalize 檢查與實際 open/rename 之間,rename 掉父目錄、塞入指向 workspace 外的 symlink。舊實作的 check-then-use 用時間序近似「父目錄沒變」,競爭一發生就逃逸;tmp+rename 只保證單一 dentry 語義,不能證明中間每層目錄仍是授權邊界內。

## 決策

所有檔案副作用改走 `cap-std` 的 `Dir` capability:

- `Dir::open_ambient_dir(working_dir)` 在函式入口先開 fd,**釘住 workspace inode**;`create_dir_all`/`write`/`rename(from,&sandbox,to)` 全部在該 fd 下相對解析。
- Linux 上 cap-std 走 `openat2(RESOLVE_BENEATH)`,其他平台走逐元件 openat+O_NOFOLLOW fallback;兩條路徑的契約相同:解析過程中任何一層被換成 symlink、目錄被 rename,結果都落在 capability 內或失敗,**不可能落在授權目錄外**。絕對路徑 symlink 目標在 capability 根內重解釋。
- `canonicalize`/prefix 檢查保留,但降級為「文件化的拒絕契約」(symlinked ancestor → OutOfSandbox、`..` → OutOfSandbox、symlink target 出外 → OutOfSandbox),不再承擔安全不變式;安全由機制提供。
- `edit` 讀取側仍走 canonicalize 路徑(找檔),寫入側 `strip_prefix` 回相對路徑後 beneath tmp+rename——同一條不變式蓋住 read→write 間的換位;rename 也順帶處理 hardlink:wd 內 dentry 連到外部 inode 時,in-place 截斷會改到共享 inode,replace-dentry 則斷開連結、外部 alias 保持原樣。

## 殘留限制(誠實標界)

- **讀取側 TOCTOU 仍在**:`read_file`/`list_dir`/`search_code` 的 canonicalize-then-open 在競爭下可能讀到換位後的檔案——界線是讀到「capability 之外但 process 權限之內」的內容。寫入面已消除,讀取面留後續。
- `working_dir` 本身若在 open 之前被替換,pinned inode 是替換後的——機械保證「不跑出 pinned inode」,不保證 pinned 的是使用者預期的那個目錄。
- `bash`/`git_*` 工具完全在 scope 外(子行程本身就不是 capability 模型)。
- Windows 上 cap-std fallback 的競爭視窗比 Linux openat2 大(逐元件解析),不變式仍成立但非單一 syscall 原子。
