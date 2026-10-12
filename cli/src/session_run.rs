//! Session run helpers — bounded binding to `SessionManager` and policy.
//!
//! Provides filesystem helpers for session persistence that mirror the
//! executor's `RUNTIMO_SESSIONS_DIR` / `data_dir()` convention. No daemon
//! changes; all writes use the same JSON-on-disk format as `core/src/session.rs`.

use runtimo_core::session::SessionStatus;
use std::path::{Path, PathBuf};

/// Returns the sessions directory, honoring `RUNTIMO_SESSIONS_DIR` when set.
///
/// Mirrors `core/src/executor.rs` fallback: `data_dir().join("sessions")`.
#[must_use]
pub fn sessions_dir() -> PathBuf {
    std::env::var("RUNTIMO_SESSIONS_DIR").map_or_else(
        |_| runtimo_core::utils::data_dir().join("sessions"),
        PathBuf::from,
    )
}

/// Updates a session's status to `Completed` or `Terminated`.
///
/// Reads the session file at `<sessions_dir>/<session_id>.json`, mutates the
/// `status` and `updated_at` fields, and writes it back. The write is
/// atomic via a temp-file + rename to avoid partial writes.
///
/// # Errors
/// Returns `Err` when the session ID is invalid (traversal), the file cannot
/// be read/parsed, or the write fails.
pub fn update_session_status(
    dir: &Path,
    session_id: &str,
    status: SessionStatus,
) -> Result<(), String> {
    // Validate session_id via the same rule as SessionManager.
    if session_id.is_empty()
        || session_id.contains('/')
        || session_id.contains('\\')
        || session_id.contains('\0')
        || session_id.contains("..")
    {
        return Err(format!(
            "Invalid session ID '{}': must be non-empty without '/', '\\', NUL, or '..'",
            session_id
        ));
    }

    let path = dir.join(format!("{}.json", session_id));
    let content = std::fs::read_to_string(&path)
        .map_err(|e| format!("Failed to read session '{}': {}", session_id, e))?;

    let mut sess: runtimo_core::session::Session = serde_json::from_str(&content)
        .map_err(|e| format!("Failed to parse session '{}': {}", session_id, e))?;

    sess.status = status;
    sess.updated_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    let serialized = serde_json::to_string_pretty(&sess)
        .map_err(|e| format!("Failed to serialize session: {}", e))?;

    // Ensure parent exists.
    std::fs::create_dir_all(dir).map_err(|e| format!("create sessions dir: {}", e))?;

    // Atomic write: write to temp then rename.
    let tmp = dir.join(format!(".{}.tmp", session_id));
    std::fs::write(&tmp, serialized).map_err(|e| format!("write tmp: {}", e))?;
    std::fs::rename(&tmp, &path).map_err(|e| format!("rename session: {}", e))?;

    Ok(())
}

/// Finds an existing session by `name` or `id` in `dir`.
///
/// Returns `Some(session)` when a session with `name == needle` or `id == needle`
/// exists, otherwise `None`. Used to implement resume-append semantics: if a
/// `runtimo session run --session <name>` names an existing session, the loop
/// reuses its ID and appends new job_ids; otherwise a fresh session is created.
#[must_use]
pub fn find_session_by_name_or_id(
    dir: &Path,
    needle: &str,
) -> Option<runtimo_core::session::Session> {
    let mgr = runtimo_core::session::SessionManager::new(dir.to_path_buf()).ok()?;
    let sessions = mgr.list_sessions().ok()?;
    for s in sessions {
        if s.id == needle {
            return Some(s);
        }
        if let Some(n) = &s.name {
            if n == needle {
                return Some(s);
            }
        }
    }
    None
}

