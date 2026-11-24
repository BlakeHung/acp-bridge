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
use tracing::{debug, warn};

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
        let mut files = 0usize;
        let mut bytes = 0u64;
        if let Ok(rd) = std::fs::read_dir(&self.dir) {
            for e in rd.flatten() {
                if let Ok(m) = e.metadata() {
                    files += 1;
                    bytes += m.len();
                }
            }
        }
        (files, bytes)
    }

    /// Rewrite the session's snapshot atomically and durably:
    /// lock → tmp+fsync → rename → `.done` commit marker → dir fsync.
    /// A crash anywhere before `.done` leaves a load() that answers
    /// corrupt (uncommitted), never a half-session.
    pub fn save(&self, session_id: &str, working_dir: &Path, messages: &[Value]) {
        let Some(path) = self.path(session_id) else {
            warn!(session_id, "unsanitizable session id; snapshot skipped");
            return;
        };
        // Single-writer: the lock exists before any side effect, and a
        // second live writer skips rather than racing the rename.
        let Some(_lock) = self.acquire_lock(session_id) else {
            return;
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
            return;
        }
        let (files, bytes) = self.store_usage();
        if files >= Self::MAX_STORE_FILES {
            warn!(
                session_id,
                files, "store file-count cap reached; save skipped"
            );
            return;
        }
        if bytes + body.len() as u64 > Self::MAX_STORE_BYTES {
            warn!(session_id, bytes, "store byte cap reached; save skipped");
            return;
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
            Ok(()) => debug!(session_id, "session snapshot saved"),
            Err(e) => {
                warn!(error = %e, session_id, "session snapshot save failed");
                // Uncommitted state must not be confused for a snapshot:
                // remove a partial .done (create order puts it last, so
                // on failure it shouldn't exist — belt and suspenders).
                if let Some(done) = self.done_path(session_id) {
                    let _ = std::fs::remove_file(done);
                }
            }
        }
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
}
