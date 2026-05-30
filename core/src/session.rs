//! Session tracking for reliable SSH.
//!
//! Sessions group related job executions together, enabling:
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

/// Manages session persistence and retrieval.
pub struct SessionManager {
    sessions_dir: PathBuf,
}

impl SessionManager {
    /// Creates a new session manager.
    ///
    /// # Errors
    /// Returns an error if the sessions directory cannot be created.
    pub fn new(sessions_dir: PathBuf) -> Result<Self> {
        std::fs::create_dir_all(&sessions_dir).map_err(|e| {
            crate::Error::BackupError(format!("Failed to create sessions dir: {}", e))
        })?;
        Ok(Self { sessions_dir })
    }

    /// Creates a new session with optional name.
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

    /// Loads a session by ID.
    /// Loads a session from disk by ID.
    ///
    /// # Errors
    /// Returns `BackupError` if the session file cannot be read or parsed.
    pub fn load_session(&self, session_id: &str) -> Result<Session> {
        let path = self.session_path(session_id);
        let content = std::fs::read_to_string(&path)
            .map_err(|e| crate::Error::BackupError(format!("Session not found {}: {}", session_id, e)))?;
        serde_json::from_str(&content)
            .map_err(|e| crate::Error::BackupError(format!("Failed to parse session: {}", e)))
    }

    /// Adds a job to a session.
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
    pub fn list_sessions(&self) -> Result<Vec<Session>> {
        let mut sessions = Vec::new();
        if !self.sessions_dir.exists() {
            return Ok(sessions);
        }

        for entry in std::fs::read_dir(&self.sessions_dir)
            .map_err(|e| crate::Error::BackupError(format!("Failed to read sessions: {}", e)))?
        {
            let entry = entry
                .map_err(|e| crate::Error::BackupError(format!("Failed to read entry: {}", e)))?;
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

    fn session_path(&self, session_id: &str) -> PathBuf {
        self.sessions_dir.join(format!("{}.json", session_id))
    }

    fn save_session(&self, session: &Session) -> Result<()> {
        let path = self.session_path(&session.id);
        let content = serde_json::to_string_pretty(session).map_err(|e| {
            crate::Error::BackupError(format!("Failed to serialize session: {}", e))
        })?;
        std::fs::write(&path, content)
            .map_err(|e| crate::Error::BackupError(format!("Failed to write session: {}", e)))?;
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
}
