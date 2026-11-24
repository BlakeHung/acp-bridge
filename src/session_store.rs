//! Session snapshot store (P1/E14 first slice).
//!
//! A JSON file per session under `LLM_SESSION_DIR`:
//! `{dir}/{session_id}.json` containing the working directory and full
//! message history, rewritten at turn boundaries. Enabling it is
//! explicit opt-in — transcripts hold whatever the model saw and said,
//! so nothing is written to disk unless the operator chose a location.
//!
//! With the store disabled (unset env var) `session/load` keeps
//! returning `-32001 no_persistence`, which is the honest answer.
//! With it enabled, a load that finds no snapshot returns `-32002
//! session_not_found`.
//!
//! This is a flat-file snapshot, not the durable session store the
//! plan's P3 work wants (concurrency, markers, crash recovery). It is
//! deliberately small: one process writes, reads are atomic-ish via
//! write-to-temp + rename.

use serde_json::{json, Value};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use tracing::{debug, info, warn};

/// Guard holding a session's writer lock — the `.lock` file's
/// existence is the lock; dropping removes it. A process crash leaves
/// the file behind, reclaimed by age after `STALE_LOCK_SECS`.
struct StoreLock {
    path: PathBuf,
}

impl Drop for StoreLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

pub struct SessionStore {
    dir: PathBuf,
}

/// A session restored from disk.
pub struct LoadedSession {
    pub working_dir: PathBuf,
    pub messages: Vec<Value>,
}

impl SessionStore {
    /// Store is enabled only when `LLM_SESSION_DIR` is set. A set-but-
    /// unusable directory (unwritable, not a dir) disables the store
    /// rather than crashing startup — persistence is optional, but the
    /// operator gets a loud warning.
    pub fn from_env() -> Option<Self> {
        let dir = std::env::var("LLM_SESSION_DIR").ok()?;
        let dir = PathBuf::from(dir);
        if let Err(e) = std::fs::create_dir_all(&dir) {
            warn!(dir = %dir.display(), error = %e, "LLM_SESSION_DIR unusable; session persistence disabled");
            return None;
        }
        Some(Self { dir })
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

    /// Age after which a leftover `.lock` is considered stale — the
    /// holder crashed mid-save. Fresh locks are honored.
    const STALE_LOCK_SECS: u64 = 300;

    fn path(&self, session_id: &str) -> Option<PathBuf> {
        Self::safe_name(session_id).map(|n| self.dir.join(format!("{n}.json")))
    }

    fn lock_path(&self, session_id: &str) -> Option<PathBuf> {
        Self::safe_name(session_id).map(|n| self.dir.join(format!("{n}.lock")))
    }

    fn done_path(&self, session_id: &str) -> Option<PathBuf> {
        Self::safe_name(session_id).map(|n| self.dir.join(format!("{n}.done")))
    }

    /// Acquire the single-writer lock for a session: atomic
    /// `create_new` — the file's existence IS the lock. A stale lock
    /// (holder crashed over `STALE_LOCK_SECS` ago) is reclaimed once.
    fn acquire_lock(&self, session_id: &str) -> Option<StoreLock> {
        let path = self.lock_path(session_id)?;
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(mut f) => {
                let _ = f.write_all(std::process::id().to_string().as_bytes());
                Some(StoreLock { path })
            }
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                let stale = std::fs::metadata(&path)
                    .and_then(|m| m.modified())
                    .ok()
                    .and_then(|t| t.elapsed().ok())
                    .map(|age| age.as_secs() > Self::STALE_LOCK_SECS)
                    .unwrap_or(false);
                if stale {
                    let _ = std::fs::remove_file(&path);
                    return std::fs::OpenOptions::new()
                        .write(true)
                        .create_new(true)
                        .open(&path)
                        .ok()
                        .map(|_| StoreLock { path });
                }
                warn!(
                    session_id,
                    "snapshot lock held by another writer; save skipped"
                );
                None
            }
            Err(e) => {
                warn!(error = %e, session_id, "snapshot lock acquisition failed");
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
    /// lock → tmp+fsync → rename → `.done` commit marker → dir fsync.
    /// A crash anywhere before `.done` leaves a load() that answers
    /// corrupt (uncommitted), never a half-session.
    pub fn save(&self, session_id: &str, working_dir: &Path, messages: &[Value]) -> bool {
        let Some(path) = self.path(session_id) else {
            warn!(session_id, "unsanitizable session id; snapshot skipped");
            return false;
        };
        // Single-writer: the lock exists before any side effect, and a
        // second live writer skips rather than racing the rename.
        let Some(_lock) = self.acquire_lock(session_id) else {
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

        // Cumulative quotas — before touching the filesystem.
        if body.len() as u64 > Self::MAX_SNAPSHOT_BYTES {
            warn!(
                session_id,
                bytes = body.len(),
                "snapshot exceeds per-file cap; save skipped"
            );
            return false;
        }
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

        let tmp = path.with_extension("json.tmp");
        let committed = (|| -> io::Result<()> {
            // tmp write + fsync BEFORE rename: a renamed-but-unflushed
            // file can still lose contents on a power cut.
            {
                let mut f = std::fs::File::create(&tmp)?;
                f.write_all(body.as_bytes())?;
                f.sync_all()?;
            }
            std::fs::rename(&tmp, &path)?;
            // Commit marker: the .done file existing means a save fully
            // completed — its absence on load means the snapshot is an
            // uncommitted leftover, i.e. corrupt.
            if let Some(done) = self.done_path(session_id) {
                let mut d = std::fs::File::create(&done)?;
                d.write_all(format!("format_version=1\npid={}\n", std::process::id()).as_bytes())?;
                d.sync_all()?;
            }
            // Flush the directory so the rename + marker survive a crash.
            #[cfg(unix)]
            {
                let _ = std::fs::File::open(&self.dir).and_then(|d| d.sync_all());
            }
            Ok(())
        })();

        match committed {
            Ok(()) => {
                debug!(session_id, "session snapshot saved");
                true
            }
            Err(e) => {
                warn!(error = %e, session_id, "session snapshot save failed");
                if let Some(done) = self.done_path(session_id) {
                    let _ = std::fs::remove_file(done);
                }
                false
            }
        }
    }

    /// G1a-Q3 quota-triggered cleanup: evict the OLDEST stored sessions
    /// that are not in `active` — snapshots (.json/.done/.lock), any
    /// `.tmp` stragglers, and that session's artifact dir — until the
    /// store is back under quota or nothing removable remains. An
    /// active session's data is never touched; a `.done` is not
    /// required for eviction (an uncommitted snapshot is already
    /// worthless). Returns evicted session ids.
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
                if Self::safe_name(id).is_none() || active.contains(id) {
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
            for suffix in ["json", "done", "lock", "json.tmp"] {
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
        // The .done marker is the commit proof: a .json without one is
        // either pre-marker format or an interrupted save — both corrupt.
        let done = self.done_path(session_id);
        if done.map(|d| !d.exists()).unwrap_or(true) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "snapshot lacks commit marker (uncommitted or pre-marker write)",
            ));
        }
        let text = std::fs::read_to_string(&path).map_err(|e| {
            if e.kind() == io::ErrorKind::NotFound {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "marker exists but snapshot body missing",
                )
            } else {
                e
            }
        })?;
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
        Ok(Some(LoadedSession {
            working_dir,
            messages,
        }))
    }

