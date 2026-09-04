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
