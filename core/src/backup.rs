//! Backup manager for undo/rollback functionality.
//!
//! Creates timestamped backups of files before mutation, enabling restoration
//! to the pre-mutation state. Backups are organized under a root directory
//! with subdirectories per job ID.
//!
//! # Security
//!
//! ## Backup Directory Symlink Check (FINDING #11)
//! The backup directory is verified to be a real directory (not a symlink)
//! during construction. This prevents an attacker from redirecting backups
//! to an arbitrary location via symlink substitution.
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

/// Checks that a path is a real directory, not a symlink (FINDING #11).
///
/// Returns `Err` if the path is a symlink, even if the symlink target is
/// a valid directory. This prevents symlink substitution attacks.
fn verify_real_directory(path: &Path) -> Result<()> {
    if path.symlink_metadata()
        .map_err(|e| crate::Error::BackupError(format!("cannot stat {}: {}", path.display(), e)))?
        .file_type()
        .is_symlink()
    {
        return Err(crate::Error::BackupError(format!(
            "backup directory is a symlink: {} (symlink attacks not allowed)",
            path.display()
        )));
    }
    Ok(())
}

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
        std::fs::create_dir_all(&backup_dir).map_err(|e| {
            crate::Error::BackupError(format!("Failed to create backup directory: {}", e))
        })?;
        // Verify backup_dir is a real directory, not a symlink (FINDING #11)
        verify_real_directory(&backup_dir)?;
        Ok(Self { backup_dir })
    }

    /// Creates a backup of a file before mutation.
    ///
    /// Copies `file_path` to `{backup_dir}/{job_id}/{filename}`.
    /// If a backup already exists, appends a numeric suffix (`.1`, `.2`, etc.)
    /// to preserve all pre-mutation states. The first backup (no suffix) always
    /// contains the original file state before any writes in this job.
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

        let base_name = file_path
            .file_name()
            .ok_or_else(|| crate::Error::BackupError("Invalid filename".to_string()))?;

        let job_dir = self.backup_dir.join(job_id);
        let parent = job_dir.clone();
        std::fs::create_dir_all(&parent).map_err(|e| {
            crate::Error::BackupError(format!("Failed to create backup directory: {}", e))
        })?;

        // Find first available backup path (no suffix, then .1, .2, ...)
        let backup_path = {
            let candidate = job_dir.join(base_name);
            if !candidate.exists() {
                candidate
            } else {
                let mut counter = 1;
                loop {
                    let suffixed = job_dir.join(format!("{}.{}", base_name.to_string_lossy(), counter));
                    if !suffixed.exists() {
                        break suffixed;
                    }
                    counter += 1;
                }
            }
        };

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_new_creates_directory() {
        let dir = std::env::temp_dir().join("runtimo_backup_test_new");
        let _ = std::fs::remove_dir_all(&dir);
        let result = BackupManager::new(dir.clone());
        assert!(result.is_ok(), "should create directory");
        assert!(dir.exists());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_rejects_symlink_backup_dir() {
        let target = std::env::temp_dir().join("runtimo_backup_target");
        let link = std::env::temp_dir().join("runtimo_backup_link");
        let _ = std::fs::remove_dir_all(&target);
        let _ = std::fs::remove_file(&link);

        std::fs::create_dir_all(&target).unwrap();

        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;
            if symlink(&target, &link).is_ok() {
                let result = BackupManager::new(link.clone());
                assert!(
                    result.is_err(),
                    "BackupManager should reject symlink backup directory"
                );
                let err = result.err().unwrap().to_string();
                assert!(err.contains("symlink"), "error should mention symlink: {}", err);
                std::fs::remove_file(&link).ok();
            }
        }

        std::fs::remove_dir_all(&target).ok();
    }

    #[test]
    fn test_verify_real_directory() {
        let dir = std::env::temp_dir().join("runtimo_verify_test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let result = verify_real_directory(&dir);
        assert!(result.is_ok(), "real directory should pass: {:?}", result);

        std::fs::remove_dir_all(&dir).ok();
    }
}
