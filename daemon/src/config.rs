//! Daemon configuration: data directory, socket path, WAL path resolution.
//!
//! Provides default path resolution for the daemon's runtime data directory,
//! Unix socket, and write-ahead log. All paths can be overridden via
//! environment variables (`XDG_DATA_HOME`, `RUNTIMO_WAL_PATH`).
//!
//! # Ownership
//! Owns path resolution and data directory lifecycle.
//!
//! # Dependencies
//! - `runtimo_core::utils` for `data_dir()` and `wal_path()` base resolution.

use std::path::PathBuf;

/// Returns the root data directory for Runtimo runtime data.
///
/// Delegates to `runtimo_core::utils::data_dir()` which uses `XDG_DATA_HOME`
/// with fallback to the default XDG path.
pub fn data_dir() -> PathBuf {
    runtimo_core::utils::data_dir()
}

/// Returns the default Unix socket path (`{data_dir}/runtimo.sock`).
pub fn default_socket_path() -> PathBuf {
    data_dir().join("runtimo.sock")
}

/// Returns the default WAL path (env-overridable via `RUNTIMO_WAL_PATH`).
pub fn default_wal_path() -> PathBuf {
    runtimo_core::utils::wal_path()
}

/// Ensures the data directory exists, creating it recursively if needed.
pub fn ensure_data_dir() -> std::io::Result<()> {
    let dir = data_dir();
    std::fs::create_dir_all(&dir)?;
    Ok(())
}