    /// Remove a session's snapshot + commit marker (session/end,
    /// session/close). The lock is left alone — deleting someone
    /// else's held lock would unlock their in-flight save.
    pub fn delete(&self, session_id: &str) {
        if let Some(done) = self.done_path(session_id) {
            let _ = std::fs::remove_file(done);
        }
        if let Some(path) = self.path(session_id) {
            if let Err(e) = std::fs::remove_file(&path) {
                if e.kind() != io::ErrorKind::NotFound {
                    warn!(error = %e, path = %path.display(), "session snapshot delete failed");
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

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

    /// A held `.lock` blocks a second writer; once released, saves resume.
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
        store.save("s2", Path::new("/tmp"), &[json!({"role":"x"})]);
        assert!(
            dir.join("s2.json").exists(),
            "save must proceed after release"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A stale lock (older than STALE_LOCK_SECS) is reclaimed.
    #[test]
    fn stale_lock_is_reclaimed() {
        let dir = fresh_dir("stale");
        let store = SessionStore { dir: dir.clone() };
        let lock = dir.join("s3.lock");
        std::fs::write(&lock, "999999").unwrap();
        let old = std::time::SystemTime::now()
            - std::time::Duration::from_secs(SessionStore::STALE_LOCK_SECS + 60);
        std::fs::File::options()
            .write(true)
            .open(&lock)
            .unwrap()
            .set_modified(old)
            .unwrap();
        store.save("s3", Path::new("/tmp"), &[json!({"role":"x"})]);
        assert!(dir.join("s3.json").exists(), "stale lock must be reclaimed");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Strict schema: wrong version / wrong session_id / missing
    /// working_dir are all corrupt, not silently defaulted.
    #[test]
    fn load_rejects_schema_violations() {
        let dir = fresh_dir("schema");
        std::fs::write(dir.join("s4.done"), "format_version=1\n").unwrap();
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
            std::fs::write(dir.join("s4.json"), doc.to_string()).unwrap();
            let store = SessionStore { dir: dir.clone() };
            assert!(store.load("s4").is_err(), "must reject: {doc}");
        }
        // And a valid one still loads.
        std::fs::write(
            dir.join("s4.json"),
            json!({"format_version":1,"session_id":"s4","working_dir":"/tmp",
                   "messages":[{"role":"x"}],"message_count":1})
            .to_string(),
        )
        .unwrap();
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
        let dir = std::env::temp_dir().join(format!(
            "acp-store-corrupt-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        // A parseable snapshot whose recorded count lies about its
        // contents — truncated-but-valid-JSON write, or tampering.
        // Committed marker is written too so this tests the COUNT
        // check, not the missing-marker check.
        std::fs::write(dir.join("sess-9.done"), "format_version=1\n").unwrap();
        std::fs::write(
            dir.join("sess-9.json"),
            json!({
                "format_version": 1,
                "session_id": "sess-9",
                "working_dir": "/tmp",
                "messages": [{"role": "system", "content": "sys"}],
                "message_count": 7,
                "writer_pid": 1,
            })
            .to_string(),
        )
        .unwrap();
        let store = SessionStore { dir: dir.clone() };
        assert!(
            store.load("sess-9").is_err(),
            "count-mismatched snapshot must load as corrupt"
        );
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
