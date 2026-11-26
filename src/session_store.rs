//! Session snapshot store (P1/E14).
//!
//! A JSON file per session under `LLM_SESSION_DIR`. Enabling it is
//! explicit opt-in — transcripts hold whatever the model saw and said,
//! so nothing is written to disk unless the operator chose a location.
//!
//! With the store disabled (unset env var) `session/load` keeps
//! returning `-32001 no_persistence`, which is the honest answer.
//! With it enabled, a load that finds no snapshot returns `-32002
//! session_not_found`.
//!
//! Per-session files (`S` = sanitized session id):
//! - `S.json`      current committed snapshot body
//! - `S.json.prev` previous committed body — the last-good generation
//! - `S.done`      commit manifest (`sha256=` of the committed body),
//!   atomically swapped via `S.done.tmp` + rename — THE commit point
//! - `S.lock`      permanent flock anchor; never deleted by anyone
//! - `S.turn`      in-progress turn intent marker (R3)
//! - `store.lock`  store-wide quota critical-section anchor (R5)
//! - `artifacts/S/` offloaded tool outputs
//!
//! Ownership & durability protocol: docs/session-store-ownership-adr.md

use fs2::FileExt;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use tracing::{debug, info, warn};

/// Guard holding a session's flock on `{id}.lock` — the kernel releases
/// it when the fd closes (drop, or process death). The anchor file is
/// never unlinked: deleting it would hand the next creator a different
/// inode and split the lock domain. A paused holder keeps the lock —
/// there is no age-based "stale" steal; a dead holder's lock is
/// released by the kernel, so legitimate takeover is instant.
/// `Session` wraps it in `Arc`, so cloned sessions share one owner.
#[derive(Debug)]
pub struct StoreLock {
    _file: std::fs::File,
}

pub struct SessionStore {
    dir: PathBuf,
}

/// A session restored from disk.
pub struct LoadedSession {
    pub working_dir: PathBuf,
    pub messages: Vec<Value>,
    /// A `.turn` marker survived — the last turn never reached durable
    /// completion (crash mid-turn). Side effects from that turn are
    /// unknown: mark, never replay (R3).
    pub interrupted: bool,
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

impl SessionStore {
    /// Store is enabled only when `LLM_SESSION_DIR` is set. A set-but-
    /// unusable directory (unwritable, not a dir) disables the store
    /// rather than crashing startup — persistence is optional, but the
    /// operator gets a loud warning. An EMPTY value counts as unset:
    /// `""` would otherwise resolve to the cwd and spray writes plus
    /// warnings on every turn.
    pub fn from_env() -> Option<Self> {
        let dir = Self::parse_dir(std::env::var("LLM_SESSION_DIR").ok())?;
        if let Err(e) = std::fs::create_dir_all(&dir) {
            warn!(dir = %dir.display(), error = %e, "LLM_SESSION_DIR unusable; session persistence disabled");
            return None;
        }
        Some(Self { dir })
    }

    fn parse_dir(value: Option<String>) -> Option<PathBuf> {
        let dir = value?;
        let dir = dir.trim();
        (!dir.is_empty()).then(|| PathBuf::from(dir))
    }

    /// Direct constructor — `from_env` is the production path; tests and
    /// embedders that already hold a validated dir can build one here.
    /// Creates the dir if missing.
    pub fn new(dir: PathBuf) -> io::Result<Self> {
        std::fs::create_dir_all(&dir)?;
        Ok(Self { dir })
    }

    /// Session ids are UUIDs in practice, but we validate strictly —
    /// ANY character outside `[A-Za-z0-9_-]` rejects the id outright.
    /// Silently stripping characters would let two different hostile
    /// ids collide onto the same file (`a/b` → `ab`, `ab` → `ab`).
    fn safe_name(session_id: &str) -> Option<String> {
        if session_id.is_empty()
            || session_id.len() > 128
            || !session_id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        {
            return None;
        }
        Some(session_id.to_string())
    }

    // ── Store bounds (E10 cumulative quotas) ──────────────────────────
    // These bound the store directory as a whole, not one call: per-call
    // output caps don't stop a long-lived bridge from filling the disk
    // one snapshot at a time. Over-limit saves are skipped with a warn —
    // fail-closed: a truncated snapshot is just a corrupt one.
    /// Largest single snapshot file we'll write.
    const MAX_SNAPSHOT_BYTES: u64 = 2_000_000;
    /// Max files in the store dir (.json/.tmp/.lock/.done combined).
    const MAX_STORE_FILES: usize = 500;
    /// Max total bytes across the store dir.
    const MAX_STORE_BYTES: u64 = 64_000_000;

    fn path(&self, session_id: &str) -> Option<PathBuf> {
        Self::safe_name(session_id).map(|n| self.dir.join(format!("{n}.json")))
    }

    fn prev_path(&self, session_id: &str) -> Option<PathBuf> {
        Self::safe_name(session_id).map(|n| self.dir.join(format!("{n}.json.prev")))
    }

    fn lock_path(&self, session_id: &str) -> Option<PathBuf> {
        Self::safe_name(session_id).map(|n| self.dir.join(format!("{n}.lock")))
    }

    fn done_path(&self, session_id: &str) -> Option<PathBuf> {
        Self::safe_name(session_id).map(|n| self.dir.join(format!("{n}.done")))
    }

    fn turn_path(&self, session_id: &str) -> Option<PathBuf> {
        Self::safe_name(session_id).map(|n| self.dir.join(format!("{n}.turn")))
    }

    fn journal_path(&self, session_id: &str) -> Option<PathBuf> {
        Self::safe_name(session_id).map(|n| self.dir.join(format!("{n}.tools.jsonl")))
    }

    // ── Tool journal (P3) ────────────────────────────────────────────
    // Append-only JSONL: one `start` record before dispatch, one `done`
    // record after. A `start` without a matching `done`/`interrupted`
    // means the process died (or the turn was aborted) mid-tool — the
    // caller surfaces those as unknown side effects rather than
    // guessing whether they ran.
    /// Journal file is compacted when it grows past this many bytes:
    /// the newest records survive, oldest drop off.
    const MAX_JOURNAL_BYTES: u64 = 256_000;
    /// Records kept on compaction.
    const JOURNAL_KEEP: usize = 256;

