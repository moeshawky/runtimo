//! Session tracking for reliable SSH.
//!
//! Sessions group related job executions together, supporting:
//! - Session resume after disconnect
//! - Audit trail per session
//! - Batch undo/rollback
//!
//! # Security Note (FINDING #18)
//!
//! Session IDs are used for **audit grouping only**, not for authentication
//! or authorization. They are not security tokens and should not be treated
//! as such. The current ID generation uses 16 random bytes from `/dev/urandom`
//! (via `utils::generate_id()`), which provides sufficient collision resistance
//! for audit purposes.
//!
//! If cryptographic uniqueness is required (e.g., for auth tokens), switch to
//! UUID v4 via the `uuid` crate. For audit grouping, the current approach is
//! adequate — P(collision) < 10⁻¹⁵ even at 100 sessions/sec for 1 hour.
//!
//! # Example
//!
//! ```rust,ignore
//! use runtimo_core::session::{Session, SessionManager};
//! use std::path::PathBuf;
//!
//! let mut mgr = SessionManager::new(PathBuf::from("/tmp/sessions")).unwrap();
//! let session = mgr.create_session(Some("ssh-import")).unwrap();
//! println!("Session ID: {}", session.id);
//! ```

use crate::Result;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

/// A session groups related jobs for audit and recovery.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(clippy::exhaustive_structs)]
pub struct Session {
    /// Unique session identifier.
    pub id: String,
    /// Optional human-readable name.
    pub name: Option<String>,
    /// Job IDs executed in this session.
    pub job_ids: Vec<String>,
    /// Unix timestamp when session was created.
    pub created_at: u64,
    /// Unix timestamp of last activity.
    pub updated_at: u64,
    /// Session status.
    pub status: SessionStatus,
}

/// Session lifecycle status.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[allow(clippy::exhaustive_enums)]
pub enum SessionStatus {
    /// Session is active and accepting jobs.
    Active,
    /// Session has been paused (e.g., disconnect).
    Paused,
    /// Session completed normally.
    Completed,
    /// Session terminated abnormally.
    Terminated,
}

/// Persistent session store backed by the filesystem.
///
/// Sessions are stored as individual JSON files under a `sessions/` directory.
/// Each session tracks its associated job IDs, status, and creation timestamps.
#[allow(clippy::exhaustive_structs)]
pub struct SessionManager {
    sessions_dir: PathBuf,
}

impl SessionManager {
    /// Creates a new session manager.
    ///
    /// # Errors
    /// Returns `SessionError` if the sessions directory cannot be created.
    pub fn new(sessions_dir: PathBuf) -> Result<Self> {
        std::fs::create_dir_all(&sessions_dir).map_err(|e| {
            crate::Error::SessionError(format!("Failed to create sessions dir: {}", e))
        })?;
        Ok(Self { sessions_dir })
    }

    /// Creates a new session with optional name.
    ///
    /// # Errors
    /// Returns `SessionError` if the session file cannot be written.
    pub fn create_session(&mut self, name: Option<&str>) -> Result<Session> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();
        let ts = now.as_secs();
        let id = crate::utils::generate_id();

        let session = Session {
            id,
            name: name.map(String::from),
            job_ids: Vec::new(),
            created_at: ts,
            updated_at: ts,
            status: SessionStatus::Active,
        };