#[cfg(test)]
// test code uses unwrap/expect for brevity — panicking on failure is desired test behavior
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use runtimo_core::session::{Session, SessionManager, SessionStatus};

    fn tmp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("runtimo_test_session_run_{}", name));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// End-to-end chain over the real components: `SessionManager` creates
    /// and persists the session, `update_session_status` mutates it on
    /// disk, and `SessionManager::load_session` reads the result back.
    #[test]
    fn update_session_status_roundtrip() {
        let dir = tmp_dir("roundtrip");
        let mut mgr = SessionManager::new(dir.clone()).unwrap();
        let session = mgr.create_session(Some("roundtrip-session")).unwrap();
        mgr.add_job(&session.id, "job-abc").unwrap();
        let created_at = session.created_at;

        update_session_status(&dir, &session.id, SessionStatus::Completed)
            .expect("status update must succeed");

        let loaded = mgr.load_session(&session.id).unwrap();
        assert_eq!(loaded.status, SessionStatus::Completed);
        assert_eq!(loaded.name, Some("roundtrip-session".to_string()));
        assert_eq!(loaded.job_ids, vec!["job-abc".to_string()]);
        assert_eq!(
            loaded.created_at, created_at,
            "created_at must be preserved"
        );
        assert!(
            loaded.updated_at >= created_at,
            "updated_at must be bumped, got {} < {}",
            loaded.updated_at,
            created_at
        );

        // The on-disk JSON must carry the new status, not just memory.
        let raw = std::fs::read_to_string(dir.join(format!("{}.json", session.id))).unwrap();
        assert!(
            raw.contains("\"completed\""),
            "on-disk session JSON must record the completed status, got: {}",
            raw
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Traversal-shaped session IDs must be rejected before any filesystem
    /// write, and a decoy file at the traversal target must survive
    /// untouched — proving the rejection is not a downstream read failure.
    #[test]
    fn update_session_status_rejects_traversal_ids() {
        let dir = tmp_dir("traversal");
        let decoy = Session {
            id: "victim".to_string(),
            name: Some("decoy".to_string()),
            job_ids: Vec::new(),
            created_at: 1,
            updated_at: 1,
            status: SessionStatus::Active,
        };
        let decoy_path = dir
            .parent()
            .unwrap()
            .join("runtimo_test_session_run_victim.json");
        std::fs::write(&decoy_path, serde_json::to_string_pretty(&decoy).unwrap()).unwrap();

        for bad_id in [
            "../runtimo_test_session_run_victim",
            "a/b",
            "..",
            "",
            "bad\0id",
        ] {
            let result = update_session_status(&dir, bad_id, SessionStatus::Terminated);
            assert!(
                result.is_err(),
                "session ID '{}' must be rejected",
                bad_id.escape_debug()
            );
            let msg = result.unwrap_err();
            assert!(
                msg.contains("Invalid session ID"),
                "rejection must come from ID validation, got: {}",
                msg
            );
        }

        let decoy_after = std::fs::read_to_string(&decoy_path).unwrap();
        assert!(
            decoy_after.contains("\"active\""),
            "decoy session must be untouched by rejected updates, got: {}",
            decoy_after
        );

        let _ = std::fs::remove_file(&decoy_path);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A well-formed but unknown session ID must surface the read failure
    /// rather than silently succeeding.
    #[test]
    fn update_session_status_missing_session_errors() {
        let dir = tmp_dir("missing");
        let result = update_session_status(&dir, "no-such-session-id", SessionStatus::Completed);
        assert!(result.is_err(), "updating a nonexistent session must error");
        let msg = result.unwrap_err();
        assert!(
            msg.contains("Failed to read session"),
            "error must name the read failure, got: {}",
            msg
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Lookup by name, by id, and the miss case (returns `None`).
    #[test]
    fn find_session_by_name_or_id_matches_name_and_id() {
        let dir = tmp_dir("find");
        let mut mgr = SessionManager::new(dir.clone()).unwrap();
        let named = mgr.create_session(Some("named-session")).unwrap();
        let unnamed = mgr.create_session(None).unwrap();

        let by_name = find_session_by_name_or_id(&dir, "named-session")
            .expect("lookup by name must find the session");
        assert_eq!(by_name.id, named.id);

        let by_id = find_session_by_name_or_id(&dir, &unnamed.id)
            .expect("lookup by id must find the session");
        assert_eq!(by_id.id, unnamed.id);

        assert!(
            find_session_by_name_or_id(&dir, "no-such-name").is_none(),
            "unknown name must return None"
        );
        assert!(
            find_session_by_name_or_id(&dir, "deadbeef").is_none(),
            "unknown id must return None"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An empty (freshly created) sessions directory yields `None` for any
    /// needle — the resume path treats this as "create a new session".
    #[test]
    fn find_session_in_empty_dir_returns_none() {
        let dir = std::env::temp_dir().join("runtimo_test_session_run_empty_find");
        let _ = std::fs::remove_dir_all(&dir);
        assert!(
            find_session_by_name_or_id(&dir, "anything").is_none(),
            "empty sessions dir must return None"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