    /// Append a record with an fsync so "written" means durable —
    /// returns false on any IO failure. A journal that cannot persist
    /// is a correctness signal, not log noise.
    fn journal_append(&self, session_id: &str, record: &serde_json::Value) -> bool {
        let Some(path) = self.journal_path(session_id) else {
            return false;
        };
        use std::io::Write;
        // Compaction goes through a tmp file + rename so a crash
        // mid-rewrite can never leave a truncated journal — the old
        // file stays whole until the swap.
        if let Ok(meta) = std::fs::metadata(&path) {
            if meta.len() > Self::MAX_JOURNAL_BYTES {
                if let Ok(text) = std::fs::read_to_string(&path) {
                    let lines: Vec<&str> = text.lines().collect();
                    let keep_from = lines.len().saturating_sub(Self::JOURNAL_KEEP);
                    let tmp = path.with_extension("jsonl.tmp");
                    let compacted = (|| -> io::Result<()> {
                        let mut f = std::fs::File::create(&tmp)?;
                        for l in &lines[keep_from..] {
                            writeln!(f, "{l}")?;
                        }
                        f.sync_all()?;
                        std::fs::rename(&tmp, &path)
                    })();
                    if let Err(e) = compacted {
                        warn!(session_id, error = %e, "tool journal compaction failed");
                    }
                }
            }
        }
        let mut line = record.to_string();
        line.push('\n');
        let written = (|| -> io::Result<()> {
            let mut f = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)?;
            f.write_all(line.as_bytes())?;
            f.sync_data()
        })();
        match written {
            Ok(()) => true,
            Err(e) => {
                warn!(session_id, error = %e, "tool journal append failed");
                false
            }
        }
    }

    fn journal_records(&self, session_id: &str) -> Vec<serde_json::Value> {
        let Some(path) = self.journal_path(session_id) else {
            return Vec::new();
        };
        let Ok(text) = std::fs::read_to_string(&path) else {
            return Vec::new();
        };
        text.lines()
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect()
    }

    /// Hash of the complete serialized args — the dedup key. Comparing
    /// a truncated args head (240B) would collide two calls that share
    /// a prefix but differ at the tail (e.g. two write_file bodies
    /// opening the same way), replaying the wrong outcome.
    fn args_hash(args: &serde_json::Value) -> String {
        sha256_hex(args.to_string().as_bytes())
    }

    /// Record a tool dispatch BEFORE it executes — with fsync, and the
    /// return value tells the caller whether the record is durable. A
    /// side-effect tool must refuse to run when this returns false:
    /// an unjournaled mutation is exactly what reconciliation cannot
    /// see. `args` stays a bounded display head — the dedup key is the
    /// full-args hash.
    pub fn journal_tool_start(
        &self,
        session_id: &str,
        call_id: &str,
        name: &str,
        args: &serde_json::Value,
    ) -> bool {
        let mut head = args.to_string();
        crate::tools::truncate_utf8(&mut head, 240);
        self.journal_append(
            session_id,
            &serde_json::json!({
                "k": "start", "call_id": call_id, "name": name,
                "args": head, "args_hash": Self::args_hash(args),
                "ts": Self::now_secs()
            }),
        )
    }

    /// Record completion. `result_head` lets a deduplicated replay
    /// return *something* without re-executing side effects.
    pub fn journal_tool_done(&self, session_id: &str, call_id: &str, ok: bool, result_text: &str) {
        let mut head = result_text.to_string();
        crate::tools::truncate_utf8(&mut head, 400);
        self.journal_append(
            session_id,
            &serde_json::json!({
                "k": "done", "call_id": call_id, "ok": ok, "result_head": head,
                "ts": Self::now_secs()
            }),
        );
    }

    /// Mark a call `interrupted` at reconciliation — the start record
    /// stays but is no longer "unclosed", so reconciliation is once-only.
    pub fn journal_mark_interrupted(&self, session_id: &str, call_id: &str) {
        self.journal_append(
            session_id,
            &serde_json::json!({
                "k": "interrupted", "call_id": call_id, "ts": Self::now_secs()
            }),
        );
    }

    /// Write the turn-boundary record at prompt start. Dedup lookups
    /// only consider records AFTER the latest boundary — a new turn
    /// re-issuing an identical call is a fresh decision to execute,
    /// not a redelivery of the previous turn's dispatch. Returns false
    /// when the boundary could not persist: the caller must disable
    /// dedup for that turn, because the lookup window would silently
    /// extend into the previous turn's records and replay stale
    /// outcomes for calls this turn never dispatched.
    pub fn journal_turn_boundary(&self, session_id: &str) -> bool {
        self.journal_append(
            session_id,
            &serde_json::json!({"k": "turn", "ts": Self::now_secs()}),
        )
    }

    /// The completed record for a same-turn redelivery: the latest
    /// `start` for `call_id` must match `name` + the FULL-args hash,
    /// and a `done` must follow it — with the search scoped to records
    /// after the latest `turn` boundary. Turn-scoping is the
    /// distinction between "this dispatch was redelivered" (dedup) and
    /// "a new turn decided to run the same call again" (execute):
    /// models recycle call ids and repeat calls across turns, so an
    /// unscoped match would replay stale outcomes — a re-read of a
    /// changed file must actually re-read.
    pub fn journal_lookup_done(
        &self,
        session_id: &str,
        call_id: &str,
        name: &str,
        args: &serde_json::Value,
    ) -> Option<serde_json::Value> {
        let records = self.journal_records(session_id);
        let since = records
            .iter()
            .rposition(|r| r.get("k").and_then(serde_json::Value::as_str) == Some("turn"))
            .map(|i| i + 1)
            .unwrap_or(0);
        let window = &records[since..];
        let hash = Self::args_hash(args);
        // The LAST start for this id defines what the current call is —
        // if it doesn't match, the id was recycled for another tool.
        let last_start = window.iter().rposition(|r| {
            r.get("k").and_then(serde_json::Value::as_str) == Some("start")
                && r.get("call_id").and_then(serde_json::Value::as_str) == Some(call_id)
        })?;
        let s = &window[last_start];
        let matches = s.get("name").and_then(serde_json::Value::as_str) == Some(name)
            && s.get("args_hash").and_then(serde_json::Value::as_str) == Some(hash.as_str());
        if !matches {
            return None;
        }
        window[last_start + 1..]
            .iter()
            .find(|r| {
                r.get("k").and_then(serde_json::Value::as_str) == Some("done")
                    && r.get("call_id").and_then(serde_json::Value::as_str) == Some(call_id)
            })
            .cloned()
    }

