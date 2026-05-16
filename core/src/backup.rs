//! Backup manager for undo/rollback functionality.
//!
//! Creates timestamped backups of files before mutation, enabling restoration
//! to the pre-mutation state. Backups are organized under a root directory
//! with subdirectories per job ID.
//!
//! # Example
//!
//! ```rust,ignore
//! use runtimo_core::BackupManager;
//! use std::path::PathBuf;
//!
//! let mgr = BackupManager::new(PathBuf::from("/tmp/backups"));
//! let backup = mgr.create_backup(
//!     &PathBuf::from("/tmp/config.toml"),
//!     "job-abc123",
//! ).unwrap();
//!
//! // After a failed mutation, restore:
//! mgr.restore(&backup, &PathBuf::from("/tmp/config.toml")).unwrap();
//! ```

use crate::Result;
use std::path::{Path, PathBuf};

/// Manages file backups for undo/rollback operations.
///
/// Backups are stored under `{backup_dir}/{job_id}/{filename}`. The manager
/// creates directories as needed.
pub struct BackupManager {
    backup_dir: PathBuf,
}

impl BackupManager {
    /// Creates a new backup manager rooted at `backup_dir`.
    ///
    /// The directory is created (recursively) if it does not exist.
    ///
    /// # Errors
    ///
    /// Returns [`crate::Error::BackupError`] if the backup
    /// directory cannot be created.
    pub fn new(backup_dir: PathBuf) -> Result<Self> {
        std::fs::create_dir_all(&backup_dir)
            .map_err(|e| crate::Error::BackupError(format!("Failed to create backup directory: {}", e)))?;
        Ok(Self { backup_dir })
    }

    /// Creates a backup of a file before mutation.
    ///
    /// Copies `file_path` to `{backup_dir}/{job_id}/{filename}`.
    ///
    /// # Arguments
    ///
    /// * `file_path` — Path to the file to back up
    /// * `job_id` — Job ID used as the backup subdirectory name
    ///
    /// # Returns
    ///
    /// The path to the backup file.
    ///
    /// # Errors
    ///
    /// Returns [`crate::Error::BackupError`] if the source
    /// file does not exist or the copy fails.
    pub fn create_backup(&self, file_path: &Path, job_id: &str) -> Result<PathBuf> {
        if !file_path.exists() {
            return Err(crate::Error::BackupError("File does not exist".to_string()));
        }

        let backup_path = self
            .backup_dir
            .join(job_id)
            .join(
                file_path
                    .file_name()
                    .ok_or_else(|| crate::Error::BackupError("Invalid filename".to_string()))?,
            );
        
        let parent = backup_path.parent()
            .ok_or_else(|| crate::Error::BackupError("Invalid backup path".to_string()))?;
        std::fs::create_dir_all(parent)
            .map_err(|e| crate::Error::BackupError(format!("Failed to create backup directory: {}", e)))?;

        std::fs::copy(file_path, &backup_path)
            .map_err(|e| crate::Error::BackupError(e.to_string()))?;

        Ok(backup_path)
    }

    /// Restores a file from a backup.
    ///
    /// Copies `backup_path` to `target_path`, overwriting the target.
    ///
    /// # Errors
    ///
    /// Returns [`crate::Error::BackupError`] if the backup
    /// file does not exist or the copy fails.
    pub fn restore(&self, backup_path: &Path, target_path: &Path) -> Result<()> {
        if !backup_path.exists() {
            return Err(crate::Error::BackupError(
                "Backup does not exist".to_string(),
            ));
        }

        std::fs::copy(backup_path, target_path)
            .map_err(|e| crate::Error::BackupError(e.to_string()))?;

        Ok(())
    }

    /// Deletes old backups older than the given age.
    ///
    /// Scans `backup_dir` for job subdirectories and removes those whose
    /// modification time is older than `older_than_secs`.
    pub fn cleanup(&self, older_than_secs: u64) -> Result<()> {
        use std::time::{SystemTime, UNIX_EPOCH};

        let cutoff = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
            .saturating_sub(older_than_secs);

        if !self.backup_dir.exists() {
            return Ok(());
        }

        for entry in std::fs::read_dir(&self.backup_dir)
            .map_err(|e| crate::Error::BackupError(e.to_string()))?
        {
            let entry = entry.map_err(|e| crate::Error::BackupError(e.to_string()))?;
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }

            let mtime = path
                .metadata()
                .map_err(|e| crate::Error::BackupError(e.to_string()))?
                .modified()
                .map_err(|e| crate::Error::BackupError(e.to_string()))?
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();

            if mtime < cutoff {
                std::fs::remove_dir_all(&path)
                    .map_err(|e| crate::Error::BackupError(e.to_string()))?;
            }
        }

        Ok(())
    }
}