        self.save_session(&session)?;
        Ok(session)
    }

    /// Loads a session from disk by ID.
    ///
    /// # Errors
    /// Returns `SessionError` if the session ID is invalid (empty, contains
    /// `/`, `\`, a NUL byte, or `..`), if the session file cannot be read,
    /// or if the session file cannot be parsed as JSON.
    pub fn load_session(&self, session_id: &str) -> Result<Session> {
        let path = self.session_path(session_id)?;
        let content = std::fs::read_to_string(&path).map_err(|e| {
            crate::Error::SessionError(format!("Session not found {}: {}", session_id, e))
        })?;
        serde_json::from_str(&content)
            .map_err(|e| crate::Error::SessionError(format!("Failed to parse session: {}", e)))
    }

    /// Adds a job to a session.
    ///
    /// # Errors
    /// Returns `SessionError` if the session ID is invalid (empty, contains
    /// `/`, `\`, a NUL byte, or `..`), if the session cannot be loaded, or
    /// if the session cannot be saved.
    pub fn add_job(&mut self, session_id: &str, job_id: &str) -> Result<()> {
        let mut session = self.load_session(session_id)?;
        session.job_ids.push(job_id.to_string());
        session.updated_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        self.save_session(&session)
    }

    /// Lists all sessions.
    ///
    /// # Errors
    /// Returns `SessionError` if the sessions directory cannot be read
    /// or a session file cannot be parsed.
    pub fn list_sessions(&self) -> Result<Vec<Session>> {
        let mut sessions = Vec::new();
        if !self.sessions_dir.exists() {
            return Ok(sessions);
        }

        for entry in std::fs::read_dir(&self.sessions_dir)
            .map_err(|e| crate::Error::SessionError(format!("Failed to read sessions: {}", e)))?
        {
            let entry = entry
                .map_err(|e| crate::Error::SessionError(format!("Failed to read entry: {}", e)))?;
            let path = entry.path();
            if path.extension().is_some_and(|ext| ext == "json") {
                if let Ok(content) = std::fs::read_to_string(&path) {
                    if let Ok(session) = serde_json::from_str(&content) {
                        sessions.push(session);
                    }
                }
            }
        }

        sessions.sort_by_key(|s: &Session| s.updated_at);
        sessions.reverse();
        Ok(sessions)
    }

    /// Validates a session ID before it is interpolated into a filesystem path.
    ///
    /// # Input
    ///
    /// `session_id: &str` — any caller-supplied identifier.
    ///
    /// # Output
    ///
    /// `Ok(())` when the ID is safe for `format!("{}.json", session_id)`.
    ///
    /// # Errors
    ///
    /// Returns `SessionError` when the ID is empty, contains a path separator
    /// (`/` or `\`), contains a NUL byte, or contains `..`.
    fn validate_session_id(session_id: &str) -> Result<()> {
        let invalid = session_id.is_empty()
            || session_id.contains('/')
            || session_id.contains('\\')
            || session_id.contains('\0')
            || session_id.contains("..");
        if invalid {
            return Err(crate::Error::SessionError(format!(
                "Invalid session ID '{}': must be non-empty without '/', '\\', NUL, or '..'",
                session_id
            )));
        }
        Ok(())
    }

    /// Resolves the on-disk path for a session ID.
    ///
    /// # Input
    ///
    /// `session_id: &str` — validates and maps to
    /// `<sessions_dir>/<session_id>.json`.
    ///
    /// # Output
    ///
    /// `Ok(PathBuf)` pointing at the session's JSON file.
    ///
    /// # Errors
    ///
    /// Returns `SessionError` from [`Self::validate_session_id`] when the ID
    /// would escape `<sessions_dir>`.
    fn session_path(&self, session_id: &str) -> Result<PathBuf> {
        Self::validate_session_id(session_id)?;
        Ok(self.sessions_dir.join(format!("{}.json", session_id)))
    }

    fn save_session(&self, session: &Session) -> Result<()> {
        let path = self.session_path(&session.id)?;
        let content = serde_json::to_string_pretty(session).map_err(|e| {
            crate::Error::SessionError(format!("Failed to serialize session: {}", e))
        })?;
        std::fs::write(&path, content)
            .map_err(|e| crate::Error::SessionError(format!("Failed to write session: {}", e)))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn tmp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("runtimo_test_sessions_{}", name));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn creates_session() {
        let dir = tmp_dir("creates");
        let mut mgr = SessionManager::new(dir).unwrap();
        let session = mgr.create_session(Some("test")).unwrap();
        assert!(!session.id.is_empty());
        assert_eq!(session.name, Some("test".to_string()));
        assert_eq!(session.job_ids.len(), 0);
    }

    #[test]
    fn adds_job_to_session() {
        let dir = tmp_dir("adds_job");
        let mut mgr = SessionManager::new(dir).unwrap();
        let session = mgr.create_session(None).unwrap();
        mgr.add_job(&session.id, "job-123").unwrap();

        let loaded = mgr.load_session(&session.id).unwrap();
        assert_eq!(loaded.job_ids.len(), 1);
        assert_eq!(loaded.job_ids[0], "job-123");
    }

    #[test]
    fn lists_sessions() {
        let dir = tmp_dir("lists");
        let mut mgr = SessionManager::new(dir).unwrap();
        let _ = mgr.create_session(Some("first")).unwrap();
        let _ = mgr.create_session(Some("second")).unwrap();

        let sessions = mgr.list_sessions().unwrap();
        assert_eq!(sessions.len(), 2);
    }

    #[test]
    fn rejects_traversal_session_ids() {
        let dir = tmp_dir("rejects_traversal");
        let mut mgr = SessionManager::new(dir).unwrap();
        for id in [
            "../victim",
            "..",
            "a/b",
            "a\\b",
            "/etc/passwd",
            "..\\..",
            "bad\0id",
        ] {
            assert!(
                mgr.load_session(id).is_err(),
                "'{}' must be rejected on load",
                id
            );
            assert!(
                mgr.add_job(id, "job-1").is_err(),
                "'{}' must be rejected on add_job",
                id
            );
        }
    }

    #[test]
    fn rejects_traversal_session_id_on_save() {
        let dir = tmp_dir("rejects_traversal_save");
        let mut mgr = SessionManager::new(dir).unwrap();
        let mut session = mgr.create_session(None).unwrap();
        session.id = "../victim".to_string();
        let result = mgr.save_session(&session);
        assert!(result.is_err(), "save_session must reject traversal ids");
    }
}