    /// Starts with no matching `done`/`interrupted` — calls that were
    /// in flight when the process or turn died. Pairing is LIFO per
    /// call_id: a `done` closes the nearest preceding unpaired `start`
    /// with the same id, so a recycled id in a later turn can never
    /// retroactively close an older lost call.
    pub fn journal_unclosed(&self, session_id: &str) -> Vec<serde_json::Value> {
        let records = self.journal_records(session_id);
        // call_id → stack of record indices still unpaired.
        let mut pending: std::collections::HashMap<String, Vec<usize>> =
            std::collections::HashMap::new();
        for (i, r) in records.iter().enumerate() {
            let k = r.get("k").and_then(serde_json::Value::as_str);
            let id = r
                .get("call_id")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("")
                .to_string();
            match k {
                Some("start") => pending.entry(id).or_default().push(i),
                Some("done") | Some("interrupted") => {
                    pending.get_mut(&id).and_then(|v| v.pop());
                }
                _ => {}
            }
        }
        pending
            .into_values()
            .flatten()
            .map(|i| records[i].clone())
            .collect()
    }

    fn now_secs() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
    }

    fn store_lock_path(&self) -> PathBuf {
        self.dir.join("store.lock")
    }

    /// Flock `{id}.lock` and hold the fd. `None` means another live
    /// process holds it — flock counts per open-file-description, so a
    /// second fd in THIS process also contends, which is exactly what
    /// keeps `save()` honest for unowned writes.
    fn acquire_lock(&self, session_id: &str) -> Option<StoreLock> {
        let path = self.lock_path(session_id)?;
        let file = match std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
        {
            Ok(f) => f,
            Err(e) => {
                warn!(error = %e, session_id, "session lock open failed");
                return None;
            }
        };
        match file.try_lock_exclusive() {
            Ok(()) => Some(StoreLock { _file: file }),
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                warn!(
                    session_id,
                    "session lock held by another live owner; skipped"
                );
                None
            }
            Err(e) => {
                warn!(error = %e, session_id, "session lock acquisition failed");
                None
            }
        }
    }

    /// Claim execution ownership of a session for this process's
    /// lifetime — the same flock the snapshot commit path uses, held in
    /// `Session::store_lock` for the whole session (ADR §2). `None`
    /// means owned elsewhere: the caller answers `session_busy`
    /// honestly instead of running a second writer (R1).
    pub fn try_claim(&self, session_id: &str) -> Option<StoreLock> {
        self.acquire_lock(session_id)
    }

    /// Store-wide critical section for quota accounting: usage scan,
    /// admission decision and the write itself serialize across
    /// processes (R5) — two writers cannot both pass the check-then-
    /// write window. Try-lock like every other save path: contention
    /// is a skip, not a queue.
    fn quota_section(&self) -> Option<StoreLock> {
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(self.store_lock_path())
            .ok()?;
        match file.try_lock_exclusive() {
            Ok(()) => Some(StoreLock { _file: file }),
            Err(e) => {
                warn!(error = %e, "store quota section busy or failed");
                None
            }
        }
    }

    fn store_usage(&self) -> (usize, u64) {
        // Recursive: artifacts live in a subdir and must count toward
        // the same cumulative quotas — otherwise offloading becomes a
        // quota bypass (G1a-Q1).
        fn walk(dir: &std::path::Path, files: &mut usize, bytes: &mut u64) {
            if let Ok(rd) = std::fs::read_dir(dir) {
                for e in rd.flatten() {
                    let Ok(m) = e.metadata() else { continue };
                    if m.is_dir() {
                        walk(&e.path(), files, bytes);
                    } else {
                        *files += 1;
                        *bytes += m.len();
                    }
                }
            }
        }
        let (mut files, mut bytes) = (0usize, 0u64);
        walk(&self.dir, &mut files, &mut bytes);
        (files, bytes)
    }

    /// Store root — engine needs it to bound artifact re-reads.
    pub fn root(&self) -> &Path {
        &self.dir
    }

    /// Per-session artifact directory (offloaded tool outputs).
    /// Created lazily on first write; `None` for hostile ids.
    pub fn artifact_dir(&self, session_id: &str) -> Option<PathBuf> {
        Self::safe_name(session_id).map(|n| self.dir.join("artifacts").join(n))
    }

    /// Persist an oversized tool output so the model can re-read it
    /// instead of carrying the whole payload in history. Same quota
    /// contract as snapshots: over-limit writes fail closed (caller
    /// falls back to truncation), never a half file — tmp+rename+fsync.
    pub fn write_artifact(
        &self,
        session_id: &str,
        call_id: &str,
        content: &str,
    ) -> Option<PathBuf> {
        let dir = self.artifact_dir(session_id)?;
        if content.len() as u64 > Self::MAX_SNAPSHOT_BYTES {
            warn!(
                session_id,
                bytes = content.len(),
                "artifact exceeds per-file quota; skipped"
            );
            return None;
        }
        // Usage check + write are one serialized section (R5).
        let _quota = self.quota_section()?;
        if let Err(e) = std::fs::create_dir_all(&dir) {
            warn!(session_id, error = %e, "artifact dir creation failed");
            return None;
        }
        let (files, bytes) = self.store_usage();
        if files >= Self::MAX_STORE_FILES || bytes + content.len() as u64 > Self::MAX_STORE_BYTES {
            warn!(
                session_id,
                files, bytes, "artifact write refused: store quota reached"
            );
            return None;
        }
        // call_id is model-controlled text — sanitize like a session id.
        let safe_call = Self::safe_name(call_id).unwrap_or_else(|| "call".to_string());
        let final_path = dir.join(format!("{safe_call}.txt"));
        let tmp = dir.join(format!("{safe_call}.tmp"));
        let result = (|| -> io::Result<()> {
            let mut f = std::fs::File::create(&tmp)?;
            f.write_all(content.as_bytes())?;
            f.sync_all()?;
            std::fs::rename(&tmp, &final_path)
        })();
        match result {
            Ok(()) => Some(final_path),
            Err(e) => {
                warn!(session_id, error = %e, "artifact write failed");
                let _ = std::fs::remove_file(&tmp);
                None
            }
        }
    }

    /// Rewrite the session's snapshot atomically and durably:
    /// flock → quota section → rotate `.prev` → tmp+fsync+rename body →
    /// tmp+fsync+rename manifest → dir fsync (errors propagate). A crash
    /// anywhere before the manifest swap leaves load() serving `.prev`,
    /// i.e. the last verifiable generation is never lost (R4).
    pub fn save(&self, session_id: &str, working_dir: &Path, messages: &[Value]) -> bool {
        let Some(lock) = self.acquire_lock(session_id) else {
            return false;
        };
        self.save_owned(session_id, working_dir, &lock, messages)
    }

    /// Save for a session whose ownership this process already holds
    /// (`Session::store_lock`). Re-acquiring the same flock on a second
    /// fd would false-conflict with ourselves — the token proves the
    /// exclusion instead of re-checking it.
    pub fn save_owned(
        &self,
        session_id: &str,
        working_dir: &Path,
        _owner: &StoreLock,
        messages: &[Value],
    ) -> bool {
        let Some(path) = self.path(session_id) else {
            warn!(session_id, "unsanitizable session id; snapshot skipped");
            return false;
        };

        let doc = json!({
            "format_version": 1,
            "session_id": session_id,
            "working_dir": working_dir.display().to_string(),
            "messages": messages,
            "message_count": messages.len(),
            "writer_pid": std::process::id(),
        });
        let body = doc.to_string();
        let hash = sha256_hex(body.as_bytes());

        // Cumulative quotas — before touching the filesystem.
        if body.len() as u64 > Self::MAX_SNAPSHOT_BYTES {
            warn!(
                session_id,
                bytes = body.len(),
                "snapshot exceeds per-file cap; save skipped"
            );
            return false;
        }
        // Usage scan + admission + write are one serialized section
        // across processes (R5): the rotated `.prev` means a re-save
        // transiently counts both generations — accounted by checking
        // `bytes + body` against the cap before any write.
        let Some(_quota) = self.quota_section() else {
            warn!(session_id, "store quota section busy; save skipped");
            return false;
        };
        let (files, bytes) = self.store_usage();
        if files >= Self::MAX_STORE_FILES {
            warn!(
                session_id,
                files, "store file-count cap reached; save skipped"
            );
            return false;
        }
        if bytes + body.len() as u64 > Self::MAX_STORE_BYTES {
            warn!(session_id, bytes, "store byte cap reached; save skipped");
            return false;
        }

        let Some(safe) = Self::safe_name(session_id) else {
            return false;
        };
        let tmp = self.dir.join(format!("{safe}.json.tmp"));
        let done_tmp = self.dir.join(format!("{safe}.done.tmp"));
        let committed = (|| -> io::Result<()> {
            // Rotate the currently committed body aside BEFORE replacing
            // it: the manifest still binds `.prev` if we die mid-commit.
            if path.exists() {
                if let Some(prev) = self.prev_path(session_id) {
                    let _ = std::fs::remove_file(&prev); // .prev is expendable
                    std::fs::rename(&path, &prev)?;
                }
            }
            // tmp write + fsync BEFORE rename: a renamed-but-unflushed
            // file can still lose contents on a power cut.
            {
                let mut f = std::fs::File::create(&tmp)?;
                f.write_all(body.as_bytes())?;
                f.sync_all()?;
            }
            std::fs::rename(&tmp, &path)?;
            // Commit marker binds THIS body: sha256 of the bytes just
            // renamed. The manifest swap (single atomic rename) is the
            // indivisible commit point.
            if let Some(done) = self.done_path(session_id) {
                let mut d = std::fs::File::create(&done_tmp)?;
                d.write_all(
                    format!(
                        "format_version=2\nsha256={hash}\npid={}\n",
                        std::process::id()
                    )
                    .as_bytes(),
                )?;
                d.sync_all()?;
                std::fs::rename(&done_tmp, &done)?;
            }
            // Flush the directory so the rename + marker survive a
            // crash. A sync failure means durability was NOT achieved —
            // report it, don't swallow it.
            #[cfg(unix)]
            std::fs::File::open(&self.dir).and_then(|d| d.sync_all())?;
            Ok(())
        })();

        match committed {
            Ok(()) => {
                debug!(session_id, "session snapshot saved");
                true
            }
            Err(e) => {
                warn!(error = %e, session_id, "session snapshot save failed");
                false
            }
        }
    }

    /// Durable "turn in progress" intent marker — written BEFORE the
    /// turn's first tool side effect (R3). `false` means the intent
    /// could not be committed: the caller must refuse to dispatch —
    /// otherwise a mid-turn kill is indistinguishable from a clean
    /// pre-turn state.
    pub fn begin_turn(&self, session_id: &str) -> bool {
        let (Some(safe), Some(turn)) = (Self::safe_name(session_id), self.turn_path(session_id))
        else {
            return false;
        };
        let tmp = self.dir.join(format!("{safe}.turn.tmp"));
        let result = (|| -> io::Result<()> {
            let mut f = std::fs::File::create(&tmp)?;
            f.write_all(
                format!(
                    "format_version=1\nstate=in_progress\npid={}\n",
                    std::process::id()
                )
                .as_bytes(),
            )?;
            f.sync_all()?;
            std::fs::rename(&tmp, &turn)?;
            #[cfg(unix)]
            std::fs::File::open(&self.dir).and_then(|d| d.sync_all())?;
            Ok(())
        })();
        match result {
            Ok(()) => true,
            Err(e) => {
                warn!(error = %e, session_id, "turn intent marker could not be committed");
                false
            }
        }
    }

    /// The turn reached durable completion — clear the intent marker.
    /// Callers invoke this only after a successful persist; a failed
    /// save leaves the marker so the next load reports `interrupted`
    /// rather than claiming a clean state it cannot prove.
    pub fn end_turn(&self, session_id: &str) {
        if let Some(turn) = self.turn_path(session_id) {
            if let Err(e) = std::fs::remove_file(&turn) {
                if e.kind() != io::ErrorKind::NotFound {
                    warn!(error = %e, session_id, "turn intent marker removal failed");
                }
            }
        }
    }

    /// G1a-Q3 quota-triggered cleanup: evict the OLDEST stored sessions
    /// that are provably not live — snapshots (.json/.done/.prev), tmp
    /// stragglers, the turn marker and that session's artifact dir —
    /// until the store is back under quota or nothing removable
    /// remains. Liveness is the flock probe, not this process's memory:
    /// a session owned by ANOTHER process keeps its lock, so it can
    /// never be evicted from under a live owner (R2). The `.lock`
    /// anchor itself is never deleted (inode identity is the lock).
    /// Returns evicted session ids.
    pub fn cleanup_stale(&self, active: &std::collections::HashSet<&str>) -> Vec<String> {
        let mut evicted = Vec::new();
        // Oldest-first candidates: every session id with a .json file.
        let mut candidates: Vec<(std::time::SystemTime, String)> = Vec::new();
        if let Ok(rd) = std::fs::read_dir(&self.dir) {
            for e in rd.flatten() {
                let name = e.file_name().to_string_lossy().to_string();
                let Some(id) = name.strip_suffix(".json") else {
                    continue;
                };
                if id.ends_with(".tmp")
                    || id.ends_with(".prev")
                    || Self::safe_name(id).is_none()
                    || active.contains(id)
                {
                    continue;
                }
                let mtime = e
                    .metadata()
                    .and_then(|m| m.modified())
                    .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
                candidates.push((mtime, id.to_string()));
            }
        }
        candidates.sort();
        for (_, id) in candidates {
            let (files, bytes) = self.store_usage();
            if files < Self::MAX_STORE_FILES && bytes < Self::MAX_STORE_BYTES * 3 / 4 {
                break; // back under quota with headroom
            }
            // Exclusive probe: fails while a live owner (this or any
            // other process) holds the session's flock.
            let Some(_probe) = self.acquire_lock(&id) else {
                continue;
            };
            for suffix in ["json", "done", "json.prev", "json.tmp", "done.tmp", "turn"] {
                let _ = std::fs::remove_file(self.dir.join(format!("{id}.{suffix}")));
            }
            if let Some(art) = self.artifact_dir(&id) {
                let _ = std::fs::remove_dir_all(art);
            }
            evicted.push(id);
        }
        if !evicted.is_empty() {
            info!(
                count = evicted.len(),
                "evicted stale sessions to relieve store quota"
            );
        }
        evicted
    }

    /// Load a snapshot. Returns `Ok(None)` when none exists — that is
    /// "session not found", distinct from a corrupt/uncommitted file
    /// which is an error the caller should surface.
    pub fn load(&self, session_id: &str) -> io::Result<Option<LoadedSession>> {
        let Some(path) = self.path(session_id) else {
            return Ok(None);
        };
        if !path.exists() {
            return Ok(None);
        }
        // The manifest binds one specific body by hash. Missing or
        // pre-v2 markers cannot prove which bytes were committed —
        // both answer corrupt, never a guess.
        let done = self.done_path(session_id);
        let manifest = match done.as_ref().map(std::fs::read_to_string) {
            Some(Ok(m)) => m,
            Some(Err(e)) if e.kind() == io::ErrorKind::NotFound => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "snapshot lacks commit marker (uncommitted or interrupted save)",
                ));
            }
            Some(Err(e)) => return Err(e),
            None => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "unsanitizable session id",
                ));
            }
        };
        let expected = manifest
            .lines()
            .find_map(|l| l.strip_prefix("sha256="))
            .map(str::trim)
            .map(str::to_string);
        let Some(expected) = expected else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "commit marker has no bound body hash (pre-v2 manifest)",
            ));
        };

        // Verify the named generation — the live `.json` first, then
        // the rotated `.prev`: a save killed between body rename and
        // manifest swap leaves the manifest binding the PREVIOUS body,
        // which is still a verifiable committed state (R4).
        let mut body: Option<Vec<u8>> = None;
        for candidate in [path.clone(), self.prev_path(session_id).unwrap_or_default()] {
            let Ok(bytes) = std::fs::read(&candidate) else {
                continue;
            };
            if sha256_hex(&bytes) == expected {
                body = Some(bytes);
                break;
            }
        }
        let Some(body) = body else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "no snapshot body matches the commit manifest",
            ));
        };
        let text = String::from_utf8_lossy(&body).into_owned();
        let doc: Value = serde_json::from_str(&text).map_err(|e| {
            io::Error::new(io::ErrorKind::InvalidData, format!("corrupt snapshot: {e}"))
        })?;

        // Schema validation — explicit, not field-by-field defaults.
        if doc["format_version"].as_u64() != Some(1) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "snapshot format_version missing or unsupported",
            ));
        }
        if doc["session_id"].as_str() != Some(session_id) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "snapshot session_id does not match the requested id",
            ));
        }
        let working_dir = doc["working_dir"]
            .as_str()
            .map(PathBuf::from)
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "snapshot missing working_dir")
            })?;
        let messages = doc["messages"].as_array().cloned().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "snapshot messages missing or not an array",
            )
        })?;
        if messages.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "snapshot has no messages",
            ));
        }
        let recorded = doc["message_count"].as_u64().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "snapshot missing message_count")
        })?;
        if recorded as usize != messages.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "snapshot message_count {recorded} != actual {}",
                    messages.len()
                ),
            ));
        }
        let interrupted = self
            .turn_path(session_id)
            .map(|t| t.exists())
            .unwrap_or(false);
        Ok(Some(LoadedSession {
            working_dir,
            messages,
            interrupted,
        }))
    }

    /// Remove a session's snapshot + commit marker + turn intent
    /// (session/end, session/close). The `.lock` anchor is left alone
    /// by design — deleting it would split the lock domain for any
    /// process still holding an fd on the old inode.
    pub fn delete(&self, session_id: &str) {
        for suffix in ["done", "json", "json.prev", "turn"] {
            let _ = std::fs::remove_file(self.dir.join(format!(
                "{}.{suffix}",
                match Self::safe_name(session_id) {
                    Some(s) => s,
                    None => return,
                }
            )));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Write a committed snapshot pair (body + v2 manifest) like save()
    /// would — for fixtures that need a valid committed state without
    /// going through the writer.
    fn commit_body(dir: &Path, id: &str, doc: &Value) {
        let body = doc.to_string();
        std::fs::write(dir.join(format!("{id}.json")), &body).unwrap();
        std::fs::write(
            dir.join(format!("{id}.done")),
            format!("format_version=2\nsha256={}\n", sha256_hex(body.as_bytes())),
        )
        .unwrap();
    }

    /// Empty or whitespace LLM_SESSION_DIR counts as unset — otherwise
    /// "" resolves to cwd and sprays snapshot writes + warnings.
    #[test]
    fn empty_dir_value_disables_store() {
        assert!(SessionStore::parse_dir(None).is_none());
        assert!(SessionStore::parse_dir(Some(String::new())).is_none());
        assert!(SessionStore::parse_dir(Some("   ".into())).is_none());
        assert!(SessionStore::parse_dir(Some("/tmp/x".into())).is_some());
    }

    #[test]
    fn roundtrip_save_load_delete() {
        let dir = std::env::temp_dir().join(format!(
            "acp-store-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let store = SessionStore { dir: dir.clone() };
        std::fs::create_dir_all(&dir).unwrap();

        let msgs = vec![
            json!({"role": "system", "content": "sys"}),
            json!({"role": "user", "content": "hi"}),
        ];
        store.save("sess-1", Path::new("/tmp/work"), &msgs);

        let loaded = store.load("sess-1").unwrap().expect("snapshot missing");
        assert_eq!(loaded.working_dir, PathBuf::from("/tmp/work"));
        assert_eq!(loaded.messages.len(), 2);
        assert!(!loaded.interrupted);

        store.delete("sess-1");
        assert!(store.load("sess-1").unwrap().is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn hostile_ids_cannot_escape_dir() {
        let dir = std::env::temp_dir().join("acp-store-traversal");
        let store = SessionStore { dir: dir.clone() };
        assert!(store.load("../../etc/passwd").unwrap().is_none());
        // save with a hostile id is REJECTED entirely — no sanitized
        // fallback name that could collide with a legitimate id.
        store.save("../../outside", Path::new("/tmp"), &[json!({"role":"x"})]);
        assert!(!dir.join("../../outside.json").exists());
        assert!(std::fs::read_dir(&dir)
            .map(|mut d| d.next().is_none())
            .unwrap_or(true));
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn fresh_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "acp-store-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A `.json` with no `.done` marker is an uncommitted leftover —
    /// load must call it corrupt, not a session.
    #[test]
    fn uncommitted_snapshot_loads_as_corrupt() {
        let dir = fresh_dir("uncommitted");
        std::fs::write(
            dir.join("s1.json"),
            json!({"format_version":1,"session_id":"s1","working_dir":"/tmp",
                   "messages":[{"role":"x"}],"message_count":1})
            .to_string(),
        )
        .unwrap();
        let store = SessionStore { dir: dir.clone() };
        assert!(
            store.load("s1").is_err(),
            "uncommitted snapshot must be corrupt"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A held flock blocks a second writer; once released, saves resume.
    /// Dropping the guard must NOT unlink the anchor — the file is the
    /// shared lock point for every later acquirer (R1).
    #[test]
    fn held_lock_blocks_second_writer() {
        let dir = fresh_dir("lock");
        let store = SessionStore { dir: dir.clone() };
        let lock = store.acquire_lock("s2").unwrap();
        store.save("s2", Path::new("/tmp"), &[json!({"role":"x"})]);
        assert!(
            !dir.join("s2.json").exists(),
            "save must skip while lock held"
        );
        drop(lock);
        assert!(
            dir.join("s2.lock").exists(),
            "dropping the guard must keep the lock anchor file"
        );
        store.save("s2", Path::new("/tmp"), &[json!({"role":"x"})]);
        assert!(
            dir.join("s2.json").exists(),
            "save must proceed after release"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A `.lock` left by a dead process holds no lock — flock state
    /// lives in the kernel, not in the file's age. Reuse is immediate:
    /// there is no stale-window in which a paused holder could lose
    /// ownership (R1).
    #[test]
    fn leftover_lock_anchor_reacquires_instantly() {
        let dir = fresh_dir("stale");
        let store = SessionStore { dir: dir.clone() };
        let lock = dir.join("s3.lock");
        std::fs::write(&lock, "999999").unwrap();
        let old = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
        std::fs::File::options()
            .write(true)
            .open(&lock)
            .unwrap()
            .set_modified(old)
            .unwrap();
        store.save("s3", Path::new("/tmp"), &[json!({"role":"x"})]);
        assert!(
            dir.join("s3.json").exists(),
            "an unheld anchor must be acquired regardless of file age"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Strict schema: wrong version / wrong session_id / missing
    /// working_dir are all corrupt, not silently defaulted.
    #[test]
    fn load_rejects_schema_violations() {
        let dir = fresh_dir("schema");
        for doc in [
            json!({"format_version":2,"session_id":"s4","working_dir":"/tmp",
                   "messages":[{"role":"x"}],"message_count":1}),
            json!({"format_version":1,"session_id":"OTHER","working_dir":"/tmp",
                   "messages":[{"role":"x"}],"message_count":1}),
            json!({"format_version":1,"session_id":"s4",
                   "messages":[{"role":"x"}],"message_count":1}),
            json!({"format_version":1,"session_id":"s4","working_dir":"/tmp",
                   "messages":[{"role":"x"}]}),
        ] {
            commit_body(&dir, "s4", &doc);
            let store = SessionStore { dir: dir.clone() };
            assert!(store.load("s4").is_err(), "must reject: {doc}");
        }
        // And a valid one still loads.
        commit_body(
            &dir,
            "s4",
            &json!({"format_version":1,"session_id":"s4","working_dir":"/tmp",
                    "messages":[{"role":"x"}],"message_count":1}),
        );
        let store = SessionStore { dir: dir.clone() };
        assert!(store.load("s4").unwrap().is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Snapshot over the per-file byte cap is skipped, not truncated.
    #[test]
    fn oversized_snapshot_is_skipped() {
        let dir = fresh_dir("oversize");
        let store = SessionStore { dir: dir.clone() };
        let big = vec![json!({"role":"x","content":"a".repeat(3_000_000)})];
        store.save("s5", Path::new("/tmp"), &big);
        assert!(
            !dir.join("s5.json").exists(),
            "oversized snapshot must not be written"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn corrupt_snapshot_count_is_an_error() {
        let dir = fresh_dir("corrupt");
        // A parseable snapshot whose recorded count lies about its
        // contents — the committed manifest is valid, so this tests
        // the COUNT check, not the commit check.
        commit_body(
            &dir,
            "sess-9",
            &json!({
                "format_version": 1,
                "session_id": "sess-9",
                "working_dir": "/tmp",
                "messages": [{"role": "system", "content": "sys"}],
                "message_count": 7,
                "writer_pid": 1,
            }),
        );
        let store = SessionStore { dir: dir.clone() };
        assert!(
            store.load("sess-9").is_err(),
            "count-mismatched snapshot must load as corrupt"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// R4: a save killed between the body rename and the manifest swap
    /// leaves the manifest binding the PREVIOUS generation — load must
    /// recover `.prev` (the last verifiable committed state), never
    /// the unverifiable newest bytes.
    #[test]
    fn stale_manifest_recovers_previous_generation() {
        let dir = fresh_dir("prevgen");
        let store = SessionStore { dir: dir.clone() };
        let v1 = vec![json!({"role":"user","content":"committed v1"})];
        let v2 = vec![json!({"role":"user","content":"uncommitted v2"})];
        store.save("s6", Path::new("/tmp"), &v1);

        // Simulate a second save killed after .json rotation+rename but
        // before the manifest swap: .prev holds v1, .json holds v2,
        // .done still names v1's hash.
        let prev = dir.join("s6.json.prev");
        let json_v2 = json!({"format_version":1,"session_id":"s6","working_dir":"/tmp",
                            "messages":v2,"message_count":1})
        .to_string();
        std::fs::remove_file(&prev).ok();
        std::fs::rename(dir.join("s6.json"), &prev).unwrap();
        std::fs::write(dir.join("s6.json"), &json_v2).unwrap();

        let loaded = store.load("s6").unwrap().expect("must recover");
        assert_eq!(loaded.messages[0]["content"], "committed v1");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// R4: a `.json` body that matches NO manifest hash and has no
    /// valid `.prev` is unrecoverable — explicit corrupt, never served.
    #[test]
    fn tampered_body_is_rejected() {
        let dir = fresh_dir("tamper");
        let store = SessionStore { dir: dir.clone() };
        store.save(
            "s7",
            Path::new("/tmp"),
            &[json!({"role":"user","content":"ok"})],
        );
        std::fs::remove_file(dir.join("s7.json.prev")).ok();
        std::fs::write(dir.join("s7.json"), "{\"tampered\":true}").unwrap();
        assert!(store.load("s7").is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// R3: the turn marker survives a mid-turn crash and marks the next
    /// load as interrupted; clearing it after durable completion
    /// restores a clean state.
    #[test]
    fn turn_marker_marks_interrupted_load() {
        let dir = fresh_dir("turn");
        let store = SessionStore { dir: dir.clone() };
        let msgs = vec![json!({"role":"user","content":"task"})];
        store.save("s8", Path::new("/tmp"), &msgs);

        assert!(store.begin_turn("s8"));
        let loaded = store.load("s8").unwrap().expect("snapshot missing");
        assert!(loaded.interrupted, "pending turn marker must mark load");

        store.end_turn("s8");
        let loaded = store.load("s8").unwrap().expect("snapshot missing");
        assert!(!loaded.interrupted);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// R3: a store that cannot write must make begin_turn's refusal
    /// explicit — the caller then refuses the turn with zero tool runs.
    /// A store dir that is actually a FILE makes every write fail
    /// deterministically.
    #[test]
    fn turn_marker_failure_is_explicit() {
        let file = fresh_dir("turnfail").join("not-a-dir");
        std::fs::write(&file, "x").unwrap();
        let store = SessionStore { dir: file.clone() };
        assert!(!store.begin_turn("s1"));
        let _ = std::fs::remove_dir_all(file.parent().unwrap());
    }

    /// R2: cleanup must never delete a session whose lock a live owner
    /// holds — even when the caller's `active` set (its own memory)
    /// doesn't know about it.
    #[test]
    fn cleanup_skips_sessions_owned_elsewhere() {
        let (store, dir) = temp_store("cleanup-owned");
        let msgs = vec![json!({"role":"user","content":"hi"})];
        for id in ["old-1", "old-2", "other-proc"] {
            store.save(id, Path::new("/tmp"), &msgs);
        }
        // Another "process" owns this session — the caller's active set
        // does NOT know about it.
        let held = store.acquire_lock("other-proc").unwrap();
        store.write_artifact("old-1", "c1", &"x".repeat(1_000_000));
        store.write_artifact("old-2", "c1", &"x".repeat(1_000_000));
        let evicted = store.cleanup_stale(&std::collections::HashSet::new());
        assert!(!evicted.contains(&"other-proc".to_string()));
        assert!(dir.join("other-proc.json").exists());
        drop(held);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// R5: while the store quota section is held (any process), new
    /// writes are skipped rather than racing the check-then-write.
    #[test]
    fn held_quota_section_skips_writes() {
        let (store, dir) = temp_store("quota-lock");
        let quota = store.quota_section().unwrap();
        assert!(!store.save("s9", Path::new("/tmp"), &[json!({"role":"x"})]));
        drop(quota);
        assert!(store.save("s9", Path::new("/tmp"), &[json!({"role":"x"})]));
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn temp_store(tag: &str) -> (SessionStore, PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "acp-art-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        (SessionStore { dir: dir.clone() }, dir)
    }

    #[test]
    fn artifact_writes_under_session_dir_and_counts_toward_quota() {
        let (store, dir) = temp_store("quota");
        let path = store
            .write_artifact("sess-a", "call-1", &"x".repeat(1000))
            .expect("artifact write should succeed");
        assert!(path.starts_with(dir.join("artifacts").join("sess-a")));
        assert_eq!(std::fs::read_to_string(&path).unwrap().len(), 1000);
        // Artifact bytes are inside the recursive usage walk — no quota bypass.
        let (_, bytes) = store.store_usage();
        assert!(bytes >= 1000);
        // Oversized content is refused outright.
        assert!(store
            .write_artifact("sess-a", "big", &"x".repeat(2_000_001))
            .is_none());
        // Hostile call ids sanitize, hostile session ids refuse.
        assert!(store.write_artifact("../escape", "c", "x").is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn tool_journal_roundtrip_unclosed_and_dedup() {
        let (store, dir) = temp_store("journal");
        let args = serde_json::json!({"path": "src/main.rs"});
        // call-1 completes; call-2 starts but never finishes.
        store.journal_tool_start("s1", "call-1", "read_file", &args);
        store.journal_tool_done("s1", "call-1", true, "file contents");
        store.journal_tool_start("s1", "call-2", "bash", &serde_json::json!({"cmd":"ls"}));

        // dedup: all three fields must match — same call_id with a
        // different tool/args is a recycled id, NOT a replay.
        assert!(store
            .journal_lookup_done("s1", "call-1", "read_file", &args)
            .is_some());
        assert!(store
            .journal_lookup_done("s1", "call-1", "bash", &args)
            .is_none());
        assert!(store
            .journal_lookup_done(
                "s1",
                "call-1",
                "read_file",
                &serde_json::json!({"path":"x"})
            )
            .is_none());

        // unclosed = the in-flight call only; done calls don't appear.
        let unclosed = store.journal_unclosed("s1");
        assert_eq!(unclosed.len(), 1);
        assert_eq!(unclosed[0]["call_id"].as_str().unwrap(), "call-2");

        // Recycled call_id: after call-1's done, a `start` for the
        // same id but a DIFFERENT tool is a new call — it must not
        // dedup against call-1's outcome and it must appear unclosed.
        store.journal_tool_start(
            "s1",
            "call-1",
            "write_file",
            &serde_json::json!({"path":"y"}),
        );
        assert!(store
            .journal_lookup_done(
                "s1",
                "call-1",
                "write_file",
                &serde_json::json!({"path":"y"})
            )
            .is_none());
        let unclosed = store.journal_unclosed("s1");
        assert_eq!(unclosed.len(), 2);
        // But a replay of call-1's ORIGINAL call still dedups — the
        // last start for the id belongs to write_file now, so this
        // check is positional: it correctly does NOT match anymore.
        assert!(store
            .journal_lookup_done("s1", "call-1", "read_file", &args)
            .is_none());

        // Reconciliation marks it interrupted → unclosed once-only.
        store.journal_mark_interrupted("s1", "call-2");
        store.journal_mark_interrupted("s1", "call-1");
        assert!(store.journal_unclosed("s1").is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn tool_journal_turn_boundary_resets_dedup_scope() {
        let (store, dir) = temp_store("journal-turn");
        let args = serde_json::json!({"path": "src/main.rs"});
        // Turn 1: call completes.
        store.journal_turn_boundary("s1");
        store.journal_tool_start("s1", "call_0", "read_file", &args);
        store.journal_tool_done("s1", "call_0", true, "old contents");
        // Turn 2: model re-issues the SAME call — a new decision to
        // execute, NOT a redelivery: dedup must not replay turn 1.
        store.journal_turn_boundary("s1");
        assert!(
            store
                .journal_lookup_done("s1", "call_0", "read_file", &args)
                .is_none(),
            "a previous turn's done must not dedup a new turn's call"
        );
        // Within-turn redelivery still dedups: the same dispatch
        // completing twice inside one turn is the replay case.
        store.journal_tool_start("s1", "call_0", "read_file", &args);
        store.journal_tool_done("s1", "call_0", true, "fresh contents");
        assert!(store
            .journal_lookup_done("s1", "call_0", "read_file", &args)
            .is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn tool_journal_args_hash_defeats_prefix_collision() {
        let (store, dir) = temp_store("journal-hash");
        // Two write_file calls whose args share a >240-char prefix but
        // differ at the tail — a truncated-head comparison would
        // wrongly dedup the second, skipping its write.
        let prefix = "x".repeat(400);
        let args_a = serde_json::json!({"path": "f", "content": format!("{prefix}AAA")});
        let args_b = serde_json::json!({"path": "f", "content": format!("{prefix}BBB")});
        store.journal_tool_start("s1", "c", "write_file", &args_a);
        store.journal_tool_done("s1", "c", true, "written");
        // Same call_id+name, different args tail → NOT a replay.
        store.journal_tool_start("s1", "c", "write_file", &args_b);
        assert!(store
            .journal_lookup_done("s1", "c", "write_file", &args_b)
            .is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn tool_journal_unclosed_lifo_pairing() {
        let (store, dir) = temp_store("journal-lifo");
        // call-9's turn-1 start is lost (no done); the recycled id in
        // a later turn must not let its done retroactively close it.
        store.journal_tool_start("s1", "call-9", "bash", &serde_json::json!({"cmd":"a"}));
        store.journal_turn_boundary("s1");
        store.journal_tool_start("s1", "call-9", "bash", &serde_json::json!({"cmd":"b"}));
        store.journal_tool_done("s1", "call-9", true, "b done");
        let unclosed = store.journal_unclosed("s1");
        assert_eq!(unclosed.len(), 1, "lost turn-1 start must stay unclosed");
        assert_eq!(
            unclosed[0]["args"].as_str().unwrap(),
            "{\"cmd\":\"a\"}",
            "the unclosed entry is the FIRST call's start"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cleanup_stale_evicts_inactive_only() {
        let (store, dir) = temp_store("cleanup");
        let msgs = vec![json!({"role":"user","content":"hi"})];
        for id in ["old-1", "old-2", "live"] {
            store.save(id, Path::new("/tmp"), &msgs);
        }
        let mut active = std::collections::HashSet::new();
        active.insert("live");
        store.write_artifact("old-1", "c1", &"x".repeat(1_000_000));
        store.write_artifact("old-2", "c1", &"x".repeat(1_000_000));
        let evicted = store.cleanup_stale(&active);
        // 'live' is never an eviction candidate, quota pressure or not.
        assert!(dir.join("live.json").exists());
        assert!(!evicted.contains(&"live".to_string()));
        for id in &evicted {
            assert_ne!(id, "live");
            assert!(!dir.join(format!("{id}.json")).exists());
            assert!(!dir.join("artifacts").join(id).exists());
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
