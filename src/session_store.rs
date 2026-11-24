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
use std::io;
use std::path::{Path, PathBuf};
use tracing::{debug, warn};

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

    fn path(&self, session_id: &str) -> Option<PathBuf> {
        Self::safe_name(session_id).map(|n| self.dir.join(format!("{n}.json")))
    }

    /// Rewrite the session's snapshot atomically (temp + rename so a
    /// crash mid-write leaves the previous snapshot intact).
    pub fn save(&self, session_id: &str, working_dir: &Path, messages: &[Value]) {
        let Some(path) = self.path(session_id) else {
            warn!(session_id, "unsanitizable session id; snapshot skipped");
            return;
        };
        let doc = json!({
            "format_version": 1,
            "session_id": session_id,
            "working_dir": working_dir.display().to_string(),
            "messages": messages,
            // Completeness + provenance markers: `message_count` lets a
            // reader detect a truncated-but-parseable write; `writer_pid`
            // records which process wrote last (diagnostic — this store
            // assumes a single writer per session and does not enforce
            // it; two live bridges sharing a dir WILL race on the same
            // session id, documented limitation).
            "message_count": messages.len(),
            "writer_pid": std::process::id(),
        });
        let tmp = path.with_extension("json.tmp");
        match std::fs::write(&tmp, doc.to_string()) {
            Ok(()) => {
                if let Err(e) = std::fs::rename(&tmp, &path) {
                    warn!(error = %e, path = %path.display(), "session snapshot rename failed");
                } else {
                    debug!(session_id, "session snapshot saved");
                }
            }
            Err(e) => warn!(error = %e, "session snapshot write failed"),
        }
    }

    /// Load a snapshot. Returns `Ok(None)` when none exists — that is
    /// "session not found", distinct from a corrupt file which is an
    /// error the caller should surface.
    pub fn load(&self, session_id: &str) -> io::Result<Option<LoadedSession>> {
        let Some(path) = self.path(session_id) else {
            return Ok(None);
        };
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e),
        };
        let doc: Value = serde_json::from_str(&text).map_err(|e| {
            io::Error::new(io::ErrorKind::InvalidData, format!("corrupt snapshot: {e}"))
        })?;
        let working_dir = doc["working_dir"]
            .as_str()
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."));
        let messages = doc["messages"].as_array().cloned().unwrap_or_default();
        if messages.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "snapshot has no messages",
            ));
        }
        // Completeness check: a snapshot whose recorded count doesn't
        // match its message array is partially-written or tampered —
        // corrupt, not a session.
        if let Some(recorded) = doc["message_count"].as_u64() {
            if recorded as usize != messages.len() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "snapshot message_count {recorded} != actual {}",
                        messages.len()
                    ),
                ));
            }
        }
        Ok(Some(LoadedSession {
            working_dir,
            messages,
        }))
    }

    /// Remove a session's snapshot (session/end, session/close).
    pub fn delete(&self, session_id: &str) {
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
